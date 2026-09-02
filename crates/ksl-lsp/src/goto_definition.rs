use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Jump target for the symbol at `position`, or `None` if there's no
/// symbol there. `uri` is always the same document — this is single-file
/// only (the resolver doesn't index `@file:Import`ed files, see
/// `ksl_core::resolver`'s module doc comment), so a name that's actually
/// declared in an imported file simply won't resolve here.
pub fn goto_definition(analysis: &Analysis, uri: &Url, position: Position) -> Option<GotoDefinitionResponse> {
    let line = position.line + 1;
    let col = position.character + 1;
    let symbol = analysis.resolved.table.symbol_at(line, col)?;

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range: span_to_range(symbol.def_span),
    }))
}
