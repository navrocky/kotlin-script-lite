//! What the parser was expecting to see immediately at a completion
//! request's cursor, given only the text before it — the "mid-typing an
//! incomplete construct" half of `ksl-lsp`'s completion mechanism (the
//! other half, facts about an already-complete construct like "is the
//! cursor inside a loop", is answered directly off the resolved tree —
//! see `resolver::SymbolTable::inside_a_loop_at`/`else_allowed_at` —
//! and doesn't need this module at all).
//!
//! The core idea: re-parse the text truncated exactly at the cursor. If it
//! parses cleanly, the cursor sits at a fresh statement/expression
//! boundary — nothing specific is required, any of the usual candidates
//! (keywords, locals, ...) are fair game. If it fails, and the failure
//! genuinely happened at the *end* of that truncated text (not at some
//! earlier, unrelated mistake the user hasn't fixed yet), the resulting
//! `error::ParseError::expected` is exactly what would make this position
//! valid — read off the very same `expect`/`expect_ident_spanned` call
//! that would have to succeed for real typing to continue.

use crate::error::Expected;
use crate::token::TokenKind;

/// See the module doc comment. `text_up_to_cursor` should be the whole
/// document from its own start through the cursor (not just the current
/// line) — the incomplete construct being typed may have started on an
/// earlier line (a multi-line `for (...)` header, say).
pub fn expected_at_cursor(text_up_to_cursor: &str) -> Vec<Expected> {
    let stripped = strip_trailing_ident_run(text_up_to_cursor);
    let Ok(tokens) = crate::lexer::tokenize(stripped) else { return Vec::new() };
    let Some(eof) = tokens.last() else { return Vec::new() };
    if !matches!(eof.kind, TokenKind::Eof) {
        return Vec::new();
    }
    let eof_pos = (eof.span.line, eof.span.col);
    match crate::parser::parse(&tokens) {
        Ok(_) => Vec::new(),
        // Only trust `expected` when the failure is genuinely at the
        // truncated text's own tail — otherwise it's some earlier,
        // unrelated syntax mistake the user hasn't fixed yet, and
        // reporting *that* position's expectation here would be
        // misleading rather than merely unhelpful.
        Err(e) if (e.span.line, e.span.col) == eof_pos => e.expected,
        Err(_) => Vec::new(),
    }
}

