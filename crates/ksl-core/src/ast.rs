use crate::error::Span;

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub path: String,
    pub span: Span,
}

/// The result of parsing a whole file: its `@file:Import(...)` declarations
/// (all appearing before any other statement) plus its own top-level
/// statements.
#[derive(Debug, Clone)]
pub struct Program {
    pub imports: Vec<ImportDecl>,
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Expr { kind, span }
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    IntLit(i64),
    DoubleLit(f64),
    BoolLit(bool),
    StringTemplate(Vec<StringPart>),
    Name(String),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    Lambda {
        params: Vec<Param>,
        body: Vec<Stmt>,
    },
    If {
        cond: Box<Expr>,
        then_branch: Vec<Stmt>,
        else_branch: Option<Vec<Stmt>>,
    },
    When {
        subject: Option<Box<Expr>>,
        arms: Vec<WhenArm>,
    },
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone)]
pub enum StringPart {
    Text(String),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Pos,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub type_hint: Option<String>,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct WhenArm {
    pub conditions: Vec<WhenCondition>,
    pub is_else: bool,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum WhenCondition {
    /// `subject_expr == condition_expr` (subject form)
    Equals(Expr),
    /// bare boolean condition (subject-less form)
    Bool(Expr),
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

impl Stmt {
    pub fn new(kind: StmtKind, span: Span) -> Self {
        Stmt { kind, span }
    }
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    ExprStmt(Expr),
    ValDecl {
        name: String,
        type_hint: Option<String>,
        mutable: bool,
        init: Expr,
    },
    Assign {
        name: String,
        op: AssignOp,
        value: Expr,
    },
    FunDecl {
        name: String,
        params: Vec<Param>,
        body: FunBody,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    For {
        var_name: String,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>),
    Break,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Debug, Clone)]
pub enum FunBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}
