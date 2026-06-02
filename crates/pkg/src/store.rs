//! The content-addressed package store (`docs/23` §7).
//!
//! Each package version is downloaded once and extracted into a store keyed by
//! its `sha256`, under `~/.otter_fusion/registry/`:
//!
//! ```text
//! ~/.otter_fusion/registry/
//!   index/   sparse-index cache
//!   cache/   raw .tar.gz tarballs (by checksum)
//!   src/     extracted source (by checksum)
//! ```
//!
//! The `sha256` recorded in the lockfile is verified on every fetch and
//! extraction, so a registry cannot serve different bytes for an already-locked
//! version without the build failing.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Compute the lowercase hex `sha256` of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A `sha256:<hex>` checksum string for `bytes` (the lockfile format).
pub fn checksum(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// Verify `bytes` hash to `expected` (a `sha256:<hex>` string). A mismatch is a
/// hard error — never a silent re-download.
pub fn verify(bytes: &[u8], expected: &str) -> Result<(), StoreError> {
    let want = expected.strip_prefix("sha256:").unwrap_or(expected);
    let got = sha256_hex(bytes);
    if got.eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err(StoreError::Checksum {
            expected: want.to_string(),
            got,
        })
    }
}

/// Errors from store operations.
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Checksum { expected: String, got: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "{e}"),
            StoreError::Checksum { expected, got } => write!(
                f,
                "checksum mismatch: expected sha256:{expected}, got sha256:{got}"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// The on-disk package store.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open the store at an explicit root (its subdirectories are created lazily).
    pub fn at(root: PathBuf) -> Store {
        Store { root }
    }

    /// Open the user-global store, honoring `OTTER_FUSION_HOME` then `HOME` /
    /// `USERPROFILE`, falling back to `.otter_fusion` in the cwd.
    pub fn user() -> Store {
        let base = std::env::var_os("OTTER_FUSION_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".otter_fusion")))
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".otter_fusion"))
            })
            .unwrap_or_else(|| PathBuf::from(".otter_fusion"));
        Store {
            root: base.join("registry"),
        }
    }

    pub fn index_dir(&self) -> PathBuf {
        self.root.join("index")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    pub fn src_dir(&self) -> PathBuf {
        self.root.join("src")
    }

    /// The extracted-source directory for a package keyed by its checksum.
    pub fn src_path(&self, checksum_hex: &str) -> PathBuf {
        let hex = checksum_hex.strip_prefix("sha256:").unwrap_or(checksum_hex);
        self.src_dir().join(hex)
    }

    /// Store a raw tarball in the cache after verifying its checksum, returning
    /// the cached path. Idempotent.
    pub fn cache_tarball(&self, tar_gz: &[u8], expected: &str) -> Result<PathBuf, StoreError> {
        verify(tar_gz, expected)?;
        let hex = expected.strip_prefix("sha256:").unwrap_or(expected);
        std::fs::create_dir_all(self.cache_dir())?;
        let path = self.cache_dir().join(format!("{hex}.tar.gz"));
        if !path.exists() {
            std::fs::write(&path, tar_gz)?;
        }
        Ok(path)
    }

    /// Extract a `.tar.gz` into the content-addressed source store, verifying its
    /// `sha256` first. Returns the extracted directory. Idempotent — an existing
    /// extraction for the same checksum is reused.
    pub fn extract(&self, tar_gz: &[u8], expected: &str) -> Result<PathBuf, StoreError> {
        verify(tar_gz, expected)?;
        let dest = self.src_path(expected);
        if dest.exists() {
            return Ok(dest);
        }
        // Extract into a temp sibling, then atomically rename into place so a
        // crash mid-extraction never leaves a half-populated keyed directory.
        let tmp = dest.with_extension("tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let decoder = flate2::read::GzDecoder::new(tar_gz);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(&tmp)?;
        // If a concurrent build won the race, keep theirs.
        if dest.exists() {
            let _ = std::fs::remove_dir_all(&tmp);
        } else {
            std::fs::create_dir_all(dest.parent().unwrap())?;
            std::fs::rename(&tmp, &dest)?;
        }
        Ok(dest)
    }

    /// Read a cached sparse-index metadata file for `name`, if present.
    pub fn read_index(&self, name: &str) -> Option<String> {
        let path = index_path(&self.index_dir(), name);
        std::fs::read_to_string(path).ok()
    }

    /// Write a sparse-index metadata file for `name`.
    pub fn write_index(&self, name: &str, contents: &str) -> Result<(), StoreError> {
        let path = index_path(&self.index_dir(), name);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, contents)?;
        Ok(())
    }
}

/// The sparse-index file layout for a crate name: 1-char → `1/<name>`,
/// 2-char → `2/<name>`, 3-char → `3/<c>/<name>`, else `<c1c2>/<c3c4>/<name>`
/// (the cargo/crates.io convention).
pub fn index_path(index_dir: &Path, name: &str) -> PathBuf {
    let lower = name.to_lowercase();
    let dir = match lower.len() {
        0 => return index_dir.join(lower),
        1 => index_dir.join("1"),
        2 => index_dir.join("2"),
        3 => index_dir.join("3").join(&lower[0..1]),
        _ => index_dir.join(&lower[0..2]).join(&lower[2..4]),
    };
    dir.join(lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("otter_store_{tag}_{n}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Build an in-memory `.tar.gz` with the given (path, contents) entries.
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
        let mut encoder = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap();
        gz
    }

    #[test]
    fn sha256_matches_known_vector() {
        // sha256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_accepts_match_and_rejects_mismatch() {
        let bytes = b"hello world";
        let ck = checksum(bytes);
        assert!(verify(bytes, &ck).is_ok());
        assert!(matches!(
            verify(b"tampered", &ck),
            Err(StoreError::Checksum { .. })
        ));
    }

    #[test]
    fn extract_verifies_and_unpacks_content_addressed() {
        let store = Store::at(temp_dir("extract"));
        let gz = make_tar_gz(&[("lib.otter", "pub function f(): i64 { 1 }\n")]);
        let ck = checksum(&gz);
        let dir = store.extract(&gz, &ck).unwrap();
        let content = std::fs::read_to_string(dir.join("lib.otter")).unwrap();
        assert!(content.contains("pub function f"));
        // Keyed by checksum.
        assert!(dir.ends_with(ck.strip_prefix("sha256:").unwrap()));
        // Idempotent: extracting again returns the same path without error.
        assert_eq!(store.extract(&gz, &ck).unwrap(), dir);
    }

    #[test]
    fn extract_rejects_a_corrupted_tarball() {
        let store = Store::at(temp_dir("corrupt"));
        let gz = make_tar_gz(&[("lib.otter", "x")]);
        let wrong = checksum(b"different bytes");
        assert!(matches!(
            store.extract(&gz, &wrong),
            Err(StoreError::Checksum { .. })
        ));
    }

    #[test]
    fn index_path_follows_the_sharding_convention() {
        let d = Path::new("/idx");
        assert_eq!(index_path(d, "a"), Path::new("/idx/1/a"));
        assert_eq!(index_path(d, "ab"), Path::new("/idx/2/ab"));
        assert_eq!(index_path(d, "abc"), Path::new("/idx/3/a/abc"));
        assert_eq!(index_path(d, "serde"), Path::new("/idx/se/rd/serde"));
    }
}