/// Strips a trailing run of identifier-continuation characters
/// (`[A-Za-z0-9_]`) — deliberately *not* including `.`, so `"import a."`
/// still ends right at the dot, not swallowing it too. Without this,
/// `"val x: I"` (mid-typing `Int`) would fail at the *next* required token
/// (`=`) instead of reporting `Identifier(TypeName)` again, since the
/// lexer has no notion of "this identifier is still being typed" — a
/// single already-lexed `Ident("I")` looks exactly like a deliberately
/// one-letter name. Stripping it re-creates the position the user was
/// *at* before typing that partial word, which is what's actually being
/// completed.
fn strip_trailing_ident_run(s: &str) -> &str {
    let cut = s
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::IdentRole;

    #[test]
    fn parses_cleanly_at_a_fresh_statement_boundary() {
        assert_eq!(expected_at_cursor("val x = 1\n"), Vec::new());
    }

    #[test]
    fn stays_empty_for_non_type_positions() {
        assert_eq!(expected_at_cursor("val x = "), Vec::new());
        assert_eq!(expected_at_cursor("println("), Vec::new());
        assert_eq!(expected_at_cursor(""), Vec::new());
    }

    #[test]
    fn reports_type_name_role_with_nothing_typed_yet() {
        assert_eq!(expected_at_cursor("val x: "), vec![Expected::Identifier(IdentRole::TypeName)]);
    }

    #[test]
    fn type_name_role_covers_a_function_types_own_parameter_list() {
        // A function type's own `(...)` (bare types only, e.g. `(Int,
        // String) -> Result`) still reports `TypeName` throughout,
        // including right after its own comma and its own `->`, and
        // through arbitrary nesting.
        for src in [
            "val f: (",
            "val f: (I",
            "val f: (Int, ",
            "val f: (Int) -> ",
            "val f: (Int) -> I",
            "val f: ((Int) -> Int) -> ",
            "val f: ((I",
        ] {
            assert_eq!(
                expected_at_cursor(src),
                vec![Expected::Identifier(IdentRole::TypeName)],
                "{src:?}"
            );
        }
    }

    #[test]
    fn type_name_role_does_not_leak_into_a_subsequent_params_own_name() {
        // The bug a naive "any ':' seen so far on this line" check would
        // have: typing `b`'s own *name* right after `a`'s type must not
        // report `TypeName` just because an earlier `:` already appeared.
        assert_eq!(
            expected_at_cursor("fun f(a: Int, b"),
            vec![Expected::Identifier(IdentRole::NewBindingName)]
        );
        // But `b`'s own type position, right after, is correctly `TypeName`.
        assert_eq!(expected_at_cursor("fun f(a: Int, b: I"), vec![Expected::Identifier(IdentRole::TypeName)]);
    }

    #[test]
    fn type_name_role_does_not_leak_into_a_default_values_own_call() {
        assert_eq!(expected_at_cursor("fun f(a: Int = compute("), Vec::new());
    }

    #[test]
    fn reports_type_name_role_mid_typing_a_partial_type_name() {
        // Proves the trailing-identifier-strip fix: without it, this would
        // fail at the *next* required token (`=`) instead.
        assert_eq!(expected_at_cursor("val x: I"), vec![Expected::Identifier(IdentRole::TypeName)]);
    }

    #[test]
    fn reports_new_binding_name_role_after_val() {
        assert_eq!(expected_at_cursor("val "), vec![Expected::Identifier(IdentRole::NewBindingName)]);
    }

    #[test]
    fn reports_in_token_right_after_a_complete_for_loop_variable() {
        assert_eq!(expected_at_cursor("for (i "), vec![Expected::Token(TokenKind::In)]);
    }

    #[test]
    fn reports_new_binding_name_role_with_no_for_loop_variable_typed_yet() {
        // The reported bug: `in` must not be offered before the loop
        // variable's own name exists at all.
        assert_eq!(expected_at_cursor("for (i"), vec![Expected::Identifier(IdentRole::NewBindingName)]);
        assert_eq!(expected_at_cursor("for ("), vec![Expected::Identifier(IdentRole::NewBindingName)]);
    }

    #[test]
    fn reports_qualified_path_segment_role_after_import_dot() {
        assert_eq!(expected_at_cursor("import a."), vec![Expected::Identifier(IdentRole::QualifiedPathSegment)]);
    }

    #[test]
    fn reports_file_import_head_role_right_after_at() {
        assert_eq!(expected_at_cursor("@"), vec![Expected::Identifier(IdentRole::FileImportHead)]);
    }

    #[test]
    fn reports_file_import_annotation_name_role_after_file_colon() {
        assert_eq!(
            expected_at_cursor("@file:"),
            vec![Expected::Identifier(IdentRole::FileImportAnnotationName)]
        );
    }

    #[test]
    fn stays_empty_when_a_genuine_unrelated_error_precedes_the_cursor() {
        // "1 +" alone is a real, unrelated syntax error well before the
        // cursor -- must not be misread as the cursor's own requirement.
        assert_eq!(expected_at_cursor("val x = 1 +\nval y: "), Vec::new());
    }

    #[test]
    fn never_panics_on_truncated_single_line_shapes() {
        // Regression guard: expected_at_cursor now feeds the real parser
        // more truncated, per-keystroke prefixes than parse_only was ever
        // exercised with before -- none of these should panic.
        for src in [
            "fun f(x: ",
            "fun f(x: Int, y: ",
            "val f: (",
            "val f: (Int, ",
            "val f: (Int) -> ",
            "when (x) {",
            "when (x) { 1 -> ",
            "if (",
            "if (true) {",
            "if (true) { 1 } else {",
            "test(a = ",
            "test(a = 1, b",
            "@file:Import(",
            "@file:Import(\"",
            "package ",
            "import ",
            "for (i in ",
            "while (",
        ] {
            let _ = expected_at_cursor(src);
        }
    }
}
