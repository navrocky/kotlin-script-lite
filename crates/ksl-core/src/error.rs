use crate::token::TokenKind;
use crate::value::Value;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A source location, 1-based (matching how editors/compilers usually
/// report positions to humans). `end_line`/`end_col` mark one past the
/// last character, mirroring how `col`/`line` mark the first — so an empty
/// range has `end == start`. `Span::new` (used at most call sites, where
/// only the start position is tracked) defaults to a 1-character-wide
/// range; `Span::with_end` is for call sites that track the real end, used
/// where it matters (e.g. semantic tokens, hover/goto-def).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl Span {
    pub fn new(line: u32, col: u32) -> Self {
        Span { line, col, end_line: line, end_col: col + 1 }
    }

    pub fn with_end(line: u32, col: u32, end_line: u32, end_col: u32) -> Self {
        Span { line, col, end_line, end_col }
    }

    /// True if the 1-based `(line, col)` position falls within this span
    /// — used to match a cursor position against a token/scope span.
    pub fn contains_line_col(&self, line: u32, col: u32) -> bool {
        if line < self.line || line > self.end_line {
            return false;
        }
        if line == self.line && col < self.col {
            return false;
        }
        if line == self.end_line && col >= self.end_col {
            return false;
        }
        true
    }

    /// The smallest span covering both `self` and `other`.
    pub fn merge(&self, other: &Span) -> Span {
        let (line, col) = if (self.line, self.col) <= (other.line, other.col) {
            (self.line, self.col)
        } else {
            (other.line, other.col)
        };
        let (end_line, end_col) = if (self.end_line, self.end_col) >= (other.end_line, other.end_col) {
            (self.end_line, self.end_col)
        } else {
            (other.end_line, other.end_col)
        };
        Span { line, col, end_line, end_col }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// What grammatical role an identifier plays at the point it was required
/// — lets a completion caller distinguish, say, "a type name was expected"
/// (offer primitive types) from "a brand-new binding name was expected"
/// (offer nothing in particular), which `Expected::Identifier` alone can't:
/// both are "an identifier goes here" at the token level, but very
/// different positions semantically. `Other` covers the two call sites
/// completion doesn't care about (a postfix `.member` name, a named
/// argument's own label) — never consulted, just keeps every
/// `expect_ident`/`expect_ident_spanned` call site honest about which role
/// applies instead of silently defaulting to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentRole {
    /// Right after a type-annotation's `:` (`val x: |`, a param's/fun's
    /// return type) — `parser::Parser::parse_type_annotation`.
    TypeName,
    /// A `val`/`var`/`fun`/parameter/`for`-loop-variable name being
    /// declared (or, for `++x`/`x++`, referenced) — every call site where
    /// completion has nothing more specific to offer than "you're naming
    /// something here."
    NewBindingName,
    /// A segment of a `package`/`import` dotted path.
    QualifiedPathSegment,
    /// The `file` in `@file:Import(...)` — `parser::Parser::parse_file_import`.
    FileImportHead,
    /// The `Import` in `@file:Import(...)`.
    FileImportAnnotationName,
    /// Not consulted by completion at all.
    Other,
}

/// What would have made a failed parse succeed at the point it failed —
/// structured, not just `ParseError::message`'s free text, so a caller
/// (`ksl-lsp`'s completion) can ask "what's grammatically valid to type
/// right here" by re-parsing the text truncated at the cursor and reading
/// this off the resulting error, instead of guessing from raw text. Only
/// ever populated at the few funnel points that fail predictably (`expect`,
/// `expect_ident`/`expect_ident_spanned`, and `parser::Parser::
/// parse_postfix`'s own enrichment of a call's argument list) — every
/// free-text `self.err(...)` site leaves `ParseError::expected` empty,
/// which is a known, accepted gap (see `ksl-lsp`'s `completion_probe`
/// module doc comment), not an oversight.
#[derive(Debug, Clone, PartialEq)]
pub enum Expected {
    /// A single specific token was required next — `expect`'s own `kind`
    /// argument, captured directly.
    Token(TokenKind),
    Identifier(IdentRole),
    /// Specifically inside a call's still-open argument list — richer than
    /// a bare `Token(RParen)`/`Token(Comma)`, since the `Call`/`MethodCall`
    /// AST node doesn't exist yet while its own arg list is incomplete, so
    /// there's nothing else a caller could read "which callee"/"which
    /// names already used" off of.
    CallArgument { callee: Option<String>, already_named: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
    /// See `Expected`'s doc comment. Empty by default — use
    /// `ParseError::new` for every free-text `self.err(...)`-style site,
    /// which leaves this empty; only `expect`/`expect_ident`/
    /// `expect_ident_spanned`'s own error branches (and `parse_postfix`'s
    /// `CallArgument` enrichment) populate it.
    pub expected: Vec<Expected>,
}

impl ParseError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        ParseError { message: message.into(), span, expected: Vec::new() }
    }
}

