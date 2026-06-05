//! The module-tree loader (`docs/17` §17.1–§17.2).
//!
//! Starting at one or more entry files, this walks the explicit `mod` tree,
//! parsing each referenced file and recording its module path from the package
//! root. It implements the two file-resolution rules exactly:
//!
//! * a `mod foo` in `dir/parent.otter` resolves to `dir/parent/foo.otter`; but
//! * a `mod foo` in a **top-level entry** (`lib.otter`/`main.otter`/a `bins`
//!   entry) resolves to a *sibling* `dir/foo.otter` — the source root stays flat.
//!
//! In project mode it also performs the **reachability walk**: every `.otter`
//! file under the source root must be reachable from an entry through a chain of
//! `mod` declarations, or it is a hard error (`docs/17` §17.1).
//!
//! The loader produces a primary [`Module`] (the entry) plus the [`Externals`]
//! map the compiler consumes, all backed by one [`SourceMap`] so diagnostics
//! carry real spans.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use compiler::ast::{ItemKind, Module, ModuleKind};
use compiler::lexer::lex;
use compiler::parser::parse;
use compiler::sema::symbols::Externals;
use compiler::span::{SourceMap, Span};

/// A loaded module tree ready for semantic analysis.
pub struct ModuleTree {
    /// The primary entry's parsed module (the compilation root).
    pub root: Module,
    /// Every file-backed submodule, keyed by module path from the root.
    pub externals: Externals,
    /// Source file backing each module path (root = `[]`).
    pub file_of: HashMap<Vec<String>, PathBuf>,
    /// `file:` import targets: normalized target file → the module-tree key under
    /// which it was loaded (`["__file__", N]`). The compiler resolves a `file:`
    /// import by recomputing the target path and looking it up here.
    pub file_targets: HashMap<PathBuf, Vec<String>>,
    /// The source map holding every loaded file (entry first → `FileId(0)`).
    pub map: SourceMap,
    /// Errors gathered while loading (lex/parse/IO/reachability).
    pub diagnostics: Vec<LoadDiag>,
}

/// One loader diagnostic. A `span` is present for lex/parse errors that point at
/// source; structural errors (missing file, unreferenced file) are message-only.
pub struct LoadDiag {
    pub message: String,
    pub span: Option<Span>,
}

impl LoadDiag {
    fn msg(message: impl Into<String>) -> Self {
        LoadDiag {
            message: message.into(),
            span: None,
        }
    }
    fn at(span: Span, message: impl Into<String>) -> Self {
        LoadDiag {
            message: message.into(),
            span: Some(span),
        }
    }
}

impl ModuleTree {
    /// Whether loading produced any error.
    pub fn has_errors(&self) -> bool {
        !self.diagnostics.is_empty()
    }
}

/// Load the module tree for a project (`docs/17` §17.13: project context).
///
/// `entries` are the project's entry files (primary first); `entries[0]` becomes
/// the compilation root. `source_root` bounds the reachability walk. `mod` is a
/// project feature, so it is permitted here.
pub fn load_project(entries: &[PathBuf], source_root: &Path) -> ModuleTree {
    let mut loader = Loader::new(/* allow_mod = */ true);
    loader.load_entries(entries);
    loader.load_file_imports();
    loader.check_reachability(source_root);
    loader.finish()
}

/// A resolved dependency package to load alongside the project: its name, its
/// library entry file, and its source root.
pub struct DepPackage {
    pub name: String,
    /// Stable package-instance key from the resolver. Different versions of the
    /// same package name must load under different keys.
    pub key: String,
    pub entry: PathBuf,
    pub source_root: PathBuf,
}

/// Load the project's module tree **and** each resolved dependency package's
/// module tree, all into one source map. A dependency's modules are keyed under
/// `["__pkg__", <package-instance-key>, …]` so the compiler can collect them as
/// distinct public subtrees even when multiple versions share the same package
/// name.
pub fn load_project_with_packages(
    entries: &[PathBuf],
    source_root: &Path,
    packages: &[DepPackage],
) -> ModuleTree {
    let mut loader = Loader::new(/* allow_mod = */ true);
    loader.load_entries(entries);
    for dep in packages {
        loader.load_package(&dep.key, &dep.entry);
    }
    loader.load_file_imports();
    loader.check_reachability(source_root);
    loader.finish()
}

