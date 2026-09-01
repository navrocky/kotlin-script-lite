use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::{ImportDecl, Program, Stmt};
use crate::error::{Flow, InterpError, RuntimeError, Span};
use crate::interpreter::Interpreter;
use crate::{lexer, parser};

/// Recursively resolves and runs `@file:Import(...)` declarations against a
/// shared `Interpreter`, before the importing file's own statements run.
pub struct Importer {
    /// Canonicalized paths already fully loaded and run — a later import
    /// resolving to one of these is skipped (diamond-import dedup) rather
    /// than re-run, since re-running would repeat side effects like
    /// `println`.
    loaded: HashSet<PathBuf>,
    /// Canonicalized paths currently being loaded, in load order — used to
    /// detect import cycles before they overflow the stack.
    stack: Vec<PathBuf>,
}

impl Importer {
    pub fn new() -> Self {
        Importer {
            loaded: HashSet::new(),
            stack: Vec::new(),
        }
    }

    /// Parses `src`, recursively resolves+runs its `@file:Import`s
    /// (relative to `base_dir`), then runs its own top-level statements —
    /// all against `interp`. `entry_path`, when the script has a real
    /// backing file, is registered on the cycle-detection stack so an
    /// import chain that loops back to the entry script itself is caught
    /// rather than silently re-executed.
    pub fn run_top_level(
        &mut self,
        src: &str,
        base_dir: &Path,
        entry_path: Option<&Path>,
        interp: &mut Interpreter,
    ) -> Result<(), InterpError> {
        let program = parse_program(src)?;

        let entry_canonical = match entry_path {
            Some(p) => Some(fs::canonicalize(p).map_err(|e| {
                InterpError::Runtime(RuntimeError::new(
                    format!("cannot resolve script path '{}': {e}", p.display()),
                    Span::default(),
                ))
            })?),
            None => None,
        };
        if let Some(canonical) = &entry_canonical {
            self.stack.push(canonical.clone());
        }

        let result = self.run_imports(&program, base_dir, interp);

        if entry_canonical.is_some() {
            self.stack.pop();
        }
        result?;

        run_stmts(interp, &program.stmts)
    }

    fn run_imports(
        &mut self,
        program: &Program,
        base_dir: &Path,
        interp: &mut Interpreter,
    ) -> Result<(), InterpError> {
        for import in &program.imports {
            self.load_import(import, base_dir, interp)?;
        }
        Ok(())
    }

    fn load_import(
        &mut self,
        import: &ImportDecl,
        base_dir: &Path,
        interp: &mut Interpreter,
    ) -> Result<(), InterpError> {
        if is_url(&import.path) {
            return Err(InterpError::Runtime(RuntimeError::new(
                format!("network imports are not yet supported: '{}'", import.path),
                import.span,
            )));
        }

        let resolved = resolve_path(base_dir, &import.path);
        let canonical = fs::canonicalize(&resolved).map_err(|e| {
            InterpError::Runtime(RuntimeError::new(
                format!("cannot resolve imported file '{}': {e}", resolved.display()),
                import.span,
            ))
        })?;

        if self.loaded.contains(&canonical) {
            return Ok(());
        }
        if let Some(pos) = self.stack.iter().position(|p| p == &canonical) {
            let mut chain: Vec<String> =
                self.stack[pos..].iter().map(|p| p.display().to_string()).collect();
            chain.push(canonical.display().to_string());
            return Err(InterpError::Runtime(RuntimeError::new(
                format!("import cycle detected: {}", chain.join(" -> ")),
                import.span,
            )));
        }

        let source = fs::read_to_string(&canonical).map_err(|e| {
            wrap(&canonical, InterpError::Runtime(RuntimeError::new(
                format!("cannot read imported file: {e}"),
                import.span,
            )))
        })?;
        let program = parse_program(&source).map_err(|e| wrap(&canonical, e))?;

        self.stack.push(canonical.clone());
        let child_base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let result: Result<(), InterpError> = (|| {
            self.run_imports(&program, &child_base_dir, interp)?;
            run_stmts(interp, &program.stmts).map_err(|e| wrap(&canonical, e))
        })();
        self.stack.pop();
        result?;

        self.loaded.insert(canonical);
        Ok(())
    }
}

fn parse_program(src: &str) -> Result<Program, InterpError> {
    let tokens = lexer::tokenize(src).map_err(InterpError::Parse)?;
    parser::parse(&tokens).map_err(InterpError::Parse)
}

