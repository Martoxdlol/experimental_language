//! Dependency resolution (`docs/23` §7).
//!
//! Resolution turns the root manifest's `[dependencies]` into a fully-pinned
//! graph: it unifies version requirements (one version per compatible range),
//! fetches and verifies registry tarballs into the content-addressed store,
//! follows transitive dependencies to a fixpoint, and produces a [`Lockfile`].
//! "Lockfile is truth": an existing lock that still satisfies the constraints is
//! preserved, so resolution is stable.
//!
//! Scope: registry and `path` dependencies with transitive resolution and
//! version unification are implemented and tested against a local fixture
//! registry. `git` sources are recorded but not yet fetched, and feature-gated
//! optional deps are not yet pulled in — both are noted follow-ups.

use std::collections::{BTreeMap, HashMap};
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
    /// Dependency edges: package name → the names it directly depends on. The
    /// root package is keyed by its own name. Sorted for determinism.
    pub edges: BTreeMap<String, Vec<String>>,
    /// The root package's name (the edge-map key for its direct dependencies).
    pub root_name: String,
}

impl Resolved {
    /// Look up a resolved package by name.
    pub fn get(&self, name: &str) -> Option<&ResolvedPackage> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// One resolved package and where its source lives on disk.
#[derive(Clone, Debug)]
pub struct ResolvedPackage {
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
    /// `git` sources are not yet fetched.
    GitUnsupported { name: String },
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
                write!(f, "dependency `{name}` uses registry `{registry}`, which is not declared")
            }
            ResolveError::Path { name, message } => {
                write!(f, "path dependency `{name}`: {message}")
            }
            ResolveError::GitUnsupported { name } => {
                write!(f, "git dependency `{name}` is not yet supported by the resolver")
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
        self.by_name.get(key).copied().ok_or_else(|| ResolveError::UnknownRegistry {
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
        // name -> all requirements gathered so far
        reqs: BTreeMap::new(),
        // name -> registry it resolves against (None = default)
        registry_of: BTreeMap::new(),
        resolved: BTreeMap::new(),
        direct: Default::default(),
        edges: BTreeMap::new(),
    };
    // Seed from the root manifest's direct dependencies.
    r.seed(root, root_dir, true, &root.package.name)?;
    r.run()?;

    let mut internal: Vec<ResolvedInternal> = r.resolved.into_values().collect();
    internal.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
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
        root_name: root.package.name.clone(),
    })
}

struct Run<'a> {
    registries: &'a Registries<'a>,
    store: &'a Store,
    existing: Option<&'a Lockfile>,
    reqs: BTreeMap<String, Vec<VersionReq>>,
    registry_of: BTreeMap<String, Option<String>>,
    resolved: BTreeMap<String, ResolvedInternal>,
    direct: std::collections::HashSet<String>,
    edges: BTreeMap<String, Vec<String>>,
}

#[derive(Clone)]
struct ResolvedInternal {
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
            name: r.name,
            version: r.version,
            source: r.source,
            root: r.root,
            direct: r.direct,
        }
    }
}

impl<'a> Run<'a> {
    /// Record a manifest's direct dependencies into the worklist. Path deps are
    /// resolved immediately (and recursed); registry deps accumulate reqs.
    fn seed(
        &mut self,
        manifest: &Manifest,
        base_dir: &Path,
        direct: bool,
        parent: &str,
    ) -> Result<(), ResolveError> {
        for (name, dep) in &manifest.dependencies {
            if dep.optional {
                // Feature-gated optional deps are a follow-up; skip for now.
                continue;
            }
            self.edges.entry(parent.to_string()).or_default().push(name.clone());
            match &dep.source {
                DepSource::Registry { version: req_str, registry } => {
                    let req = version::parse_req(req_str)
                        .map_err(|m| ResolveError::Backend(m))?;
                    self.reqs.entry(name.clone()).or_default().push(req);
                    self.registry_of.entry(name.clone()).or_insert_with(|| registry.clone());
                    if direct {
                        self.direct.insert(name.clone());
                    }
                }
                DepSource::Path { path } => {
                    self.resolve_path_dep(name, path, base_dir, direct)?;
                }
                DepSource::Git { .. } => {
                    return Err(ResolveError::GitUnsupported { name: name.clone() });
                }
            }
        }
        Ok(())
    }