/// Load a single loose file with **no project context** (`docs/17` §17.13:
/// direct/`exec` mode). `mod` declarations are rejected here — they are a
/// project-only feature that builds the explicit module tree.
pub fn load_loose(entry: &Path) -> ModuleTree {
    let mut loader = Loader::new(/* allow_mod = */ false);
    loader.load_entries(std::slice::from_ref(&entry.to_path_buf()));
    loader.load_file_imports();
    loader.finish()
}

struct Loader {
    map: SourceMap,
    externals: Externals,
    file_of: HashMap<Vec<String>, PathBuf>,
    file_targets: HashMap<PathBuf, Vec<String>>,
    diagnostics: Vec<LoadDiag>,
    /// The parsed root (primary entry).
    root: Option<Module>,
    /// Files reached through the `mod` tree (canonicalized when possible).
    reached: BTreeSet<PathBuf>,
    /// Whether `mod` declarations are permitted (project mode only).
    allow_mod: bool,
}

impl Loader {
    fn new(allow_mod: bool) -> Self {
        Loader {
            map: SourceMap::new(),
            externals: Externals::new(),
            file_of: HashMap::new(),
            file_targets: HashMap::new(),
            diagnostics: Vec::new(),
            root: None,
            reached: BTreeSet::new(),
            allow_mod,
        }
    }

    /// Follow `file:` imports: load each referenced `.otter` file as a standalone
    /// module keyed under `["__file__", N]` so the compiler can bind names from
    /// it (`docs/17` §17.4). Allowlist/escape gating is the compiler's job; the
    /// loader just makes the target available (best-effort).
    fn load_file_imports(&mut self) {
        use compiler::imports::{Scheme, classify};
        // Collect (importing file, raw path) pairs without holding a borrow.
        let mut work: Vec<(PathBuf, String)> = Vec::new();
        let scan = |module: &Module, file: &Path, work: &mut Vec<(PathBuf, String)>| {
            for item in &module.items {
                if let ItemKind::Import(imp) = &item.kind {
                    let raw = import_path_text(&imp.path);
                    if matches!(classify(&raw), Ok(p) if p.scheme == Scheme::File) {
                        work.push((file.to_path_buf(), raw));
                    }
                }
            }
        };
        if let Some(root) = &self.root {
            if let Some(f) = self.file_of.get(&Vec::new()).cloned() {
                scan(root, &f, &mut work);
            }
        }
        for (key, module) in &self.externals {
            if let Some(f) = self.file_of.get(key).cloned() {
                scan(module, &f, &mut work);
            }
        }
        let mut next = 0usize;
        for (importing_file, raw) in work {
            let Ok(parsed) = classify(&raw) else { continue };
            let target = file_import_target(&importing_file, &parsed);
            if self.file_targets.contains_key(&target) {
                continue;
            }
            let Some(module) = self.parse_file(&target) else {
                continue;
            };
            self.mark_reached(&target);
            let key = vec!["__file__".to_string(), next.to_string()];
            next += 1;
            self.file_of.insert(key.clone(), target.clone());
            self.externals.insert(key.clone(), module);
            self.file_targets.insert(target, key);
        }
    }

    fn load_entries(&mut self, entries: &[PathBuf]) {
        for (i, entry) in entries.iter().enumerate() {
            let Some(module) = self.parse_file(entry) else {
                continue;
            };
            self.mark_reached(entry);
            if i == 0 {
                self.file_of.insert(Vec::new(), entry.clone());
                self.descend(entry, &module, &mut Vec::new(), /* is_entry = */ true);
                self.root = Some(module);
            } else {
                // Extra `bins` entries: walk them for reachability only (each is
                // its own program; the compiler builds the primary entry here).
                self.descend(entry, &module, &mut Vec::new(), true);
            }
        }
        if self.root.is_none() {
            // Ensure a usable (empty) root so callers always get a Module.
            self.root = Some(Module {
                items: Vec::new(),
                inner_docs: Vec::new(),
                span: Span::new(
                    compiler::span::FileId(0),
                    compiler::span::BytePos(0),
                    compiler::span::BytePos(0),
                ),
            });
        }
    }

