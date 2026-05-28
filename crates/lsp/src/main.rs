//! `otter_fusion_lsp` — the Language Server Protocol implementation for the language.
//!
//! Speaks LSP over stdio (the transport every editor uses). The actual feature
//! logic lives in [`server::Backend`]; this entry point only wires the
//! `tower-lsp` service to stdin/stdout.

mod analysis;
mod server;

use server::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
