use std::fs;
use std::path::Path;

/// Runs every `tests/examples/*.ksl` script through `run_source` and diffs
/// its stdout against the matching `*.out` fixture.
#[test]
fn examples_match_expected_output() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples");
    let mut scripts: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().map(|e| e == "ksl").unwrap_or(false))
        .collect();
    scripts.sort();

    assert!(!scripts.is_empty(), "no .ksl example scripts found in {}", dir.display());

    let mut failures = Vec::new();
    for script_path in scripts {
        let expected_path = script_path.with_extension("out");
        let source = fs::read_to_string(&script_path).unwrap();
        let expected = fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", expected_path.display()));

        let mut out = Vec::new();
        let base_dir = script_path.parent().unwrap();
        let result = ksl_core::run_source(&source, &mut out, base_dir, Some(&script_path));
        let actual = String::from_utf8(out).unwrap();

        let name = script_path.file_name().unwrap().to_string_lossy().to_string();
        match result {
            Ok(()) => {
                if actual != expected {
                    failures.push(format!(
                        "{name}: output mismatch\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
                    ));
                }
            }
            Err(e) => failures.push(format!("{name}: script failed: {e}\n--- partial output ---\n{actual}")),
        }
    }

    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
