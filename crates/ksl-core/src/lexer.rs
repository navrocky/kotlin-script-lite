use std::collections::HashMap;

use crate::error::{ParseError, Span};
use crate::token::{Token, TokenKind};

/// Lexer context. `Normal` is top-level code. `TemplateText` is the literal
/// text portion of a `"..."` string. `TemplateExpr` is the code inside a
/// `${ ... }` interpolation — `brace_depth` tracks nested `{`/`}` (e.g. from
/// a lambda literal used inside the interpolation) so the lexer can tell a
/// nested closing brace apart from the one that ends the interpolation.
enum LexMode {
    Normal,
    TemplateText,
    TemplateExpr { brace_depth: u32 },
}

pub struct Lexer<'a> {
    chars: Vec<char>,
    pos: usize,
    line: u32,
    col: u32,
    mode_stack: Vec<LexMode>,
    /// Stack of every currently-open `(`/`[`/`{` (the literal char),
    /// pushed/popped in `lex_operator_or_punct` alongside their matching
    /// closer — independent of `mode_stack`/`LexMode::TemplateExpr`'s own
    /// `brace_depth` (which exists only to find a `${...}` interpolation's
    /// own closing `}`). Consulted by the `Some('\n')` arm of
    /// `lex_normal_token`: a newline while the top is `(` or `[` is always
    /// insignificant (pure grouping — a call's argument list, a condition's
    /// parens, ...), never a statement separator, regardless of the
    /// previous token; a newline while the top is `{` (or the stack is
    /// empty) falls back to the existing `can_end_statement`-based rule,
    /// since `{` always starts a fresh statement list (a block/lambda body)
    /// where newlines matter again, even when that block sits inside an
    /// enclosing `(...)` (e.g. a multi-statement lambda passed as a call
    /// argument).
    bracket_stack: Vec<char>,
    tokens: Vec<Token>,
    /// Raw text of every `/** ... */` doc comment found in `LexMode::Normal`
    /// (never inside a `${ ... }` interpolation), keyed by the source line
    /// immediately after its closing `*/` — i.e. "a doc comment attaches to
    /// this exact next line" (no blank line allowed in between, matching
    /// real KDoc/IntelliJ convention). Comments are otherwise fully
    /// discarded, exactly as before (see `lex_normal_token`'s block-comment
    /// branch) — this is a side channel, not a token in `tokens`, so it
    /// never affects the main token stream `tokenize()` produces.
    doc_comments: HashMap<u32, String>,
    _source: &'a str,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
            mode_stack: vec![LexMode::Normal],
            bracket_stack: Vec::new(),
            tokens: Vec::new(),
            doc_comments: HashMap::new(),
            _source: source,
        }
    }

    /// Tokenizes `source`, discarding any doc comments found — the token
    /// stream this produces is identical whether or not the source has any
    /// `/** ... */` comments (see `run`/`tokenize_with_docs`).
    pub fn tokenize(mut self) -> Result<Vec<Token>, ParseError> {
        self.run()?;
        Ok(self.tokens)
    }

    /// Like `tokenize`, but also returns the doc-comment index (see
    /// `doc_comments`'s doc comment) built along the way.
    pub fn tokenize_with_docs(mut self) -> Result<(Vec<Token>, HashMap<u32, String>), ParseError> {
        self.run()?;
        Ok((self.tokens, self.doc_comments))
    }

    fn run(&mut self) -> Result<(), ParseError> {
        self.skip_shebang();
        loop {
            match self.mode_stack.last().expect("mode stack never empty") {
                LexMode::TemplateText => self.lex_template_text()?,
                LexMode::Normal | LexMode::TemplateExpr { .. } => {
                    if self.lex_normal_token()? {
                        break;
                    }
                }
            }
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: self.span(),
        });
        Ok(())
    }

    fn span(&self) -> Span {
        Span::new(self.line, self.col)
    }

    /// If the script starts with `#!` (a Unix shebang line, e.g.
    /// `#!/usr/bin/env ksl`), consume it up to and including the newline so
    /// the rest of the lexer never sees it. Only recognized at the very
    /// start of the file, matching shell/shebang conventions.
    fn skip_shebang(&mut self) {
        if self.chars.first() == Some(&'#') && self.chars.get(1) == Some(&'!') {
            while let Some(c) = self.peek() {
                if c == '\n' {
                    break;
                }
                self.advance();
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    /// Pushes a token spanning from `start` to the lexer's current
    /// position. Every call site captures `start` before consuming the
    /// token's characters and calls `push` right after consuming the last
    /// of them, so "current position" at push-time is exactly the token's
    /// real end (one past its last character) — this holds for every call
    /// site in this file except `StringText`, which tracks its own start
    /// explicitly for the same reason (see `lex_template_text`).
    fn push(&mut self, kind: TokenKind, start: Span) {
        let span = Span::with_end(start.line, start.col, self.line, self.col);
        self.tokens.push(Token { kind, span });
    }

    fn prev_can_end_statement(&self) -> bool {
        self.tokens.last().map(|t| t.kind.can_end_statement()).unwrap_or(false)
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            span: self.span(),
        }
    }

    /// Consumes and emits exactly one token in "normal" (non-template-text)
    /// lexing context. Returns `Ok(true)` once EOF has been reached.
    fn lex_normal_token(&mut self) -> Result<bool, ParseError> {
        loop {
            match self.peek() {
                None => return Ok(true),
                Some(' ') | Some('\t') | Some('\r') => {
                    self.advance();
                }
                Some('\n') => {
                    // Inside an open `(`/`[` (pure grouping — call args, a
                    // condition's parens, ...), a newline is never
                    // significant, regardless of the previous token — see
                    // `bracket_stack`'s doc comment.
                    let inside_grouping = matches!(self.bracket_stack.last(), Some('(') | Some('['));
                    let emit = !inside_grouping && self.prev_can_end_statement();
                    let span = self.span();
                    self.advance();
                    if emit {
                        self.push(TokenKind::Newline, span);
                        return Ok(false);
                    }
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.advance();
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.advance();
                    self.advance();
                    // Accumulate the body regardless of whether this turns
                    // out to be a doc comment — cheap, and it's the only
                    // way to tell `/**` (doc) from `/*` (plain) correctly.
                    // A naive "is the very next char also '*'" check run
                    // *before* scanning for the close gets `/**/` wrong:
                    // consuming the 2-char `/*` opener leaves the scanner
                    // sitting on the 3rd character, and for `/**/` that 3rd
                    // character (`*`) is simultaneously the first half of
                    // the *closing* `*/` — so it's consumed as part of the
                    // close, never makes it into `body`, and `body` ends up
                    // empty. Checking `body.starts_with('*')` *after* the
                    // scan is what correctly rejects that case (an empty
                    // `/**/` isn't a doc comment — same convention as
                    // Javadoc/KDoc, which need at least one more character
                    // to hold a body) while still accepting a real
                    // `/** ... */`, whose leading `*` survives into `body`
                    // because the actual close is further along.
                    let mut body = String::new();
                    let end_line = loop {
                        match self.peek() {
                            None => return Err(self.err("unterminated block comment")),
                            Some('*') if self.peek_at(1) == Some('/') => {
                                self.advance();
                                self.advance();
                                break self.line;
                            }
                            Some(c) => {
                                body.push(c);
                                self.advance();
                            }
                        }
                    };
                    if matches!(self.mode_stack.last(), Some(LexMode::Normal)) {
                        if let Some(doc_text) = body.strip_prefix('*') {
                            self.doc_comments.insert(end_line + 1, doc_text.to_string());
                        }
                    }
                }
                _ => break,
            }
        }

        let start = self.span();
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(true),
        };

        if c == '"' {
            self.advance();
            self.push(TokenKind::StringStart, start);
            self.mode_stack.push(LexMode::TemplateText);
            return Ok(false);
        }

        if c.is_ascii_digit() {
            self.lex_number(start)?;
            return Ok(false);
        }

        if c.is_alphabetic() || c == '_' {
            self.lex_ident_or_keyword(start);
            return Ok(false);
        }

        self.lex_operator_or_punct(start)?;
        Ok(false)
    }

    fn lex_number(&mut self, start: Span) -> Result<(), ParseError> {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                text.push(c);
                self.advance();
            } else {
                break;
            }
        }

        let mut is_double = false;

        if self.peek() == Some('.')
            && self.peek_at(1).map(|c| c.is_ascii_digit()).unwrap_or(false)
        {
            is_double = true;
            text.push('.');
            self.advance();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    text.push(c);
                    self.advance();
                } else {
                    break;
                }
            }
        }

        if matches!(self.peek(), Some('e') | Some('E')) {
            let mut lookahead = 1;
            if matches!(self.peek_at(lookahead), Some('+') | Some('-')) {
                lookahead += 1;
            }
            if self.peek_at(lookahead).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                is_double = true;
                text.push(self.advance().unwrap()); // e/E
                if matches!(self.peek(), Some('+') | Some('-')) {
                    text.push(self.advance().unwrap());
                }
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        text.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
        }

        if is_double {
            let v: f64 = text.parse().map_err(|_| self.err("invalid number literal"))?;
            self.push(TokenKind::Double(v), start);
        } else {
            let v: i64 = text.parse().map_err(|_| self.err("invalid integer literal"))?;
            self.push(TokenKind::Int(v), start);
        }
        Ok(())
    }

    fn lex_ident_or_keyword(&mut self, start: Span) {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                text.push(c);
                self.advance();
            } else {
                break;
            }
        }
        let kind = match text.as_str() {
            "fun" => TokenKind::Fun,
            "val" => TokenKind::Val,
            "var" => TokenKind::Var,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "when" => TokenKind::When,
            "while" => TokenKind::While,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "return" => TokenKind::Return,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "package" => TokenKind::Package,
            "import" => TokenKind::Import,
            "null" => TokenKind::Null,
            "true" => TokenKind::Bool(true),
            "false" => TokenKind::Bool(false),
            _ => TokenKind::Ident(text),
        };
        self.push(kind, start);
    }

    fn lex_operator_or_punct(&mut self, start: Span) -> Result<(), ParseError> {
        let c = self.advance().unwrap();
        let kind = match c {
            '(' => {
                self.bracket_stack.push('(');
                TokenKind::LParen
            }
            ')' => {
                self.bracket_stack.pop();
                TokenKind::RParen
            }
            '{' => {
                if let Some(LexMode::TemplateExpr { brace_depth }) = self.mode_stack.last_mut() {
                    *brace_depth += 1;
                }
                // The `{` that opens a `${...}` interpolation is consumed
                // directly by `lex_template_text` and never reaches here —
                // only an ordinary block/lambda brace, or one nested inside
                // an interpolation, does — so this is always a real,
                // bracket_stack-tracked open.
                self.bracket_stack.push('{');
                TokenKind::LBrace
            }
            '}' => {
                if let Some(LexMode::TemplateExpr { brace_depth }) = self.mode_stack.last_mut() {
                    *brace_depth -= 1;
                    if *brace_depth == 0 {
                        self.mode_stack.pop();
                        // Symmetric with `{`'s own doc comment: this `}`
                        // closes the interpolation itself, whose opening
                        // `{` never pushed onto `bracket_stack` — so this
                        // path must not pop it either.
                        self.push(TokenKind::TemplateExprEnd, start);
                        return Ok(());
                    }
                }
                self.bracket_stack.pop();
                TokenKind::RBrace
            }
            '[' => {
                self.bracket_stack.push('[');
                TokenKind::LBracket
            }
            ']' => {
                self.bracket_stack.pop();
                TokenKind::RBracket
            }
            ',' => TokenKind::Comma,
            ':' => TokenKind::Colon,
            ';' => TokenKind::Semicolon,
            '@' => TokenKind::At,
            '.' => {
                if self.peek() == Some('.') {
                    self.advance();
                    TokenKind::Range
                } else {
                    TokenKind::Dot
                }
            }
            '=' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::Eq
                } else {
                    TokenKind::Assign
                }
            }
            '!' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::NotEq
                } else if self.peek() == Some('!') {
                    self.advance();
                    TokenKind::BangBang
                } else {
                    TokenKind::Not
                }
            }
            '?' => {
                if self.peek() == Some('.') {
                    self.advance();
                    TokenKind::QuestionDot
                } else if self.peek() == Some(':') {
                    self.advance();
                    TokenKind::QuestionColon
                } else {
                    TokenKind::Question
                }
            }
            '<' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::LtEq
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            '&' if self.peek() == Some('&') => {
                self.advance();
                TokenKind::AndAnd
            }
            '|' if self.peek() == Some('|') => {
                self.advance();
                TokenKind::OrOr
            }
            '+' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::PlusAssign
                } else {
                    TokenKind::Plus
                }
            }
            '-' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::MinusAssign
                } else if self.peek() == Some('>') {
                    self.advance();
                    TokenKind::Arrow
                } else {
                    TokenKind::Minus
                }
            }
            '*' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::StarAssign
                } else {
                    TokenKind::Star
                }
            }
            '/' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::SlashAssign
                } else {
                    TokenKind::Slash
                }
            }
            '%' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::PercentAssign
                } else {
                    TokenKind::Percent
                }
            }
            other => return Err(self.err(format!("unexpected character '{other}'"))),
        };
        self.push(kind, start);
        Ok(())
    }

    /// Consumes literal text inside a `"..."` string until `$`, `${`, or the
    /// closing `"`. Escapes are resolved here.
    fn lex_template_text(&mut self) -> Result<(), ParseError> {
        let mut text = String::new();
        // Tracks the start of the current run of accumulating text
        // (distinct from `self.span()`, which by the time a `StringText`
        // is pushed is already the *end* of that run) — unlike every other
        // token in this lexer, text characters are appended one at a time
        // via the catch-all `Some(c)` arm below rather than consumed in a
        // single pass right before `push`, so `push`'s own start-to-current
        // span computation doesn't apply here.
        let mut text_start = self.span();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string literal")),
                Some('"') => {
                    if !text.is_empty() {
                        self.push(TokenKind::StringText(std::mem::take(&mut text)), text_start);
                    }
                    let span = self.span();
                    self.advance();
                    self.mode_stack.pop();
                    self.push(TokenKind::StringEnd, span);
                    return Ok(());
                }
                Some('\\') => {
                    let span = self.span();
                    self.advance();
                    let escaped = match self.advance() {
                        Some('n') => '\n',
                        Some('t') => '\t',
                        Some('r') => '\r',
                        Some('\\') => '\\',
                        Some('"') => '"',
                        Some('$') => '$',
                        Some(other) => {
                            return Err(ParseError {
                                message: format!("unknown escape sequence '\\{other}'"),
                                span,
                            })
                        }
                        None => return Err(self.err("unterminated string literal")),
                    };
                    text.push(escaped);
                }
                Some('$') if self.peek_at(1) == Some('{') => {
                    if !text.is_empty() {
                        self.push(TokenKind::StringText(std::mem::take(&mut text)), text_start);
                    }
                    let span = self.span();
                    self.advance();
                    self.advance();
                    self.push(TokenKind::TemplateExprStart, span);
                    self.mode_stack.push(LexMode::TemplateExpr { brace_depth: 1 });
                    return Ok(());
                }
                Some('$') if self.peek_at(1).map(|c| c.is_alphabetic() || c == '_').unwrap_or(false) => {
                    if !text.is_empty() {
                        self.push(TokenKind::StringText(std::mem::take(&mut text)), text_start);
                    }
                    let span = self.span();
                    self.advance();
                    let mut name = String::new();
                    while let Some(c) = self.peek() {
                        if c.is_alphanumeric() || c == '_' {
                            name.push(c);
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    self.push(TokenKind::StringInterpText(name), span);
                    text_start = self.span();
                }
                Some(c) => {
                    text.push(c);
                    self.advance();
                }
            }
        }
    }
}

