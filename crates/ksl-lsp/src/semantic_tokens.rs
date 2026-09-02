use ksl_core::token::{Token, TokenKind};
use tower_lsp::lsp_types::{SemanticToken, SemanticTokenType, SemanticTokens};

/// The legend registered in `initialize`'s server capabilities — indices
/// into this slice are what `token_type_index` returns.
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,
    SemanticTokenType::NUMBER,
    SemanticTokenType::STRING,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::VARIABLE,
];

const KEYWORD: u32 = 0;
const NUMBER: u32 = 1;
const STRING: u32 = 2;
const OPERATOR: u32 = 3;
const VARIABLE: u32 = 4;

/// Classifies a token purely by its `TokenKind` — no scope/symbol
/// resolution yet (that's a later milestone, and would let bare
/// identifiers, e.g. inside a `${...}` interpolation or outside a string
/// entirely, be classified as variable/function/parameter). Identifiers
/// and punctuation like `( ) { } [ ] , : .` are left unclassified here and
/// fall back to the TextMate grammar's (or the editor's default) styling.
///
/// `StringInterpText` (the `$name` shorthand) is the one exception: unlike
/// a bare `Ident`, its *only* possible meaning is "a variable reference",
/// so it's classified as `VARIABLE` — no resolver needed to know that much.
fn token_type_index(kind: &TokenKind) -> Option<u32> {
    use TokenKind::*;
    let idx = match kind {
        Fun | Val | Var | If | Else | When | While | For | In | Return | Break | Continue
        | Bool(_) => KEYWORD,

        Int(_) | Double(_) => NUMBER,

        StringStart | StringText(_) | StringEnd => STRING,

        StringInterpText(_) => VARIABLE,

        Assign | Eq | NotEq | Lt | Gt | LtEq | GtEq | AndAnd | OrOr | Not | Plus | Minus | Star
        | Slash | Percent | PlusAssign | MinusAssign | StarAssign | SlashAssign
        | PercentAssign | Arrow | Range | At | TemplateExprStart | TemplateExprEnd => OPERATOR,

        Ident(_) | LParen | RParen | LBrace | RBrace | LBracket | RBracket | Comma | Colon
        | Dot | Semicolon | Newline | Eof => return None,
    };
    Some(idx)
}

/// Builds the LSP-encoded (delta-line/delta-start) semantic token list for
/// `text`. Returns `None` only if the text doesn't even tokenize (e.g. an
/// unterminated string) — a syntactically incomplete-but-tokenizable
/// buffer still gets tokens for whatever lexed successfully.
pub fn semantic_tokens(text: &str) -> Option<SemanticTokens> {
    let tokens = ksl_core::lexer::tokenize(text).ok()?;
    Some(encode(&tokens))
}

fn encode(tokens: &[Token]) -> SemanticTokens {
    let mut data = Vec::new();
    // LSP encodes each token relative to the previous one; using (0, 0) as
    // the implicit "previous token" position makes the first real token's
    // delta come out correctly absolute, with no special-casing needed.
    let mut prev_line: u32 = 0;
    let mut prev_start: u32 = 0;

    for tok in tokens {
        let Some(token_type) = token_type_index(&tok.kind) else { continue };
        let span = tok.span;
        if span.end_line != span.line {
            // No token kind spans multiple source lines today (strings
            // disallow raw newlines; comments aren't tokens) — skip
            // defensively rather than emit a malformed entry.
            continue;
        }
        let line = span.line.saturating_sub(1);
        let start = span.col.saturating_sub(1);
        let length = span.end_col.saturating_sub(span.col);
        if length == 0 {
            continue;
        }

        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { start - prev_start } else { start };

        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        });

        prev_line = line;
        prev_start = start;
    }

    SemanticTokens { result_id: None, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(src: &str) -> Vec<SemanticToken> {
        semantic_tokens(src).unwrap().data
    }

    #[test]
    fn classifies_keyword_number_string_operator() {
        let toks = tokens("val x = 1 + 2");
        // val(keyword) x(skip,ident) =(operator) 1(number) +(operator) 2(number)
        assert_eq!(
            toks,
            vec![
                SemanticToken { delta_line: 0, delta_start: 0, length: 3, token_type: KEYWORD, token_modifiers_bitset: 0 },
                SemanticToken { delta_line: 0, delta_start: 6, length: 1, token_type: OPERATOR, token_modifiers_bitset: 0 },
                SemanticToken { delta_line: 0, delta_start: 2, length: 1, token_type: NUMBER, token_modifiers_bitset: 0 },
                SemanticToken { delta_line: 0, delta_start: 2, length: 1, token_type: OPERATOR, token_modifiers_bitset: 0 },
                SemanticToken { delta_line: 0, delta_start: 2, length: 1, token_type: NUMBER, token_modifiers_bitset: 0 },
            ]
        );
    }

    #[test]
    fn string_pieces_are_classified_as_string() {
        let toks = tokens("\"hi ${x}\"");
        // StringStart, StringText("hi "), TemplateExprStart(operator),
        // Ident (skipped), TemplateExprEnd(operator), StringEnd.
        let kinds: Vec<u32> = toks.iter().map(|t| t.token_type).collect();
        assert_eq!(kinds, vec![STRING, STRING, OPERATOR, OPERATOR, STRING]);
    }

    #[test]
    fn deltas_reset_on_a_new_line() {
        let toks = tokens("val x = 1\nval y = 2");
        let vals: Vec<&SemanticToken> = toks.iter().filter(|t| t.token_type == KEYWORD).collect();
        assert_eq!(vals.len(), 2);
        assert_eq!(vals[0].delta_line, 0);
        // second "val" starts a new line: delta_line == 1, delta_start absolute (0)
        assert_eq!(vals[1].delta_line, 1);
        assert_eq!(vals[1].delta_start, 0);
    }

    #[test]
    fn unterminated_string_yields_no_tokens_not_a_panic() {
        assert!(semantic_tokens("\"unterminated").is_none());
    }

    #[test]
    fn dollar_shorthand_interpolation_is_classified_as_variable() {
        let toks = tokens("\"hi $name!\"");
        // StringStart, StringText("hi "), StringInterpText("name")(variable), StringText("!"), StringEnd
        let kinds: Vec<u32> = toks.iter().map(|t| t.token_type).collect();
        assert_eq!(kinds, vec![STRING, STRING, VARIABLE, STRING, STRING]);
    }
}
