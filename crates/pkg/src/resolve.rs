//! Dependency resolution (`docs/23` §7).
//!
//! Resolution turns the root manifest's `[dependencies]` into a fully-pinned
//! graph: it unifies version requirements (one version per compatible range),
//! fetches and verifies registry tarballs into the content-addressed store,
//! follows transitive dependencies (including feature-gated optional deps) to a
//! fixpoint, and produces a [`Lockfile`].
//! "Lockfile is truth": an existing lock that still satisfies the constraints is
//! preserved, so resolution is stable.
//!
//! Scope: registry and `path` dependencies with transitive resolution and
//! version unification are implemented and tested against a local fixture
//! registry. Git dependencies are fetched through a bare mirror cache, resolved
//! to exact commits, and materialized into immutable source checkouts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::lockfile::{LockSource, LockedPackage, Lockfile};
use crate::manifest::{DepSource, Manifest};
use crate::registry::Registry;
use crate::store::Store;
use crate::version::{self, Version, VersionReq};

/// The result of resolving a dependency graph.
#[derive(Debug)]
pub struct Resolved {
    /// The pinned lockfile.
    pub lockfile: Lockfile,
    /// Each resolved package's local source root and pinned source.
    pub packages: Vec<ResolvedPackage>,
    /// Dependency edges: package instance id → the package instance ids it
    /// directly depends on. The root package is keyed by [`Self::root_id`].
    pub edges: BTreeMap<String, Vec<String>>,
    /// Dependency-name edges: package instance id → dependency name → resolved
    /// package instance id. This preserves each package's own `pkg:name`
    /// context when different versions of the same package coexist.
    pub dependency_edges: BTreeMap<String, BTreeMap<String, String>>,
    /// The root package's name (the edge-map key for its direct dependencies).
    pub root_name: String,
    /// The synthetic root package instance id.
    pub root_id: String,
}

impl Resolved {
    /// Look up a resolved package by name.
    pub fn get(&self, name: &str) -> Option<&ResolvedPackage> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Look up all resolved package instances with this package name.
    pub fn packages_named<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a ResolvedPackage> {
        self.packages.iter().filter(move |p| p.name == name)
    }

    /// Look up a resolved package by package instance id.
    pub fn get_by_id(&self, id: &str) -> Option<&ResolvedPackage> {
        self.packages.iter().find(|p| p.id == id)
    }

    /// Return the package id for a direct root dependency name.
    pub fn direct_dependency_id(&self, name: &str) -> Option<&str> {
        self.dependency_edges
            .get(&self.root_id)
            .and_then(|deps| deps.get(name))
            .map(String::as_str)
    }

    /// True when more than one resolved package instance has `name`.
    pub fn has_duplicate_name(&self, name: &str) -> bool {
        self.packages_named(name).nth(1).is_some()
    }
}

/// One resolved package and where its source lives on disk.
#[derive(Clone, Debug)]
pub struct ResolvedPackage {
    /// Stable package-instance id used for graph edges and contextual `pkg:`
    /// imports. It is intentionally not part of the source-level package name.
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: LockSource,
    /// The directory holding the package's `project.toml` (path deps) or its
    /// extracted source (registry deps).
    pub root: PathBuf,
    /// Whether this is a direct dependency of the root package.
    pub direct: bool,
}

