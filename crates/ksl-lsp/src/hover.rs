use ksl_core::error::Span;
use ksl_core::resolver::{Symbol, SymbolKind};
use ksl_core::stdlib::BUILTIN_METHOD_SIGNATURES;
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Hover info for the symbol at `position` (0-based, LSP convention), or
/// `None` if there's no symbol there. Same-file declarations/uses are
/// checked first; a package-qualified `a.b.c` reference falls back to the
/// cross-file package index (see `analysis::Analysis::packages`); a `.`
/// method call that isn't a package reference (e.g. `s.uppercase()` where
/// `s` is a real local) falls back further to `BUILTIN_METHOD_SIGNATURES`.
pub fn hover(analysis: &Analysis, position: Position) -> Option<Hover> {
    let line = position.line + 1;
    let col = position.character + 1;

    if let Some((occurrence_span, symbol)) = analysis.resolved.table.occurrence_at(line, col) {
        return Some(build_hover(symbol, occurrence_span, analysis.is_current));
    }

    match analysis.resolved.table.qualified_ref_at(line, col) {
        Some((qualified_span, path)) => {
            let (package, member) = path.split_at(path.len() - 1);
            let found = analysis.packages.find(&package.join("."), &member[0])?;
            Some(build_hover(&found.symbol, qualified_span, analysis.is_current))
        }
        // No qualified-name candidate at all at this position — either the
        // receiver is a real local (`s.uppercase()`) or not a dotted-name
        // chain to begin with (`"hi".uppercase()`); either way, try it as a
        // builtin method instead of giving up.
        None => {
            let method = analysis.resolved.table.method_ref_at(line, col)?;
            let (_, signature, doc) = BUILTIN_METHOD_SIGNATURES.iter().find(|(name, ..)| *name == method)?;
            Some(build_builtin_method_hover(signature, doc))
        }
    }
}

fn build_builtin_method_hover(signature: &str, doc: &str) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```ksl\n{signature}\n```\n\n*builtin method*\n\n{doc}"),
        }),
        range: None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;
    use std::path::Path;

    fn hover_at(text: &str, line: u32, col: u32) -> Option<Hover> {
        let analysis =
            analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).expect("should parse");
        hover(&analysis, Position::new(line - 1, col - 1))
    }

    fn markdown_value(h: &Hover) -> &str {
        match &h.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("expected markup contents"),
        }
    }

    #[test]
    fn hover_on_a_local_still_resolves_its_declaration() {
        // `s.uppercase()`'s receiver `s` is a real local — hovering `s`
        // itself must still find its declaration, not fall through to the
        // builtin-method branch meant for `uppercase`.
        let text = "val s = \"hi\"\nprintln(s.uppercase())\n";
        let h = hover_at(text, 2, 9).expect("s should hover");
        assert!(markdown_value(&h).contains("val s"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_builtin_method_call_on_a_local_receiver() {
        let text = "val s = \"hi\"\nprintln(s.uppercase())\n";
        // "println(s." is 10 chars, so "uppercase" starts at column 11.
        let h = hover_at(text, 2, 11).expect("uppercase should hover");
        assert!(markdown_value(&h).contains("uppercase"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("builtin method"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_builtin_method_call_on_a_string_literal() {
        let text = "println(\"hi\".length())\n";
        // "println(\"hi\"." is 13 chars, so "length" starts at column 14.
        let h = hover_at(text, 1, 14).expect("length should hover");
        assert!(markdown_value(&h).contains("length"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_an_unknown_package_qualified_call_stays_none() {
        // `a.b` never resolves to a real package here, and `greet` isn't a
        // builtin method name — must not show a misleading builtin-method
        // hover for it.
        let h = hover_at("println(a.b.greet(\"x\"))\n", 1, 14);
        assert!(h.is_none());
    }
}
