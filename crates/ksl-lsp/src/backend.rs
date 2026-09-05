use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, Diagnostic, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    MessageType, OneOf, SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{self, Analysis};
use crate::diagnostics::static_diagnostics;
use crate::document_store::DocumentStore;
use crate::semantic_tokens::{semantic_tokens, TOKEN_TYPES};

pub struct Backend {
    client: Client,
    docs: RwLock<DocumentStore>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend { client, docs: RwLock::new(DocumentStore::default()) }
    }

    async fn publish(&self, uri: Url, diagnostics: Vec<Diagnostic>) {
        self.client.publish_diagnostics(uri, diagnostics, None).await;
    }

    async fn publish_static_diagnostics(&self, uri: Url, text: &str) {
        let diags = static_diagnostics(text, &uri_to_path(&uri));
        self.publish(uri, diags).await;
    }

    /// If `text` parses, caches it as `uri`'s new "last good" tree — what
    /// hover/goto-definition fall back to while the buffer doesn't parse.
    fn update_last_good_tree(&self, uri: &Url, text: &str) {
        if let Ok(program) = ksl_core::parse_only(text) {
            self.docs.write().unwrap().set_last_good(uri, Arc::new(program));
        }
    }

    /// Builds hover/goto-definition/completion analysis for `uri` from its
    /// current text, falling back to its last good tree if the current
    /// text doesn't parse. `None` only if the document isn't open or
    /// neither the current text nor any fallback parses.
    fn analysis_for(&self, uri: &Url) -> Option<Analysis> {
        let docs = self.docs.read().unwrap();
        let doc = docs.get(uri)?;
        analysis::analyze(&doc.text, doc.last_good.clone(), &uri_to_path(uri))
    }

}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> RpcResult<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions::default()),
                semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
                    SemanticTokensOptions {
                        legend: SemanticTokensLegend {
                            token_types: TOKEN_TYPES.to_vec(),
                            token_modifiers: Vec::new(),
                        },
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "ksl-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client.log_message(MessageType::INFO, "ksl-lsp initialized").await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.docs.write().unwrap().open(uri.clone(), text.clone());
        self.update_last_good_tree(&uri, &text);
        self.publish_static_diagnostics(uri, &text).await;
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        // Full-document sync: the (single) change event carries the whole
        // new text, not an incremental patch.
        let text = match params.content_changes.pop() {
            Some(change) => change.text,
            None => return,
        };
        self.docs.write().unwrap().update(&uri, text.clone());
        self.update_last_good_tree(&uri, &text);
        self.publish_static_diagnostics(uri, &text).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = match self.docs.read().unwrap().get(&uri) {
            Some(doc) => doc.text.clone(),
            None => return,
        };
        self.publish_static_diagnostics(uri, &text).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.docs.write().unwrap().close(&uri);
        self.publish(uri, Vec::new()).await;
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> RpcResult<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let text = match self.docs.read().unwrap().get(&uri) {
            Some(doc) => doc.text.clone(),
            None => return Ok(None),
        };
        Ok(semantic_tokens(&text).map(SemanticTokensResult::Tokens))
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(analysis) = self.analysis_for(&uri) else { return Ok(None) };
        Ok(crate::hover::hover(&analysis, position))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(analysis) = self.analysis_for(&uri) else { return Ok(None) };
        Ok(crate::goto_definition::goto_definition(&analysis, &uri, position))
    }

    async fn completion(&self, params: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(analysis) = self.analysis_for(&uri) else { return Ok(None) };
        // `completion` also needs the raw text (to detect an in-progress
        // `a.b.` prefix, which doesn't exist as an AST node until it's a
        // complete qualified name — see `completion::dotted_prefix_before_cursor`),
        // so it's fetched separately rather than threaded through `Analysis`.
        let text = match self.docs.read().unwrap().get(&uri) {
            Some(doc) => doc.text.clone(),
            None => return Ok(None),
        };
        let items: Vec<CompletionItem> = crate::completion::completion(&analysis, &text, position);
        Ok(Some(CompletionResponse::Array(items)))
    }
}

/// `Url::to_file_path` only fails for a non-`file://` URI, which `ksl-lsp`
/// never receives in practice (the client only ever opens real files) — the
/// current directory is a harmless fallback rather than a case worth
/// propagating as an error.
fn uri_to_path(uri: &Url) -> PathBuf {
    uri.to_file_path().unwrap_or_else(|_| PathBuf::from("."))
}
