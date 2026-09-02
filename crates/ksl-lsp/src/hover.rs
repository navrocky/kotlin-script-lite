use ksl_core::resolver::SymbolKind;
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Hover info for the symbol at `position` (0-based, LSP convention), or
/// `None` if there's no symbol there.
pub fn hover(analysis: &Analysis, position: Position) -> Option<Hover> {
    let line = position.line + 1;
    let col = position.character + 1;
    let (occurrence_span, symbol) = analysis.resolved.table.occurrence_at(line, col)?;

    let kind_label = match symbol.kind {
        SymbolKind::Val => "val",
        SymbolKind::Var => "var",
        SymbolKind::Fun => "function",
        SymbolKind::Param => "parameter",
    };
    let mut value = format!("```ksl\n{}\n```\n\n*{}*", symbol.signature, kind_label);
    if !analysis.is_current {
        value.push_str("\n\n_(from the last version of this file that parsed successfully)_");
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
        // Highlights the actual token under the cursor, not the (possibly
        // elsewhere) declaration.
        range: Some(span_to_range(occurrence_span)),
    })
}
