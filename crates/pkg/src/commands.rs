//! Dependency-command logic (`docs/23` §3) — the resolver-backed operations the
//! CLI exposes as `tree`, `why`, `add`, `remove`, `lock`, and `update`. Kept
//! here (not in the driver) so they are unit-testable without spawning a process.

use toml_edit::{DocumentMut, Item, Table, value};

use crate::resolve::Resolved;

/// Render the resolved dependency graph as an indented tree (`otter_fusion tree`),
/// starting at the root package. Cycles are guarded (a repeat is marked `(*)`).
pub fn render_tree(resolved: &Resolved) -> String {
    let mut out = String::new();
    let root = &resolved.root_id;
    out.push_str(&resolved.root_name);
    out.push('\n');
    let mut on_path = vec![root.clone()];
    render_children(resolved, root, "", &mut on_path, &mut out);
    out
}

fn render_children(
    resolved: &Resolved,
    node: &str,
    prefix: &str,
    on_path: &mut Vec<String>,
    out: &mut String,
) {
    let children = resolved.edges.get(node).cloned().unwrap_or_default();
    let n = children.len();
    for (i, child) in children.iter().enumerate() {
        let last = i + 1 == n;
        let branch = if last { "└── " } else { "├── " };
        let (name, version) = resolved
            .get_by_id(child)
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .unwrap_or((child.as_str(), "?"));
        let cyclic = on_path.contains(child);
        out.push_str(prefix);
        out.push_str(branch);
        out.push_str(&format!("{name} v{version}"));
        if cyclic {
            out.push_str(" (*)");
        }
        out.push('\n');
        if !cyclic {
            let child_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            on_path.push(child.clone());
            render_children(resolved, child, &child_prefix, on_path, out);
            on_path.pop();
        }
    }
}

/// Explain why `target` is in the graph (`otter_fusion why`): every dependency
/// path from the root to `target`. Returns `None` if `target` is not present.
pub fn explain_why(resolved: &Resolved, target: &str) -> Option<String> {
    if resolved.packages_named(target).next().is_none() && target != resolved.root_name {
        return None;
    }
    let mut paths = Vec::new();
    let mut stack = vec![resolved.root_id.clone()];
    let mut visited = std::collections::HashSet::new();
    find_paths(
        resolved,
        &resolved.root_id,
        target,
        &mut stack,
        &mut visited,
        &mut paths,
    );
    if paths.is_empty() {
        return None;
    }
    let mut out = String::new();
    for path in paths {
        let labels: Vec<String> = path.iter().map(|id| why_label(resolved, id)).collect();
        out.push_str(&labels.join(" → "));
        out.push('\n');
    }
    Some(out)
}

fn find_paths(
    resolved: &Resolved,
    node: &str,
    target: &str,
    stack: &mut Vec<String>,
    visited: &mut std::collections::HashSet<String>,
    paths: &mut Vec<Vec<String>>,
) {
    let is_target = if node == resolved.root_id {
        target == resolved.root_name
    } else {
        resolved
            .get_by_id(node)
            .map(|p| p.name == target)
            .unwrap_or(false)
    };
    if is_target && stack.len() > 1 {
        paths.push(stack.clone());
        return;
    }
    if !visited.insert(node.to_string()) {
        return;
    }
    for child in resolved.edges.get(node).cloned().unwrap_or_default() {
        stack.push(child.clone());
        find_paths(resolved, &child, target, stack, visited, paths);
        stack.pop();
    }
    visited.remove(node);
}

fn why_label(resolved: &Resolved, id: &str) -> String {
    if id == resolved.root_id {
        return resolved.root_name.clone();
    }
    let Some(pkg) = resolved.get_by_id(id) else {
        return id.to_string();
    };
    if resolved.has_duplicate_name(&pkg.name) {
        format!("{} v{}", pkg.name, pkg.version)
    } else {
        pkg.name.clone()
    }
}

/// A dependency source for `add`.
pub enum AddSpec {
    /// `name = "<version>"`.
    Version(String),
    /// `name = { path = "<path>" }`.
    Path(String),
    /// `name = { git = "<url>" }`.
    Git(String),
}

/// Add (or replace) a dependency in a manifest's `[dependencies]` table,
/// preserving the rest of the document's formatting and comments.
pub fn add_dependency(manifest_text: &str, name: &str, spec: AddSpec) -> Result<String, String> {
    let mut doc: DocumentMut = manifest_text
        .parse()
        .map_err(|e| format!("invalid manifest: {e}"))?;
    let deps = ensure_table(&mut doc, "dependencies");
    match spec {
        AddSpec::Version(v) => {
            deps[name] = value(v);
        }
        AddSpec::Path(p) => {
            let mut t = toml_edit::InlineTable::new();
            t.insert("path", p.into());
            deps[name] = value(t);
        }
        AddSpec::Git(u) => {
            let mut t = toml_edit::InlineTable::new();
            t.insert("git", u.into());
            deps[name] = value(t);
        }
    }
    Ok(doc.to_string())
}