pub fn tokenize(source: &str) -> Result<Vec<Token>, ParseError> {
    Lexer::new(source).tokenize()
}

/// Like `tokenize`, but also returns a `/** ... */` doc-comment index (see
/// `Lexer::doc_comments`'s doc comment) alongside the identical token
/// stream `tokenize` would have produced.
pub fn tokenize_with_docs(source: &str) -> Result<(Vec<Token>, HashMap<u32, String>), ParseError> {
    Lexer::new(source).tokenize_with_docs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    /// `(kind, start_col, end_col)` for every token on a single source
    /// line — used by span tests below, where line is always 1 and only
    /// columns vary.
    fn kinds_and_cols(src: &str) -> Vec<(TokenKind, u32, u32)> {
        tokenize(src)
            .unwrap()
            .into_iter()
            .map(|t| (t.kind, t.span.col, t.span.end_col))
            .collect()
    }

    #[test]
    fn numbers() {
        assert_eq!(kinds("42"), vec![TokenKind::Int(42), TokenKind::Eof]);
        assert_eq!(kinds("3.14"), vec![TokenKind::Double(3.14), TokenKind::Eof]);
        assert_eq!(
            kinds("1..10"),
            vec![TokenKind::Int(1), TokenKind::Range, TokenKind::Int(10), TokenKind::Eof]
        );
        assert_eq!(
            kinds("1e3"),
            vec![TokenKind::Double(1000.0), TokenKind::Eof]
        );
    }

    #[test]
    fn keywords_and_idents() {
        assert_eq!(
            kinds("val x = true"),
            vec![
                TokenKind::Val,
                TokenKind::Ident("x".into()),
                TokenKind::Assign,
                TokenKind::Bool(true),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn at_token_lexes() {
        assert_eq!(
            kinds("@file:Import(\"x\")"),
            vec![
                TokenKind::At,
                TokenKind::Ident("file".into()),
                TokenKind::Colon,
                TokenKind::Ident("Import".into()),
                TokenKind::LParen,
                TokenKind::StringStart,
                TokenKind::StringText("x".into()),
                TokenKind::StringEnd,
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn null_safety_tokens() {
        assert_eq!(kinds("null"), vec![TokenKind::Null, TokenKind::Eof]);
        assert_eq!(
            kinds("a?.b"),
            vec![TokenKind::Ident("a".into()), TokenKind::QuestionDot, TokenKind::Ident("b".into()), TokenKind::Eof]
        );
        assert_eq!(
            kinds("a ?: b"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::QuestionColon,
                TokenKind::Ident("b".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("a!!"),
            vec![TokenKind::Ident("a".into()), TokenKind::BangBang, TokenKind::Eof]
        );
        assert_eq!(kinds("String?"), vec![TokenKind::Ident("String".into()), TokenKind::Question, TokenKind::Eof]);
    }

    #[test]
    fn null_safety_tokens_require_immediate_adjacency() {
        // A space breaks the multi-char token, same as every other
        // multi-char operator in this lexer (e.g. `& &` isn't `&&`).
        assert_eq!(
            kinds("a ? . b"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Question,
                TokenKind::Dot,
                TokenKind::Ident("b".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("a ! ! b"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Not,
                TokenKind::Not,
                TokenKind::Ident("b".into()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds("a && b || !c == d != e <= f"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::AndAnd,
                TokenKind::Ident("b".into()),
                TokenKind::OrOr,
                TokenKind::Not,
                TokenKind::Ident("c".into()),
                TokenKind::Eq,
                TokenKind::Ident("d".into()),
                TokenKind::NotEq,
                TokenKind::Ident("e".into()),
                TokenKind::LtEq,
                TokenKind::Ident("f".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn comments_stripped_including_trailing_eof_comment() {
        assert_eq!(
            kinds("1 // trailing comment, no newline"),
            vec![TokenKind::Int(1), TokenKind::Eof]
        );
        assert_eq!(
            kinds("1 /* block */ + 2"),
            vec![TokenKind::Int(1), TokenKind::Plus, TokenKind::Int(2), TokenKind::Eof]
        );
    }

    #[test]
    fn tokenize_with_docs_produces_the_same_tokens_as_tokenize() {
        let src = "/** doc */\nfun f() {}\n";
        assert_eq!(tokenize(src).unwrap(), tokenize_with_docs(src).unwrap().0);
    }

    #[test]
    fn doc_comment_is_indexed_on_the_line_right_after_it() {
        let src = "/** summary */\nfun f() {}\n";
        let (_, docs) = tokenize_with_docs(src).unwrap();
        assert_eq!(docs.get(&2).map(String::as_str), Some(" summary "));
    }

    #[test]
    fn multi_line_doc_comment_is_indexed_on_the_line_after_its_close() {
        let src = "/**\n * line one\n * line two\n */\nfun f() {}\n";
        let (_, docs) = tokenize_with_docs(src).unwrap();
        assert!(docs.contains_key(&5), "{docs:?}");
        assert!(docs.get(&5).unwrap().contains("line two"));
    }

    #[test]
    fn empty_or_plain_block_comments_are_never_indexed_as_docs() {
        for src in ["/**/\nfun f() {}\n", "/* plain */\nfun f() {}\n"] {
            let (_, docs) = tokenize_with_docs(src).unwrap();
            assert!(docs.is_empty(), "{src:?} -> {docs:?}");
        }
    }

    #[test]
    fn a_stray_extra_star_still_only_strips_one_leading_star() {
        let src = "/*** blah */\nfun f() {}\n";
        let (_, docs) = tokenize_with_docs(src).unwrap();
        assert_eq!(docs.get(&2).map(String::as_str), Some("* blah "));
    }

    #[test]
    fn doc_comment_inside_a_template_expression_is_not_indexed() {
        let src = "val x = \"${/** not a doc */ 1}\"\nfun f() {}\n";
        let (_, docs) = tokenize_with_docs(src).unwrap();
        assert!(docs.is_empty(), "{docs:?}");
    }

    #[test]
    fn asi_inserts_newline_after_statement_end_only() {
        // newline after an identifier ends the statement
        assert_eq!(
            kinds("a\nb"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Newline,
                TokenKind::Ident("b".into()),
                TokenKind::Eof
            ]
        );
        // newline after an operator is a continuation, not a separator
        assert_eq!(
            kinds("a +\nb"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Plus,
                TokenKind::Ident("b".into()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn newline_inside_parens_is_never_a_statement_separator() {
        // `f(\n  x\n)` — a newline right before the closing `)` of a call's
        // argument list (after `x`, an `Ident`, which is normally
        // `can_end_statement`) must not become a real `Newline` token.
        assert_eq!(
            kinds("f(\n  x\n)"),
            vec![
                TokenKind::Ident("f".into()),
                TokenKind::LParen,
                TokenKind::Ident("x".into()),
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn newline_inside_brackets_is_never_a_statement_separator() {
        assert_eq!(
            kinds("[\n  x\n]"),
            vec![
                TokenKind::LBracket,
                TokenKind::Ident("x".into()),
                TokenKind::RBracket,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn newline_right_after_a_closing_paren_is_still_a_statement_separator() {
        // The suppression is specifically for newlines *inside* an open
        // `(`/`[` — once `)` closes it, a following newline is back to the
        // ordinary `can_end_statement`-based rule (`RParen` is in that
        // list), so `if (x)\nprintln()`'s gap is unaffected by this lexer
        // change alone (the parser separately skips it — see `parser.rs`'s
        // `skip_newlines`).
        assert_eq!(
            kinds("f(x)\ny"),
            vec![
                TokenKind::Ident("f".into()),
                TokenKind::LParen,
                TokenKind::Ident("x".into()),
                TokenKind::RParen,
                TokenKind::Newline,
                TokenKind::Ident("y".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn newline_inside_a_brace_block_nested_in_parens_stays_significant() {
        // Regression guard: a multi-statement lambda/block passed as a
        // call argument (`f({ ... })`) must keep separating its own
        // statements by newline even though the whole `{...}` sits inside
        // the call's `(...)` — `{` resets the innermost-bracket check back
        // to "newlines matter" for its own direct content.
        // The trailing newline right before the block's own closing `}` is
        // also still significant (top of the bracket stack is `{` right up
        // until `}` itself is lexed) — matches how an ordinary block body
        // (`{ a\n b\n }`) already behaves today, with or without this fix.
        assert_eq!(
            kinds("f({\n  a\n  b\n})"),
            vec![
                TokenKind::Ident("f".into()),
                TokenKind::LParen,
                TokenKind::LBrace,
                TokenKind::Ident("a".into()),
                TokenKind::Newline,
                TokenKind::Ident("b".into()),
                TokenKind::Newline,
                TokenKind::RBrace,
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn newline_inside_parens_nested_in_a_brace_block_is_suppressed_again() {
        // One level deeper: `(...)` nested inside a `{...}` (itself inside
        // the outer call's `(...)`) suppresses newlines again for its own
        // content — the innermost open bracket always wins.
        // As above, the newline between `g(...)`'s closing `)` and the
        // block's own closing `}` is significant again (top of the stack
        // is back to `{` once `g(...)`'s `(` is closed).
        assert_eq!(
            kinds("f({\n  g(\n    a\n  )\n})"),
            vec![
                TokenKind::Ident("f".into()),
                TokenKind::LParen,
                TokenKind::LBrace,
                TokenKind::Ident("g".into()),
                TokenKind::LParen,
                TokenKind::Ident("a".into()),
                TokenKind::RParen,
                TokenKind::Newline,
                TokenKind::RBrace,
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn shebang_is_ignored() {
        assert_eq!(
            kinds("#!/usr/bin/env ksl\nval x = 1"),
            vec![
                TokenKind::Val,
                TokenKind::Ident("x".into()),
                TokenKind::Assign,
                TokenKind::Int(1),
                TokenKind::Eof
            ]
        );
        // shebang with no trailing newline (whole file is just the shebang)
        assert_eq!(kinds("#!/usr/bin/env ksl"), vec![TokenKind::Eof]);
        // a '#' that isn't at the very start of the file is still an error
        assert!(tokenize("val x = 1\n#!not a shebang").is_err());
    }

    #[test]
    fn plain_string() {
        assert_eq!(
            kinds("\"hi\""),
            vec![
                TokenKind::StringStart,
                TokenKind::StringText("hi".into()),
                TokenKind::StringEnd,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn string_interp_shorthand() {
        assert_eq!(
            kinds("\"a $x b\""),
            vec![
                TokenKind::StringStart,
                TokenKind::StringText("a ".into()),
                TokenKind::StringInterpText("x".into()),
                TokenKind::StringText(" b".into()),
                TokenKind::StringEnd,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn string_template_expr() {
        assert_eq!(
            kinds("\"sum=${1 + 2}\""),
            vec![
                TokenKind::StringStart,
                TokenKind::StringText("sum=".into()),
                TokenKind::TemplateExprStart,
                TokenKind::Int(1),
                TokenKind::Plus,
                TokenKind::Int(2),
                TokenKind::TemplateExprEnd,
                TokenKind::StringEnd,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn nested_string_and_braces_inside_template() {
        // "${ "inner ${x}" }" — a string containing a template inside a template
        assert_eq!(
            kinds("\"${ \"inner ${x}\" }\""),
            vec![
                TokenKind::StringStart,
                TokenKind::TemplateExprStart,
                TokenKind::StringStart,
                TokenKind::StringText("inner ".into()),
                TokenKind::TemplateExprStart,
                TokenKind::Ident("x".into()),
                TokenKind::TemplateExprEnd,
                TokenKind::StringEnd,
                TokenKind::TemplateExprEnd,
                TokenKind::StringEnd,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lambda_braces_inside_template_dont_confuse_brace_depth() {
        // "${ list.map { it * 2 } }" — nested { } from a lambda literal
        let toks = kinds("\"${ f { it * 2 } }\"");
        assert_eq!(
            toks,
            vec![
                TokenKind::StringStart,
                TokenKind::TemplateExprStart,
                TokenKind::Ident("f".into()),
                TokenKind::LBrace,
                TokenKind::Ident("it".into()),
                TokenKind::Star,
                TokenKind::Int(2),
                TokenKind::RBrace,
                TokenKind::TemplateExprEnd,
                TokenKind::StringEnd,
                TokenKind::Eof,
            ]
        );
    }

    /// Semantic tokens need real end positions, not just a start point.
    /// String templates are the trickiest case: `StringText` is
    /// accumulated one character at a time via a different code path than
    /// every other token, so it needs its own start-tracking (see
    /// `lex_template_text`) rather than `push`'s usual
    /// start-to-current-position span.
    #[test]
    fn token_end_spans() {
        assert_eq!(
            kinds_and_cols("abc == 12"),
            vec![
                (TokenKind::Ident("abc".into()), 1, 4),
                (TokenKind::Eq, 5, 7),
                (TokenKind::Int(12), 8, 10),
                (TokenKind::Eof, 10, 11),
            ]
        );

        // "hello" -> StringStart '"' (1..2), StringText "hello" (2..7),
        // StringEnd '"' (7..8).
        assert_eq!(
            kinds_and_cols("\"hello\""),
            vec![
                (TokenKind::StringStart, 1, 2),
                (TokenKind::StringText("hello".into()), 2, 7),
                (TokenKind::StringEnd, 7, 8),
                (TokenKind::Eof, 8, 9),
            ]
        );

        // "a${x}b" -> text before AND after an interpolation, exercising
        // text_start's reset after StringInterpText/TemplateExprStart.
        assert_eq!(
            kinds_and_cols("\"a${x}b\""),
            vec![
                (TokenKind::StringStart, 1, 2),
                (TokenKind::StringText("a".into()), 2, 3),
                (TokenKind::TemplateExprStart, 3, 5), // "${" - 2 chars
                (TokenKind::Ident("x".into()), 5, 6),
                (TokenKind::TemplateExprEnd, 6, 7), // "}" - 1 char
                (TokenKind::StringText("b".into()), 7, 8),
                (TokenKind::StringEnd, 8, 9),
                (TokenKind::Eof, 9, 10),
            ]
        );

        // "$name" shorthand interpolation spans the '$' through the ident.
        assert_eq!(
            kinds_and_cols("\"$name\""),
            vec![
                (TokenKind::StringStart, 1, 2),
                (TokenKind::StringInterpText("name".into()), 2, 7), // "$name" - 5 chars
                (TokenKind::StringEnd, 7, 8),
                (TokenKind::Eof, 8, 9),
            ]
        );
    }
}