/// Why resolution failed.
#[derive(Debug)]
pub enum ResolveError {
    /// No version of `name` satisfies the combined requirements.
    NoMatch { name: String, reqs: Vec<String> },
    /// A named registry was referenced but not declared.
    UnknownRegistry { name: String, registry: String },
    /// A path dependency's directory or manifest is missing/invalid.
    Path { name: String, message: String },
    /// A git dependency could not be fetched or read.
    Git { name: String, message: String },
    /// A package's feature graph is malformed.
    Feature { package: String, message: String },
    /// A registry/transport/store failure.
    Backend(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoMatch { name, reqs } => write!(
                f,
                "no version of `{name}` satisfies all requirements: {}",
                reqs.join(", ")
            ),
            ResolveError::UnknownRegistry { name, registry } => {
                write!(
                    f,
                    "dependency `{name}` uses registry `{registry}`, which is not declared"
                )
            }
            ResolveError::Path { name, message } => {
                write!(f, "path dependency `{name}`: {message}")
            }
            ResolveError::Git { name, message } => {
                write!(f, "git dependency `{name}`: {message}")
            }
            ResolveError::Feature { package, message } => {
                write!(f, "feature resolution for `{package}`: {message}")
            }
            ResolveError::Backend(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// A set of registries the resolver can query, keyed by name, with a default.
pub struct Registries<'a> {
    pub by_name: HashMap<String, &'a dyn Registry>,
    pub default: String,
}

impl<'a> Registries<'a> {
    fn get(&self, name: Option<&str>, dep: &str) -> Result<&'a dyn Registry, ResolveError> {
        let key = name.unwrap_or(&self.default);
        self.by_name
            .get(key)
            .copied()
            .ok_or_else(|| ResolveError::UnknownRegistry {
                name: dep.to_string(),
                registry: key.to_string(),
            })
    }
}

/// Resolve `root`'s dependency graph against `registries`, fetching into
/// `store`. An `existing` lockfile is honored where it still satisfies the
/// constraints (lockfile stability).
pub fn resolve(
    root: &Manifest,
    root_dir: &Path,
    registries: &Registries,
    store: &Store,
    existing: Option<&Lockfile>,
) -> Result<Resolved, ResolveError> {
    let mut r = Run {
        registries,
        store,
        existing,
        root_id: root_id(&root.package.name),
        // package instance id -> all requirements gathered so far
        reqs: BTreeMap::new(),
        req_keys: BTreeMap::new(),
        features_of: BTreeMap::new(),
        processed_features: BTreeMap::new(),
        resolved: BTreeMap::new(),
        direct: Default::default(),
        edges: BTreeMap::new(),
        dependency_edges: BTreeMap::new(),
    };
    // Seed from the root manifest's direct dependencies.
    let root_id = r.root_id.clone();
    r.request_default_features(&root_id);
    r.seed(root, root_dir, true, &root_id)?;
    r.run()?;

    let mut internal: Vec<ResolvedInternal> = r.resolved.into_values().collect();
    internal.sort_by(|a, b| {
        (&a.name, &a.version, &a.source.encode()).cmp(&(&b.name, &b.version, &b.source.encode()))
    });
    let lockfile = Lockfile {
        version: crate::lockfile::LOCKFILE_VERSION,
        packages: internal
            .iter()
            .map(|p| LockedPackage {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source.clone(),
                checksum: match &p.source {
                    LockSource::Registry { .. } | LockSource::Git { .. } => p.checksum_hint.clone(),
                    LockSource::Path { .. } => None,
                },
            })
            .collect(),
    };
    let mut edges = r.edges;
    for v in edges.values_mut() {
        v.sort();
        v.dedup();
    }
    Ok(Resolved {
        lockfile,
        packages: internal.into_iter().map(Into::into).collect(),
        edges,
        dependency_edges: r.dependency_edges,
        root_name: root.package.name.clone(),
        root_id: root_id.clone(),
    })
}

struct Run<'a> {
    registries: &'a Registries<'a>,
    store: &'a Store,
    existing: Option<&'a Lockfile>,
    root_id: String,
    reqs: BTreeMap<String, Vec<VersionReq>>,
    req_keys: BTreeMap<String, ReqKey>,
    features_of: BTreeMap<String, BTreeSet<String>>,
    processed_features: BTreeMap<String, BTreeSet<String>>,
    resolved: BTreeMap<String, ResolvedInternal>,
    direct: std::collections::HashSet<String>,
    edges: BTreeMap<String, Vec<String>>,
    dependency_edges: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone)]
struct ResolvedInternal {
    id: String,
    name: String,
    version: String,
    source: LockSource,
    root: PathBuf,
    direct: bool,
    checksum_hint: Option<String>,
}

