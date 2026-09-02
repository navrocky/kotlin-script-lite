use ksl_core::error::Span;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Cheap, always-safe diagnostics: parse errors only, no execution, no file
/// I/O. Safe to run on every `didChange`, including on syntactically
/// incomplete (mid-typing) buffers.
pub fn parse_diagnostics(text: &str) -> Vec<Diagnostic> {
    match ksl_core::parse_only(text) {
        Ok(_) => Vec::new(),
        Err(e) => vec![new_diagnostic(span_to_range(e.span), e.message)],
    }
}

pub fn new_diagnostic(range: Range, message: impl Into<String>) -> Diagnostic {
    let mut diag = Diagnostic::new_simple(range, message.into());
    diag.severity = Some(DiagnosticSeverity::ERROR);
    diag.source = Some("ksl".to_string());
    diag
}

/// `Span` is 1-based (`line`/`col`); LSP `Position` is 0-based.
pub fn span_to_range(span: Span) -> Range {
    Range::new(
        Position::new(span.line.saturating_sub(1), span.col.saturating_sub(1)),
        Position::new(span.end_line.saturating_sub(1), span.end_col.saturating_sub(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_script_has_no_diagnostics() {
        assert!(parse_diagnostics("println(\"hi\")\n").is_empty());
    }

    #[test]
    fn syntax_error_produces_one_diagnostic() {
        let diags = parse_diagnostics("val x = \n");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source.as_deref(), Some("ksl"));
    }

    #[test]
    fn incomplete_buffer_mid_typing_still_produces_a_diagnostic_not_a_panic() {
        // Editors call this on every keystroke; a mid-typed buffer must
        // report an error, not panic.
        let diags = parse_diagnostics("fun f(x: ");
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn span_to_range_converts_1_based_to_0_based_positions() {
        let range = span_to_range(Span::new(3, 5));
        assert_eq!(range.start, Position::new(2, 4));
        assert_eq!(range.end, Position::new(2, 5));
    }
}
