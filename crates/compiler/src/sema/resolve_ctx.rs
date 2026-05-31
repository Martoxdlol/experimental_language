//! Resolution context for imports (`docs/17` §17.13).
//!
//! The compiler builds the module tree from the parsed root plus the loaded
//! submodule bodies, but *availability* of an import scheme depends on the run
//! mode — `pkg:` and both `self:` forms need **project context**; `file:`
//! escaping the package needs an allowlist (project mode) or is unrestricted
//! (direct mode). The driver constructs a [`ResolveContext`] describing these
//! facts and hands it to analysis; [`crate::sema::symbols::Program`] consults it
//! while binding imports.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Everything import resolution needs to know about the run mode and project.
#[derive(Clone, Debug, Default)]
pub struct ResolveContext {
    /// Whether there is project context (a manifest + module tree). When
    /// `false`, `pkg:` and `self:` imports are hard errors.
    pub project: bool,
    /// The package name, for `self:`-escape diagnostics.
    pub package_name: Option<String>,
    /// `no-std`: any `std:` import is a hard error (`docs/23` §6).
    pub no_std: bool,
    /// The absolute source root — the package boundary that `self:` may not
    /// cross and that gates escaping `file:` paths.
    pub source_root: Option<PathBuf>,
    /// The package root (directory holding the manifest). `[file-imports]`
    /// allowlist entries resolve relative to it.
    pub package_root: Option<PathBuf>,
    /// Module path (from root) → its source file, for relative `self:`/`file:`
    /// resolution and for inverting a resolved file back to a module.
    pub file_of: HashMap<Vec<String>, PathBuf>,
    /// `[file-imports] allow` roots/globs (project mode).
    pub file_import_allow: Vec<String>,
    /// Declared dependency names, for `pkg:` resolution + diagnostics.
    pub dependencies: HashSet<String>,
    /// Resolved dependency packages: `pkg:<name>` → the module-tree key under
    /// which the driver loaded that package's entry into `externals` (e.g.
    /// `["__pkg__", "json"]`). The compiler collects those subtrees and resolves
    /// `pkg:<name>` against the package's public surface.
    pub packages: HashMap<String, Vec<String>>,
    /// `file:` import targets: normalized target file → the `externals` key under
    /// which the driver loaded it (`["__file__", N]`). The compiler collects each
    /// as a standalone module and binds `file:` imports against it.
    pub file_targets: HashMap<PathBuf, Vec<String>>,
    /// `[macros] recursion_limit` from the manifest, if set — the procedural-
    /// macro expansion depth limit (`docs/22` §10). `None` uses the default.
    pub macro_recursion_limit: Option<usize>,
}

impl ResolveContext {
    /// A direct-mode context with no project: only `core:`/`std:`/`file:` work.
    pub fn direct() -> Self {
        ResolveContext::default()
    }

    /// The inverse of [`Self::file_of`]: normalized file → module path.
    pub fn file_to_module(&self) -> HashMap<PathBuf, Vec<String>> {
        self.file_of.iter().map(|(mp, f)| (normalize(f), mp.clone())).collect()
    }
}

/// Lexically normalize a path (resolve `.`/`..` without touching the
/// filesystem), so two spellings of the same file compare equal. Unlike
/// `canonicalize`, this works for files that may not exist and never performs
/// IO — exactly what import resolution needs.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
