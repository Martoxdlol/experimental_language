//! `pkg` — the Otter Fusion package manager.
//!
//! This crate owns everything *outside* the compiler proper that the
//! module/import and dependency systems need:
//!
//! * [`manifest`] — `project.toml` parsing (`docs/23` §8).
//! * [`project`] — project discovery + context (`docs/17` §17.1, §17.13).
//!
//! Later phases add the lockfile, semver resolver, content-addressed store, and
//! registry protocol (`docs/23` §7). The compiler stays pure: it receives
//! resolved module sources and the external-package map as plain data.

pub mod commands;
pub mod credentials;
pub mod loader;
pub mod lockfile;
pub mod manifest;
pub mod package;
pub mod project;
pub mod registry;
pub mod resolve;
pub mod server;
pub mod store;
pub mod version;

pub use lockfile::{LOCKFILE_VERSION, LockSource, LockedPackage, Lockfile};
pub use manifest::{
    DepSource, Dependency, GitRef, Manifest, ManifestError, Package, PackageKind, Registry,
};
pub use project::{DiscoverError, MANIFEST_NAME, ProjectContext};
pub use store::{Store, StoreError, checksum, sha256_hex, verify};
