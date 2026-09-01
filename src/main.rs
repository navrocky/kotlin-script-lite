use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    let source = match args.get(1).map(|s| s.as_str()) {
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
            s
        }
        Some(path) => match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read '{path}': {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    match kotlin_script_lite::run_source(&source, &mut handle) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = handle.flush();
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