/// One call-stack frame — `signature` is a human-readable rendering (e.g.
/// `"add(a: Int, b: Int)"` or a native's bare name), `file` is which file
/// the function itself was declared in. `position` is where *this* frame
/// is currently paused *within its own `file`* — the span of whatever call
/// it made into the next (inner) frame, kept up to date as execution moves
/// through its body (see `Interpreter::call_function`/`call_value`, which
/// update the *previous* top-of-stack frame's `position` right before
/// pushing a new one for the callee — never the callee's own file paired
/// with the caller's coordinates, which would silently mix two different
/// files' line numbers together). For the innermost frame specifically,
/// `position` is superseded by the error's own `span` (already shown on
/// the message line above the trace) rather than relied on. Attached to a
/// `RuntimeError` only once, when it's about to leave
/// `Interpreter::run_in_env` uncaught — see that function.
#[derive(Debug, Clone)]
pub struct StackFrame {
    pub signature: String,
    pub file: Arc<Path>,
    pub position: Span,
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
    /// The call stack at the point this error left `run_in_env` uncaught
    /// — empty for every error still just passing through normal `?`
    /// propagation (the overwhelming majority of a `RuntimeError`'s
    /// lifetime); only ever populated once, right at the top. Innermost
    /// frame (where the error actually happened) first.
    pub stack: Vec<StackFrame>,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        RuntimeError {
            message: message.into(),
            span,
            stack: Vec::new(),
        }
    }
}

/// Non-local control transfer used while evaluating a function/loop body.
/// `return`/`break`/`continue` are implemented by threading this through
/// `Result::Err` and using `?` to unwind, exactly like a runtime error.
pub enum Flow {
    Error(RuntimeError),
    Return(Value),
    Break,
    Continue,
    /// `throw <value>` — a *separate* unwinding path from `Error` above:
    /// `Error` wraps an internal `RuntimeError` and stays fatal/uncatchable
    /// (no `try`/`catch` in this language catches it), while `Throw` wraps
    /// a real, catchable ksl value (always a `Value::Object` of `Exception`/
    /// `RuntimeException` or a subclass, in practice — nothing stops a
    /// script from evaluating a `throw` operand to some other `Value`, but
    /// `resolver.rs`'s `ExprKind::Throw` handling flags anything else as a
    /// static error). See `interpreter.rs`'s `ExprKind::Try` handling for
    /// where this is caught, and `importer.rs` for how one that reaches the
    /// top of a program uncaught is reported.
    Throw(Value),
}

impl From<RuntimeError> for Flow {
    fn from(e: RuntimeError) -> Self {
        Flow::Error(e)
    }
}

pub enum InterpError {
    Parse(ParseError),
    Runtime(RuntimeError),
    /// Static type-checking failures (`resolver::ResolveResult::type_errors`)
    /// found while parsing+resolving a file *before* running any of it —
    /// see `importer.rs`'s unified parse-resolve-then-run flow. A script
    /// with any of these never executes a single statement, matching what
    /// the old, separate `static_check` guaranteed.
    TypeErrors(Vec<RuntimeError>),
    /// Wraps an error that occurred while reading/parsing/running a file
    /// pulled in via `@file:Import(...)`, naming which file it came from.
    /// `Span` has no file field, so this is how import errors stay
    /// attributable without threading a file identity through every
    /// token/AST construction site.
    InFile { file: PathBuf, source: Box<InterpError> },
}

impl fmt::Display for InterpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterpError::Parse(e) => write!(f, "parse error: {} at {}", e.message, e.span),
            InterpError::Runtime(e) => {
                write!(f, "runtime error: {} at {}", e.message, e.span)?;
                for frame in e.stack.iter().rev() {
                    write!(f, "\n    at {} ({}:{})", frame.signature, frame.file.display(), frame.position)?;
                }
                Ok(())
            }
            InterpError::TypeErrors(errors) => {
                for (i, e) in errors.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "error: {} at {}", e.message, e.span)?;
                }
                Ok(())
            }
            InterpError::InFile { file, source } => {
                write!(f, "in imported file '{}': {source}", file.display())
            }
        }
    }
}
