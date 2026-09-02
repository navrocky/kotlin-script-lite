use crate::value::Value;
use std::fmt;
use std::path::PathBuf;

/// A source location, 1-based (matching how editors/compilers usually
/// report positions to humans). `end_line`/`end_col` mark one past the
/// last character, mirroring how `col`/`line` mark the first — so an empty
/// range has `end == start`. `Span::new` (used at most call sites, where
/// only the start position is tracked) defaults to a 1-character-wide
/// range; `Span::with_end` is for call sites that track the real end, used
/// where it matters (e.g. semantic tokens, hover/goto-def).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        RuntimeError {
            message: message.into(),
            span,
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
            InterpError::Runtime(e) => write!(f, "runtime error: {} at {}", e.message, e.span),
            InterpError::InFile { file, source } => {
                write!(f, "in imported file '{}': {source}", file.display())
            }
        }
    }
}
