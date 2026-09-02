pub mod ast;
pub mod environment;
pub mod error;
pub mod importer;
pub mod interpreter;
pub mod lexer;
pub mod parser;
pub mod stdlib;
pub mod token;
pub mod value;

use std::path::Path;

use error::{InterpError, ParseError};
use importer::Importer;
use interpreter::Interpreter;

/// Lexes and parses `src` without resolving imports, executing anything, or
/// touching the filesystem. Never panics, including on truncated/malformed
/// input (unterminated strings, dangling `${`, trailing operators) — callers
/// like an LSP invoke this on every keystroke, where the buffer is
/// syntactically incomplete much of the time.
pub fn parse_only(src: &str) -> Result<ast::Program, ParseError> {
    let tokens = lexer::tokenize(src)?;
    parser::parse(&tokens)
}

/// Parses and runs a whole script, writing anything it `println`s/`print`s
/// to `out`. Resolves any `@file:Import(...)` declarations first
/// (recursively, each relative to *its own* file's parent directory)
/// before running the script's own top-level code, all against one shared
/// `Interpreter`. `base_dir` is the directory the top-level script's own
/// relative imports resolve against — normally the script file's parent
/// directory; pass the current working directory when the script has no
/// backing file (e.g. read from stdin via `ksl -`). `entry_path`, when the
/// script has a real backing file, is used to detect an import cycle that
/// loops back to the entry script itself. This is the single entry point
/// both the CLI and the tests use.
pub fn run_source(
    src: &str,
    out: &mut dyn std::io::Write,
    base_dir: &Path,
    entry_path: Option<&Path>,
) -> Result<(), InterpError> {
    let mut interp = Interpreter::new(out);
    Importer::new().run_top_level(src, base_dir, entry_path, &mut interp)
}

#[cfg(test)]
mod tests {
    use super::parse_only;

    /// `parse_only` is called on every keystroke by editor tooling, so the
    /// buffer is syntactically incomplete much of the time (mid-statement,
    /// mid-token, mid-string). It must return a `ParseError`, never panic.
    #[test]
    fn parse_only_never_panics_on_truncated_or_malformed_input() {
        let inputs = [
            "",
            "\"unterminated",
            "\"unterminated ${",
            "\"unterminated ${ 1 + ",
            "fun f(",
            "fun f(x",
            "fun f() {",
            "val x =",
            "val x = \"${",
            "1 +",
            "\\",
            "\"\\",
            "\"${}}}}}",
            "if (",
            "for (",
            "@file:Import(",
            "@file:",
        ];
        for src in inputs {
            let _ = parse_only(src); // must not panic, Ok or Err both fine
        }
    }
}
