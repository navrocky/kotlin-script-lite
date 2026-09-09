use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const SIGINT: i32 = 2;
const SIG_DFL: usize = 0;

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn raise(signum: i32) -> i32;
}

/// Ctrl+C while a script is mid-`enableRawMode()` (`ksl.term`) would
/// otherwise leave the real terminal in raw mode forever: `SIGINT`'s
/// *default* disposition is immediate process termination, which never
/// runs Rust destructors, so `Interpreter`'s own `Drop`-based restore
/// (`stdlib::terminal`'s module doc comment) never gets a chance to run.
/// This handler restores the terminal first, then re-arms `SIGINT`'s
/// default disposition and re-raises it — so Ctrl+C still terminates the
/// process exactly as it always did, just after a clean restore.
///
/// # Safety
/// A signal handler may only call async-signal-safe functions —
/// `emergency_restore_terminal` is written to satisfy that (no
/// allocation, no panics, see `stdlib::terminal::restore_if_active`'s own
/// doc comment); `signal`/`raise` themselves are async-signal-safe by
/// POSIX definition.
extern "C" fn handle_sigint(_signum: i32) {
    ksl_core::emergency_restore_terminal();
    unsafe {
        signal(SIGINT, SIG_DFL);
        raise(SIGINT);
    }
}

fn main() -> ExitCode {
    // SAFETY: installed once, before any script runs (in particular before
    // any call to `enableRawMode()` is even possible) — no reentrancy
    // concern at install time.
    unsafe {
        signal(SIGINT, handle_sigint as *const () as usize);
    }

    let args: Vec<String> = env::args().collect();

    let (source, base_dir, entry_path): (String, PathBuf, Option<PathBuf>) =
        match args.get(1).map(|s| s.as_str()) {
            None => {
                eprintln!("usage: ksl <script.ksl>");
                eprintln!("       ksl -            (read script from stdin)");
                return ExitCode::FAILURE;
            }
            Some("-h") | Some("--help") => {
                println!("usage: ksl <script.ksl>");
                println!("       ksl -            (read script from stdin)");
                return ExitCode::SUCCESS;
            }
            Some("-") => {
                let mut s = String::new();
                if let Err(e) = io::stdin().read_to_string(&mut s) {
                    eprintln!("error: cannot read stdin: {e}");
                    return ExitCode::FAILURE;
                }
                let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                (s, cwd, None)
            }
            Some(path) => {
                let s = match fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("error: cannot read '{path}': {e}");
                        return ExitCode::FAILURE;
                    }
                };
                let base = Path::new(path)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                (s, base, Some(PathBuf::from(path)))
            }
        };

    // `run_source` now statically checks (and refuses to run) an ill-typed
    // script itself, in the same pass that parses it — see
    // `importer.rs`'s unified parse-resolve-then-run flow — so there's no
    // separate `static_check` call needed here anymore; a type error
    // surfaces through the same `Err(e)` branch below as any other error,
    // via `InterpError::TypeErrors`'s own `Display` (one line per error,
    // same wording this used to print by hand).
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    match ksl_core::run_source(&source, &mut handle, &base_dir, entry_path.as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = handle.flush();
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
