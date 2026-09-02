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
    tokens: Vec<Token>,
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
            tokens: Vec::new(),
            _source: source,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, ParseError> {
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
        Ok(self.tokens)
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

    fn push(&mut self, kind: TokenKind, span: Span) {
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
                    let emit = self.prev_can_end_statement();
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
                    loop {
                        match self.peek() {
                            None => return Err(self.err("unterminated block comment")),
                            Some('*') if self.peek_at(1) == Some('/') => {
                                self.advance();
                                self.advance();
                                break;
                            }
                            _ => {
                                self.advance();
                            }
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
            "true" => TokenKind::Bool(true),
            "false" => TokenKind::Bool(false),
            _ => TokenKind::Ident(text),
        };
        self.push(kind, start);
    }

    fn lex_operator_or_punct(&mut self, start: Span) -> Result<(), ParseError> {
        let c = self.advance().unwrap();
        let kind = match c {
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => {
                if let Some(LexMode::TemplateExpr { brace_depth }) = self.mode_stack.last_mut() {
                    *brace_depth += 1;
                }
                TokenKind::LBrace
            }
            '}' => {
                if let Some(LexMode::TemplateExpr { brace_depth }) = self.mode_stack.last_mut() {
                    *brace_depth -= 1;
                    if *brace_depth == 0 {
                        self.mode_stack.pop();
                        self.push(TokenKind::TemplateExprEnd, start);
                        return Ok(());
                    }
                }
                TokenKind::RBrace
            }
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
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
                } else {
                    TokenKind::Not
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
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string literal")),
                Some('"') => {
                    if !text.is_empty() {
                        self.push(TokenKind::StringText(std::mem::take(&mut text)), self.span());
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
                        self.push(TokenKind::StringText(std::mem::take(&mut text)), self.span());
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
                        self.push(TokenKind::StringText(std::mem::take(&mut text)), self.span());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
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
}
