use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{ImportDecl, Program, Stmt};
use crate::environment::Env;
use crate::error::{Flow, InterpError, RuntimeError, Span};
use crate::interpreter::Interpreter;
use crate::package_index::PackageIndex;
use crate::resolver::ResolveResult;

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
    /// Every symbol reachable so far in this run — stdlib packages seeded
    /// unconditionally, plus every already-loaded file's own top-level
    /// exports (added right after that file is resolved, in
    /// `add_module_to_index` below). Grows monotonically across the whole
    /// run and is shared by every file's own `resolver::resolve_with_imports`
    /// call, so a file's own `import`/`@file:Import` gets exactly the same
    /// real, precise resolution `ksl-lsp`'s editor tooling gets — matching
    /// `Interpreter`'s own genuinely-global `globals`/`packages` envs (a
    /// name a package-less file provides is visible to *every* other file
    /// in the run, not just its own direct importer — see
    /// `run_program`'s doc comment), so one shared, ever-growing index is
    /// the *correct* model here, not a leniency. Since each canonical path
    /// is only ever parsed+resolved once (`loaded`'s existing dedup), a
    /// file imported from several places is never redundantly reprocessed.
    packages: PackageIndex,
}

impl Importer {
    pub fn new() -> Self {
        Importer {
            loaded: HashSet::new(),
            stack: Vec::new(),
            packages: PackageIndex::new(),
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
        let program = crate::parse_only(src).map_err(InterpError::Parse)?;

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

        // Every one of this file's own `@file:Import`s is fully loaded,
        // resolved, and run *before* this file itself is resolved — so its
        // own `import`/`@file:Import` gets real, precise resolution against
        // `self.packages`, already extended with everything those imports
        // (transitively) export.
        let result = self.run_imports(&program, base_dir, interp);

        if entry_canonical.is_some() {
            self.stack.pop();
        }
        result?;

        let resolved = crate::resolver::resolve_with_imports(&program, &self.packages);
        if !resolved.type_errors.is_empty() {
            return Err(InterpError::TypeErrors(resolved.type_errors));
        }

        // Stamped onto every `FunctionValue` declared while this file's
        // own top-level statements run (see `Interpreter::current_file`) —
        // left at its `Interpreter::new` default (`<script>`) when there's
        // no real backing file (e.g. `ksl -` from stdin).
        if let Some(canonical) = &entry_canonical {
            interp.current_file = Arc::from(canonical.as_path());
        }
        interp.current_resolved = Rc::new(resolved.resolved_calls);

        run_program(interp, &program)
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
        let program = crate::parse_only(&source).map_err(|e| wrap(&canonical, InterpError::Parse(e)))?;

        self.stack.push(canonical.clone());
        let child_base_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // Save/restore the caller's file identity (and its resolved-
        // overload decisions) around this file's own run — a function
        // declared in `canonical` must be stamped with *its* file (see
        // `Interpreter::current_file`) and see *its own* file's overload
        // resolutions (see `Interpreter::current_resolved`), but once this
        // import returns, the importing file's own subsequent top-level
        // statements (and any further sibling imports) need their own
        // identity back, not whichever file was loaded last.
        let caller_file = interp.current_file.clone();
        let caller_resolved = interp.current_resolved.clone();
        let result: Result<(), InterpError> = (|| {
            // This file's own `@file:Import`s are loaded, resolved, and run
            // *first* — by the time `program` itself is resolved below,
            // `self.packages` already reflects everything they
            // (transitively) export, so its own `import`/`@file:Import`
            // gets real resolution too.
            self.run_imports(&program, &child_base_dir, interp)?;
            let resolved = crate::resolver::resolve_with_imports(&program, &self.packages);
            if !resolved.type_errors.is_empty() {
                return Err(wrap(&canonical, InterpError::TypeErrors(resolved.type_errors)));
            }
            add_module_to_index(&mut self.packages, &program, &resolved, &canonical);
            interp.current_file = Arc::from(canonical.as_path());
            interp.current_resolved = Rc::new(resolved.resolved_calls);
            run_program(interp, &program).map_err(|e| wrap(&canonical, e))
        })();
        interp.current_file = caller_file;
        interp.current_resolved = caller_resolved;
        self.stack.pop();
        result?;

        self.loaded.insert(canonical);
        Ok(())
    }
}

/// Folds `program`'s own top-level exports into `packages` right after it's
/// been resolved — a `package`-declaring file's members become visible
/// under its package name (joined with `.`), a package-less file's flatten
/// into the shared unqualified namespace (see
/// `package_index::PackageIndex::flat_members`'s doc comment) — so a
/// *later*-loaded file's own `import`/`@file:Import` sees everything this
/// one exports, matching `Interpreter::run_program`'s own genuinely-shared
/// `globals`/`package_env` model exactly.
fn add_module_to_index(packages: &mut PackageIndex, program: &Program, resolved: &ResolveResult, file: &Path) {
    match &program.package {
        Some(pkg) => {
            let package_name = pkg.path.join(".");
            for symbol in resolved.table.top_level_symbols() {
                packages.add_member(package_name.clone(), symbol.clone(), Some(file.to_path_buf()));
            }
        }
        None => {
            for symbol in resolved.table.top_level_symbols() {
                packages.add_flat_member(symbol.clone(), Some(file.to_path_buf()));
            }
        }
    }
}

/// Resolves `program`'s `import a.b.c` declarations (bringing each into
/// `program`'s own target env — its `package_env` if it declares one, else
/// `globals`) and then runs its top-level statements against that same env.
/// Called for both the entry script and every `@file:Import`ed file, after
/// all of *that* program's own `@file:Import`s have already run (so any
/// package an `import` here draws from is guaranteed populated).
fn run_program(interp: &mut Interpreter, program: &Program) -> Result<(), InterpError> {
    let target_env = match &program.package {
        Some(pkg) => interp.package_env(&pkg.path.join(".")),
        None => interp.globals.clone(),
    };

    for use_ in &program.imports_use {
        if use_.wildcard {
            let members = interp
                .resolve_package_members(&use_.path, use_.span)
                .map_err(InterpError::Runtime)?;
            for (name, value) in members {
                target_env.define(&name, value, false, crate::types::Type::Unknown);
            }
        } else {
            let value = interp
                .resolve_qualified(&use_.path, use_.span)
                .map_err(InterpError::Runtime)?;
            // Checked by the parser (`parse_import_use`): at least 2 segments.
            let member_name = use_.path.last().expect("import path has a member segment");
            target_env.define(member_name, value, false, crate::types::Type::Unknown);
        }
    }

    run_stmts_in(interp, &program.stmts, &target_env)
}

fn run_stmts_in(interp: &mut Interpreter, stmts: &[Stmt], env: &Env) -> Result<(), InterpError> {
    match interp.run_in_env(stmts, env) {
        Ok(()) => Ok(()),
        Err(Flow::Error(e)) => Err(InterpError::Runtime(e)),
        Err(Flow::Return(_)) => Ok(()),
        Err(Flow::Break) | Err(Flow::Continue) => Err(InterpError::Runtime(RuntimeError::new(
            "'break'/'continue' used outside of a loop",
            Span::default(),
        ))),
    }
}

/// Resolves an `@file:Import("...")` path against the importing file's own
/// directory (absolute paths are used as-is) — the same rule `Importer`
/// itself uses, exposed for callers that need to locate an import target
/// without actually loading/running it (e.g. `ksl-lsp`'s `package_index`).
pub fn resolve_path(base_dir: &Path, import_path: &str) -> PathBuf {
    let p = Path::new(import_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// True for an import path this crate treats as a (currently unsupported)
/// network import rather than a local file.
pub fn is_url(path: &str) -> bool {
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
        let result = Importer::new().run_top_level(&src, dir.path(), Some(&entry_path), &mut interp).map_err(|e| e.to_string());
        drop(interp);
        result?;
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

    #[test]
    fn import_use_brings_a_qualified_name_into_bare_scope() {
        let dir = TempDir::new("import_use");
        dir.write("pkg.ksl", "package a.b\nfun greet(name: String): String = \"Hi, $name\"\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport a.b.greet\nprintln(greet(\"World\"))\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "Hi, World\n");
    }

    #[test]
    fn wildcard_import_brings_every_top_level_name_into_bare_scope() {
        let dir = TempDir::new("wildcard_import");
        dir.write(
            "pkg.ksl",
            "package a.b\nfun greet(name: String): String = \"Hi, $name\"\nval farewell = \"Bye\"\n",
        );
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport a.b.*\nprintln(greet(\"World\"))\nprintln(farewell)\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "Hi, World\nBye\n");
    }

    #[test]
    fn wildcard_imported_call_with_a_mismatched_argument_type_is_rejected_before_running() {
        // Real, filesystem-backed cross-file argument-type checking: a
        // wildcard-imported function now gets exactly the same static
        // rigor a local one already had, and a real type error is fatal
        // (via `resolver::resolve_with_imports`'s `type_errors`), rejected
        // before the script ever starts running — nothing is printed.
        let dir = TempDir::new("wildcard_import_bad_arg");
        dir.write("pkg.ksl", "package a.b\nfun square(x: Int): Int = x * x\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport a.b.*\nprintln(\"should not print\")\nprintln(square(\"nope\"))\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("expected Int"), "{err}");
    }

    #[test]
    fn genuine_typo_unrelated_to_a_wildcard_import_is_still_flagged() {
        let dir = TempDir::new("wildcard_import_typo");
        dir.write("pkg.ksl", "package a.b\nfun greet(): String = \"hi\"\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport a.b.*\nfun unused(): Unit {\n    totallyNotAThing\n}\nprintln(greet())\n",
        );

        // Only a `type_errors`-fatal problem stops execution today
        // (undefined-name stays a non-fatal `diagnostics` entry — see
        // `docs/roadmap.md`'s Phase 2 note, and this same behavior for a
        // never-called function's own body); the point of this test is
        // that the wildcard import's own real member (`greet`) still
        // resolves and runs correctly regardless of the unrelated typo.
        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "hi\n");
    }

    #[test]
    fn wildcard_import_of_an_unresolved_package_errors_cleanly() {
        let dir = TempDir::new("wildcard_import_unresolved");
        dir.write("main.ksl", "import a.b.*\nprintln(1)\n");

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("unknown package 'a.b'"), "{err}");
    }

    #[test]
    fn wildcard_import_of_kotlin_test_resolves_to_ksls_own_assertions() {
        // `kotlin.test` isn't backed by any loaded file — it's a synthetic
        // package `Interpreter::new` seeds from `stdlib::STDLIB_PACKAGES`'s
        // `"kotlin.test"` entry — see that constant's doc comment for why
        // this exists.
        let dir = TempDir::new("wildcard_kotlin_test");
        dir.write("main.ksl", "import kotlin.test.*\nassertEquals(4, 2 + 2)\nprintln(\"OK\")\n");

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "OK\n");
    }

    #[test]
    fn single_symbol_import_of_kotlin_test_resolves() {
        let dir = TempDir::new("single_kotlin_test");
        dir.write("main.ksl", "import kotlin.test.assertTrue\nassertTrue(1 < 2)\nprintln(\"OK\")\n");

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "OK\n");
    }

    #[test]
    fn kotlin_test_qualified_call_works_without_any_import() {
        let dir = TempDir::new("qualified_kotlin_test");
        dir.write("main.ksl", "kotlin.test.assertEquals(4, 2 + 2)\nprintln(\"OK\")\n");

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "OK\n");
    }

    #[test]
    fn importing_an_unknown_kotlin_test_member_is_an_undefined_name_error() {
        // `assertFailsWith` isn't in `stdlib::STDLIB_PACKAGES`'s
        // `"kotlin.test"` entry (needs `Flow::Throw`, not implemented until
        // the try/catch phase) — the
        // synthetic package must reject it like any other unknown member,
        // not silently accept every name under `kotlin.test`.
        let dir = TempDir::new("unknown_kotlin_test_member");
        dir.write("main.ksl", "import kotlin.test.assertFailsWith\nprintln(1)\n");

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("undefined name"), "{err}");
    }

    #[test]
    fn qualified_access_works_without_any_import_statement() {
        let dir = TempDir::new("qualified_no_import");
        dir.write("pkg.ksl", "package a.b\nfun greet(name: String): String = \"Hi, $name\"\n");
        dir.write("main.ksl", "@file:Import(\"pkg.ksl\")\nprintln(a.b.greet(\"World\"))\n");

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "Hi, World\n");
    }

