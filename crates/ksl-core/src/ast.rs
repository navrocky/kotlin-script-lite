use crate::error::Span;

/// A type as written in source — `Int`, `String`, or a function type like
/// `(Int, String) -> Result`. Distinct from `types::Type`, the *resolved*
/// representation the resolver/interpreter actually check against: this is
/// still raw syntax (an unresolved name might be a typo, a generic
/// parameter, or a not-yet-declared class — see `types::resolve_type_annotation`,
/// which turns one of these into a real `Type`).
#[derive(Debug, Clone)]
pub enum TypeAnnotation {
    Named(String, Span),
    Function(Vec<TypeAnnotation>, Box<TypeAnnotation>, Span),
    /// `T?` — wraps any other annotation, including another `Nullable` in
    /// principle (the parser only ever produces one layer, see
    /// `Parser::parse_type_annotation`). `Span` covers the whole `T?`.
    Nullable(Box<TypeAnnotation>, Span),
}

impl TypeAnnotation {
    pub fn span(&self) -> Span {
        match self {
            TypeAnnotation::Named(_, s) => *s,
            TypeAnnotation::Function(_, _, s) => *s,
            TypeAnnotation::Nullable(_, s) => *s,
        }
    }
}

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
    /// Span of the whole `@file:Import(...)` construct (starting at `@`) —
    /// used for runtime error reporting (`importer.rs`), where pointing at
    /// the declaration as a whole reads better than just its string.
    pub span: Span,
    /// Span of just the path string literal (quotes included) — separate
    /// from `span` so a goto-definition on the file name itself (see
    /// `ksl-lsp`'s `goto_definition.rs`) can target precisely that, not the
    /// whole declaration.
    pub path_span: Span,
}

/// `import a.b.c` — brings the package-qualified name `a.b.c` (package
/// `a.b`, member `c`) into this file's own unqualified scope — or, when
/// `wildcard` is set, `import a.b.*` brings in *every* top-level name
/// package `a.b` declares. When `wildcard` is `false`, `path` always has at
/// least 2 segments and its last is the member name, everything before it
/// the package name; when `true`, every segment of `path` is part of the
/// package name (the member position held `*` instead, consumed by the
/// parser rather than stored). Orthogonal to `ImportDecl`/`@file:Import`,
/// which loads a *file* by path — `import` only aliases a name (or names)
/// some already-loaded file's `package` declaration put there, or one of
/// the handful of synthetic packages (`kotlin.test`) that alias ksl's own
/// stdlib without needing a loaded file at all — never loads anything by
/// itself. See `Interpreter::resolve_qualified`/`resolve_package_members`.
#[derive(Debug, Clone)]
pub struct ImportUse {
    pub path: Vec<String>,
    pub wildcard: bool,
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
        /// `true` for `?.` — short-circuits to `Value::Null` without
        /// erroring when the receiver is `Value::Null`, instead of the
        /// usual "has no field" error.
        safe: bool,
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
        /// `true` for `?.` — short-circuits to `Value::Null` without
        /// evaluating `args` when the receiver is `Value::Null`.
        safe: bool,
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
    /// `lhs ?: rhs` — short-circuits, evaluating `rhs` only when `lhs`
    /// evaluates to `Value::Null`. Not a `BinaryOp` since it needs the same
    /// non-eager evaluation `&&`/`||` get, not `apply_binary`'s eager rule.
    Elvis {
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
    NullLit,
    /// `++x`/`--x` (`prefix: true`) or `x++`/`x--` (`prefix: false`) —
    /// restricted to a bare variable target, same as `StmtKind::Assign`'s
    /// `name` (no `.field`/`[i]` target exists to increment yet). Mutates
    /// `name` in place (sugar over `name = name + 1`/`name = name - 1`,
    /// reusing the same `apply_binary` arithmetic the interpreter already
    /// uses for `+=`/`-=`) and evaluates to the *new* value when `prefix`,
    /// the *old* value otherwise — matching Kotlin's own semantics.
    IncDec {
        op: IncDecOp,
        name: String,
        name_span: Span,
        prefix: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncDecOp {
    Inc,
    Dec,
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
    /// `x!!` — postfix, parsed in `parse_postfix` (not `parse_unary`), but
    /// shares this node shape since the interpreter doesn't care which side
    /// of the expression constructed it.
    NotNullAssert,
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
    /// Span of just the parameter name (not its type annotation or
    /// default), used for hover/goto-definition.
    pub name_span: Span,
    pub type_annotation: Option<TypeAnnotation>,
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
        type_annotation: Option<TypeAnnotation>,
        mutable: bool,
        init: Expr,
        /// Raw text of an immediately-preceding `/** ... */` doc comment,
        /// if any (see `parser::parse_with_docs`) — resolved into a
        /// `doc_comment::ParsedDoc` by the resolver, not here.
        doc_comment: Option<String>,
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
        /// Raw return-type annotation (`fun f(): Int`), if any — resolved
        /// into a real `types::Type` by the resolver/interpreter, not here.
        return_type: Option<TypeAnnotation>,
        body: FunBody,
        /// Raw text of an immediately-preceding `/** ... */` doc comment,
        /// if any — see `ValDecl::doc_comment`.
        doc_comment: Option<String>,
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