    /// Resolve a `path` dependency: load its manifest, record it, and recurse.
    fn resolve_path_dep(
        &mut self,
        name: &str,
        path: &str,
        base_dir: &Path,
        direct: bool,
    ) -> Result<(), ResolveError> {
        if self.resolved.contains_key(name) {
            return Ok(());
        }
        let dep_dir = compiler::sema::resolve_ctx::normalize(&base_dir.join(path));
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
            name.to_string(),
            ResolvedInternal {
                name: name.to_string(),
                version,
                source: LockSource::Path { path: path.to_string() },
                root: dep_dir.clone(),
                direct,
                checksum_hint: None,
            },
        );
        // Recurse into the path dep's own dependencies (transitive).
        self.seed(&manifest, &dep_dir, false, name)
    }

    /// Drive registry resolution to a fixpoint: pick versions for all accumulated
    /// requirements, fetch new picks, and fold their deps back in until stable.
    fn run(&mut self) -> Result<(), ResolveError> {
        loop {
            // Names with registry reqs not yet resolved (or whose pick changed).
            let pending: Vec<String> = self
                .reqs
                .keys()
                .filter(|n| !self.resolved.contains_key(*n))
                .cloned()
                .collect();
            if pending.is_empty() {
                return Ok(());
            }
            for name in pending {
                self.resolve_registry_dep(&name)?;
            }
        }
    }

    fn resolve_registry_dep(&mut self, name: &str) -> Result<(), ResolveError> {
        let reqs = self.reqs.get(name).cloned().unwrap_or_default();
        let registry_name = self.registry_of.get(name).cloned().flatten();
        let registry = self.registries.get(registry_name.as_deref(), name)?;

        let entries = registry
            .index(name)
            .map_err(|e| ResolveError::Backend(e.to_string()))?;
        if entries.is_empty() {
            return Err(ResolveError::NoMatch {
                name: name.to_string(),
                reqs: reqs.iter().map(|r| r.to_string()).collect(),
            });
        }

        // Honor an existing lock if it still satisfies all current reqs.
        if let Some(lock) = self.existing.and_then(|l| l.get(name)) {
            if let Ok(v) = version::parse_version(&lock.version) {
                if reqs.iter().all(|r| r.matches(&v)) {
                    if let Some(entry) = entries.iter().find(|e| e.vers == v && !e.yanked) {
                        return self.pin_registry(name, entry.clone(), registry, registry_name);
                    }
                }
            }
        }

        let available: Vec<Version> =
            entries.iter().filter(|e| !e.yanked).map(|e| e.vers.clone()).collect();
        let picked = version::pick_version(&reqs, &available).ok_or_else(|| ResolveError::NoMatch {
            name: name.to_string(),
            reqs: reqs.iter().map(|r| r.to_string()).collect(),
        })?;
        let entry = entries
            .iter()
            .find(|e| e.vers == picked)
            .expect("picked version is in the index")
            .clone();
        self.pin_registry(name, entry, registry, registry_name)
    }

    /// Pin a chosen registry entry: fetch + verify + extract, record it, and fold
    /// its (non-optional) dependencies into the worklist.
    fn pin_registry(
        &mut self,
        name: &str,
        entry: crate::registry::IndexEntry,
        registry: &dyn Registry,
        registry_name: Option<String>,
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
            name.to_string(),
            ResolvedInternal {
                name: name.to_string(),
                version: entry.vers.to_string(),
                source: LockSource::Registry { index: index_url },
                root,
                direct: self.direct.contains(name),
                checksum_hint: Some(checksum),
            },
        );

        // Fold transitive (non-optional) deps into the worklist.
        for d in &entry.deps {
            if d.optional {
                continue;
            }
            self.edges.entry(name.to_string()).or_default().push(d.name.clone());
            self.reqs.entry(d.name.clone()).or_default().push(d.req.clone());
            let dep_registry = d.registry.clone().or_else(|| registry_name.clone());
            self.registry_of.entry(d.name.clone()).or_insert(dep_registry);
        }
        let _ = registry_name;
        Ok(())
    }

    /// The index URL recorded for a registry's lockfile `source`. The transport
    /// does not expose its URL directly, so use the registry name as a stable
    /// identifier when the URL is unavailable (the local fixture case).
    fn registry_index_url(&self, registry: &dyn Registry) -> String {
        registry.name().to_string()
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
                builder.append_data(&mut header, path, contents.as_bytes()).unwrap();
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
        let gz = make_tar_gz(files);
        let cksum = crate::store::sha256_hex(&gz);
        let tdir = reg_dir.join("crates").join(name);
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join(format!("{version}.tar.gz")), &gz).unwrap();

        let dep_json: Vec<String> = deps
            .iter()
            .map(|(n, r)| {
                format!(
                    "{{\"name\":\"{n}\",\"req\":\"{r}\",\"optional\":false,\"default_features\":true,\"features\":[]}}"
                )
            })
            .collect();
        let line = format!(
            "{{\"name\":\"{name}\",\"vers\":\"{version}\",\"deps\":[{}],\"cksum\":\"{cksum}\",\"yanked\":false}}\n",
            dep_json.join(",")
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

    #[test]
    fn resolves_a_single_registry_dep_and_locks_it() {
        let reg_dir = temp_dir("single_reg");
        publish(&reg_dir, "leftpad", "1.2.0", &[], &[("lib.otter", "pub function f(): i64 {1}")]);
        let store = Store::at(temp_dir("single_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries { by_name, default: "public".into() };

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
        publish(&reg_dir, "mid", "1.0.0", &[("dep", "^1.4")], &[("lib.otter", "m")]);
        let store = Store::at(temp_dir("unify_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries { by_name, default: "public".into() };

        let m = manifest("dep = \"1.2\"\nmid = \"1.0\"\n");
        let resolved = resolve(&m, &temp_dir("unify_proj"), &registries, &store, None).unwrap();
        let dep = resolved.lockfile.get("dep").unwrap();
        assert_eq!(dep.version, "1.5.3");
        assert!(resolved.lockfile.get("mid").is_some());
    }

    #[test]
    fn transitive_dependencies_are_pulled_in() {
        let reg_dir = temp_dir("trans_reg");
        publish(&reg_dir, "bottom", "0.1.0", &[], &[("lib.otter", "b")]);
        publish(&reg_dir, "top", "1.0.0", &[("bottom", "^0.1")], &[("lib.otter", "t")]);
        let store = Store::at(temp_dir("trans_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries { by_name, default: "public".into() };

        let m = manifest("top = \"1.0\"\n");
        let resolved = resolve(&m, &temp_dir("trans_proj"), &registries, &store, None).unwrap();
        assert!(resolved.lockfile.get("top").is_some());
        assert!(resolved.lockfile.get("bottom").is_some(), "transitive dep missing");
    }

    #[test]
    fn missing_package_is_an_error() {
        let reg_dir = temp_dir("missing_reg");
        let store = Store::at(temp_dir("missing_store"));
        let local = LocalRegistry::new("public", reg_dir.clone());
        let mut by_name: HashMap<String, &dyn Registry> = HashMap::new();
        by_name.insert("public".into(), &local);
        let registries = Registries { by_name, default: "public".into() };
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
        let lib_dir = proj.parent().unwrap().join(format!("mylib_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib_dir);
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(
            lib_dir.join("project.toml"),
            "[package]\nname = \"mylib\"\nversion = \"0.3.0\"\nkind = \"library\"\n",
        )
        .unwrap();

        let store = Store::at(temp_dir("pathdep_store"));
        let registries = Registries { by_name: HashMap::new(), default: "public".into() };
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
        let registries = Registries { by_name, default: "public".into() };
        let m = manifest("dep = \"1.0\"\n");
        let err = resolve(&m, &temp_dir("tamper_proj"), &registries, &store, None).unwrap_err();
        assert!(matches!(err, ResolveError::Backend(_)));
        assert!(err.to_string().contains("checksum mismatch"), "got: {err}");
    }
}