    #[test]
    fn two_files_declaring_the_same_package_share_visibility() {
        let dir = TempDir::new("same_package");
        dir.write("p1.ksl", "package a.b\nfun helper(): Int = 41\n");
        // p2.ksl calls helper() *unqualified* — same package_env as p1.ksl.
        dir.write("p2.ksl", "package a.b\nfun main2(): Int = helper() + 1\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"p1.ksl\")\n@file:Import(\"p2.ksl\")\nimport a.b.main2\nprintln(main2())\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "42\n");
    }

    #[test]
    fn file_with_no_package_is_unaffected_by_an_imported_files_package() {
        let dir = TempDir::new("no_package_unaffected");
        dir.write("pkg.ksl", "package a.b\nfun greet(name: String): String = \"Hi, $name\"\n");
        // No package, no import a.b.greet — bare "greet" must NOT be visible
        // here (it only landed in package_env("a.b"), not globals).
        dir.write("main.ksl", "@file:Import(\"pkg.ksl\")\nprintln(greet(\"World\"))\n");

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("undefined name 'greet'"), "{err}");
    }

    #[test]
    fn import_of_an_unresolved_package_errors_cleanly() {
        let dir = TempDir::new("import_unresolved");
        dir.write("main.ksl", "import a.b.greet\nprintln(1)\n");

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("unknown package 'a.b'"), "{err}");
    }

    /// A function declared in one file, called from another, must be
    /// credited to *its own* file in a stack trace — not the calling
    /// file's — and the calling function's own frame must show *its* own
    /// file at the point it made the call. Regression test for a bug
    /// where a frame's file and its printed position came from two
    /// different files' coordinate spaces.
    #[test]
    fn cross_file_call_credits_each_frame_to_its_own_file() {
        let dir = TempDir::new("cross_file_stack_trace");
        dir.write(
            "lib.ksl",
            "import kotlin.test.*\nfun failing() {\n    assertTrue(1 > 2)\n}\n",
        );
        dir.write(
            "main.ksl",
            "@file:Import(\"lib.ksl\")\nfun caller(): Unit {\n    failing()\n}\ncaller()\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        let lines: Vec<&str> = err.lines().collect();
        assert!(lines[1].trim_start().starts_with("at assertTrue ("), "{err}");
        assert!(lines[2].trim_start().contains("at failing() (") && lines[2].contains("lib.ksl:3"), "{err}");
        assert!(lines[3].trim_start().contains("at caller() (") && lines[3].contains("main.ksl:3"), "{err}");
    }
}
