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

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
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
