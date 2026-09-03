use crate::error::Span;

/// `package a.b` — optional, must be the very first thing in a file if
/// present. Namespaces the file's own top-level `fun`/`val`/`var`
/// declarations under `path` (joined with `.`) instead of the flat global
/// scope; see `Interpreter::package_env`.
#[derive(Debug, Clone)]
pub struct PackageDecl {
    pub path: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub path: String,
    pub span: Span,
}

/// `import a.b.c` — brings the package-qualified name `a.b.c` (package
/// `a.b`, member `c`) into this file's own unqualified scope. `path` always
/// has at least 2 segments; the last is the member name, everything before
/// it is the package name. Orthogonal to `ImportDecl`/`@file:Import`, which
/// loads a *file* by path — `import` only aliases a name that some already-
/// loaded file's `package` declaration put there; it never loads anything
/// by itself. See `Interpreter::resolve_qualified`.
#[derive(Debug, Clone)]
pub struct ImportUse {
    pub path: Vec<String>,
    pub span: Span,
}

/// The result of parsing a whole file: its optional `package` declaration,
/// its `@file:Import(...)`/`import a.b.c` declarations (all appearing
/// before any other statement), and its own top-level statements.
#[derive(Debug, Clone)]
pub struct Program {
    pub package: Option<PackageDecl>,
    pub imports: Vec<ImportDecl>,
    pub imports_use: Vec<ImportUse>,
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
    /// `receiver.field` — a `.` access not immediately followed by `(`.
    /// The parser can't tell locally declared names from package names
    /// apart (no scope/type info at parse time), so this is produced for
    /// *any* `.`-access, including what's really a package-qualified
    /// reference like `a.b` (package `a`, member `b`) — resolved by
    /// `eval_expr` at runtime, which tries local evaluation first and
    /// falls back to `Interpreter::resolve_qualified` over the flattened
    /// dotted chain. See `docs/syntax.md`'s "Packages" section.
    FieldAccess {
        receiver: Box<Expr>,
        field: String,
        field_span: Span,
    },
    /// `receiver.method(args)` — a `.` access immediately followed by `(`.
    /// Same local-vs-package-qualified ambiguity as `FieldAccess`, resolved
    /// the same way at evaluation time; also the call site for a builtin
    /// method (e.g. `"x".uppercase()`) once the receiver evaluates to a
    /// real value.
    MethodCall {
        receiver: Box<Expr>,
        method: String,
        method_span: Span,
        args: Vec<Arg>,
    },
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
    /// Span of just the parameter name (not its type hint or default),
    /// used for hover/goto-definition.
    pub name_span: Span,
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
        /// Span of just the declared name, used for hover/goto-definition.
        name_span: Span,
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
        /// Span of just the function name, used for hover/goto-definition.
        name_span: Span,
        params: Vec<Param>,
        body: FunBody,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    For {
        var_name: String,
        /// Span of just the loop variable name, used for hover/goto-definition.
        var_name_span: Span,
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
