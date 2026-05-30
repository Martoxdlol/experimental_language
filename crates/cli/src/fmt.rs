//! `otter_fusion fmt` formatting — the implementation lives in `compiler::fmt`
//! so it is shared with the LSP's document-formatting provider. This module
//! re-exports the entry points the CLI driver uses.

pub use compiler::fmt::{format_source, token_stream_preserved};
