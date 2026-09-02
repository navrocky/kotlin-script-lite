use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Jump target for the symbol at `position`, or `None` if there's no
/// symbol there. Same-file declarations/uses resolve to a `Location` in
/// `uri` (the current document); a package-qualified `a.b.c` reference
/// resolves — via the cross-file package index, see
/// `analysis::Analysis::packages` — to a `Location` in whichever file
/// actually declares it.
pub fn goto_definition(analysis: &Analysis, uri: &Url, position: Position) -> Option<GotoDefinitionResponse> {
    let line = position.line + 1;
    let col = position.character + 1;

    if let Some(symbol) = analysis.resolved.table.symbol_at(line, col) {
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: span_to_range(symbol.def_span),
        }));
    }

    let (_, path) = analysis.resolved.table.qualified_ref_at(line, col)?;
    let (package, member) = path.split_at(path.len() - 1);
    let found = analysis.packages.find(&package.join("."), &member[0])?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: found.file.clone(),
        range: span_to_range(found.symbol.def_span),
    }))
}
