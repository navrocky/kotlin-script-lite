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
    Do,
    For,
    In,
    Return,
    Break,
    Continue,
    Package,
    Import,
    Null,
    Class,
    This,
    Try,
    Catch,
    Finally,
    Throw,
    Typealias,

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
    PlusPlus,
    MinusMinus,
    Arrow,
    Range,
    Question,
    QuestionDot,
    QuestionColon,
    BangBang,

    Newline,
    Eof,
}

/// Every keyword text the lexer recognizes, paired with the `TokenKind` it
/// produces — the single source of truth both `lex_ident_or_keyword`
/// (`lexer.rs`) and `ksl-lsp`'s keyword completion derive from, so the two
/// can never drift apart the way a hand-maintained keyword list in
/// `completion.rs` once could (it was missing `package`/`import`/`null` —
/// present here, but never added there by hand when these three were
/// added to the lexer).
pub const KEYWORDS: &[(&str, TokenKind)] = &[
    ("fun", TokenKind::Fun),
    ("val", TokenKind::Val),
    ("var", TokenKind::Var),
    ("if", TokenKind::If),
    ("else", TokenKind::Else),
    ("when", TokenKind::When),
    ("while", TokenKind::While),
    ("do", TokenKind::Do),
    ("for", TokenKind::For),
    ("in", TokenKind::In),
    ("return", TokenKind::Return),
    ("break", TokenKind::Break),
    ("continue", TokenKind::Continue),
    ("package", TokenKind::Package),
    ("import", TokenKind::Import),
    ("null", TokenKind::Null),
    ("class", TokenKind::Class),
    ("this", TokenKind::This),
    ("try", TokenKind::Try),
    ("catch", TokenKind::Catch),
    ("finally", TokenKind::Finally),
    ("throw", TokenKind::Throw),
    ("typealias", TokenKind::Typealias),
    ("true", TokenKind::Bool(true)),
    ("false", TokenKind::Bool(false)),
];

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
                | TokenKind::PlusPlus
                | TokenKind::MinusMinus
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}
