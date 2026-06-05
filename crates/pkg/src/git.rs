//! Git dependency fetching for the package manager (`docs/23` §7).
//!
//! Git dependencies are resolved at lock time to an exact commit. The fetcher
//! keeps a bare mirror cache per URL and materializes immutable source
//! checkouts under `~/.otter_fusion/git/<url-hash>/<rev>/`, matching the layout
//! documented in the dependency model. The resolver records the exact commit in
//! `project.lock` and uses a deterministic tree checksum for verification and
//! lockfile stability.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::manifest::GitRef;
use crate::store::{Store, sha256_hex};

/// A fetched git dependency source tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCheckout {
    pub root: PathBuf,
    pub rev: String,
    pub checksum: String,
}

/// Why a git dependency could not be fetched or materialized.
#[derive(Debug)]
pub enum GitError {
    Io(std::io::Error),
    Command {
        program: &'static str,
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    MissingCheckout(PathBuf),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Io(e) => write!(f, "{e}"),
            GitError::Command {
                program,
                args,
                status,
                stderr,
            } => write!(
                f,
                "`{program} {}` failed{}{}",
                args.join(" "),
                status
                    .map(|s| format!(" with status {s}"))
                    .unwrap_or_default(),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", stderr.trim())
                }
            ),
            GitError::MissingCheckout(path) => {
                write!(f, "git checkout was not created at {}", path.display())
            }
        }
    }
}

impl std::error::Error for GitError {}

impl From<std::io::Error> for GitError {
    fn from(e: std::io::Error) -> Self {
        GitError::Io(e)
    }
}

/// Fetch `url`, resolve `reference` to a commit, and materialize its source
/// checkout in the store.
pub fn fetch(url: &str, reference: &GitRef, store: &Store) -> Result<GitCheckout, GitError> {
    let url_hash = sha256_hex(url.as_bytes());
    let mirror = store.git_cache_dir().join(format!("{url_hash}.git"));
    ensure_mirror(url, &mirror)?;
    let rev = resolve_commit(&mirror, reference)?;
    let checkout = store.git_checkout_path(&url_hash, &rev);
    ensure_checkout(&mirror, &rev, &checkout)?;
    let checksum = checksum_tree(&checkout)?;
    Ok(GitCheckout {
        root: checkout,
        rev,
        checksum,
    })
}

fn ensure_mirror(url: &str, mirror: &Path) -> Result<(), GitError> {
    if mirror.exists() {
        run_git(
            &[
                "-C",
                path_arg(mirror).as_str(),
                "fetch",
                "--prune",
                "--tags",
                "origin",
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
            None,
        )
    } else {
        if let Some(parent) = mirror.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run_git(&["clone", "--mirror", url, path_arg(mirror).as_str()], None)
    }
}

fn resolve_commit(mirror: &Path, reference: &GitRef) -> Result<String, GitError> {
    let spec = match reference {
        GitRef::Rev(rev) => format!("{rev}^{{commit}}"),
        GitRef::Branch(branch) => format!("refs/heads/{branch}^{{commit}}"),
        GitRef::Tag(tag) => format!("refs/tags/{tag}^{{commit}}"),
        GitRef::Default => "HEAD^{commit}".to_string(),
    };
    let out = run_git_capture(
        &[
            "-C",
            path_arg(mirror).as_str(),
            "rev-parse",
            "--verify",
            &spec,
        ],
        None,
    )?;
    Ok(out.trim().to_string())
}

fn ensure_checkout(mirror: &Path, rev: &str, checkout: &Path) -> Result<(), GitError> {
    if checkout.exists() {
        return Ok(());
    }
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = checkout.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    run_git(
        &[
            "clone",
            "--shared",
            "--no-checkout",
            path_arg(mirror).as_str(),
            path_arg(&tmp).as_str(),
        ],
        None,
    )?;
    run_git(
        &["-C", path_arg(&tmp).as_str(), "checkout", "--detach", rev],
        None,
    )?;
    let _ = std::fs::remove_dir_all(tmp.join(".git"));
    if checkout.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
    } else {
        std::fs::rename(&tmp, checkout)?;
    }
    if !checkout.exists() {
        return Err(GitError::MissingCheckout(checkout.to_path_buf()));
    }
    Ok(())
}

/// Deterministic checksum of a checked-out source tree, excluding `.git`.
pub fn checksum_tree(root: &Path) -> Result<String, GitError> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (rel, path) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(path)?);
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(format!("sha256:{hex}"))
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), GitError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) == Some(".git") {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
    Ok(())
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<(), GitError> {
    let output = command(args, cwd).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(args, output))
    }
}

fn run_git_capture(args: &[&str], cwd: Option<&Path>) -> Result<String, GitError> {
    let output = command(args, cwd).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(command_error(args, output))
    }
}

fn command(args: &[&str], cwd: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd
}

fn command_error(args: &[&str], output: std::process::Output) -> GitError {
    GitError::Command {
        program: "git",
        args: args.iter().map(|s| (*s).to_string()).collect(),
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("otter_git_{tag}_{n}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn run(repo: &Path, args: &[&str]) {
        run_git(args, Some(repo)).unwrap();
    }

    fn make_repo() -> (PathBuf, String) {
        let repo = temp_dir("repo");
        run(&repo, &["init"]);
        run(&repo, &["config", "user.name", "Otter Test"]);
        run(&repo, &["config", "user.email", "otter@example.invalid"]);
        std::fs::write(
            repo.join("project.toml"),
            "[package]\nname = \"gitlib\"\nversion = \"0.1.0\"\nkind = \"library\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.otter"), "pub function f(): i64 { 1 }\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-m", "initial"]);
        let rev = run_git_capture(&["rev-parse", "HEAD"], Some(&repo))
            .unwrap()
            .trim()
            .to_string();
        (repo, rev)
    }

    #[test]
    fn fetches_exact_rev_into_content_stable_checkout() {
        let (repo, rev) = make_repo();
        let store = Store::at(temp_dir("store").join("registry"));
        let checkout = fetch(repo.to_str().unwrap(), &GitRef::Rev(rev.clone()), &store).unwrap();
        assert_eq!(checkout.rev, rev);
        assert!(checkout.root.join("project.toml").exists());
        assert!(checkout.root.join("src/lib.otter").exists());
        assert!(!checkout.root.join(".git").exists());
        assert!(checkout.checksum.starts_with("sha256:"));

        let again = fetch(repo.to_str().unwrap(), &GitRef::Rev(rev), &store).unwrap();
        assert_eq!(again.root, checkout.root);
        assert_eq!(again.checksum, checkout.checksum);
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn fetches_branch_and_tag_to_exact_commit() {
        let (repo, rev) = make_repo();
        run(&repo, &["tag", "v0.1.0"]);
        let store = Store::at(temp_dir("store_branch").join("registry"));

        let branch = fetch(
            repo.to_str().unwrap(),
            &GitRef::Branch("master".into()),
            &store,
        )
        .or_else(|_| {
            fetch(
                repo.to_str().unwrap(),
                &GitRef::Branch("main".into()),
                &store,
            )
        })
        .unwrap();
        assert_eq!(branch.rev, rev);
        let tag = fetch(
            repo.to_str().unwrap(),
            &GitRef::Tag("v0.1.0".into()),
            &store,
        )
        .unwrap();
        assert_eq!(tag.rev, branch.rev);
        let _ = std::fs::remove_dir_all(repo);
    }
}
