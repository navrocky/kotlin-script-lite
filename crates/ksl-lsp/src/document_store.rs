use std::collections::HashMap;
use std::sync::Arc;

use ksl_core::ast::Program;
use tower_lsp::lsp_types::Url;

/// A document's current text as the client last sent it, plus the most
/// recent tree that actually parsed. `text` fails to parse a large
/// fraction of the time an editor calls in (mid-statement, mid-token) —
/// `last_good` is what hover/goto-definition fall back to when that
/// happens, so they don't go blank on every incomplete edit. Diagnostics
/// always reflect `text` itself, never the fallback.
#[derive(Debug, Clone)]
pub struct DocumentState {
    pub text: String,
    pub last_good: Option<Arc<Program>>,
}

/// Open documents, keyed by URI. Full-text sync only (`TextDocumentSyncKind::FULL`)
/// — every `didChange` replaces the whole text, so no incremental patching.
#[derive(Default)]
pub struct DocumentStore {
    docs: HashMap<Url, DocumentState>,
}

impl DocumentStore {
    pub fn open(&mut self, uri: Url, text: String) {
        self.docs.insert(uri, DocumentState { text, last_good: None });
    }

    /// Replaces the text, carrying the previous `last_good` tree forward
    /// (if any) until a fresh parse of the new text succeeds and replaces
    /// it via `set_last_good`.
    pub fn update(&mut self, uri: &Url, text: String) {
        let last_good = self.docs.get(uri).and_then(|d| d.last_good.clone());
        self.docs.insert(uri.clone(), DocumentState { text, last_good });
    }

    pub fn close(&mut self, uri: &Url) {
        self.docs.remove(uri);
    }

    pub fn get(&self, uri: &Url) -> Option<&DocumentState> {
        self.docs.get(uri)
    }

    pub fn set_last_good(&mut self, uri: &Url, program: Arc<Program>) {
        if let Some(doc) = self.docs.get_mut(uri) {
            doc.last_good = Some(program);
        }
    }
}
