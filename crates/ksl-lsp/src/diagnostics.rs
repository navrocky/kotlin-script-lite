use std::path::Path;

use ksl_core::error::Span;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Always-safe diagnostics: parsing plus static resolver checks (undefined
/// names, duplicate declarations, and type-compatibility problems — see
/// `ksl_core::resolver`), no execution. Safe to run on every `didChange`,
/// including on syntactically incomplete (mid-typing) buffers — if `text`
/// doesn't parse, only the parse error is reported (resolver checks need a
/// tree to run against, and reflect `text` itself, never a last-good
/// fallback: that fallback is only for hover/goto-definition, see
/// `analysis.rs`).
///
/// `own_file` is used to build the cross-file package index (see
/// `package_index::build_package_index`) real `import a.b.c`/`import a.b.*`
/// resolution needs — this does real filesystem I/O (reading every
/// transitively-`@file:Import`ed file), a deliberate trade-off so live
/// diagnostics catch exactly what `ksl-cli` would, not a more lenient
/// approximation of it.
pub fn static_diagnostics(text: &str, own_file: &Path) -> Vec<Diagnostic> {
    match ksl_core::parse_only(text) {
        Ok(program) => {
            let base_dir = own_file.parent().unwrap_or_else(|| Path::new("."));
            let packages = crate::package_index::build_package_index(&program, base_dir, Some(own_file));
            let resolved = ksl_core::resolver::resolve_with_imports(&program, &packages);
            resolved
                .diagnostics
                .into_iter()
                .chain(resolved.type_errors)
                .map(|e| new_diagnostic(span_to_range(e.span), e.message))
                .collect()
        }
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

    /// No test here has a real backing file — `own_file` doesn't need to
    /// exist for `static_diagnostics` to work (a nonexistent path just means
    /// this document's own declarations aren't self-indexed for a package
    /// reference, which none of these scripts rely on).
    fn no_file() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent/test.ksl")
    }

    #[test]
    fn valid_script_has_no_diagnostics() {
        assert!(static_diagnostics("println(\"hi\")\n", &no_file()).is_empty());
    }

    #[test]
    fn syntax_error_produces_one_diagnostic() {
        let diags = static_diagnostics("val x = \n", &no_file());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source.as_deref(), Some("ksl"));
    }

    #[test]
    fn incomplete_buffer_mid_typing_still_produces_a_diagnostic_not_a_panic() {
        // Editors call this on every keystroke; a mid-typed buffer must
        // report an error, not panic.
        let diags = static_diagnostics("fun f(x: ", &no_file());
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn undefined_name_is_flagged_live_without_a_save() {
        // The specific gap this closes: calling an undefined function used
        // to only surface as a diagnostic on didSave (running the script);
        // now the resolver catches it on didOpen/didChange too.
        let diags = static_diagnostics("printlnn(\"hi\")\n", &no_file());
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("printlnn"));
    }

    #[test]
    fn stdlib_calls_are_not_flagged_as_undefined() {
        assert!(static_diagnostics("println(uppercase(\"hi\"))\n", &no_file()).is_empty());
    }

    #[test]
    fn break_outside_a_loop_is_flagged_live() {
        let diags = static_diagnostics("break\n", &no_file());
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("'break'"), "{}", diags[0].message);
    }

    #[test]
    fn type_mismatch_is_flagged_live() {
        let diags = static_diagnostics("val x: Int = \"hi\"\n", &no_file());
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.contains("type mismatch"), "{}", diags[0].message);
    }

    #[test]
    fn span_to_range_converts_1_based_to_0_based_positions() {
        let range = span_to_range(Span::new(3, 5));
        assert_eq!(range.start, Position::new(2, 4));
        assert_eq!(range.end, Position::new(2, 5));
    }
}
