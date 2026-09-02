use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
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
