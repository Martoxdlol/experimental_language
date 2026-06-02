//! `project.toml` manifest parsing (`docs/23` §8).
//!
//! A faithful, strongly-typed model of the manifest tables the dependency and
//! module systems need: `[package]`, `[dependencies]`, `[features]`,
//! `[registry]`, `[registries]`, and `[file-imports]`. Tables outside this set
//! (`[profile.*]`, `[target.*]`, `[scripts]`, `[package.links]`, …) are
//! tolerated and ignored here; they belong to the build/FFI layers.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

/// A parsed manifest. Dependency sources are normalized and validated by
/// [`Manifest::parse`]; the raw serde shape is internal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub package: Package,
    /// Declared dependencies, by the name used in `pkg:<name>` imports.
    pub dependencies: BTreeMap<String, Dependency>,
    /// `[features]` — feature name → the deps/features it enables.
    pub features: BTreeMap<String, Vec<String>>,
    /// `[registry] default = "..."` — which registry bare `pkg:` deps use.
    pub default_registry: Option<String>,
    /// `[registries]` — name → registry index location.
    pub registries: BTreeMap<String, Registry>,
    /// `[file-imports] allow = [...]` — roots/globs an escaping `file:` import
    /// may resolve into (`docs/17` §17.4).
    pub file_import_allow: Vec<String>,
    /// `[macros] recursion_limit = N` — procedural-macro expansion depth limit
    /// (`docs/22` §10). `None` uses the compiler default (128).
    pub macro_recursion_limit: Option<usize>,
}

/// The `[package]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub kind: PackageKind,
    /// Explicit `entry = "..."`; defaulted by [`Manifest::entry_path`] when absent.
    pub entry: Option<String>,
    /// Source root, conventionally `src` (`docs/17` §17.1).
    pub src: String,
    /// Extra binary entries for `library+bins`.
    pub bins: Vec<String>,
    pub edition: Option<String>,
    /// `no-std = true` makes any `std:` import a hard error (`docs/23` §6).
    pub no_std: bool,
    pub default_features: Vec<String>,
}

/// `kind = "binary" | "library" | "library+bins"` (`docs/17` §17.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageKind {
    /// Produces an executable; not consumable via `pkg:`.
    Binary,
    /// Public API is everything `pub` in the library entry (`lib.otter`).
    Library,
    /// A library plus extra binary entries.
    LibraryBins,
}

impl PackageKind {
    /// Whether other packages can depend on this one via `pkg:` (libraries only).
    pub fn is_consumable(self) -> bool {
        matches!(self, PackageKind::Library | PackageKind::LibraryBins)
    }
    /// Whether this package builds an executable (`main`).
    pub fn has_binary(self) -> bool {
        matches!(self, PackageKind::Binary | PackageKind::LibraryBins)
    }
    /// The default entry file (relative to root) for this kind, given `src`.
    fn default_entry(self, src: &str) -> String {
        match self {
            PackageKind::Binary => format!("{src}/main.otter"),
            PackageKind::Library | PackageKind::LibraryBins => format!("{src}/lib.otter"),
        }
    }
}

/// A normalized, validated dependency declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    /// Where the package comes from.
    pub source: DepSource,
    /// Features enabled on the dependency.
    pub features: Vec<String>,
    /// Whether to enable the dependency's own `default` feature.
    pub default_features: bool,
    /// `optional = true` deps are only pulled in by a feature that names them
    /// (`dep:<name>`).
    pub optional: bool,
}

/// The resolved source of a dependency — exactly one of the manifest forms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// `serde = "1.2"` or `{ version = "...", registry = "..." }`.
    Registry {
        version: String,
        registry: Option<String>,
    },
    /// `{ path = "../foo" }` — a local path dependency.
    Path { path: String },
    /// `{ git = "https://...", rev/branch/tag = "..." }`.
    Git { url: String, reference: GitRef },
}

/// The pin form of a git dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitRef {
    Rev(String),
    Branch(String),
    Tag(String),
    /// No explicit pin — the default branch's tip (resolved at lock time).
    Default,
}

/// A registry index location from `[registries]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registry {
    /// The index URL, e.g. `sparse+https://pkgs.example.dev/index`.
    pub index: String,
    /// Whether requests carry credentials (a private registry).
    pub auth: bool,
}