fn run_stmts(interp: &mut Interpreter, stmts: &[Stmt]) -> Result<(), InterpError> {
    match interp.run(stmts) {
        Ok(()) => Ok(()),
        Err(Flow::Error(e)) => Err(InterpError::Runtime(e)),
        Err(Flow::Return(_)) => Ok(()),
        Err(Flow::Break) | Err(Flow::Continue) => Err(InterpError::Runtime(RuntimeError::new(
            "'break'/'continue' used outside of a loop",
            Span::default(),
        ))),
    }
}

fn resolve_path(base_dir: &Path, import_path: &str) -> PathBuf {
    let p = Path::new(import_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

fn is_url(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

fn wrap(file: &Path, err: InterpError) -> InterpError {
    InterpError::InFile {
        file: file.to_path_buf(),
        source: Box::new(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn resolve_path_relative_joins_base_dir() {
        assert_eq!(resolve_path(Path::new("/a/b"), "c.ksl"), PathBuf::from("/a/b/c.ksl"));
    }

    #[test]
    fn resolve_path_absolute_used_as_is() {
        assert_eq!(resolve_path(Path::new("/a/b"), "/x/y.ksl"), PathBuf::from("/x/y.ksl"));
    }

    #[test]
    fn is_url_detects_http_https_only() {
        assert!(is_url("https://example.com/x.ksl"));
        assert!(is_url("http://example.com/x.ksl"));
        assert!(!is_url("./local.ksl"));
        assert!(!is_url("httpfoo.ksl"));
    }

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh temp directory for a single test, cleaned up on drop. No
    /// `tempfile` crate, per the project's zero-dependency rule.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("ksl_importer_test_{name}_{n}_{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_in(dir: &TempDir, entry_file: &str) -> Result<String, String> {
        let src = fs::read_to_string(dir.path().join(entry_file)).unwrap();
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        let entry_path = dir.path().join(entry_file);
        Importer::new()
            .run_top_level(&src, dir.path(), Some(&entry_path), &mut interp)
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn diamond_import_runs_shared_file_once() {
        let dir = TempDir::new("diamond");
        dir.write("d.ksl", "println(\"d\")\n");
        dir.write("b.ksl", "@file:Import(\"d.ksl\")\nprintln(\"b\")\n");
        dir.write("c.ksl", "@file:Import(\"d.ksl\")\nprintln(\"c\")\n");
        dir.write(
            "a.ksl",
            "@file:Import(\"b.ksl\")\n@file:Import(\"c.ksl\")\nprintln(\"a\")\n",
        );

        let out = run_in(&dir, "a.ksl").unwrap();
        assert_eq!(out, "d\nb\nc\na\n");
    }

    #[test]
    fn cycle_between_imported_files_errors() {
        let dir = TempDir::new("cycle");
        dir.write("b.ksl", "@file:Import(\"c.ksl\")\n");
        dir.write("c.ksl", "@file:Import(\"b.ksl\")\n");
        dir.write("a.ksl", "@file:Import(\"b.ksl\")\n");

        let err = run_in(&dir, "a.ksl").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("import cycle detected"), "{msg}");
        assert!(msg.contains("b.ksl") && msg.contains("c.ksl"), "{msg}");
    }

    #[test]
    fn cycle_back_to_entry_script_errors() {
        let dir = TempDir::new("entry_cycle");
        dir.write("a.ksl", "@file:Import(\"b.ksl\")\n");
        dir.write("b.ksl", "@file:Import(\"a.ksl\")\n");

        let err = run_in(&dir, "a.ksl").unwrap_err();
        assert!(err.to_string().contains("import cycle detected"), "{}", err);
    }

    #[test]
    fn missing_import_file_errors() {
        let dir = TempDir::new("missing");
        dir.write("a.ksl", "@file:Import(\"nope.ksl\")\n");

        let err = run_in(&dir, "a.ksl").unwrap_err();
        assert!(err.to_string().contains("cannot resolve imported file"), "{}", err);
    }

    #[test]
    fn network_import_rejected() {
        let dir = TempDir::new("network");
        dir.write("a.ksl", "@file:Import(\"https://example.com/x.ksl\")\n");

        let err = run_in(&dir, "a.ksl").unwrap_err();
        assert!(err.to_string().contains("network imports are not yet supported"), "{}", err);
    }
}
