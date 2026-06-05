//! Packaging a project into a publishable `.tar.gz` (`docs/23` §3 `publish`).
//!
//! A published artifact is a gzipped tarball of the manifest plus the source
//! root, verified by `sha256` on download. This module builds that tarball
//! deterministically (entries sorted) so the same source yields the same bytes.

use std::io::Write;
use std::path::Path;

use crate::manifest::DepSource;
use crate::project::ProjectContext;
use crate::registry::{IndexDep, PublishMetadata};

/// Build a `.tar.gz` of a project's `project.toml` and source root, returning
/// the bytes and their `sha256:<hex>` checksum.
pub fn pack(proj: &ProjectContext) -> std::io::Result<(Vec<u8>, String)> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    // The manifest at the archive root.
    files.push((
        "project.toml".to_string(),
        std::fs::read(proj.root.join(crate::project::MANIFEST_NAME))?,
    ));
    // Every file under the source root, path-relative to the project root.
    let src = proj.source_root();
    let mut src_files = Vec::new();
    collect_files(&src, &mut src_files);
    src_files.sort();
    for path in src_files {
        let rel = path
            .strip_prefix(&proj.root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        files.push((rel, std::fs::read(&path)?));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        for (name, contents) in &files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0); // deterministic
            header.set_cksum();
            builder.append_data(&mut header, name, contents.as_slice())?;
        }
        builder.finish()?;
    }
    let mut gz = Vec::new();
    let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
    enc.write_all(&tar_bytes)?;
    enc.finish()?;
    let checksum = crate::store::checksum(&gz);
    Ok((gz, checksum))
}

/// Build the sparse-index metadata sidecar for publishing `proj`.
///
/// Registry metadata can only describe registry dependencies. A package that
/// depends on local paths or git sources cannot be resolved by downstream
/// consumers from a sparse index, so publishing rejects those manifests instead
/// of silently omitting edges.
pub fn publish_metadata(proj: &ProjectContext) -> Result<PublishMetadata, PublishMetadataError> {
    let mut deps = Vec::new();
    for (name, dep) in &proj.manifest.dependencies {
        match &dep.source {
            DepSource::Registry { version, registry } => {
                let req = crate::version::parse_req(version).map_err(|message| {
                    PublishMetadataError::InvalidRequirement {
                        name: name.clone(),
                        message,
                    }
                })?;
                deps.push(IndexDep {
                    name: name.clone(),
                    req,
                    optional: dep.optional,
                    default_features: dep.default_features,
                    features: dep.features.clone(),
                    registry: registry.clone(),
                });
            }
            DepSource::Path { path } => {
                return Err(PublishMetadataError::UnsupportedSource {
                    name: name.clone(),
                    source: format!("path `{path}`"),
                });
            }
            DepSource::Git { url, .. } => {
                return Err(PublishMetadataError::UnsupportedSource {
                    name: name.clone(),
                    source: format!("git `{url}`"),
                });
            }
        }
    }
    Ok(PublishMetadata {
        name: proj.manifest.package.name.clone(),
        vers: proj.manifest.package.version.clone(),
        deps,
        features: proj.manifest.features.clone(),
    })
}

/// Why publish metadata could not be generated from a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishMetadataError {
    InvalidRequirement { name: String, message: String },
    UnsupportedSource { name: String, source: String },
}

impl std::fmt::Display for PublishMetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishMetadataError::InvalidRequirement { name, message } => {
                write!(
                    f,
                    "dependency `{name}` has invalid version requirement: {message}"
                )
            }
            PublishMetadataError::UnsupportedSource { name, source } => write!(
                f,
                "dependency `{name}` uses {source}; registry-published packages must use registry dependencies"
            ),
        }
    }
}

impl std::error::Error for PublishMetadataError {}

fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("otter_pack_{tag}_{n}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn packs_manifest_and_sources_and_round_trips() {
        let root = temp_dir("pack");
        std::fs::write(
            root.join("project.toml"),
            "[package]\nname = \"mylib\"\nversion = \"1.0.0\"\nkind = \"library\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.otter"), "pub function f(): i64 { 1 }\n").unwrap();

        let proj = ProjectContext::load(&root.join("project.toml")).unwrap();
        let (gz, checksum) = pack(&proj).unwrap();
        assert!(checksum.starts_with("sha256:"));

        // Extract via the store and confirm the contents survive the round trip.
        let store = crate::store::Store::at(temp_dir("pack_store"));
        let extracted = store.extract(&gz, &checksum).unwrap();
        assert!(extracted.join("project.toml").exists());
        let lib = std::fs::read_to_string(extracted.join("src/lib.otter")).unwrap();
        assert!(lib.contains("pub function f"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn packing_is_deterministic() {
        let root = temp_dir("det");
        std::fs::write(
            root.join("project.toml"),
            "[package]\nname = \"x\"\nkind = \"library\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.otter"), "pub function f(): i64 { 1 }\n").unwrap();
        let proj = ProjectContext::load(&root.join("project.toml")).unwrap();
        let (_, c1) = pack(&proj).unwrap();
        let (_, c2) = pack(&proj).unwrap();
        assert_eq!(c1, c2, "packing must be reproducible");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn publish_metadata_records_registry_dependency_edges() {
        let root = temp_dir("metadata");
        std::fs::write(
            root.join("project.toml"),
            "[package]\nname = \"mylib\"\nversion = \"1.2.3\"\nkind = \"library\"\n\
             [dependencies]\n\
             dep = { version = \"0.4\", features = [\"tls\"], default-features = false, optional = true, registry = \"myco\" }\n\
             [features]\n\
             default = [\"dep:dep\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.otter"), "pub function f(): i64 { 1 }\n").unwrap();
        let proj = ProjectContext::load(&root.join("project.toml")).unwrap();

        let meta = publish_metadata(&proj).unwrap();

        assert_eq!(meta.name, "mylib");
        assert_eq!(meta.vers, "1.2.3");
        assert_eq!(meta.features["default"], ["dep:dep"]);
        assert_eq!(meta.deps.len(), 1);
        assert_eq!(meta.deps[0].name, "dep");
        assert_eq!(meta.deps[0].req.to_string(), "^0.4");
        assert!(meta.deps[0].optional);
        assert!(!meta.deps[0].default_features);
        assert_eq!(meta.deps[0].features, ["tls"]);
        assert_eq!(meta.deps[0].registry.as_deref(), Some("myco"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn publish_metadata_rejects_path_and_git_dependencies() {
        let path_root = temp_dir("metadata_path");
        std::fs::write(
            path_root.join("project.toml"),
            "[package]\nname = \"mylib\"\nkind = \"library\"\n\
             [dependencies]\nlocal = { path = \"../local\" }\n",
        )
        .unwrap();
        std::fs::create_dir_all(path_root.join("src")).unwrap();
        std::fs::write(
            path_root.join("src/lib.otter"),
            "pub function f(): i64 { 1 }\n",
        )
        .unwrap();
        let path_proj = ProjectContext::load(&path_root.join("project.toml")).unwrap();
        let err = publish_metadata(&path_proj).unwrap_err();
        assert!(err.to_string().contains("uses path"));

        let git_root = temp_dir("metadata_git");
        std::fs::write(
            git_root.join("project.toml"),
            "[package]\nname = \"mylib\"\nkind = \"library\"\n\
             [dependencies]\nremote = { git = \"https://example.com/repo.git\", rev = \"abc\" }\n",
        )
        .unwrap();
        std::fs::create_dir_all(git_root.join("src")).unwrap();
        std::fs::write(
            git_root.join("src/lib.otter"),
            "pub function f(): i64 { 1 }\n",
        )
        .unwrap();
        let git_proj = ProjectContext::load(&git_root.join("project.toml")).unwrap();
        let err = publish_metadata(&git_proj).unwrap_err();
        assert!(err.to_string().contains("uses git"));

        let _ = std::fs::remove_dir_all(&path_root);
        let _ = std::fs::remove_dir_all(&git_root);
    }
}