/// Why a manifest could not be parsed or is semantically invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// TOML syntax error.
    Toml(String),
    /// A required field is missing.
    Missing(String),
    /// A field has an invalid value.
    Invalid(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Toml(m) => write!(f, "invalid manifest: {m}"),
            ManifestError::Missing(m) => write!(f, "manifest is missing required field `{m}`"),
            ManifestError::Invalid(m) => write!(f, "manifest: {m}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    /// Parse a `project.toml` from its text.
    pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|e| ManifestError::Toml(e.message().to_string()))?;
        raw.into_manifest()
    }

    /// The entry file path (relative to the project root), using the explicit
    /// `entry` or the kind's default.
    pub fn entry_path(&self) -> String {
        self.package
            .entry
            .clone()
            .unwrap_or_else(|| self.package.kind.default_entry(&self.package.src))
    }

    /// Every entry the compiler must walk: the primary entry plus any extra
    /// `bins` (`docs/17` §17.1).
    pub fn entry_paths(&self) -> Vec<String> {
        let mut entries = vec![self.entry_path()];
        entries.extend(self.package.bins.iter().cloned());
        entries
    }
}

// --- serde shapes (raw, before normalization) -------------------------------

#[derive(Deserialize)]
struct RawManifest {
    package: RawPackage,
    #[serde(default)]
    dependencies: BTreeMap<String, RawDep>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    registry: Option<RawRegistryDefault>,
    #[serde(default)]
    registries: BTreeMap<String, RawRegistry>,
    #[serde(default, rename = "file-imports")]
    file_imports: Option<RawFileImports>,
    #[serde(default)]
    macros: Option<RawMacros>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    src: Option<String>,
    #[serde(default)]
    bins: Vec<String>,
    #[serde(default)]
    edition: Option<String>,
    #[serde(default, rename = "no-std")]
    no_std: bool,
    #[serde(default, rename = "default-features")]
    default_features: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawDep {
    /// `serde = "1.2"`.
    Version(String),
    /// `serde = { version = "1.2", ... }`.
    Detailed(RawDetailedDep),
}

#[derive(Deserialize)]
struct RawDetailedDep {
    version: Option<String>,
    path: Option<String>,
    git: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
    registry: Option<String>,
    #[serde(default)]
    features: Vec<String>,
    #[serde(rename = "default-features")]
    default_features: Option<bool>,
    #[serde(default)]
    optional: bool,
}

#[derive(Deserialize)]
struct RawRegistryDefault {
    default: Option<String>,
}

#[derive(Deserialize)]
struct RawRegistry {
    index: String,
    #[serde(default)]
    auth: bool,
}

#[derive(Deserialize)]
struct RawFileImports {
    #[serde(default)]
    allow: Vec<String>,
}

#[derive(Deserialize)]
struct RawMacros {
    #[serde(default, rename = "recursion_limit", alias = "recursion-limit")]
    recursion_limit: Option<usize>,
}

impl RawManifest {
    fn into_manifest(self) -> Result<Manifest, ManifestError> {
        let src = self
            .package
            .src
            .clone()
            .unwrap_or_else(|| "src".to_string());
        let kind = match self.package.kind.as_deref() {
            None | Some("binary") => PackageKind::Binary,
            Some("library") => PackageKind::Library,
            Some("library+bins") => PackageKind::LibraryBins,
            Some(other) => {
                return Err(ManifestError::Invalid(format!(
                    "unknown package kind `{other}` (expected `binary`, `library`, or `library+bins`)"
                )));
            }
        };
        if self.package.name.trim().is_empty() {
            return Err(ManifestError::Missing("package.name".into()));
        }
        let package = Package {
            name: self.package.name,
            version: self.package.version.unwrap_or_else(|| "0.0.0".to_string()),
            kind,
            entry: self.package.entry,
            src,
            bins: self.package.bins,
            edition: self.package.edition,
            no_std: self.package.no_std,
            default_features: self.package.default_features,
        };

        let mut dependencies = BTreeMap::new();
        for (name, raw) in self.dependencies {
            dependencies.insert(name.clone(), raw.normalize(&name)?);
        }

        let registries = self
            .registries
            .into_iter()
            .map(|(name, r)| {
                (
                    name,
                    Registry {
                        index: r.index,
                        auth: r.auth,
                    },
                )
            })
            .collect();

        Ok(Manifest {
            package,
            dependencies,
            features: self.features,
            default_registry: self.registry.and_then(|r| r.default),
            registries,
            file_import_allow: self.file_imports.map(|f| f.allow).unwrap_or_default(),
            macro_recursion_limit: self.macros.and_then(|m| m.recursion_limit),
        })
    }
}

impl RawDep {
    fn normalize(self, name: &str) -> Result<Dependency, ManifestError> {
        match self {
            RawDep::Version(version) => Ok(Dependency {
                source: DepSource::Registry {
                    version,
                    registry: None,
                },
                features: Vec::new(),
                default_features: true,
                optional: false,
            }),
            RawDep::Detailed(d) => {
                let has_path = d.path.is_some();
                let has_git = d.git.is_some();
                let has_version = d.version.is_some();
                // Exactly one source form.
                let source = match (has_path, has_git) {
                    (true, true) => {
                        return Err(ManifestError::Invalid(format!(
                            "dependency `{name}` sets both `path` and `git`"
                        )));
                    }
                    (true, false) => {
                        if has_version || d.registry.is_some() {
                            return Err(ManifestError::Invalid(format!(
                                "path dependency `{name}` may not also set `version`/`registry`"
                            )));
                        }
                        DepSource::Path {
                            path: d.path.unwrap(),
                        }
                    }
                    (false, true) => {
                        let reference = match (d.rev, d.branch, d.tag) {
                            (Some(r), None, None) => GitRef::Rev(r),
                            (None, Some(b), None) => GitRef::Branch(b),
                            (None, None, Some(t)) => GitRef::Tag(t),
                            (None, None, None) => GitRef::Default,
                            _ => {
                                return Err(ManifestError::Invalid(format!(
                                    "git dependency `{name}` sets more than one of `rev`/`branch`/`tag`"
                                )));
                            }
                        };
                        DepSource::Git {
                            url: d.git.unwrap(),
                            reference,
                        }
                    }
                    (false, false) => {
                        let version = d.version.ok_or_else(|| {
                            ManifestError::Invalid(format!(
                                "dependency `{name}` needs a `version`, `path`, or `git` source"
                            ))
                        })?;
                        DepSource::Registry {
                            version,
                            registry: d.registry,
                        }
                    }
                };
                Ok(Dependency {
                    source,
                    features: d.features,
                    default_features: d.default_features.unwrap_or(true),
                    optional: d.optional,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[package]
name = "myapp"
version = "0.3.1"
kind = "binary"
entry = "src/main.otter"
edition = "2026"
no-std = false
default-features = ["json"]

[dependencies]
serde     = "1.2"
http      = { version = "0.4", features = ["tls"] }
internal  = { version = "2.0", registry = "myco" }
foo-local = { path = "../foo" }
bar-git   = { git = "https://example.com/bar", rev = "abc123" }
opt-dep   = { version = "1", optional = true, default-features = false }

[registry]
default = "public"

[registries]
public = { index = "sparse+https://pkgs.example.dev/index" }
myco   = { index = "sparse+https://pkgs.internal/index", auth = true }

[features]
json = ["dep:serde"]
default = ["json"]

[file-imports]
allow = ["../shared", "assets/**"]

[profile.release]
opt-level = 3

[scripts]
ci = "otter_fusion test"
"#;

    #[test]
    fn parses_the_full_manifest() {
        let m = Manifest::parse(FULL).expect("parse");
        assert_eq!(m.package.name, "myapp");
        assert_eq!(m.package.version, "0.3.1");
        assert_eq!(m.package.kind, PackageKind::Binary);
        assert_eq!(m.entry_path(), "src/main.otter");
        assert_eq!(m.package.default_features, ["json"]);
        assert!(!m.package.no_std);
    }

    #[test]
    fn registry_version_dep_short_form() {
        let m = Manifest::parse(FULL).unwrap();
        let serde = &m.dependencies["serde"];
        assert_eq!(
            serde.source,
            DepSource::Registry {
                version: "1.2".into(),
                registry: None
            }
        );
        assert!(serde.default_features);
    }

    #[test]
    fn detailed_registry_dep_with_features_and_named_registry() {
        let m = Manifest::parse(FULL).unwrap();
        assert_eq!(m.dependencies["http"].features, ["tls"]);
        assert_eq!(
            m.dependencies["internal"].source,
            DepSource::Registry {
                version: "2.0".into(),
                registry: Some("myco".into())
            }
        );
    }

    #[test]
    fn path_and_git_dep_forms() {
        let m = Manifest::parse(FULL).unwrap();
        assert_eq!(
            m.dependencies["foo-local"].source,
            DepSource::Path {
                path: "../foo".into()
            }
        );
        assert_eq!(
            m.dependencies["bar-git"].source,
            DepSource::Git {
                url: "https://example.com/bar".into(),
                reference: GitRef::Rev("abc123".into()),
            }
        );
    }

    #[test]
    fn optional_and_default_features_flag() {
        let m = Manifest::parse(FULL).unwrap();
        let opt = &m.dependencies["opt-dep"];
        assert!(opt.optional);
        assert!(!opt.default_features);
    }

    #[test]
    fn macros_recursion_limit_parses() {
        let m =
            Manifest::parse("[package]\nname = \"x\"\n[macros]\nrecursion_limit = 256\n").unwrap();
        assert_eq!(m.macro_recursion_limit, Some(256));
        // Absent table → None (default applies).
        let m2 = Manifest::parse("[package]\nname = \"x\"\n").unwrap();
        assert_eq!(m2.macro_recursion_limit, None);
    }

    #[test]
    fn registries_and_default() {
        let m = Manifest::parse(FULL).unwrap();
        assert_eq!(m.default_registry.as_deref(), Some("public"));
        assert_eq!(
            m.registries["public"].index,
            "sparse+https://pkgs.example.dev/index"
        );
        assert!(m.registries["myco"].auth);
        assert!(!m.registries["public"].auth);
    }

    #[test]
    fn features_and_file_import_allowlist() {
        let m = Manifest::parse(FULL).unwrap();
        assert_eq!(m.features["json"], ["dep:serde"]);
        assert_eq!(m.file_import_allow, ["../shared", "assets/**"]);
    }

    #[test]
    fn library_kind_defaults_entry_to_lib() {
        let m = Manifest::parse("[package]\nname=\"l\"\nkind=\"library\"\n").unwrap();
        assert_eq!(m.package.kind, PackageKind::Library);
        assert_eq!(m.entry_path(), "src/lib.otter");
        assert!(m.package.kind.is_consumable());
        assert!(!m.package.kind.has_binary());
    }

    #[test]
    fn library_bins_walks_extra_entries() {
        let src = "[package]\nname=\"l\"\nkind=\"library+bins\"\nbins=[\"src/bin/tool.otter\"]\n";
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.entry_paths(), ["src/lib.otter", "src/bin/tool.otter"]);
        assert!(m.package.kind.is_consumable());
        assert!(m.package.kind.has_binary());
    }

    #[test]
    fn binary_kind_defaults() {
        let m = Manifest::parse("[package]\nname=\"app\"\n").unwrap();
        assert_eq!(m.package.kind, PackageKind::Binary);
        assert_eq!(m.entry_path(), "src/main.otter");
        assert!(!m.package.kind.is_consumable());
    }

    #[test]
    fn conflicting_dep_sources_are_rejected() {
        let src =
            "[package]\nname=\"a\"\n[dependencies]\nx = { path = \"../x\", git = \"http://y\" }\n";
        assert!(matches!(
            Manifest::parse(src),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn git_with_two_pins_is_rejected() {
        let src = "[package]\nname=\"a\"\n[dependencies]\nx = { git = \"http://y\", rev=\"a\", tag=\"b\" }\n";
        assert!(matches!(
            Manifest::parse(src),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn dep_without_source_is_rejected() {
        let src = "[package]\nname=\"a\"\n[dependencies]\nx = { features = [\"a\"] }\n";
        assert!(matches!(
            Manifest::parse(src),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let src = "[package]\nname=\"a\"\nkind=\"frobnicator\"\n";
        assert!(matches!(
            Manifest::parse(src),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn syntactically_broken_toml_is_an_error() {
        assert!(matches!(
            Manifest::parse("[package"),
            Err(ManifestError::Toml(_))
        ));
    }

    #[test]
    fn git_default_ref_when_unpinned() {
        let src = "[package]\nname=\"a\"\n[dependencies]\nx = { git = \"http://y\" }\n";
        let m = Manifest::parse(src).unwrap();
        assert_eq!(
            m.dependencies["x"].source,
            DepSource::Git {
                url: "http://y".into(),
                reference: GitRef::Default
            }
        );
    }
}