impl From<ResolvedInternal> for ResolvedPackage {
    fn from(r: ResolvedInternal) -> Self {
        ResolvedPackage {
            id: r.id,
            name: r.name,
            version: r.version,
            source: r.source,
            root: r.root,
            direct: r.direct,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReqKey {
    id: String,
    name: String,
    registry: String,
    compat: String,
}

impl ReqKey {
    fn new(name: &str, registry: String, req: &VersionReq) -> ReqKey {
        let compat = version::compat_key(req)
            .map(|(major, minor)| format!("{major}.{minor}"))
            .unwrap_or_else(|| format!("req:{}", req));
        let id = format!("registry:{registry}:{name}:{compat}");
        ReqKey {
            id,
            name: name.to_string(),
            registry,
            compat,
        }
    }
}

fn root_id(name: &str) -> String {
    format!("root:{name}")
}

fn path_id(name: &str, path: &Path) -> String {
    let digest = crate::store::sha256_hex(path.to_string_lossy().as_bytes());
    format!("path:{name}:{}", &digest[..16])
}

fn git_id(name: &str, url: &str) -> String {
    let digest = crate::store::sha256_hex(url.as_bytes());
    format!("git:{name}:{}", &digest[..16])
}

impl<'a> Run<'a> {
    /// Record a manifest's direct dependencies into the worklist. Path deps are
    /// resolved immediately (and recursed); registry deps accumulate reqs.
    fn seed(
        &mut self,
        manifest: &Manifest,
        base_dir: &Path,
        direct: bool,
        parent_id: &str,
    ) -> Result<(), ResolveError> {
        let feature_plan =
            FeaturePlan::from_manifest(manifest, &self.requested_features(parent_id))?;
        for (name, dep) in &manifest.dependencies {
            if dep.optional && !feature_plan.optional_deps.contains(name) {
                continue;
            }
            match &dep.source {
                DepSource::Registry {
                    version: req_str,
                    registry,
                } => {
                    let req = version::parse_req(req_str).map_err(|m| ResolveError::Backend(m))?;
                    let registry_name = registry
                        .clone()
                        .unwrap_or_else(|| self.registries.default.clone());
                    let key = ReqKey::new(name, registry_name, &req);
                    let dep_id = key.id.clone();
                    self.reqs.entry(dep_id.clone()).or_default().push(req);
                    self.req_keys.entry(dep_id.clone()).or_insert(key);
                    self.record_dependency(parent_id, name, &dep_id);
                    self.request_dependency_features(
                        &dep_id,
                        dep.default_features,
                        &dep.features,
                        feature_plan.dep_features.get(name),
                    );
                    if direct {
                        self.direct.insert(dep_id);
                    }
                }
                DepSource::Path { path } => {
                    let dep_dir = compiler::sema::resolve_ctx::normalize(&base_dir.join(path));
                    let dep_id = path_id(name, &dep_dir);
                    self.record_dependency(parent_id, name, &dep_id);
                    if direct {
                        self.direct.insert(dep_id.clone());
                    }
                    self.request_dependency_features(
                        &dep_id,
                        dep.default_features,
                        &dep.features,
                        feature_plan.dep_features.get(name),
                    );
                    self.resolve_path_dep(&dep_id, name, path, dep_dir, direct)?;
                }
                DepSource::Git { url, reference } => {
                    let dep_id = git_id(name, url);
                    self.record_dependency(parent_id, name, &dep_id);
                    if direct {
                        self.direct.insert(dep_id.clone());
                    }
                    self.request_dependency_features(
                        &dep_id,
                        dep.default_features,
                        &dep.features,
                        feature_plan.dep_features.get(name),
                    );
                    self.resolve_git_dep(&dep_id, name, url, reference, direct)?;
                }
            }
        }
        Ok(())
    }

    fn record_dependency(&mut self, parent_id: &str, dep_name: &str, dep_id: &str) {
        self.edges
            .entry(parent_id.to_string())
            .or_default()
            .push(dep_id.to_string());
        self.dependency_edges
            .entry(parent_id.to_string())
            .or_default()
            .insert(dep_name.to_string(), dep_id.to_string());
    }

    fn requested_features(&self, package: &str) -> BTreeSet<String> {
        self.features_of.get(package).cloned().unwrap_or_default()
    }

    fn request_default_features(&mut self, package: &str) {
        self.features_of
            .entry(package.to_string())
            .or_default()
            .insert("default".to_string());
    }

    fn request_dependency_features(
        &mut self,
        package: &str,
        default_features: bool,
        declared: &[String],
        from_parent_features: Option<&BTreeSet<String>>,
    ) {
        let set = self.features_of.entry(package.to_string()).or_default();
        if default_features {
            set.insert("default".to_string());
        }
        set.extend(declared.iter().cloned());
        if let Some(extra) = from_parent_features {
            set.extend(extra.iter().cloned());
        }
    }

    /// Resolve a `path` dependency: load its manifest, record it, and recurse.
    fn resolve_path_dep(
        &mut self,
        id: &str,
        name: &str,
        path: &str,
        dep_dir: PathBuf,
        direct: bool,
    ) -> Result<(), ResolveError> {
        if self.resolved.contains_key(id) {
            return Ok(());
        }
        let manifest_path = dep_dir.join(crate::project::MANIFEST_NAME);
        let text = std::fs::read_to_string(&manifest_path).map_err(|e| ResolveError::Path {
            name: name.to_string(),
            message: format!("cannot read {}: {e}", manifest_path.display()),
        })?;
        let manifest = Manifest::parse(&text).map_err(|e| ResolveError::Path {
            name: name.to_string(),
            message: e.to_string(),
        })?;
        let version = manifest.package.version.clone();
        self.resolved.insert(
            id.to_string(),
            ResolvedInternal {
                id: id.to_string(),
                name: name.to_string(),
                version,
                source: LockSource::Path {
                    path: path.to_string(),
                },
                root: dep_dir.clone(),
                direct,
                checksum_hint: None,
            },
        );
        // Recurse into the path dep's own dependencies (transitive).
        self.seed(&manifest, &dep_dir, false, id)
    }

    /// Resolve a `git` dependency: fetch the requested reference, pin the exact
    /// commit, record its tree checksum, and recurse through its manifest.
    fn resolve_git_dep(
        &mut self,
        id: &str,
        name: &str,
        url: &str,
        reference: &crate::manifest::GitRef,
        direct: bool,
    ) -> Result<(), ResolveError> {
        if self.resolved.contains_key(id) {
            return Ok(());
        }
        let locked = self.existing.and_then(|lock| {
            lock.packages.iter().find(|p| {
                p.name == name && matches!(&p.source, LockSource::Git { url: locked_url, .. } if locked_url == url)
            })
        });
        let locked_rev = match locked.map(|p| &p.source) {
            Some(LockSource::Git {
                url: locked_url,
                rev,
            }) if locked_url == url => Some(rev.clone()),
            _ => None,
        };
        let locked_ref = locked_rev
            .as_ref()
            .map(|rev| crate::manifest::GitRef::Rev(rev.clone()));
        let checkout = crate::git::fetch(url, locked_ref.as_ref().unwrap_or(reference), self.store)
            .map_err(|e| ResolveError::Git {
                name: name.to_string(),
                message: e.to_string(),
            })?;
        if let Some(expected) = locked.and_then(|p| p.checksum.as_deref()) {
            if expected != checkout.checksum {
                return Err(ResolveError::Git {
                    name: name.to_string(),
                    message: format!(
                        "checksum mismatch: expected {expected}, got {}",
                        checkout.checksum
                    ),
                });
            }
        }
        let manifest_path = checkout.root.join(crate::project::MANIFEST_NAME);
        let text = std::fs::read_to_string(&manifest_path).map_err(|e| ResolveError::Git {
            name: name.to_string(),
            message: format!("cannot read {}: {e}", manifest_path.display()),
        })?;
        let manifest = Manifest::parse(&text).map_err(|e| ResolveError::Git {
            name: name.to_string(),
            message: e.to_string(),
        })?;
        let version = manifest.package.version.clone();
        self.resolved.insert(
            id.to_string(),
            ResolvedInternal {
                id: id.to_string(),
                name: name.to_string(),
                version,
                source: LockSource::Git {
                    url: url.to_string(),
                    rev: checkout.rev,
                },
                root: checkout.root.clone(),
                direct,
                checksum_hint: Some(checkout.checksum),
            },
        );
        self.seed(&manifest, &checkout.root, false, id)
    }

    /// Drive registry resolution to a fixpoint: pick versions for all accumulated
    /// requirements, fetch new picks, and fold their deps back in until stable.
    fn run(&mut self) -> Result<(), ResolveError> {
        loop {
            // Names with registry reqs not yet resolved (or whose pick changed).
            let pending: Vec<String> = self
                .reqs
                .keys()
                .filter(|id| {
                    !self.resolved.contains_key(*id)
                        || self.processed_features.get(*id) != Some(&self.requested_features(id))
                })
                .cloned()
                .collect();
            if pending.is_empty() {
                return Ok(());
            }
            for id in pending {
                self.resolve_registry_dep(&id)?;
            }
        }
    }

    fn resolve_registry_dep(&mut self, id: &str) -> Result<(), ResolveError> {
        let reqs = self.reqs.get(id).cloned().unwrap_or_default();
        let key = self
            .req_keys
            .get(id)
            .cloned()
            .ok_or_else(|| ResolveError::Backend(format!("missing request key for `{id}`")))?;
        let registry = self.registries.get(Some(&key.registry), &key.name)?;

        let entries = registry
            .index(&key.name)
            .map_err(|e| ResolveError::Backend(e.to_string()))?;
        if entries.is_empty() {
            return Err(ResolveError::NoMatch {
                name: key.name,
                reqs: reqs.iter().map(|r| r.to_string()).collect(),
            });
        }

        // Honor an existing lock if it still satisfies all current reqs.
        let index_url = self.registry_index_url(registry);
        if let Some(lock) = self.existing.and_then(|l| {
            l.packages.iter().find(|p| {
                p.name == key.name
                    && matches!(&p.source, LockSource::Registry { index } if index == &index_url)
            })
        }) {
            if let Ok(v) = version::parse_version(&lock.version) {
                if reqs.iter().all(|r| r.matches(&v)) {
                    if let Some(entry) = entries.iter().find(|e| e.vers == v && !e.yanked) {
                        return self.pin_registry(id, &key, entry.clone(), registry);
                    }
                }
            }
        }

        let available: Vec<Version> = entries
            .iter()
            .filter(|e| !e.yanked)
            .map(|e| e.vers.clone())
            .collect();
        let picked =
            version::pick_version(&reqs, &available).ok_or_else(|| ResolveError::NoMatch {
                name: key.name.clone(),
                reqs: reqs.iter().map(|r| r.to_string()).collect(),
            })?;
        let entry = entries
            .iter()
            .find(|e| e.vers == picked)
            .expect("picked version is in the index")
            .clone();
        self.pin_registry(id, &key, entry, registry)
    }

    /// Pin a chosen registry entry: fetch + verify + extract, record it, and fold
    /// its (non-optional) dependencies into the worklist.
    fn pin_registry(
        &mut self,
        id: &str,
        key: &ReqKey,
        entry: crate::registry::IndexEntry,
        registry: &dyn Registry,
    ) -> Result<(), ResolveError> {
        let tarball = registry
            .download(&entry)
            .map_err(|e| ResolveError::Backend(e.to_string()))?;
        let checksum = format!("sha256:{}", entry.cksum);
        let root = self
            .store
            .extract(&tarball, &checksum)
            .map_err(|e| ResolveError::Backend(e.to_string()))?;

        // The lockfile records the registry index URL as the source.
        let index_url = self.registry_index_url(registry);
        self.resolved.insert(
            id.to_string(),
            ResolvedInternal {
                id: id.to_string(),
                name: key.name.clone(),
                version: entry.vers.to_string(),
                source: LockSource::Registry { index: index_url },
                root,
                direct: self.direct.contains(id),
                checksum_hint: Some(checksum),
            },
        );

        let requested = self.requested_features(id);
        let feature_plan = FeaturePlan::from_index(&entry, &requested)?;
        self.processed_features.insert(id.to_string(), requested);

        // Fold transitive deps enabled by the resolved package's active
        // features into the worklist.
        for d in &entry.deps {
            if d.optional && !feature_plan.optional_deps.contains(&d.name) {
                continue;
            }
            let dep_registry = d.registry.clone().unwrap_or_else(|| key.registry.clone());
            let dep_key = ReqKey::new(&d.name, dep_registry, &d.req);
            let dep_id = dep_key.id.clone();
            self.record_dependency(id, &d.name, &dep_id);
            self.reqs
                .entry(dep_id.clone())
                .or_default()
                .push(d.req.clone());
            self.req_keys.entry(dep_id.clone()).or_insert(dep_key);
            self.request_dependency_features(
                &dep_id,
                d.default_features,
                &d.features,
                feature_plan.dep_features.get(&d.name),
            );
        }
        Ok(())
    }

    /// The index URL recorded for a registry's lockfile `source`. The transport
    /// does not expose its URL directly, so use the registry name as a stable
    /// identifier when the URL is unavailable (the local fixture case).
    fn registry_index_url(&self, registry: &dyn Registry) -> String {
        registry.name().to_string()
    }
}

#[derive(Default)]
struct FeaturePlan {
    optional_deps: BTreeSet<String>,
    dep_features: BTreeMap<String, BTreeSet<String>>,
}

impl FeaturePlan {
    fn from_manifest(
        manifest: &Manifest,
        requested: &BTreeSet<String>,
    ) -> Result<FeaturePlan, ResolveError> {
        let dep_flags = manifest
            .dependencies
            .iter()
            .map(|(name, dep)| (name.clone(), dep.optional))
            .collect::<BTreeMap<_, _>>();
        let mut features = manifest.features.clone();
        if !manifest.package.default_features.is_empty() {
            features
                .entry("default".to_string())
                .or_default()
                .extend(manifest.package.default_features.iter().cloned());
        }
        Self::from_dependency_flags(&manifest.package.name, &dep_flags, &features, requested)
    }

    fn from_index(
        entry: &crate::registry::IndexEntry,
        requested: &BTreeSet<String>,
    ) -> Result<FeaturePlan, ResolveError> {
        let dep_flags = entry
            .deps
            .iter()
            .map(|d| (d.name.clone(), d.optional))
            .collect::<BTreeMap<_, _>>();
        Self::from_dependency_flags(&entry.name, &dep_flags, &entry.features, requested)
    }

    fn from_dependency_flags(
        package: &str,
        deps: &BTreeMap<String, bool>,
        features: &BTreeMap<String, Vec<String>>,
        requested: &BTreeSet<String>,
    ) -> Result<FeaturePlan, ResolveError> {
        let mut plan = FeaturePlan::default();
        let mut seen = BTreeSet::new();
        let mut stack: Vec<String> = requested.iter().cloned().collect();
        while let Some(feature) = stack.pop() {
            if !seen.insert(feature.clone()) {
                continue;
            }
            let Some(items) = features.get(&feature) else {
                if feature == "default" {
                    continue;
                }
                return Err(ResolveError::Feature {
                    package: package.to_string(),
                    message: format!("unknown feature `{feature}`"),
                });
            };
            for item in items {
                if let Some(dep) = item.strip_prefix("dep:") {
                    if dep.is_empty() || dep.contains('/') {
                        return Err(ResolveError::Feature {
                            package: package.to_string(),
                            message: format!("invalid dependency feature entry `{item}`"),
                        });
                    }
                    match deps.get(dep) {
                        Some(true) => {
                            plan.optional_deps.insert(dep.to_string());
                        }
                        Some(false) => {}
                        None => {
                            return Err(ResolveError::Feature {
                                package: package.to_string(),
                                message: format!(
                                    "feature `{feature}` references unknown dependency `{dep}`"
                                ),
                            });
                        }
                    }
                    continue;
                }
                if let Some((dep, dep_feature)) = item.split_once('/') {
                    if dep.is_empty() || dep_feature.is_empty() {
                        return Err(ResolveError::Feature {
                            package: package.to_string(),
                            message: format!("invalid dependency feature entry `{item}`"),
                        });
                    }
                    match deps.get(dep) {
                        Some(optional) => {
                            if *optional {
                                plan.optional_deps.insert(dep.to_string());
                            }
                            plan.dep_features
                                .entry(dep.to_string())
                                .or_default()
                                .insert(dep_feature.to_string());
                        }
                        None => {
                            return Err(ResolveError::Feature {
                                package: package.to_string(),
                                message: format!(
                                    "feature `{feature}` references unknown dependency `{dep}`"
                                ),
                            });
                        }
                    }
                    continue;
                }
                if features.contains_key(item) {
                    stack.push(item.clone());
                } else {
                    return Err(ResolveError::Feature {
                        package: package.to_string(),
                        message: format!("feature `{feature}` references unknown feature `{item}`"),
                    });
                }
            }
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::LocalRegistry;
    use std::io::Write;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("otter_resolve_{tag}_{n}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn make_tar_gz(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, contents) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, contents.as_bytes())
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap();
        gz
    }

    /// Publish a package version into a local fixture registry: write its tarball
    /// and append its index line (with the real checksum + deps).
    fn publish(
        reg_dir: &Path,
        name: &str,
        version: &str,
        deps: &[(&str, &str)],
        files: &[(&str, &str)],
    ) {
        let deps = deps
            .iter()
            .map(|(name, req)| (*name, *req, false))
            .collect::<Vec<_>>();
        publish_with_features(reg_dir, name, version, &deps, &[], files)
    }

    fn publish_with_features(
        reg_dir: &Path,
        name: &str,
        version: &str,
        deps: &[(&str, &str, bool)],
        features: &[(&str, &[&str])],
        files: &[(&str, &str)],
    ) {
        let gz = make_tar_gz(files);
        let cksum = crate::store::sha256_hex(&gz);
        let tdir = reg_dir.join("crates").join(name);
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join(format!("{version}.tar.gz")), &gz).unwrap();

        let dep_json: Vec<String> = deps
            .iter()
            .map(|(n, r, optional)| {
                format!(
                    "{{\"name\":\"{n}\",\"req\":\"{r}\",\"optional\":{optional},\"default_features\":true,\"features\":[]}}"
                )
            })
            .collect();
        let feature_json: Vec<String> = features
            .iter()
            .map(|(name, items)| {
                let items_json = items
                    .iter()
                    .map(|item| format!("\"{item}\""))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("\"{name}\":[{items_json}]")
            })
            .collect();
        let line = format!(
            "{{\"name\":\"{name}\",\"vers\":\"{version}\",\"deps\":[{}],\"features\":{{{}}},\"cksum\":\"{cksum}\",\"yanked\":false}}\n",
            dep_json.join(","),
            feature_json.join(",")
        );
        let idx = crate::store::index_path(&reg_dir.join("index"), name);
        std::fs::create_dir_all(idx.parent().unwrap()).unwrap();
        let mut existing = std::fs::read_to_string(&idx).unwrap_or_default();
        existing.push_str(&line);
        std::fs::write(&idx, existing).unwrap();
    }

    fn manifest(deps_toml: &str) -> Manifest {
        Manifest::parse(&format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n[dependencies]\n{deps_toml}"
        ))
        .unwrap()
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_out(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn make_git_package(version: &str, body: &str) -> PathBuf {
        let repo = temp_dir("git_repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Otter Test"]);
        git(&repo, &["config", "user.email", "otter@example.invalid"]);
        write_git_package(&repo, version, body);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", &format!("version {version}")]);
        repo
    }

    fn write_git_package(repo: &Path, version: &str, body: &str) {
        std::fs::write(
            repo.join("project.toml"),
            format!("[package]\nname = \"gitlib\"\nversion = \"{version}\"\nkind = \"library\"\n"),
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.otter"), body).unwrap();
    }

    #[test]
    fn resolves_a_single_registry_dep_and_locks_it() {
        let reg_dir = temp_dir("single_reg");
        publish(
            &reg_dir,
            "leftpad",
            "1.2.0",
            &[],
            &[("lib.otter", "pub function f(): i64 {1}")],
        );
        let store = Store::at(temp_dir("single_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = manifest("leftpad = \"1.2\"\n");
        let resolved = resolve(&m, &temp_dir("single_proj"), &registries, &store, None).unwrap();
        assert_eq!(resolved.lockfile.packages.len(), 1);
        let p = &resolved.lockfile.packages[0];
        assert_eq!(p.name, "leftpad");
        assert_eq!(p.version, "1.2.0");
        assert!(p.checksum.as_ref().unwrap().starts_with("sha256:"));
        // The tarball was extracted into the content store.
        assert!(resolved.packages[0].root.join("lib.otter").exists());
    }

    #[test]
    fn unifies_compatible_requirements() {
        let reg_dir = temp_dir("unify_reg");
        publish(&reg_dir, "dep", "1.2.0", &[], &[("lib.otter", "x")]);
        publish(&reg_dir, "dep", "1.4.0", &[], &[("lib.otter", "y")]);
        publish(&reg_dir, "dep", "1.5.3", &[], &[("lib.otter", "z")]);
        publish(&reg_dir, "dep", "2.0.0", &[], &[("lib.otter", "w")]);
        // `mid` depends on dep ^1.4; the root depends on dep ^1.2. Unify → 1.5.3.
        publish(
            &reg_dir,
            "mid",
            "1.0.0",
            &[("dep", "^1.4")],
            &[("lib.otter", "m")],
        );
        let store = Store::at(temp_dir("unify_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = manifest("dep = \"1.2\"\nmid = \"1.0\"\n");
        let resolved = resolve(&m, &temp_dir("unify_proj"), &registries, &store, None).unwrap();
        let dep = resolved.lockfile.get("dep").unwrap();
        assert_eq!(dep.version, "1.5.3");
        assert!(resolved.lockfile.get("mid").is_some());
    }

    #[test]
    fn incompatible_major_requirements_coexist_as_separate_packages() {
        let reg_dir = temp_dir("multi_major_reg");
        publish(&reg_dir, "shared", "1.5.0", &[], &[("lib.otter", "s1")]);
        publish(&reg_dir, "shared", "2.3.0", &[], &[("lib.otter", "s2")]);
        publish(
            &reg_dir,
            "left",
            "1.0.0",
            &[("shared", "^1")],
            &[("lib.otter", "l")],
        );
        publish(
            &reg_dir,
            "right",
            "1.0.0",
            &[("shared", "^2")],
            &[("lib.otter", "r")],
        );
        let store = Store::at(temp_dir("multi_major_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = manifest("left = \"1.0\"\nright = \"1.0\"\n");
        let resolved =
            resolve(&m, &temp_dir("multi_major_proj"), &registries, &store, None).unwrap();
        let shared = resolved
            .lockfile
            .packages
            .iter()
            .filter(|p| p.name == "shared")
            .map(|p| p.version.as_str())
            .collect::<Vec<_>>();
        assert_eq!(shared, ["1.5.0", "2.3.0"]);
        let left = resolved.get("left").unwrap();
        let right = resolved.get("right").unwrap();
        let left_shared_id = &resolved.dependency_edges[&left.id]["shared"];
        let right_shared_id = &resolved.dependency_edges[&right.id]["shared"];
        assert_ne!(left_shared_id, right_shared_id);
        assert_eq!(resolved.get_by_id(left_shared_id).unwrap().version, "1.5.0");
        assert_eq!(
            resolved.get_by_id(right_shared_id).unwrap().version,
            "2.3.0"
        );
    }

    #[test]
    fn transitive_dependencies_are_pulled_in() {
        let reg_dir = temp_dir("trans_reg");
        publish(&reg_dir, "bottom", "0.1.0", &[], &[("lib.otter", "b")]);
        publish(
            &reg_dir,
            "top",
            "1.0.0",
            &[("bottom", "^0.1")],
            &[("lib.otter", "t")],
        );
        let store = Store::at(temp_dir("trans_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = manifest("top = \"1.0\"\n");
        let resolved = resolve(&m, &temp_dir("trans_proj"), &registries, &store, None).unwrap();
        assert!(resolved.lockfile.get("top").is_some());
        assert!(
            resolved.lockfile.get("bottom").is_some(),
            "transitive dep missing"
        );
    }

    #[test]
    fn optional_dependency_is_skipped_until_a_feature_enables_it() {
        let reg_dir = temp_dir("opt_skip_reg");
        publish(&reg_dir, "opt", "1.0.0", &[], &[("lib.otter", "o")]);
        let store = Store::at(temp_dir("opt_skip_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = manifest("opt = { version = \"1.0\", optional = true }\n");
        let resolved = resolve(&m, &temp_dir("opt_skip_proj"), &registries, &store, None).unwrap();
        assert!(
            resolved.lockfile.get("opt").is_none(),
            "optional dep should stay out of the graph until a feature names it"
        );
    }

    #[test]
    fn default_feature_can_enable_an_optional_root_dependency() {
        let reg_dir = temp_dir("opt_default_reg");
        publish(&reg_dir, "opt", "1.0.0", &[], &[("lib.otter", "o")]);
        let store = Store::at(temp_dir("opt_default_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = Manifest::parse(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [dependencies]\nopt = { version = \"1.0\", optional = true }\n\
             [features]\ndefault = [\"dep:opt\"]\n",
        )
        .unwrap();
        let resolved =
            resolve(&m, &temp_dir("opt_default_proj"), &registries, &store, None).unwrap();
        assert!(resolved.lockfile.get("opt").is_some());
        let opt_id = &resolved.dependency_edges[&resolved.root_id]["opt"];
        assert_eq!(resolved.get_by_id(opt_id).unwrap().name, "opt");
    }

    #[test]
    fn dependency_feature_can_enable_registry_optional_transitive_dep() {
        let reg_dir = temp_dir("opt_trans_reg");
        publish(&reg_dir, "bottom", "1.0.0", &[], &[("lib.otter", "b")]);
        publish_with_features(
            &reg_dir,
            "top",
            "1.0.0",
            &[("bottom", "^1.0", true)],
            &[("extra", &["dep:bottom"])],
            &[("lib.otter", "t")],
        );
        let store = Store::at(temp_dir("opt_trans_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = manifest("top = { version = \"1.0\", features = [\"extra\"] }\n");
        let resolved = resolve(&m, &temp_dir("opt_trans_proj"), &registries, &store, None).unwrap();
        assert!(resolved.lockfile.get("top").is_some());
        assert!(
            resolved.lockfile.get("bottom").is_some(),
            "dependency feature did not activate optional transitive dependency"
        );
        let top_id = &resolved.dependency_edges[&resolved.root_id]["top"];
        let bottom_id = &resolved.dependency_edges[top_id]["bottom"];
        assert_eq!(resolved.get_by_id(bottom_id).unwrap().name, "bottom");
    }

    #[test]
    fn unknown_feature_dependency_is_a_clear_error() {
        let reg_dir = temp_dir("bad_feature_reg");
        let store = Store::at(temp_dir("bad_feature_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };

        let m = Manifest::parse(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [features]\ndefault = [\"dep:ghost\"]\n",
        )
        .unwrap();
        let err =
            resolve(&m, &temp_dir("bad_feature_proj"), &registries, &store, None).unwrap_err();
        assert!(matches!(err, ResolveError::Feature { .. }));
        assert!(err.to_string().contains("unknown dependency `ghost`"));
    }

    #[test]
    fn missing_package_is_an_error() {
        let reg_dir = temp_dir("missing_reg");
        let store = Store::at(temp_dir("missing_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };
        let m = manifest("ghost = \"1.0\"\n");
        assert!(matches!(
            resolve(&m, &temp_dir("missing_proj"), &registries, &store, None),
            Err(ResolveError::NoMatch { .. })
        ));
    }

    #[test]
    fn path_dependency_is_resolved_from_its_manifest() {
        let proj = temp_dir("pathdep_proj");
        // A sibling `../mylib` path dependency.
        let lib_dir = proj
            .parent()
            .unwrap()
            .join(format!("mylib_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib_dir);
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(
            lib_dir.join("project.toml"),
            "[package]\nname = \"mylib\"\nversion = \"0.3.0\"\nkind = \"library\"\n",
        )
        .unwrap();

        let store = Store::at(temp_dir("pathdep_store"));
        let registries = Registries {
            by_name: HashMap::new(),
            default: "public".into(),
        };
        let rel = format!("../mylib_{}", std::process::id());
        let m = manifest(&format!("mylib = {{ path = \"{rel}\" }}\n"));
        let resolved = resolve(&m, &proj, &registries, &store, None).unwrap();
        let p = resolved.lockfile.get("mylib").unwrap();
        assert_eq!(p.version, "0.3.0");
        assert_eq!(p.source, LockSource::Path { path: rel });
        assert!(p.checksum.is_none(), "path deps carry no checksum");
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn git_dependency_is_fetched_and_locked_to_exact_rev() {
        let repo = make_git_package("0.1.0", "pub function f(): i64 { 1 }\n");
        let rev = git_out(&repo, &["rev-parse", "HEAD"]);
        let store = Store::at(temp_dir("git_store").join("registry"));
        let registries = Registries {
            by_name: HashMap::new(),
            default: "public".into(),
        };

        let m = manifest(&format!(
            "gitlib = {{ git = \"{}\", rev = \"{rev}\" }}\n",
            repo.to_string_lossy()
        ));
        let resolved = resolve(&m, &temp_dir("git_proj"), &registries, &store, None).unwrap();

        let p = resolved.lockfile.get("gitlib").unwrap();
        assert_eq!(p.version, "0.1.0");
        assert_eq!(
            p.source,
            LockSource::Git {
                url: repo.to_string_lossy().into_owned(),
                rev: rev.clone()
            }
        );
        assert!(p.checksum.as_deref().unwrap().starts_with("sha256:"));
        let pkg = resolved.get("gitlib").unwrap();
        assert!(pkg.root.join("project.toml").exists());
        assert!(pkg.root.join("src/lib.otter").exists());
        assert!(!pkg.root.join(".git").exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn git_branch_dependency_honors_existing_lock_until_update() {
        let repo = make_git_package("0.1.0", "pub function f(): i64 { 1 }\n");
        let branch = git_out(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let rev1 = git_out(&repo, &["rev-parse", "HEAD"]);
        let store = Store::at(temp_dir("git_branch_store").join("registry"));
        let registries = Registries {
            by_name: HashMap::new(),
            default: "public".into(),
        };
        let m = manifest(&format!(
            "gitlib = {{ git = \"{}\", branch = \"{branch}\" }}\n",
            repo.to_string_lossy()
        ));

        let first = resolve(&m, &temp_dir("git_branch_proj1"), &registries, &store, None).unwrap();
        assert_eq!(first.lockfile.get("gitlib").unwrap().version, "0.1.0");
        assert!(matches!(
            &first.lockfile.get("gitlib").unwrap().source,
            LockSource::Git { rev, .. } if rev == &rev1
        ));

        write_git_package(&repo, "0.2.0", "pub function f(): i64 { 2 }\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "version 0.2.0"]);
        let rev2 = git_out(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(rev1, rev2);

        let locked = resolve(
            &m,
            &temp_dir("git_branch_proj2"),
            &registries,
            &store,
            Some(&first.lockfile),
        )
        .unwrap();
        assert_eq!(locked.lockfile.get("gitlib").unwrap().version, "0.1.0");
        assert!(matches!(
            &locked.lockfile.get("gitlib").unwrap().source,
            LockSource::Git { rev, .. } if rev == &rev1
        ));

        let updated =
            resolve(&m, &temp_dir("git_branch_proj3"), &registries, &store, None).unwrap();
        assert_eq!(updated.lockfile.get("gitlib").unwrap().version, "0.2.0");
        assert!(matches!(
            &updated.lockfile.get("gitlib").unwrap().source,
            LockSource::Git { rev, .. } if rev == &rev2
        ));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn corrupted_registry_tarball_fails_verification() {
        let reg_dir = temp_dir("tamper_reg");
        // Publish, then tamper with the tarball so its bytes no longer hash to
        // the index checksum.
        publish(&reg_dir, "dep", "1.0.0", &[], &[("lib.otter", "ok")]);
        let tarball = reg_dir.join("crates").join("dep").join("1.0.0.tar.gz");
        std::fs::write(&tarball, b"tampered").unwrap();
        let store = Store::at(temp_dir("tamper_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries {
            by_name,
            default: "public".into(),
        };
        let m = manifest("dep = \"1.0\"\n");
        let err = resolve(&m, &temp_dir("tamper_proj"), &registries, &store, None).unwrap_err();
        assert!(matches!(err, ResolveError::Backend(_)));
        assert!(err.to_string().contains("checksum mismatch"), "got: {err}");
    }
}
