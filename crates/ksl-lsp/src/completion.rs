use ksl_core::resolver::SymbolKind;
use ksl_core::stdlib::{BUILTIN_METHOD_SIGNATURES, STDLIB_SIGNATURES};
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Documentation, Position};

use crate::analysis::Analysis;

/// From `TokenKind`'s keyword variants (`token.rs`) plus the two boolean
/// literals — not itself a `TokenKind`, but a reserved word from the
/// user's point of view.
const KEYWORDS: &[&str] = &[
    "fun", "val", "var", "if", "else", "when", "while", "for", "in", "return", "break",
    "continue", "true", "false",
];

/// Completion candidates at `position` (0-based, LSP convention) within
/// `text` (needed, alongside `analysis`, to detect a package-qualified
/// prefix already typed before the cursor — see `dotted_prefix_before_cursor`).
///
/// If the cursor sits right after `a.b.`: when `a.b` is a single bare
/// identifier that resolves to a local with a statically known type (e.g.
/// `s` in `s.`, where `s: String`), only that type's builtin methods are
/// offered — the concrete fix for the bug this filtering exists for
/// (offering `.length()`/`.uppercase()` after a receiver that obviously
/// isn't a `String`, e.g. a number). Otherwise (the type is `Unknown`, the
/// name doesn't resolve, or the prefix has multiple segments and so looks
/// package-shaped) there's no way to statically tell a local receiver from
/// a package apart (the same ambiguity `ExprKind::FieldAccess`/`MethodCall`
/// defer to evaluation time, see `docs/syntax.md`'s "Packages" section), so
/// both possibilities are offered together: `a.b`'s members if it's a known
/// package, and every builtin method in case `a.b` is actually a real
/// receiver value — this over-suggests until classes give real per-type
/// member info, but never under-suggests. Otherwise (no `.` prefix at all):
/// locals and outer-scope symbols visible there, every stdlib function,
/// every known package's full name, and language keywords — merged in that
/// order (most relevant first).
pub fn completion(analysis: &Analysis, text: &str, position: Position) -> Vec<CompletionItem> {
    let line_text = text.lines().nth(position.line as usize).unwrap_or("");
    let up_to_cursor: String = line_text.chars().take(position.character as usize).collect();

    if type_position_before_cursor(&up_to_cursor) {
        return ksl_core::types::PRIMITIVE_TYPE_NAMES
            .iter()
            .map(|name| CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::CLASS),
                ..Default::default()
            })
            .collect();
    }

    let (prefix_path, _partial) = dotted_prefix_before_cursor(&up_to_cursor);

    if !prefix_path.is_empty() {
        // A single-segment prefix resolving to a local with a known
        // concrete type narrows the builtin-method suggestions to that
        // type; anything else (multi-segment, unresolved, or `Unknown`)
        // falls back to the over-suggesting "show everything" behavior.
        // A numeric-literal prefix (`1`, or the two-segment `1.5` shape)
        // is recognized directly — never a real identifier, so a symbol
        // lookup for it would always fail — instead of falling through to
        // the unfiltered case.
        let receiver_type = match prefix_path.as_slice() {
            [int_part] if is_digits(int_part) => Some(ksl_core::types::Type::Int),
            [int_part, frac_part] if is_digits(int_part) && is_digits(frac_part) => {
                Some(ksl_core::types::Type::Double)
            }
            [name] => {
                let line = position.line + 1;
                let col = position.character + 1;
                analysis
                    .resolved
                    .table
                    .symbols_in_scope(line, col)
                    .into_iter()
                    .find(|s| s.name == *name)
                    .map(|s| s.ty.clone())
                    .filter(|ty| *ty != ksl_core::types::Type::Unknown)
            }
            _ => None,
        };

        let package_name = prefix_path.join(".");
        let mut items: Vec<CompletionItem> = analysis
            .packages
            .members_of(&package_name)
            .iter()
            .map(|m| CompletionItem {
                label: m.symbol.name.clone(),
                kind: Some(symbol_completion_kind(m.symbol.kind)),
                detail: Some(m.symbol.signature.clone()),
                ..Default::default()
            })
            .collect();
        items.extend(
            BUILTIN_METHOD_SIGNATURES
                .iter()
                .filter(|(recv_ty, ..)| match &receiver_type {
                    Some(ty) => ty.name() == *recv_ty,
                    None => true,
                })
                .map(|(_, name, signature, doc)| CompletionItem {
                    label: name.to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some(signature.to_string()),
                    documentation: Some(Documentation::String(doc.to_string())),
                    ..Default::default()
                }),
        );
        return items;
    }

    let line = position.line + 1;
    let col = position.character + 1;
    let mut items = Vec::new();

    for symbol in analysis.resolved.table.symbols_in_scope(line, col) {
        items.push(CompletionItem {
            label: symbol.name.clone(),
            kind: Some(symbol_completion_kind(symbol.kind)),
            detail: Some(symbol.signature.clone()),
            ..Default::default()
        });
    }

    for (name, signature, doc) in STDLIB_SIGNATURES {
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(signature.to_string()),
            documentation: Some(Documentation::String(doc.to_string())),
            ..Default::default()
        });
    }

    for package_name in analysis.packages.package_names() {
        items.push(CompletionItem {
            label: package_name.clone(),
            kind: Some(CompletionItemKind::MODULE),
            ..Default::default()
        });
    }

    for kw in KEYWORDS {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    items
}

