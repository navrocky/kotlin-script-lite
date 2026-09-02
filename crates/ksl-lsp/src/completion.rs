use ksl_core::resolver::SymbolKind;
use ksl_core::stdlib::STDLIB_SIGNATURES;
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
/// If the cursor sits right after `a.b.` (a known package), only that
/// package's members are offered — matching what's actually valid there,
/// since a bare local/stdlib/keyword name can't follow a `.`. Otherwise:
/// locals and outer-scope symbols visible there, every stdlib function,
/// every known package's full name, and language keywords — merged in
/// that order (most relevant first).
pub fn completion(analysis: &Analysis, text: &str, position: Position) -> Vec<CompletionItem> {
    let line_text = text.lines().nth(position.line as usize).unwrap_or("");
    let up_to_cursor: String = line_text.chars().take(position.character as usize).collect();
    let (prefix_path, _partial) = dotted_prefix_before_cursor(&up_to_cursor);

    if !prefix_path.is_empty() {
        let package_name = prefix_path.join(".");
        return analysis
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
    let mut parts: Vec<String> = chunk.split('.').map(String::from).collect();
    let partial = parts.pop().unwrap_or_default();
    (parts, partial)
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