/// Remove a dependency from a manifest's `[dependencies]` table. Returns the new
/// text and whether anything was removed.
pub fn remove_dependency(manifest_text: &str, name: &str) -> Result<(String, bool), String> {
    let mut doc: DocumentMut = manifest_text
        .parse()
        .map_err(|e| format!("invalid manifest: {e}"))?;
    let removed = doc
        .get_mut("dependencies")
        .and_then(Item::as_table_mut)
        .map(|t| t.remove(name).is_some())
        .unwrap_or(false);
    Ok((doc.to_string(), removed))
}

/// Get a mutable `[<key>]` table, creating it if absent.
fn ensure_table<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut Table {
    if doc.get(key).and_then(Item::as_table).is_none() {
        doc[key] = Item::Table(Table::new());
    }
    doc[key].as_table_mut().expect("just created")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{LockSource, Lockfile};
    use crate::resolve::{Resolved, ResolvedPackage};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn pkg(name: &str, version: &str) -> ResolvedPackage {
        ResolvedPackage {
            id: name.into(),
            name: name.into(),
            version: version.into(),
            source: LockSource::Path {
                path: format!("../{name}"),
            },
            root: PathBuf::from(format!("/{name}")),
            direct: true,
        }
    }

    /// A small graph: app → a → c, app → b → c.
    fn sample() -> Resolved {
        let mut edges = BTreeMap::new();
        edges.insert("app".to_string(), vec!["a".to_string(), "b".to_string()]);
        edges.insert("a".to_string(), vec!["c".to_string()]);
        edges.insert("b".to_string(), vec!["c".to_string()]);
        Resolved {
            lockfile: Lockfile::empty(),
            packages: vec![pkg("a", "1.0.0"), pkg("b", "2.0.0"), pkg("c", "0.5.0")],
            edges,
            dependency_edges: BTreeMap::new(),
            root_name: "app".to_string(),
            root_id: "app".to_string(),
        }
    }

    #[test]
    fn tree_renders_indented_graph() {
        let tree = render_tree(&sample());
        assert!(tree.starts_with("app\n"));
        assert!(tree.contains("├── a v1.0.0"));
        assert!(tree.contains("└── b v2.0.0"));
        assert!(tree.contains("c v0.5.0"));
    }

    #[test]
    fn why_lists_every_path_to_a_dep() {
        let why = explain_why(&sample(), "c").unwrap();
        assert!(why.contains("app → a → c"));
        assert!(why.contains("app → b → c"));
    }

    #[test]
    fn why_returns_none_for_absent_dep() {
        assert!(explain_why(&sample(), "nonexistent").is_none());
    }

    #[test]
    fn add_version_dependency() {
        let m = "[package]\nname = \"app\"\n";
        let out = add_dependency(m, "serde", AddSpec::Version("1.2".into())).unwrap();
        assert!(out.contains("[dependencies]"));
        assert!(out.contains("serde = \"1.2\""));
    }

    #[test]
    fn add_path_dependency_inline_table() {
        let m = "[package]\nname = \"app\"\n[dependencies]\nexisting = \"1\"\n";
        let out = add_dependency(m, "foo", AddSpec::Path("../foo".into())).unwrap();
        assert!(out.contains("existing = \"1\""), "preserves existing deps");
        assert!(out.contains("foo = { path = \"../foo\" }"));
    }

    #[test]
    fn add_replaces_existing_dependency() {
        let m = "[package]\nname = \"app\"\n[dependencies]\nserde = \"1.0\"\n";
        let out = add_dependency(m, "serde", AddSpec::Version("2.0".into())).unwrap();
        assert!(out.contains("serde = \"2.0\""));
        assert!(!out.contains("serde = \"1.0\""));
    }

    #[test]
    fn remove_dependency_drops_the_entry() {
        let m = "[package]\nname = \"app\"\n[dependencies]\nserde = \"1\"\nhttp = \"2\"\n";
        let (out, removed) = remove_dependency(m, "serde").unwrap();
        assert!(removed);
        assert!(!out.contains("serde"));
        assert!(out.contains("http = \"2\""));
    }

    #[test]
    fn remove_absent_dependency_reports_false() {
        let m = "[package]\nname = \"app\"\n[dependencies]\nhttp = \"2\"\n";
        let (_out, removed) = remove_dependency(m, "ghost").unwrap();
        assert!(!removed);
    }

    #[test]
    fn add_preserves_comments() {
        let m = "# my project\n[package]\nname = \"app\"\n";
        let out = add_dependency(m, "x", AddSpec::Version("1".into())).unwrap();
        assert!(out.contains("# my project"));
    }
}