/// Splits the text immediately before the cursor into a dotted-path prefix
/// (if any) and the partial identifier currently being typed — e.g.
/// `"util.math.sq"` -> (`["util", "math"]`, `"sq"`); `"util.math."` ->
/// (`["util", "math"]`, `""`); `"sq"` -> (`[]`, `"sq"`).
///
/// A qualified-name expression like `a.b.c` doesn't exist as an AST node
/// while it's mid-typed (the parser requires a complete identifier after
/// each `.`, so `a.b.` alone fails to parse at all) — so, unlike every
/// other completion source here, this can't be read off `analysis` and has
/// to look at the raw text instead. Operates on `char`s, matching how the
/// rest of this codebase already treats `Position::character` as a plain
/// character count rather than a true UTF-16 code unit count (see
/// `diagnostics::span_to_range`) — a pre-existing simplification, not a
/// new one introduced here.
fn dotted_prefix_before_cursor(text_before_cursor: &str) -> (Vec<String>, String) {
    let mut start = text_before_cursor.len();
    for (i, c) in text_before_cursor.char_indices().rev() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            start = i;
        } else {
            break;
        }
    }
    let chunk = &text_before_cursor[start..];
    // A chunk starting with a digit is a numeric literal (`1`, `1.5`, ...),
    // never a dotted *name* chain — identifiers can't start with a digit
    // (see `lexer.rs`'s `lex_ident_or_keyword` vs `lex_number`). It's
    // still returned as an ordinary prefix, though (not rejected/emptied
    // out): `completion()` recognizes an all-digit segment as a numeric
    // literal and assigns it a real `Int`/`Double` type directly (no
    // symbol to look up), so the existing type-aware filtering still
    // applies — an earlier version of this function instead returned an
    // empty prefix here, which fell through to the *general* completion
    // branch (locals/stdlib/keywords) and still offered `length`/etc. as
    // plain stdlib functions right after a number, just not flagged as
    // `.`-methods — the visible bug this comment now guards against.
    let mut parts: Vec<String> = chunk.split('.').map(String::from).collect();
    let partial = parts.pop().unwrap_or_default();
    (parts, partial)
}

/// Whether the cursor sits in type-annotation position: right after a `:`
/// (optionally with whitespace and a partially-typed name already in
/// between, e.g. `val x: I` mid-typing `Int`). Every `:` in the grammar
/// introduces a type — `val`/`var`/parameter annotations and `fun` return
/// types, `parser.rs`'s only three `Colon` call sites — so this plain
/// suffix check is unambiguous, unlike method-receiver detection.
fn type_position_before_cursor(text_before_cursor: &str) -> bool {
    let mut start = text_before_cursor.len();
    for (i, c) in text_before_cursor.char_indices().rev() {
        if c.is_alphanumeric() || c == '_' {
            start = i;
        } else {
            break;
        }
    }
    text_before_cursor[..start].trim_end().ends_with(':')
}

