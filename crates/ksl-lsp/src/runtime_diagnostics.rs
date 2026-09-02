use std::sync::mpsc;
use std::time::Duration;

use ksl_core::error::InterpError;
use tower_lsp::lsp_types::{Diagnostic, Url};

use crate::diagnostics::{new_diagnostic, span_to_range};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Diagnostics for one file produced by actually running the script:
/// `uri` names the file the diagnostics apply to (the entry document, or an
/// imported file the error was attributed to via `InterpError::InFile`).
pub struct RuntimeDiagnostics {
    pub uri: Url,
    pub diagnostics: Vec<Diagnostic>,
}

/// Runs `text` (the full script: imports + top-level code) to find runtime
/// errors, on a dedicated OS thread with a timeout.
///
/// `ksl_core::Interpreter` is `Rc`-based and non-`Send` by design (see
/// AGENTS.md), so it cannot be awaited inside the async LSP runtime — only
/// the `Result` crosses the thread boundary. This is deliberately
/// best-effort: on timeout (e.g. `while(true){}`) the thread is detached,
/// not cooperatively cancelled, and a single explanatory diagnostic is
/// returned instead of hanging the server. Called only on `didSave`, never
/// on every keystroke, since it executes the script (side effects, no
/// cancellation).
pub fn run_diagnostics(entry_uri: Url, text: String) -> RuntimeDiagnostics {
    run_diagnostics_with_timeout(entry_uri, text, TIMEOUT)
}

fn run_diagnostics_with_timeout(entry_uri: Url, text: String, timeout: Duration) -> RuntimeDiagnostics {
    let entry_path = entry_uri.to_file_path().ok();
    let base_dir = entry_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = std::io::sink();
        let result = ksl_core::run_source(&text, &mut sink, &base_dir, entry_path.as_deref());
        // The receiver may already be gone if we timed out; that's fine.
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => RuntimeDiagnostics { uri: entry_uri, diagnostics: Vec::new() },
        Ok(Err(e)) => error_to_diagnostics(entry_uri, e),
        Err(_) => RuntimeDiagnostics {
            uri: entry_uri,
            diagnostics: vec![timeout_diagnostic(timeout)],
        },
    }
}

/// `InterpError::InFile` wraps errors that happened while running an
/// imported file, naming which file. Unwrap recursively so the diagnostic
/// lands on the file that actually caused it, not always the entry document.
fn error_to_diagnostics(current_uri: Url, err: InterpError) -> RuntimeDiagnostics {
    match err {
        InterpError::InFile { file, source } => {
            let uri = Url::from_file_path(&file).unwrap_or(current_uri);
            error_to_diagnostics(uri, *source)
        }
        InterpError::Parse(e) => RuntimeDiagnostics {
            uri: current_uri,
            diagnostics: vec![new_diagnostic(span_to_range(e.span), e.message)],
        },
        InterpError::Runtime(e) => RuntimeDiagnostics {
            uri: current_uri,
            diagnostics: vec![new_diagnostic(span_to_range(e.span), e.message)],
        },
    }
}

fn timeout_diagnostic(timeout: Duration) -> Diagnostic {
    use tower_lsp::lsp_types::{Position, Range};
    new_diagnostic(
        Range::new(Position::new(0, 0), Position::new(0, 1)),
        format!(
            "execution did not finish within {:.1}s (possibly an infinite loop) \u{2014} abandoned; \
             diagnostics from running this script are best-effort and were not completed",
            timeout.as_secs_f64()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Writes `text` to a fresh temp file and returns its `file://` URI —
    /// `run_diagnostics` resolves `@file:Import`s and cycle-detection
    /// relative to the entry path's real location on disk, so tests need a
    /// real file, not just an in-memory string.
    fn temp_ksl_file(text: &str) -> (Url, PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "ksl_lsp_runtime_diag_test_{n}_{}.ksl",
            std::process::id()
        ));
        fs::write(&path, text).unwrap();
        (Url::from_file_path(&path).unwrap(), path)
    }

    #[test]
    fn clean_script_has_no_diagnostics() {
        let (uri, path) = temp_ksl_file("println(\"ok\")\n");
        let result = run_diagnostics(uri.clone(), "println(\"ok\")\n".to_string());
        assert_eq!(result.uri, uri);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn undefined_name_surfaces_as_a_diagnostic_on_the_entry_file() {
        let (uri, path) = temp_ksl_file("println(nope)\n");
        let result = run_diagnostics(uri.clone(), "println(nope)\n".to_string());
        assert_eq!(result.uri, uri);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].message.contains("nope"), "{:?}", result.diagnostics[0]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn import_error_is_attributed_to_the_imported_file() {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ksl_lsp_runtime_diag_import_{n}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let imported = dir.join("broken.ksl");
        fs::write(&imported, "println(nope)\n").unwrap();
        let entry = dir.join("entry.ksl");
        let entry_text = "@file:Import(\"broken.ksl\")\n";
        fs::write(&entry, entry_text).unwrap();
        let entry_uri = Url::from_file_path(&entry).unwrap();
        let imported_uri = Url::from_file_path(&imported).unwrap();

        let result = run_diagnostics(entry_uri, entry_text.to_string());
        assert_eq!(result.uri, imported_uri, "diagnostic should land on the imported file, not the entry file");
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].message.contains("nope"), "{:?}", result.diagnostics[0]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn infinite_loop_times_out_instead_of_hanging() {
        let (uri, path) = temp_ksl_file("while (true) {}\n");
        let result = run_diagnostics_with_timeout(uri.clone(), "while (true) {}\n".to_string(), Duration::from_millis(200));
        assert_eq!(result.uri, uri);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].message.contains("did not finish"), "{:?}", result.diagnostics[0]);
        let _ = fs::remove_file(path);
    }
}
