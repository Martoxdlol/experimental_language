//! `otter_fusion lint` — the lint analysis lives in `compiler::lint` so it is
//! shared with the LSP (which publishes the same warnings as editor
//! diagnostics). This module re-exports the entry points the CLI driver uses.

pub use compiler::lint::{analyze, collect_lints};
