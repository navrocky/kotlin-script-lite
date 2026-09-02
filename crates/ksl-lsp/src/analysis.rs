use std::sync::Arc;

use ksl_core::ast::Program;
use ksl_core::resolver::{self, ResolveResult};

/// A resolved document, ready for hover/goto-definition queries.
pub struct Analysis {
    pub resolved: ResolveResult,
    /// False if `text` itself didn't parse and this fell back to a
    /// previously cached tree — see the module doc comment on
    /// `document_store::DocumentState`.
    pub is_current: bool,
}

/// Parses `text`; on success, resolves it and returns that. On failure,
/// falls back to re-resolving `last_good` if given, so hover/goto-definition
/// keep working against the last text that did parse rather than going
/// blank on every incomplete edit. Returns `None` only when `text` fails to
/// parse and there's no fallback tree yet (e.g. the very first keystroke of
/// a new, empty-then-broken document).
///
/// Callers that also need to cache the newly-parsed tree as the document's
/// new "last good" (see `Backend::update_last_good_tree`) do their own
/// separate `parse_only` for that, rather than this function handing back
/// `program` — resolving is real work (a full AST walk), and running it on
/// every keystroke just to maybe cache a tree nobody asked to resolve yet
/// would be wasted when no hover/goto-definition request is pending.
pub fn analyze(text: &str, last_good: Option<Arc<Program>>) -> Option<Analysis> {
    match ksl_core::parse_only(text) {
        Ok(program) => {
            let resolved = resolver::resolve(&program);
            Some(Analysis { resolved, is_current: true })
        }
        Err(_) => last_good.map(|program| {
            let resolved = resolver::resolve(&program);
            Analysis { resolved, is_current: false }
        }),
    }
}
