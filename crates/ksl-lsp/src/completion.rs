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

/// Completion candidates at `position` (0-based, LSP convention): locals
/// and outer-scope symbols visible there, every stdlib function, and
/// language keywords — merged in that order (most relevant first).
pub fn completion(analysis: &Analysis, position: Position) -> Vec<CompletionItem> {
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

    for kw in KEYWORDS {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    items
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

    fn labels(text: &str, position: Position) -> Vec<String> {
        let analysis = analysis::analyze(text, None).expect("should parse");
        completion(&analysis, position).into_iter().map(|i| i.label).collect()
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
        let analysis = analysis::analyze(good, None).unwrap();
        assert!(!analysis.resolved.table.symbols_in_scope(2, 5).is_empty());

        let broken = format!("{good}val z = \n");
        let last_good = std::sync::Arc::new(ksl_core::parse_only(good).unwrap());
        let fallback = analysis::analyze(&broken, Some(last_good)).expect("should fall back");
        assert!(!fallback.is_current);
        let names: Vec<String> =
            completion(&fallback, Position::new(1, 4)).into_iter().map(|i| i.label).collect();
        assert!(names.contains(&"p".to_string()));
    }
}
