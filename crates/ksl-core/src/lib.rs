pub mod ast;
pub mod class;
pub mod completion_probe;
pub mod doc_comment;
pub mod environment;
pub mod error;
pub(crate) mod gc;
pub mod importer;
pub mod interpreter;
pub mod lexer;
pub mod package_index;
pub mod parser;
pub mod resolver;
pub mod stdlib;
pub mod token;
pub mod types;
pub mod value;

use std::path::Path;

use error::{InterpError, ParseError, RuntimeError};
use importer::Importer;
use interpreter::Interpreter;

/// Lexes and parses `src` without resolving imports, executing anything, or
/// touching the filesystem. Never panics, including on truncated/malformed
/// input (unterminated strings, dangling `${`, trailing operators) — callers
/// like an LSP invoke this on every keystroke, where the buffer is
/// syntactically incomplete much of the time.
pub fn parse_only(src: &str) -> Result<ast::Program, ParseError> {
    let (tokens, docs) = lexer::tokenize_with_docs(src)?;
    parser::parse_with_docs(&tokens, docs)
}

/// Runs the static type checker (parse + `resolver::resolve`, no execution,
/// no file I/O, no `@file:Import` resolution) and returns the type-
/// compatibility problems found, if any — this is the closest ksl has to
/// Kotlin's compile-time rejection of ill-typed code, since ksl has no
/// separate compile step (see `docs/roadmap.md`'s type-system phase).
/// `ksl-cli` calls this before `run_source` and refuses to run a script
/// with any error in the returned list.
///
/// Returns `Ok(())` if `src` doesn't even parse — reporting a parse error
/// is `run_source`'s own job, unchanged; this function only ever surfaces
/// `resolver::ResolveResult::type_errors`, never `diagnostics` (undefined-
/// name, duplicate-declaration stay an editor-only concern for now — see
/// that field's doc comment).
pub fn static_check(src: &str) -> Result<(), Vec<RuntimeError>> {
    let Ok(program) = parse_only(src) else {
        return Ok(());
    };
    let type_errors = resolver::resolve(&program).type_errors;
    if type_errors.is_empty() {
        Ok(())
    } else {
        Err(type_errors)
    }
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
    use super::{parse_only, static_check};

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
            "package",
            "package a.",
            "package a.b",
            "import",
            "import a.",
            "import a",
            "a.",
            "a.b.",
        ];
        for src in inputs {
            let _ = parse_only(src); // must not panic, Ok or Err both fine
        }
    }

    /// `ksl-cli` calls this before `run_source` and refuses to run the
    /// script at all if it returns `Err` — see `main.rs`, verified manually
    /// end-to-end (a script with a genuine type mismatch exits non-zero
    /// and prints nothing from its body; a well-typed one runs normally).
    /// This test covers the logic `static_check` itself is responsible for.
    #[test]
    fn static_check_rejects_a_genuine_type_mismatch() {
        let errors = static_check("val x: Int = \"hi\"\n").unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("type mismatch: expected Int, got String"), "{}", errors[0].message);
    }

    #[test]
    fn static_check_accepts_a_well_typed_script() {
        assert!(static_check("fun add(a: Int, b: Int): Int { return a + b }\nprintln(add(1, 2))").is_ok());
    }

    #[test]
    fn static_check_ignores_a_script_that_does_not_even_parse() {
        // Reporting the parse error is `run_source`'s job, unchanged.
        assert!(static_check("val x = ").is_ok());
    }
}
