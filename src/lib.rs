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

use error::InterpError;
use importer::Importer;
use interpreter::Interpreter;

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