    /// Load a resolved dependency package's module tree under the
    /// `["__pkg__", <package-instance-key>]` key prefix (its entry is itself a
    /// top-level entry, so its `mod` children are siblings).
    fn load_package(&mut self, key: &str, entry: &Path) {
        let Some(module) = self.parse_file(entry) else {
            return;
        };
        self.mark_reached(entry);
        let mut prefix = vec!["__pkg__".to_string(), key.to_string()];
        self.file_of.insert(prefix.clone(), entry.to_path_buf());
        self.descend(entry, &module, &mut prefix, /* is_entry = */ true);
        self.externals
            .insert(vec!["__pkg__".to_string(), key.to_string()], module);
    }

    /// Lex + parse one file, recording its source and any front-end errors.
    fn parse_file(&mut self, path: &Path) -> Option<Module> {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                self.diagnostics.push(LoadDiag::msg(format!(
                    "cannot read `{}`: {e}",
                    path.display()
                )));
                return None;
            }
        };
        let file = self.map.add_file(path.display().to_string(), src.clone());
        let (tokens, lex_errors) = lex(&src, file);
        for er in &lex_errors {
            self.diagnostics
                .push(LoadDiag::at(er.span, er.kind.to_string()));
        }
        let (module, parse_errors) = parse(&src, &tokens);
        for er in &parse_errors {
            self.diagnostics
                .push(LoadDiag::at(er.span, er.kind.to_string()));
        }
        Some(module)
    }

    /// Recursively follow external `mod` declarations from `module` (whose file
    /// is `file`, at module path `mod_path`). `is_entry` selects the sibling vs.
    /// child-directory file-resolution rule (`docs/17` §17.2).
    fn descend(
        &mut self,
        file: &Path,
        module: &Module,
        mod_path: &mut Vec<String>,
        is_entry: bool,
    ) {
        // Child files live beside an entry, else under `<parent-stem>/`.
        let dir = if is_entry {
            file.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        } else {
            file.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(file.file_stem().unwrap_or_default())
        };
        for item in &module.items {
            let ItemKind::Module(m) = &item.kind else {
                continue;
            };
            if !matches!(m.kind, ModuleKind::External) {
                continue;
            }
            if !self.allow_mod {
                self.diagnostics.push(LoadDiag::msg(format!(
                    "`mod {}` requires a project: declaring modules needs a `project.toml` \
                     (run inside a package, not as a loose file)",
                    m.name.name
                )));
                continue;
            }
            let child_path = dir.join(format!("{}.otter", m.name.name));
            let Some(child) = self.parse_file(&child_path) else {
                self.diagnostics.push(LoadDiag::msg(format!(
                    "cannot find module `{}`: expected file `{}`",
                    m.name.name,
                    child_path.display()
                )));
                continue;
            };
            self.mark_reached(&child_path);
            mod_path.push(m.name.name.clone());
            self.file_of.insert(mod_path.clone(), child_path.clone());
            self.descend(&child_path, &child, mod_path, /* is_entry = */ false);
            self.externals.insert(mod_path.clone(), child);
            mod_path.pop();
        }
    }

    fn mark_reached(&mut self, path: &Path) {
        self.reached
            .insert(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    }

    /// Enforce that every `.otter` under `source_root` is reachable from an entry
    /// (`docs/17` §17.1). An unreferenced file is a hard error.
    fn check_reachability(&mut self, source_root: &Path) {
        let mut all = Vec::new();
        collect_otter_files(source_root, &mut all);
        for f in all {
            let canon = f.canonicalize().unwrap_or_else(|_| f.clone());
            if !self.reached.contains(&canon) {
                self.diagnostics.push(LoadDiag::msg(format!(
                    "unreferenced source file: {}\n  expected a `mod` declaration reaching it \
                     from an entry (`docs/17` §17.1)",
                    f.display()
                )));
            }
        }
    }

    fn finish(self) -> ModuleTree {
        ModuleTree {
            root: self.root.expect("root set by load_entries"),
            externals: self.externals,
            file_of: self.file_of,
            file_targets: self.file_targets,
            map: self.map,
            diagnostics: self.diagnostics,
        }
    }
}

