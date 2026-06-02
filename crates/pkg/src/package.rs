//! Packaging a project into a publishable `.tar.gz` (`docs/23` §3 `publish`).
//!
//! A published artifact is a gzipped tarball of the manifest plus the source
//! root, verified by `sha256` on download. This module builds that tarball
//! deterministically (entries sorted) so the same source yields the same bytes.

use std::io::Write;
use std::path::Path;

use crate::project::ProjectContext;

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
}
