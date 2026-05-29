//! Project discovery and context (`docs/17` §17.1, §17.13).
//!
//! A *project* is a directory containing a `project.toml` manifest. Discovery
//! walks up from a starting path to find that manifest; the resulting
//! [`ProjectContext`] carries everything later phases need to anchor `self:`
//! and `pkg:` imports: the root, the manifest, the source root, and the entry
//! set.

use std::path::{Path, PathBuf};

use crate::manifest::{Manifest, ManifestError};

/// The conventional manifest filename.
pub const MANIFEST_NAME: &str = "project.toml";

/// A discovered, parsed project.
#[derive(Clone, Debug)]
pub struct ProjectContext {
    /// The project root (the directory holding `project.toml`).
    pub root: PathBuf,
    /// The parsed manifest.
    pub manifest: Manifest,
}

/// Why project discovery failed when one was expected.
#[derive(Clone, Debug)]
pub enum DiscoverError {
    /// No `project.toml` found walking up from the start path.
    NotFound { start: PathBuf },
    /// The manifest was found but could not be read.
    Read { path: PathBuf, message: String },
    /// The manifest was found but is invalid.
    Parse { path: PathBuf, error: ManifestError },
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoverError::NotFound { start } => write!(
                f,
                "no `{MANIFEST_NAME}` found in `{}` or any parent directory",
                start.display()
            ),
            DiscoverError::Read { path, message } => {
                write!(f, "cannot read manifest `{}`: {message}", path.display())
            }
            DiscoverError::Parse { path, error } => {
                write!(f, "in `{}`: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for DiscoverError {}

impl ProjectContext {
    /// Walk up from `start` (a file or directory) looking for `project.toml`,
    /// parsing the first one found. Returns `Ok(None)` if none exists — that is
    /// not an error in itself (it means "no project context", `docs/17` §17.13).
    pub fn discover(start: &Path) -> Result<Option<ProjectContext>, DiscoverError> {
        let mut dir: Option<&Path> = if start.is_dir() { Some(start) } else { start.parent() };
        while let Some(d) = dir {
            let candidate = d.join(MANIFEST_NAME);
            if candidate.is_file() {
                let ctx = Self::load(&candidate)?;
                return Ok(Some(ctx));
            }
            dir = d.parent();
        }
        Ok(None)
    }

    /// Load a project from an explicit manifest path.
    pub fn load(manifest_path: &Path) -> Result<ProjectContext, DiscoverError> {
        let text = std::fs::read_to_string(manifest_path).map_err(|e| DiscoverError::Read {
            path: manifest_path.to_path_buf(),
            message: e.to_string(),
        })?;
        let manifest = Manifest::parse(&text)
            .map_err(|error| DiscoverError::Parse { path: manifest_path.to_path_buf(), error })?;
        let root = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(ProjectContext { root, manifest })
    }

    /// The absolute source root (`<root>/<src>`), the package boundary that
    /// `self:` imports may not escape (`docs/17` §17.4).
    pub fn source_root(&self) -> PathBuf {
        self.root.join(&self.manifest.package.src)
    }

    /// The primary entry file (absolute path).
    pub fn entry_file(&self) -> PathBuf {
        self.root.join(self.manifest.entry_path())
    }

    /// Every entry file the compiler walks (primary + extra `bins`), absolute.
    pub fn entry_files(&self) -> Vec<PathBuf> {
        self.manifest.entry_paths().into_iter().map(|e| self.root.join(e)).collect()
    }

    /// Whether `file` lies within this project's source root (a prerequisite
    /// for being part of the module tree).
    pub fn contains_source(&self, file: &Path) -> bool {
        let src = self.source_root();
        match (file.canonicalize(), src.canonicalize()) {
            (Ok(f), Ok(s)) => f.starts_with(&s),
            // Fall back to a lexical check if canonicalization fails (e.g. the
            // file does not exist yet).
            _ => file.starts_with(&src),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique temp dir under the system temp root (no external crates).
    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("otter_pkg_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn discover_walks_up_to_the_manifest() {
        let root = temp_dir("discover");
        fs::write(root.join("project.toml"), "[package]\nname=\"app\"\n").unwrap();
        let deep = root.join("src").join("util").join("nested");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("x.otter"), "// x\n").unwrap();

        let ctx = ProjectContext::discover(&deep.join("x.otter")).unwrap().unwrap();
        assert_eq!(ctx.manifest.package.name, "app");
        assert_eq!(
            ctx.root.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_returns_none_with_no_manifest() {
        let root = temp_dir("nomanifest");
        let f = root.join("loose.otter");
        fs::write(&f, "function main() {}\n").unwrap();
        assert!(ProjectContext::discover(&f).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn source_root_and_entry_paths() {
        let root = temp_dir("paths");
        fs::write(root.join("project.toml"), "[package]\nname=\"app\"\n").unwrap();
        let ctx = ProjectContext::load(&root.join("project.toml")).unwrap();
        assert_eq!(ctx.source_root(), ctx.root.join("src"));
        assert_eq!(ctx.entry_file(), ctx.root.join("src/main.otter"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn contains_source_lexical_check() {
        let root = temp_dir("contains");
        fs::write(root.join("project.toml"), "[package]\nname=\"app\"\n").unwrap();
        let ctx = ProjectContext::load(&root.join("project.toml")).unwrap();
        assert!(ctx.contains_source(&ctx.root.join("src/util/log.otter")));
        assert!(!ctx.contains_source(&ctx.root.join("other/thing.otter")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_manifest_surfaces_a_parse_error() {
        let root = temp_dir("badmanifest");
        fs::write(root.join("project.toml"), "[package]\n").unwrap(); // missing name
        match ProjectContext::load(&root.join("project.toml")) {
            Err(DiscoverError::Parse { .. }) => {}
            other => panic!("expected parse error, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }
}
