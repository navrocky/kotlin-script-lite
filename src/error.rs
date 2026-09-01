use crate::value::Value;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub line: u32,
    pub col: u32,
}

impl Span {
    pub fn new(line: u32, col: u32) -> Self {
        Span { line, col }
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
