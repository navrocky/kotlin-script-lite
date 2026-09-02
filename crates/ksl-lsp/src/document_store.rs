use std::collections::HashMap;

use tower_lsp::lsp_types::Url;

/// A document's current text as the client last sent it.
#[derive(Debug, Clone)]
pub struct DocumentState {
    pub text: String,
}

/// Open documents, keyed by URI. Full-text sync only (`TextDocumentSyncKind::FULL`)
/// — every `didChange` replaces the whole text, so no incremental patching.
#[derive(Default)]
pub struct DocumentStore {
    docs: HashMap<Url, DocumentState>,
}

impl DocumentStore {
    pub fn open(&mut self, uri: Url, text: String) {
        self.docs.insert(uri, DocumentState { text });
    }

    pub fn update(&mut self, uri: &Url, text: String) {
        self.docs.insert(uri.clone(), DocumentState { text });
    }

    pub fn close(&mut self, uri: &Url) {
        self.docs.remove(uri);
    }

    pub fn get(&self, uri: &Url) -> Option<&DocumentState> {
        self.docs.get(uri)
    }
}
