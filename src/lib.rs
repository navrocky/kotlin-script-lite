pub mod ast;
pub mod environment;
pub mod error;
pub mod interpreter;
pub mod lexer;
pub mod parser;
pub mod stdlib;
pub mod token;
pub mod value;

use error::{Flow, InterpError, RuntimeError, Span};
use interpreter::Interpreter;

/// Parses and runs a whole script, writing anything it `println`s/`print`s
/// to `out`. This is the single entry point both the CLI and the tests use.
pub fn run_source(src: &str, out: &mut dyn std::io::Write) -> Result<(), InterpError> {
    let tokens = lexer::tokenize(src).map_err(InterpError::Parse)?;
    let stmts = parser::parse(&tokens).map_err(InterpError::Parse)?;
    let mut interp = Interpreter::new(out);
    match interp.run(&stmts) {
        Ok(()) => Ok(()),
        Err(Flow::Error(e)) => Err(InterpError::Runtime(e)),
        Err(Flow::Return(_)) => Ok(()),
        Err(Flow::Break) | Err(Flow::Continue) => Err(InterpError::Runtime(RuntimeError::new(
            "'break'/'continue' used outside of a loop",
            Span::default(),
        ))),
    }
}
