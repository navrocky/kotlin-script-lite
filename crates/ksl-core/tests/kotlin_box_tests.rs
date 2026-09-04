use std::fs;
use std::path::{Path, PathBuf};

/// Runs every vendored, hand-verified-passing Kotlin compiler "box test"
/// under `tests/kotlin-box/` through ksl and checks it actually behaves
/// like real Kotlin — see `tests/kotlin-box/README.md` and
/// `docs/roadmap.md`'s Phase 0.5 for how these were selected and vendored
/// (via `scripts/fetch-kotlin-box-tests.sh`).
///
/// Each file is real, unmodified upstream Kotlin (`fun box(): String`,
/// returning the literal `"OK"` on success — JetBrains's own compiler test
/// convention), including its own `import kotlin.test.*`/`import
/// kotlin.test.assertX` line where it has one — `kotlin.test` is a real,
/// resolvable package to ksl (see `Interpreter::new`'s synthetic
/// `"kotlin.test"` package env and `stdlib::KOTLIN_TEST_MEMBERS`), so no
/// text rewriting is needed for that anymore. The only adaptation applied
/// here, never written back to the vendored file: `println(box())` is
/// appended in memory, and stdout is asserted to be exactly `"OK\n"` — the
/// ksl-side equivalent of "the compiler test passed".
///
/// If this test ever fails after a `scripts/fetch-kotlin-box-tests.sh` run
/// (a file's content changed upstream at the same pinned SHA — shouldn't
/// happen — or a file was promoted here before it actually passes), fix
/// the vendored file list or move the file back to `kotlin-box-pending/`,
/// don't hand-edit the vendored file itself.
///
/// Vendored files carry a `.ksl` extension, not upstream's own `.kt` — same
/// content, just renamed by the fetch script on the way in, so an editor's
/// Kotlin plugin doesn't pick them up as real project files and flag every
/// ksl-unsupported construct as an error.
#[test]
fn vendored_box_tests_pass() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/kotlin-box");
    let mut files = Vec::new();
    collect_ksl_files(&dir, &mut files);
    files.sort();

    assert!(!files.is_empty(), "no vendored .ksl files found in {}", dir.display());

    let mut failures = Vec::new();
    for path in files {
        let rel = path.strip_prefix(&dir).unwrap().display().to_string();
        let source = fs::read_to_string(&path).unwrap();
        let adapted = append_box_driver(&source);

        let mut out = Vec::new();
        let base_dir = path.parent().unwrap();
        let result = ksl_core::run_source(&adapted, &mut out, base_dir, Some(&path));
        let actual = String::from_utf8(out).unwrap();

        match result {
            Ok(()) if actual == "OK\n" => {}
            Ok(()) => failures.push(format!("{rel}: expected \"OK\\n\", got {actual:?}")),
            Err(e) => failures.push(format!("{rel}: script failed: {e}\n--- partial output ---\n{actual}")),
        }
    }

    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

/// Companion to `vendored_box_tests_pass`: walks `tests/kotlin-box-pending/`
/// and only checks that each file *parses* (with the same
/// `append_box_driver` trailer) — a much weaker bar, kept deliberately non-fatal (no
/// `assert!`, just a printed summary) so this file stays green as a
/// growing, low-maintenance corpus rather than a wall of expected
/// failures. Run `cargo test --workspace -- --nocapture` to see the
/// summary; promote a file to `kotlin-box/` (moving it, not copying) once
/// it both parses *and* actually prints `"OK\n"`.
#[test]
fn pending_box_tests_parse_status() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/kotlin-box-pending");
    let mut files = Vec::new();
    collect_ksl_files(&dir, &mut files);
    files.sort();

    for path in &files {
        let rel = path.strip_prefix(&dir).unwrap().display().to_string();
        let source = fs::read_to_string(path).unwrap();
        let adapted = append_box_driver(&source);
        match ksl_core::parse_only(&adapted) {
            Ok(_) => println!("PARSES  {rel}"),
            Err(e) => println!("BLOCKED {rel}: {} at {}:{}", e.message, e.span.line, e.span.col),
        }
    }
}

fn collect_ksl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_ksl_files(&path, out);
        } else if path.extension().map(|e| e == "ksl").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// The one adaptation every vendored file gets before running — appends a
/// driver call in memory, never mutates anything on disk. See this file's
/// own top doc comment for why nothing else is needed.
fn append_box_driver(source: &str) -> String {
    format!("{source}\nprintln(box())\n")
}
