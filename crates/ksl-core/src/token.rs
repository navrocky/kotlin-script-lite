use crate::error::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    Int(i64),
    Double(f64),
    Bool(bool),
    Ident(String),

    // String template pieces
    StringStart,
    StringText(String),
    StringInterpText(String),
    TemplateExprStart,
    TemplateExprEnd,
    StringEnd,

    // Keywords
    Fun,
    Val,
    Var,
    If,
    Else,
    When,
    While,
    For,
    In,
    Return,
    Break,
    Continue,
    Package,
    Import,
    Null,

    // Punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    Semicolon,
    At,

    // Operators
    Assign,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AndAnd,
    OrOr,
    Not,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    Arrow,
    Range,
    Question,
    QuestionDot,
    QuestionColon,
    BangBang,

    Newline,
    Eof,
}

impl TokenKind {
    /// True for tokens that can legally be the last token of a statement or
    /// expression — used by the lexer's automatic-semicolon-insertion: a
    /// source newline becomes a real `Newline` token only right after one of
    /// these, otherwise it's dropped as line-continuation whitespace.
    pub fn can_end_statement(&self) -> bool {
        matches!(
            self,
            TokenKind::Int(_)
                | TokenKind::Double(_)
                | TokenKind::Bool(_)
                | TokenKind::Ident(_)
                | TokenKind::StringEnd
                | TokenKind::RParen
                | TokenKind::RBracket
                | TokenKind::RBrace
                | TokenKind::Return
                | TokenKind::Break
                | TokenKind::Continue
                | TokenKind::Null
                | TokenKind::BangBang
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}