/// `true` for a non-empty run of ASCII digits — used to recognize a
/// dotted-prefix segment that's actually (part of) a numeric literal
/// rather than an identifier, so its type can be assigned directly
/// (`Int`, or `Double` for the two-segment `N.M` shape) instead of
/// attempting — and always failing — a symbol lookup for something that
/// was never a legal identifier to begin with.
fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn symbol_completion_kind(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::Val | SymbolKind::Var | SymbolKind::Param => CompletionItemKind::VARIABLE,
        SymbolKind::Fun => CompletionItemKind::FUNCTION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;
    use std::path::Path;

    fn labels(text: &str, position: Position) -> Vec<String> {
        let analysis = analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).expect("should parse");
        completion(&analysis, text, position).into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn includes_locals_stdlib_and_keywords() {
        let text = "val outer = 1\nfun f(p: Int): Int {\n    val inner = 2\n    return inner\n}\n";
        // position inside f's body, on the `return inner` line (0-based line 3).
        let names = labels(text, Position::new(3, 4));
        assert!(names.contains(&"inner".to_string()));
        assert!(names.contains(&"p".to_string()));
        assert!(names.contains(&"f".to_string()));
        assert!(names.contains(&"outer".to_string()));
        assert!(names.contains(&"println".to_string()));
        assert!(names.contains(&"fun".to_string()));
    }

    #[test]
    fn falls_back_to_last_good_tree_when_mid_typing() {
        let good = "fun f(p: Int): Int {\n    return p\n}\n";
        let path = Path::new("/tmp/nonexistent_test.ksl");
        let analysis = analysis::analyze(good, None, path).unwrap();
        assert!(!analysis.resolved.table.symbols_in_scope(2, 5).is_empty());

        let broken = format!("{good}val z = \n");
        let last_good = std::sync::Arc::new(ksl_core::parse_only(good).unwrap());
        let fallback = analysis::analyze(&broken, Some(last_good), path).expect("should fall back");
        assert!(!fallback.is_current);
        let names: Vec<String> =
            completion(&fallback, &broken, Position::new(1, 4)).into_iter().map(|i| i.label).collect();
        assert!(names.contains(&"p".to_string()));
    }

    #[test]
    fn completion_after_a_dot_offers_builtin_methods() {
        // `s` is a local `String` — its builtin methods are offered after
        // `.`. Uses a fully-typed line (`dotted_prefix_before_cursor` only
        // reads up to `position`, so the cursor placed right after "s."
        // still exercises the same prefix detection `s.` alone would,
        // without needing an incomplete-parse fallback tree).
        let text = "val s = \"hi\"\ns.uppercase()\n";
        let names = labels(text, Position::new(1, 2));
        assert!(names.contains(&"uppercase".to_string()), "{names:?}");
        assert!(names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_a_non_string_local_does_not_offer_string_methods() {
        // Regression test for the bug this filtering exists to fix: `n` is
        // a local of a statically known, non-`String` type — `.length()`/
        // `.uppercase()` etc. must not be offered for it.
        let text = "val n: Int = 5\nn.uppercase()\n";
        let names = labels(text, Position::new(1, 2));
        assert!(!names.contains(&"uppercase".to_string()), "{names:?}");
        assert!(!names.contains(&"length".to_string()), "{names:?}");
    }

    #[test]
    fn completion_after_a_dot_on_an_int_literal_offers_nothing_string_shaped() {
        // The exact bug report: `1.length()` — `1` is an `Int` literal
        // (recognized directly, no symbol lookup involved), so no
        // `String` method, and nothing from the general branch either.
        let text = "1.length()\n";
        let names = labels(text, Position::new(0, 2)); // right after "1."
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn completion_after_a_numeric_literal_dot_does_not_offer_string_methods() {
        // Regression test for the real bug: right after `1.` (an `Int`
        // literal), nothing offered may be a `String` method — and,
        // critically, general completion items (locals, stdlib functions
        // as plain names, keywords) must not leak in either. An earlier
        // version of this fix only blocked the `.`-method-table branch,
        // which made the function fall through to the general branch —
        // `length` etc. still appeared in the list (just as a plain
        // stdlib function instead of a flagged `.`-method), which reads
        // identically to "1.length() is offered" in an editor.
        let text = "println(1.5)\n";
        let analysis = analysis::analyze(text, None, Path::new("/tmp/nonexistent_test.ksl")).expect("should parse");
        let items = completion(&analysis, text, Position::new(0, 10)); // right after "println(1."
        let labels: Vec<&String> = items.iter().map(|i| &i.label).collect();
        assert!(!labels.iter().any(|l| *l == "length"), "{labels:?}");
        assert!(!labels.iter().any(|l| *l == "uppercase"), "{labels:?}");
        assert!(!labels.iter().any(|l| *l == "fun"), "{labels:?}");
    }

    #[test]
    fn dotted_prefix_before_cursor_returns_a_numeric_prefix_as_is() {
        // Not rejected/emptied out (see the function's doc comment) — a
        // numeric segment is returned like any other, so `completion()`
        // can recognize it and assign a real type directly.
        assert_eq!(dotted_prefix_before_cursor("1."), (vec!["1".to_string()], String::new()));
        assert_eq!(dotted_prefix_before_cursor("1.5"), (vec!["1".to_string()], "5".to_string()));
        assert_eq!(dotted_prefix_before_cursor("println(1."), (vec!["1".to_string()], String::new()));
    }

    #[test]
    fn completion_in_type_annotation_position_offers_primitive_type_names() {
        // The cursor sits mid-line, right after "val x: " — the rest of
        // the (fully valid) line is irrelevant to `completion()`, which
        // only ever looks at text up to `position`.
        let text = "val x: Int = 0\n";
        let names = labels(text, Position::new(0, 7));
        assert!(names.contains(&"Int".to_string()), "{names:?}");
        assert!(names.contains(&"String".to_string()), "{names:?}");
        assert!(!names.contains(&"println".to_string()), "{names:?}");
        assert!(!names.contains(&"fun".to_string()), "{names:?}");
    }

    #[test]
    fn completion_mid_typing_a_type_name_still_offers_primitive_type_names() {
        let text = "val x: Int = 0\n";
        let names = labels(text, Position::new(0, 8)); // right after "val x: I"
        assert!(names.contains(&"Int".to_string()), "{names:?}");
    }

    #[test]
    fn completion_in_a_function_parameter_type_position_offers_primitive_type_names() {
        let text = "fun f(p: Int): Int {\n    return p\n}\n";
        let names = labels(text, Position::new(0, 9)); // right after "fun f(p: "
        assert!(names.contains(&"Int".to_string()), "{names:?}");
    }

    #[test]
    fn type_position_before_cursor_rejects_non_type_positions() {
        assert!(!type_position_before_cursor("val x = "));
        assert!(!type_position_before_cursor("println("));
        assert!(!type_position_before_cursor(""));
    }

    #[test]
    fn dotted_prefix_before_cursor_splits_package_and_partial() {
        assert_eq!(
            dotted_prefix_before_cursor("util.math.sq"),
            (vec!["util".to_string(), "math".to_string()], "sq".to_string())
        );
        assert_eq!(
            dotted_prefix_before_cursor("util.math."),
            (vec!["util".to_string(), "math".to_string()], String::new())
        );
        assert_eq!(dotted_prefix_before_cursor("sq"), (vec![], "sq".to_string()));
        assert_eq!(dotted_prefix_before_cursor(""), (vec![], String::new()));
        // Only the trailing identifier-chain matters, not the whole line.
        assert_eq!(
            dotted_prefix_before_cursor("println(util.math.sq"),
            (vec!["util".to_string(), "math".to_string()], "sq".to_string())
        );
    }
}
