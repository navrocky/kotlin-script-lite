use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{ImportDecl, Program, Stmt, Visibility};
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

        run_program(interp, &program, &self.packages)
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
            run_program(interp, &program, &self.packages).map_err(|e| wrap(&canonical, e))
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
            // A `private` top-level declaration is never indexed — see
            // `package_index::index_program`'s identical filter (this is
            // the runtime-path mirror of that same static/LSP-path gate).
            for symbol in resolved.table.top_level_symbols().filter(|s| s.visibility == Visibility::Public) {
                let class_members = crate::package_index::class_members_of(resolved, symbol);
                packages.add_member(package_name.clone(), symbol.clone(), Some(file.to_path_buf()), class_members);
            }
        }
        None => {
            for symbol in resolved.table.top_level_symbols().filter(|s| s.visibility == Visibility::Public) {
                let class_members = crate::package_index::class_members_of(resolved, symbol);
                packages.add_flat_member(symbol.clone(), Some(file.to_path_buf()), class_members);
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
///
/// `packages` is the same, already-`private`-filtered `PackageIndex` the
/// static resolver just checked this same `program` against (see
/// `index_program`'s/`add_module_to_index`'s own `Visibility::Public`
/// filter) — consulted here too, purely to decide *which* runtime bindings
/// this loop is allowed to materialize. This is the actual enforcement
/// point for top-level `private` at execution time: `Interpreter::
/// resolve_qualified`/`resolve_package_members` read straight off `Interpreter
/// ::packages`, a genuinely flat runtime `Env` per package with no
/// visibility concept of its own at all (every top-level declaration binds
/// there unconditionally the moment its own file's top-level statements
/// run, regardless of any modifier) — without this check, a `private`
/// symbol would still be reachable via `import` from another file the
/// moment *any* script in the same run had already executed its declaring
/// file, defeating the whole point of the resolver-side filter.
fn run_program(interp: &mut Interpreter, program: &Program, packages: &PackageIndex) -> Result<(), InterpError> {
    let target_env = match &program.package {
        Some(pkg) => interp.package_env(&pkg.path.join(".")),
        None => interp.globals.clone(),
    };

    for use_ in &program.imports_use {
        if use_.wildcard {
            let package_name = use_.path.join(".");
            let public_names: HashSet<&str> =
                packages.members_of(&package_name).iter().map(|m| m.symbol.name.as_str()).collect();
            let members = interp
                .resolve_package_members(&use_.path, use_.span)
                .map_err(InterpError::Runtime)?;
            for (name, value) in members {
                if public_names.contains(name.as_str()) {
                    target_env.define(&name, value, false, crate::types::Type::Unknown);
                }
            }
        } else {
            // Checked by the parser (`parse_import_use`): at least 2 segments.
            let (package_segments, member) = use_.path.split_at(use_.path.len() - 1);
            let member_name = &member[0];
            let package_name = package_segments.join(".");
            // Mirrors `Interpreter::resolve_qualified`'s own two error
            // messages exactly (a genuinely unknown package vs. a known
            // package with no such — or a `private` — member), just
            // checked against the static, already-filtered index instead
            // of the runtime one, so a `private` member reports the same
            // "undefined name" a truly nonexistent one would, not a
            // different message that would leak whether it exists at all.
            if packages.has_package(&package_name) {
                if packages.find(&package_name, member_name).is_none() {
                    return Err(InterpError::Runtime(RuntimeError::new(
                        format!("undefined name '{}'", use_.path.join(".")),
                        use_.span,
                    )));
                }
            } else {
                return Err(InterpError::Runtime(RuntimeError::new(
                    format!("unknown package '{package_name}'"),
                    use_.span,
                )));
            }
            let value = interp
                .resolve_qualified(&use_.path, use_.span)
                .map_err(InterpError::Runtime)?;
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
        // A `throw` no `try`/`catch` anywhere caught, all the way up to the
        // top of the program — real Kotlin's own uncaught-exception
        // behavior, just reported through `InterpError` rather than a
        // native stack trace. `value_to_display_string` dispatches through
        // the thrown value's own `toString()` (`Exception`'s own override,
        // see `stdlib::exceptions`, prints `"ClassName: message"`); a
        // secondary error from that call itself (defensive only — nothing
        // in this codebase throws a value whose `toString()` can fail)
        // falls back to `Value`'s plain `Display`.
        Err(Flow::Throw(v)) => {
            let message = interp.value_to_display_string(&v, Span::default()).unwrap_or_else(|_| v.to_string());
            Err(InterpError::Runtime(RuntimeError::new(format!("uncaught exception: {message}"), Span::default())))
        }
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

    /// A `class` declared in one file and imported via `@file:Import` must
    /// resolve `.field`/`.method()` on an instance with real types — the
    /// original bug this cross-file-classes phase fixes (`PackageMember`
    /// never carried a class's shape before, so this used to fail with
    /// "has no field/method" diagnostics even though construction worked).
    #[test]
    fn cross_file_class_field_and_method_resolve_with_real_types() {
        let dir = TempDir::new("cross_class_flat");
        dir.write(
            "point.ksl",
            "class Point(val x: Int, val y: Int) {\n    fun distanceSquared(other: Point): Int {\n        val dx = x - other.x\n        val dy = y - other.y\n        return dx * dx + dy * dy\n    }\n}\n",
        );
        dir.write(
            "main.ksl",
            "@file:Import(\"point.ksl\")\nval p = Point(3, 4)\nprintln(p.x)\nprintln(p.y)\nprintln(p.distanceSquared(Point(0, 0)))\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "3\n4\n25\n");
    }

    /// Same as above, but through a real `package`-declaring file plus
    /// `import` (not a bare `@file:Import`) — both cross-file paths already
    /// behaved identically for plain functions, so a class must not regress
    /// that parity.
    #[test]
    fn cross_file_class_field_and_method_resolve_via_package_import() {
        let dir = TempDir::new("cross_class_package");
        dir.write(
            "point.ksl",
            "package geo\nclass Point(val x: Int, val y: Int) {\n    fun sum(): Int = x + y\n}\n",
        );
        dir.write(
            "main.ksl",
            "@file:Import(\"point.ksl\")\nimport geo.Point\nval p = Point(3, 4)\nprintln(p.x)\nprintln(p.sum())\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "3\n7\n");
    }

    /// Named arguments to an imported class's primary constructor must be
    /// checked by real parameter name, not silently flattened to
    /// positional order.
    #[test]
    fn named_arguments_to_a_cross_file_class_constructor_are_checked_by_name() {
        let dir = TempDir::new("cross_class_named_args");
        dir.write("point.ksl", "class Point(val x: Int, val y: Int)\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"point.ksl\")\nval p = Point(y = 10, x = 5)\nprintln(p.x)\nprintln(p.y)\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "5\n10\n");
    }

    /// The same named-argument fix is not class-specific — an ordinary
    /// cross-file function's real parameter names must be honored too.
    #[test]
    fn named_arguments_to_an_ordinary_cross_file_function_are_checked_by_name() {
        let dir = TempDir::new("cross_fun_named_args");
        dir.write("greeter.ksl", "fun greet(greeting: String, name: String): String = \"$greeting, $name!\"\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"greeter.ksl\")\nprintln(greet(name = \"World\", greeting = \"Hi\"))\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "Hi, World!\n");
    }

    /// A cross-file class method missing a required argument must be
    /// caught statically, before any of the calling file's own statements
    /// run — not just at runtime, after everything up to the bad call has
    /// already executed. The exact shape of a real user-reported bug: the
    /// resolver's `check_call_signature` never validated arity at all
    /// (locally or cross-file) until this fix. `err` starting with
    /// `"error:"` (a static `TypeErrors`) rather than `"runtime error:"`
    /// (a `RuntimeError` reached by actually executing `bind_params`) is
    /// what proves this ran statically — see `error::InterpError`'s
    /// `Display` impl for the two distinct prefixes.
    #[test]
    fn cross_file_class_method_missing_a_required_argument_is_caught_statically() {
        let dir = TempDir::new("cross_class_missing_arg");
        dir.write(
            "point.ksl",
            "class Point(val x: Int, val y: Int) {\n    fun distanceSquared(other: Point): Int {\n        val dx = x - other.x\n        val dy = y - other.y\n        return dx * dx + dy * dy\n    }\n}\n",
        );
        dir.write(
            "main.ksl",
            "@file:Import(\"point.ksl\")\nval p = Point(3, 4)\np.distanceSquared()\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.starts_with("error:"), "{err}");
        assert!(err.contains("missing argument 'other'"), "{err}");
    }

    /// Same as above, for an ordinary (non-class) cross-file function.
    #[test]
    fn cross_file_function_missing_a_required_argument_is_caught_statically() {
        let dir = TempDir::new("cross_fun_missing_arg");
        dir.write("greeter.ksl", "fun greet(name: String): String = \"Hi, $name\"\n");
        dir.write("main.ksl", "@file:Import(\"greeter.ksl\")\nprintln(greet())\n");

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.starts_with("error:"), "{err}");
        assert!(err.contains("missing argument 'name'"), "{err}");
    }

    /// A class method with no explicit return-type annotation must keep its
    /// real, inferred return type once exported across a file boundary —
    /// the prerequisite fix for this phase (`walk_class_method` used to
    /// patch only `class_info`, not the method's own `Symbol`, leaving
    /// `class_members()` reporting `Unknown` for this exact case).
    #[test]
    fn cross_file_classs_inferred_method_return_type_survives_export() {
        let dir = TempDir::new("cross_class_inferred_return");
        dir.write("dog.ksl", "class Dog {\n    fun bark() = \"woof\"\n}\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"dog.ksl\")\nval sound: String = Dog().bark()\nprintln(sound)\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "woof\n");
    }

    /// Assigning the wrong type to an imported class's field is a real
    /// static error, not a silent no-op.
    #[test]
    fn wrong_type_assigned_to_a_cross_file_class_field_is_rejected() {
        let dir = TempDir::new("cross_class_field_wrong_type");
        dir.write("point.ksl", "class Point(var x: Int, val y: Int)\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"point.ksl\")\nval p = Point(3, 4)\np.x = \"nope\"\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("cannot assign String to field 'x' of type Int"), "{err}");
    }

    /// Reassigning an imported class's `val` field is rejected, not
    /// silently accepted.
    #[test]
    fn reassigning_a_cross_file_class_val_field_is_rejected() {
        let dir = TempDir::new("cross_class_val_field_reassign");
        dir.write("point.ksl", "class Point(val x: Int, val y: Int)\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"point.ksl\")\nval p = Point(3, 4)\np.y = 9\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("cannot reassign val 'y'"), "{err}");
    }

    /// Reassigning a cross-file `val` (a bare name, not a field) must also
    /// be rejected — `StmtKind::Assign`'s cross-file fallback used to skip
    /// mutability/type checking entirely for this case.
    #[test]
    fn reassigning_a_cross_file_val_bare_name_is_rejected() {
        let dir = TempDir::new("cross_val_bare_name_reassign");
        dir.write("consts.ksl", "val greeting = \"Hi\"\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"consts.ksl\")\ngreeting = \"Bye\"\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("cannot reassign val 'greeting'"), "{err}");
    }

    /// Assigning an incompatible type to a cross-file `var` (a bare name)
    /// must also be rejected, same fallback as the previous test.
    #[test]
    fn wrong_type_assigned_to_a_cross_file_var_bare_name_is_rejected() {
        let dir = TempDir::new("cross_var_bare_name_wrong_type");
        dir.write("consts.ksl", "var count = 0\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"consts.ksl\")\ncount = \"nope\"\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("cannot assign String to var 'count' of type Int"), "{err}");
    }

    /// A `private` top-level `fun`, imported by explicit name from another
    /// file, fails the same way any genuinely undefined name would — the
    /// resolver's own static check produces a non-fatal `diagnostics`
    /// entry (see `resolve_with_imports`'s doc comment on why an
    /// unresolved import is lenient there), so this must be caught at
    /// runtime, in `run_program`'s own `imports_use` handling, or the
    /// symbol would still be reachable via `Interpreter::resolve_qualified`
    /// once `pkg.ksl` has actually run its own top-level statements.
    #[test]
    fn importing_a_private_top_level_fun_by_explicit_name_fails_like_any_undefined_name() {
        let dir = TempDir::new("private_named_import");
        dir.write("pkg.ksl", "package mylib\nprivate fun secret(): Int = 42\nfun exposed(): Int = secret()\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport mylib.secret\nprintln(secret())\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("undefined name 'mylib.secret'"), "{err}");
    }

    /// The public sibling in the same file imports and runs normally —
    /// regression guard alongside the previous test.
    #[test]
    fn importing_a_public_top_level_fun_by_explicit_name_still_works() {
        let dir = TempDir::new("public_named_import");
        dir.write("pkg.ksl", "package mylib\nprivate fun secret(): Int = 42\nfun exposed(): Int = secret()\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport mylib.exposed\nprintln(exposed())\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "42\n");
    }

    /// A wildcard import of a file mixing `public`/`private` top-level
    /// declarations only brings in the public ones — the private one stays
    /// unreachable even though its declaring file has definitely already
    /// run (and so definitely already bound it into the runtime package
    /// env `Interpreter::resolve_package_members` reads from).
    #[test]
    fn wildcard_importing_a_mix_of_public_and_private_declarations_brings_in_only_the_public_ones() {
        let dir = TempDir::new("wildcard_mixed_visibility");
        dir.write("pkg.ksl", "package mylib\nprivate fun secret(): Int = 42\nfun exposed(): Int = secret()\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport mylib.*\nprintln(exposed())\nprintln(secret())\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("undefined name 'secret'"), "{err}");
    }

    /// Cross-file class-member visibility: a `private` field is unreachable
    /// from the *importing* file even though the class itself is public and
    /// successfully constructed there — but a public method of that same
    /// class (declared in the same file the class is, so it's genuinely
    /// "inside the class") can still read the private field internally.
    #[test]
    fn a_private_field_on_a_cross_file_class_is_rejected_but_its_own_public_method_still_works() {
        let dir = TempDir::new("cross_file_private_field");
        dir.write(
            "pkg.ksl",
            "class Foo(private val secret: Int) {\n    fun reveal(): Int = secret\n}\n",
        );
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nval f = Foo(7)\nprintln(f.reveal())\nprintln(f.secret)\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("'secret' is private in class 'Foo'"), "{err}");
    }

    /// Same shape as `importing_a_private_top_level_fun_by_explicit_name_
    /// fails_like_any_undefined_name`, for a top-level `val` instead of a
    /// `fun` — regression coverage for the bug where `private`/`public`
    /// wasn't even parseable in front of `val`/`var` at all.
    #[test]
    fn importing_a_private_top_level_val_by_explicit_name_fails_like_any_undefined_name() {
        let dir = TempDir::new("private_named_val_import");
        dir.write("pkg.ksl", "package mylib\nprivate val secret = 42\nval exposed = 7\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport mylib.secret\nprintln(secret)\n",
        );

        let err = run_in(&dir, "main.ksl").unwrap_err();
        assert!(err.contains("undefined name 'mylib.secret'"), "{err}");
    }

    #[test]
    fn importing_a_public_top_level_val_by_explicit_name_still_works() {
        let dir = TempDir::new("public_named_val_import");
        dir.write("pkg.ksl", "package mylib\nprivate val secret = 42\nval exposed = 7\n");
        dir.write(
            "main.ksl",
            "@file:Import(\"pkg.ksl\")\nimport mylib.exposed\nprintln(exposed)\n",
        );

        assert_eq!(run_in(&dir, "main.ksl").unwrap(), "7\n");
    }
}