/// The literal text of an import path string literal (only `Text` parts).
fn import_path_text(lit: &compiler::ast::StringLit) -> String {
    use compiler::ast::StringPart;
    let mut s = String::new();
    for part in &lit.parts {
        if let StringPart::Text { text, .. } = part {
            s.push_str(text);
        }
    }
    s
}

/// Compute the normalized target file of a `file:` import relative to the
/// importing file's directory (`.otter` appended when no extension is given).
/// Must match the compiler's `file:` resolution.
pub fn file_import_target(
    importing_file: &Path,
    parsed: &compiler::imports::ImportPath,
) -> PathBuf {
    let mut dir = importing_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    for _ in 0..parsed.up {
        if let Some(p) = dir.parent() {
            dir = p.to_path_buf();
        }
    }
    for seg in &parsed.segments {
        dir.push(seg);
    }
    if dir.extension().is_none() {
        dir.set_extension("otter");
    }
    compiler::sema::resolve_ctx::normalize(&dir)
}

/// Recursively collect every `.otter` file under `dir` (sorted for determinism).
fn collect_otter_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            collect_otter_files(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("otter") {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("otter_loader_{tag}_{}", nonce()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn nonce() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn entry_mod_resolves_to_a_sibling_then_children_nest() {
        // src/main.otter: `mod util` -> src/util.otter (sibling, entry rule)
        // src/util.otter: `mod helpers` -> src/util/helpers.otter (child rule)
        let root = temp_dir("nest");
        let src = root.join("src");
        fs::create_dir_all(src.join("util")).unwrap();
        fs::write(src.join("main.otter"), "mod util;\nfunction main() {}\n").unwrap();
        fs::write(src.join("util.otter"), "mod helpers;\n").unwrap();
        fs::write(src.join("util/helpers.otter"), "// leaf\n").unwrap();

        let tree = load_project(&[src.join("main.otter")], &src);
        assert!(
            !tree.has_errors(),
            "diags: {:?}",
            tree.diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            tree.file_of[&vec!["util".to_string()]],
            src.join("util.otter")
        );
        assert_eq!(
            tree.file_of[&vec!["util".to_string(), "helpers".to_string()]],
            src.join("util/helpers.otter")
        );
        assert!(tree.externals.contains_key(&vec!["util".to_string()]));
        assert!(
            tree.externals
                .contains_key(&vec!["util".to_string(), "helpers".to_string()])
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreferenced_file_is_a_hard_error() {
        let root = temp_dir("unref");
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.otter"), "function main() {}\n").unwrap();
        fs::write(src.join("dead.otter"), "// not referenced by any mod\n").unwrap();

        let tree = load_project(&[src.join("main.otter")], &src);
        assert!(tree.has_errors());
        assert!(
            tree.diagnostics
                .iter()
                .any(|d| d.message.contains("unreferenced source file")
                    && d.message.contains("dead.otter"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_mod_file_is_reported() {
        let root = temp_dir("missing");
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("main.otter"), "mod ghost;\nfunction main() {}\n").unwrap();

        let tree = load_project(&[src.join("main.otter")], &src);
        assert!(
            tree.diagnostics
                .iter()
                .any(|d| d.message.contains("cannot find module `ghost`"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn loose_file_rejects_mod_declarations() {
        let root = temp_dir("loose");
        fs::create_dir_all(&root).unwrap();
        let f = root.join("script.otter");
        fs::write(&f, "mod helper;\nfunction main() {}\n").unwrap();

        let tree = load_loose(&f);
        assert!(
            tree.diagnostics
                .iter()
                .any(|d| d.message.contains("requires a project"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn loose_file_with_no_mod_loads_cleanly() {
        let root = temp_dir("loose_ok");
        fs::create_dir_all(&root).unwrap();
        let f = root.join("script.otter");
        fs::write(&f, "function main() {}\n").unwrap();
        let tree = load_loose(&f);
        assert!(!tree.has_errors());
        let _ = fs::remove_dir_all(&root);
    }
}
