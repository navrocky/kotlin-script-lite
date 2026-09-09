use std::fs;
use std::io::IsTerminal;
use std::path::Path;

/// Runs every entry script in `tests/demos/` through `ksl_core::run_source`
/// and asserts only that it doesn't error — **not** a golden-output test
/// like `run_examples.rs` (these are interactive/real-time scripts, with
/// no fixed expected stdout to diff against; see `tests/demos/README.md`).
/// `snake.ksl` itself checks `isTerminal()` and returns early with a
/// friendly message instead of erroring when there's no real terminal —
/// exactly the path this test exercises — so a real regression in
/// parsing, type-checking, or that early bail-out itself still fails this
/// test, even though the actual interactive gameplay can only be verified
/// by a human (`tests/demos/README.md`'s own "What to verify manually").
///
/// Skips itself entirely if stdin genuinely *is* a real terminal when this
/// runs (an interactive shell, not CI) — same guard `stdlib::terminal`'s
/// own unit tests use, and for the same reason: `isTerminal()`/
/// `enableRawMode()` read the real process-wide stdin/stdout file
/// descriptors directly, entirely independent of the `Vec<u8>` sink this
/// test gives `run_source` for captured output, so `snake.ksl`'s own
/// internal `isTerminal()` check would see a real terminal here too and
/// actually start the (non-terminating-until-a-wall-hit) game loop against
/// it — never something an automated test should risk doing.
#[test]
fn demo_scripts_parse_and_run_cleanly() {
    if std::io::stdin().is_terminal() {
        return;
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/demos");
    // `tui.ksl` is a library, never run directly (see its own header
    // comment) — only real entry scripts belong in this list.
    let entry_scripts = ["snake.ksl"];
    for name in entry_scripts {
        let path = dir.join(name);
        let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let mut out = Vec::new();
        let result = ksl_core::run_source(&source, &mut out, &dir, Some(&path));
        if let Err(e) = result {
            panic!("{name} failed: {e}\n--- partial output ---\n{}", String::from_utf8_lossy(&out));
        }
    }
}
