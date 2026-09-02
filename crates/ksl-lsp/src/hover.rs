use ksl_core::error::Span;
use ksl_core::resolver::{Symbol, SymbolKind};
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Hover info for the symbol at `position` (0-based, LSP convention), or
/// `None` if there's no symbol there. Same-file declarations/uses are
/// checked first; a package-qualified `a.b.c` reference falls back to the
/// cross-file package index (see `analysis::Analysis::packages`).
pub fn hover(analysis: &Analysis, position: Position) -> Option<Hover> {
    let line = position.line + 1;
    let col = position.character + 1;

    if let Some((occurrence_span, symbol)) = analysis.resolved.table.occurrence_at(line, col) {
        return Some(build_hover(symbol, occurrence_span, analysis.is_current));
    }

    let (qualified_span, path) = analysis.resolved.table.qualified_ref_at(line, col)?;
    let (package, member) = path.split_at(path.len() - 1);
    let found = analysis.packages.find(&package.join("."), &member[0])?;
    Some(build_hover(&found.symbol, qualified_span, analysis.is_current))
}

fn build_hover(symbol: &Symbol, range_span: Span, is_current: bool) -> Hover {
    let kind_label = match symbol.kind {
        SymbolKind::Val => "val",
        SymbolKind::Var => "var",
        SymbolKind::Fun => "function",
        SymbolKind::Param => "parameter",
    };
    let mut value = format!("```ksl\n{}\n```\n\n*{}*", symbol.signature, kind_label);
    if !is_current {
        value.push_str("\n\n_(from the last version of this file that parsed successfully)_");
    }

    Hover {
        contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
        // Highlights the actual token/expression under the cursor, not the
        // (possibly elsewhere, possibly cross-file) declaration.
        range: Some(span_to_range(range_span)),
    }
}
