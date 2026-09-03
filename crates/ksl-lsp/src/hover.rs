use ksl_core::error::Span;
use ksl_core::resolver::{Symbol, SymbolKind};
use ksl_core::stdlib::{BUILTIN_METHOD_SIGNATURES, STDLIB_SIGNATURES};
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::analysis::Analysis;
use crate::diagnostics::span_to_range;

/// Hover info for the symbol at `position` (0-based, LSP convention), or
/// `None` if there's no symbol there. Same-file declarations/uses are
/// checked first; a package-qualified `a.b.c` reference falls back to the
/// cross-file package index (see `analysis::Analysis::packages`); a `.`
/// method call that isn't a package reference (e.g. `s.uppercase()` where
/// `s` is a real local) falls back further to `BUILTIN_METHOD_SIGNATURES`;
/// finally, a bare name that never resolved to a local symbol at all (this
/// resolver is single-file, see its module doc comment) falls back to
/// `STDLIB_SIGNATURES` (`println`, etc.) and then to a package-less
/// `@file:Import`ed file's flat member (`greet`, brought in unqualified —
/// see `package_index::PackageIndex::flat_member`).
pub fn hover(analysis: &Analysis, position: Position) -> Option<Hover> {
    let line = position.line + 1;
    let col = position.character + 1;

    if let Some((occurrence_span, symbol)) = analysis.resolved.table.occurrence_at(line, col) {
        return Some(build_hover(symbol, Some(occurrence_span), analysis.is_current));
    }

    if let Some((qualified_span, path)) = analysis.resolved.table.qualified_ref_at(line, col) {
        let (package, member) = path.split_at(path.len() - 1);
        let found = analysis.packages.find(&package.join("."), &member[0])?;
        return Some(build_hover(&found.symbol, Some(qualified_span), analysis.is_current));
    }

    // No qualified-name candidate at all at this position — either the
    // receiver is a real local (`s.uppercase()`) or not a dotted-name
    // chain to begin with (`"hi".uppercase()`); either way, try it as a
    // builtin method instead of giving up. When the receiver's type is
    // concretely known, it must also match the candidate's receiver
    // type — so a nonsensical `5.uppercase()` (an `Int` receiver)
    // correctly finds nothing here, rather than describing
    // `String.uppercase()` by name alone.
    if let Some(method) = analysis.resolved.table.method_ref_at(line, col) {
        let receiver_ty = analysis.resolved.table.method_receiver_type_at(line, col);
        if let Some((_, _, signature, doc)) = BUILTIN_METHOD_SIGNATURES.iter().find(|(recv_ty, name, ..)| {
            *name == method && receiver_ty.map(|t| t.name() == *recv_ty).unwrap_or(true)
        }) {
            return Some(build_builtin_method_hover(signature, doc));
        }
    }

    // Last resort: a bare name with no local declaration of its own at
    // all — `occurrence_at` above only covers names that resolve to a real
    // local symbol, so this is what's left for a stdlib function or a
    // flat cross-file import.
    let name = analysis.resolved.table.name_ref_at(line, col)?;
    if let Some((_, signature, doc)) = STDLIB_SIGNATURES.iter().find(|(n, ..)| *n == name) {
        return Some(build_stdlib_function_hover(signature, doc));
    }
    let found = analysis.packages.flat_member(name)?;
    Some(build_hover(&found.symbol, None, analysis.is_current))
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

fn build_stdlib_function_hover(signature: &str, doc: &str) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```ksl\n{signature}\n```\n\n*stdlib function*\n\n{doc}"),
        }),
        range: None,
    }
}

fn build_hover(symbol: &Symbol, range_span: Option<Span>, is_current: bool) -> Hover {
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
        // (possibly elsewhere, possibly cross-file) declaration. `None`
        // only for a flat cross-file member, which has no dedicated
        // occurrence span of its own at this call site (see
        // `SymbolTable::name_ref_at`'s doc comment) — same as a builtin
        // method hover, which has never highlighted a specific range either.
        range: range_span.map(span_to_range),
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
    fn hover_on_a_stdlib_function_shows_its_signature() {
        let text = "println(\"hi\")\n";
        let h = hover_at(text, 1, 3).expect("println should hover");
        assert!(markdown_value(&h).contains("println"), "{}", markdown_value(&h));
        assert!(markdown_value(&h).contains("stdlib function"), "{}", markdown_value(&h));
    }

    #[test]
    fn hover_on_a_fun_shows_its_return_type() {
        // Annotated return type.
        let text = "fun greet(name: String): String {\n    return \"Hello, $name!\"\n}\nprintln(greet(\"x\"))\n";
        let h = hover_at(text, 1, 5).expect("greet should hover");
        assert!(markdown_value(&h).contains("fun greet(name: String): String"), "{}", markdown_value(&h));

        // Inferred (unannotated) return type.
        let text2 = "fun add(a: Int, b: Int) = a + b\nprintln(add(1, 2))\n";
        let h2 = hover_at(text2, 1, 5).expect("add should hover");
        assert!(markdown_value(&h2).contains("fun add(a: Int, b: Int): Int"), "{}", markdown_value(&h2));
    }

    #[test]
    fn hover_on_a_flat_cross_file_import_resolves_to_the_other_file() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_hover_flat_test_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("greeter.ksl"), "fun greet(name: String): String {\n    return name\n}\n").unwrap();
        let main_path = dir.join("main.ksl");
        let text = "@file:Import(\"greeter.ksl\")\nprintln(greet(\"World\"))\n";
        fs::write(&main_path, text).unwrap();

        let a = analysis::analyze(text, None, &main_path).expect("should parse");
        // "println(" is 8 chars, so "greet" starts at column 9.
        let h = hover(&a, Position::new(1, 8)).expect("greet should hover");
        assert!(markdown_value(&h).contains("fun greet"), "{}", markdown_value(&h));

        let _ = fs::remove_dir_all(&dir);
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
    fn hover_on_a_hintless_local_shows_its_inferred_type() {
        // `count` has no type annotation — hover must still show the type
        // worked out for it from the initializer, not a bare "var count".
        let text = "var count = 0\nprintln(count)\n";
        let h = hover_at(text, 1, 5).expect("count should hover");
        assert!(markdown_value(&h).contains("var count: Int"), "{}", markdown_value(&h));
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
    fn hover_on_a_builtin_method_call_on_a_non_matching_receiver_type_stays_none() {
        // `n` is a real local of type `Int` — `.length()`/`.uppercase()`
        // etc. are `String`-only, so this must not show a misleading
        // "String.uppercase()" hover just because the method name matches.
        let text = "val n: Int = 5\nprintln(n.uppercase())\n";
        // "println(n." is 10 chars, so "uppercase" starts at column 11.
        let h = hover_at(text, 2, 11);
        assert!(h.is_none(), "{h:?}");
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
