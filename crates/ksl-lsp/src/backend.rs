use std::collections::HashMap;
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
use crate::runtime_diagnostics::{run_diagnostics, RuntimeDiagnostics};
use crate::semantic_tokens::{semantic_tokens, TOKEN_TYPES};

pub struct Backend {
    client: Client,
    docs: RwLock<DocumentStore>,
    /// Per entry document, the URI (if any, and if different from the entry
    /// document itself) that the last `didSave` published *runtime*
    /// diagnostics to — e.g. a file pulled in via `@file:Import`. Needed so
    /// that fixing the import error clears the diagnostic on that other
    /// file rather than leaving it stuck.
    last_runtime_target: RwLock<HashMap<Url, Url>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            docs: RwLock::new(DocumentStore::default()),
            last_runtime_target: RwLock::new(HashMap::new()),
        }
    }

    async fn publish(&self, uri: Url, diagnostics: Vec<Diagnostic>) {
        self.client.publish_diagnostics(uri, diagnostics, None).await;
    }

    async fn publish_static_diagnostics(&self, uri: Url, text: &str) {
        let diags = static_diagnostics(text);
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

    /// Runs the script for runtime diagnostics, but only if it actually
    /// parses — a script that doesn't parse fails identically as a parse
    /// error, which `publish_static_diagnostics` already surfaced.
    async fn run_and_publish_runtime_diagnostics(&self, uri: Url, text: String) {
        if ksl_core::parse_only(&text).is_err() {
            self.clear_stale_runtime_target(&uri, None).await;
            return;
        }

        let entry_uri = uri.clone();
        // `run_diagnostics` blocks its calling thread on `recv_timeout` (up
        // to several seconds) while the script runs on a separate OS
        // thread, so run it off the async runtime's own worker threads.
        let result = match tokio::task::spawn_blocking(move || run_diagnostics(entry_uri, text)).await {
            Ok(r) => r,
            Err(_) => return,
        };
        let RuntimeDiagnostics { uri: target_uri, diagnostics } = result;

        self.clear_stale_runtime_target(&uri, Some(&target_uri)).await;
        self.publish(target_uri.clone(), diagnostics).await;

        let mut map = self.last_runtime_target.write().unwrap();
        if target_uri == uri {
            map.remove(&uri);
        } else {
            map.insert(uri, target_uri);
        }
    }

    /// Clears diagnostics left on a previously-targeted file (from an
    /// earlier `didSave`) when this save's target has moved elsewhere or
    /// disappeared, so a fixed import error doesn't stay stuck forever.
    async fn clear_stale_runtime_target(&self, entry_uri: &Url, new_target: Option<&Url>) {
        let previous = {
            let map = self.last_runtime_target.read().unwrap();
            map.get(entry_uri).cloned()
        };
        if let Some(prev) = previous {
            if Some(&prev) != new_target {
                self.publish(prev, Vec::new()).await;
            }
        }
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
        self.publish_static_diagnostics(uri.clone(), &text).await;
        self.run_and_publish_runtime_diagnostics(uri, text).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.docs.write().unwrap().close(&uri);
        self.last_runtime_target.write().unwrap().remove(&uri);
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
