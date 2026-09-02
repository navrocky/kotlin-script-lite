mod analysis;
mod backend;
mod completion;
mod diagnostics;
mod document_store;
mod goto_definition;
mod hover;
mod package_index;
mod runtime_diagnostics;
mod semantic_tokens;

use backend::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
