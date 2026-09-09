//! `ksl.term` — the minimal set of terminal primitives that genuinely
//! cannot be expressed in a ksl script at all: raw/cbreak input mode,
//! non-blocking byte reads, terminal-size/`isatty` queries, and
//! sleep/clock. **Not** auto-imported. The first non-`kotlin.*` package —
//! none of this has a real Kotlin stdlib equivalent (`docs/stdlib.md`'s
//! Terminal section carries the same "not real Kotlin" note).
//!
//! **Deliberately excludes** screen/cursor control (`clearScreen`,
//! `moveCursor`, colors, ...) — those are pure ANSI escape-sequence
//! strings, buildable entirely in ordinary ksl script code via the
//! `\u{XXXX}` string escape (`lexer.rs`) with no syscall involved at all,
//! so they live in a ksl-level TUI library instead
//! (`tests/demos/tui.ksl`), not here. Everything in this file needs a real
//! OS syscall `std` either doesn't expose at all (termios/ioctl — hand-
//! written `extern "C"` FFI to the already-linked system libc, Linux/Unix
//! only, no Cargo dependency — matches `ksl-core`'s zero-dependency policy,
//! `AGENTS.md`) or already does (`std::thread::sleep`, `SystemTime`,
//! `std::io::IsTerminal`).
//!
//! **Terminal restoration.** Raw mode must never survive past the script
//! that enabled it, even if that script errors out or is interrupted by
//! Ctrl+C mid-run:
//! - The ordinary case (a script errors, or just ends) is handled by
//!   `Interpreter`'s own `Drop` impl (`interpreter.rs`), which calls
//!   [`restore_if_active`] unconditionally as its first line — covers
//!   every exit from `run_source`, `Ok` or `Err` alike.
//! - Ctrl+C while raw mode is active is a separate, real gap: `ISIG` is
//!   deliberately left set by `enableRawMode` below (so Ctrl+C keeps
//!   delivering `SIGINT` rather than becoming just another raw byte), but
//!   `SIGINT`'s *default* disposition is immediate process termination,
//!   which never runs Rust destructors — `Drop` never fires. `ksl-cli`'s
//!   `main.rs` installs its own `SIGINT` handler before running any
//!   script, calling [`restore_if_active`] (re-exported at the crate root
//!   as `ksl_core::emergency_restore_terminal`) before re-raising the
//!   signal with its default disposition restored, so Ctrl+C still
//!   terminates the process, just after a clean terminal restore.

use std::io::IsTerminal;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::environment::Env;
use crate::error::{Flow, RuntimeError, Span};
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{arity_error, register, type_error, BuiltinPackage, BuiltinParam, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "ksl.term",
    default_imported: false,
    symbols: &[
        BuiltinSymbol {
            name: "enableRawMode",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Turns off line buffering and echo on stdin (cbreak mode) so readByte() sees each keypress immediately, without waiting for Enter. Errors if stdin isn't a real terminal.",
        },
        BuiltinSymbol {
            name: "disableRawMode",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Restores stdin to its normal (line-buffered, echoing) mode. Safe to call even if raw mode isn't active.",
        },
        BuiltinSymbol {
            name: "readByte",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            // Really `Int?` — see `list_get`'s doc comment in `stdlib.rs`
            // for why `BuiltinSymbol::return_type` can't express
            // `Nullable`; this one genuinely returns null on every call
            // with nothing waiting, so `Type::Any` (not `Type::Int`) is
            // the only sound choice here, same reasoning as `readLine`.
            return_type: Type::Any,
            doc: "The next byte waiting on stdin (0-255), or null if none is available right now. Only useful after enableRawMode() — otherwise it blocks like an ordinary read.",
        },
        BuiltinSymbol {
            name: "isTerminal",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Bool,
            doc: "Whether stdin is connected to a real terminal — check this before enableRawMode() to fail gracefully instead of erroring.",
        },
        BuiltinSymbol {
            name: "terminalWidth",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Int,
            doc: "The terminal's current width, in columns.",
        },
        BuiltinSymbol {
            name: "terminalHeight",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Int,
            doc: "The terminal's current height, in rows.",
        },
        BuiltinSymbol {
            name: "sleep",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "millis", ty: Type::Int, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Pauses execution for the given number of milliseconds.",
        },
        BuiltinSymbol {
            name: "currentTimeMillis",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Int,
            doc: "Milliseconds since the Unix epoch — for frame timing, or seeding your own generator.",
        },
    ],
    populate,
};

fn populate(env: &Env) {
    register(env, "enableRawMode", |_interp, args, span| {
        expect_no_args("enableRawMode", &args, span)?;
        enable_raw_mode(span)?;
        Ok(Value::Unit)
    });

    register(env, "disableRawMode", |_interp, args, span| {
        expect_no_args("disableRawMode", &args, span)?;
        restore_if_active();
        Ok(Value::Unit)
    });

    register(env, "readByte", |_interp, args, span| {
        expect_no_args("readByte", &args, span)?;
        read_byte(span)
    });

    register(env, "isTerminal", |_interp, args, span| {
        expect_no_args("isTerminal", &args, span)?;
        Ok(Value::Bool(std::io::stdin().is_terminal()))
    });

    register(env, "terminalWidth", |_interp, args, span| {
        expect_no_args("terminalWidth", &args, span)?;
        Ok(Value::Int(terminal_size(span)?.0 as i64))
    });

    register(env, "terminalHeight", |_interp, args, span| {
        expect_no_args("terminalHeight", &args, span)?;
        Ok(Value::Int(terminal_size(span)?.1 as i64))
    });

    register(env, "sleep", |_interp, args, span| {
        let millis = match args.as_slice() {
            [Value::Int(m)] => *m,
            [_] => return Err(type_error("sleep", "an Int", span)),
            _ => return Err(arity_error("sleep", "1", args.len(), span)),
        };
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis as u64));
        }
        Ok(Value::Unit)
    });

    register(env, "currentTimeMillis", |_interp, args, span| {
        expect_no_args("currentTimeMillis", &args, span)?;
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        Ok(Value::Int(millis))
    });
}

fn expect_no_args(name: &str, args: &[Value], span: Span) -> Result<(), Flow> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(arity_error(name, "0", args.len(), span))
    }
}

/// Hand-written bindings to exactly the libc functions/structs this module
/// needs — no `libc`/`termios`/`nix` crate (`ksl-core` stays zero-
/// dependency, `AGENTS.md`), Linux x86_64/aarch64 only (matches the
/// project's own musl release target, `AGENTS.md`'s build commands).
mod ffi {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct Termios {
        pub c_iflag: u32,
        pub c_oflag: u32,
        pub c_cflag: u32,
        pub c_lflag: u32,
        pub c_line: u8,
        pub c_cc: [u8; 32],
        pub c_ispeed: u32,
        pub c_ospeed: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct Winsize {
        pub ws_row: u16,
        pub ws_col: u16,
        pub ws_xpixel: u16,
        pub ws_ypixel: u16,
    }

    // c_lflag bits (Linux, both glibc's <bits/termios.h> and musl agree).
    pub(super) const ICANON: u32 = 0o0000002;
    pub(super) const ECHO: u32 = 0o0000010;
    // c_cc indices.
    pub(super) const VMIN: usize = 6;
    pub(super) const VTIME: usize = 5;
    pub(super) const TCSANOW: i32 = 0;
    pub(super) const STDIN_FD: i32 = 0;
    pub(super) const STDOUT_FD: i32 = 1;
    // Linux ioctl request number for "get window size" (asm-generic/ioctls.h).
    pub(super) const TIOCGWINSZ: u64 = 0x5413;

    extern "C" {
        pub(super) fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
        pub(super) fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
        pub(super) fn ioctl(fd: i32, request: u64, ...) -> i32;
        pub(super) fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    }
}

/// The terminal's state as `enableRawMode` found it, so `restore_if_active`
/// can put it back — `static mut`, not a `Mutex`, deliberately: this also
/// gets read from a signal handler (see this module's own doc comment),
/// where taking a lock is unsound (the interrupted thread might already
/// hold it, deadlocking the handler). Sound only because ksl's interpreter
/// is single-threaded by design (`AGENTS.md`: "don't introduce
/// Send/Sync/threading") — the only reentrancy possible is a signal handler
/// interrupting this same thread mid-call, never a second real OS thread.
static mut ORIGINAL_TERMIOS: Option<ffi::Termios> = None;

fn enable_raw_mode(span: Span) -> Result<(), Flow> {
    if !std::io::stdin().is_terminal() {
        return Err(RuntimeError::new("enableRawMode: stdin is not connected to a terminal", span).into());
    }
    let mut original = ffi::Termios::default();
    // SAFETY: `original` is a valid, correctly-sized `Termios` for
    // `tcgetattr` to write into; fd 0 is always open.
    if unsafe { ffi::tcgetattr(ffi::STDIN_FD, &mut original) } != 0 {
        return Err(RuntimeError::new("enableRawMode: tcgetattr failed", span).into());
    }
    let mut raw = original;
    raw.c_lflag &= !(ffi::ICANON | ffi::ECHO);
    raw.c_cc[ffi::VMIN] = 0;
    raw.c_cc[ffi::VTIME] = 0;
    // SAFETY: `raw` is a valid, initialized `Termios` (copied from a
    // successful `tcgetattr` above, with only two `c_lflag` bits cleared).
    if unsafe { ffi::tcsetattr(ffi::STDIN_FD, ffi::TCSANOW, &raw) } != 0 {
        return Err(RuntimeError::new("enableRawMode: tcsetattr failed", span).into());
    }
    // SAFETY: see `ORIGINAL_TERMIOS`'s own doc comment. `addr_of_mut!`
    // (rather than `&mut ORIGINAL_TERMIOS`) avoids ever materializing a
    // `&'static mut` to the static itself, just a raw pointer to write
    // through — the pattern `rustc` itself recommends for a `static mut`.
    unsafe {
        *std::ptr::addr_of_mut!(ORIGINAL_TERMIOS) = Some(original);
    }
    Ok(())
}

/// Restores the terminal to whatever `enableRawMode` last snapshotted, if
/// raw mode is currently active — a silent no-op otherwise. Never
/// allocates and never panics, so it's sound to call from a signal handler
/// (see this module's own doc comment) as well as ordinary code.
pub(crate) fn restore_if_active() {
    // SAFETY: see `ORIGINAL_TERMIOS`'s own doc comment and
    // `enable_raw_mode`'s identical `addr_of_mut!` note.
    unsafe {
        if let Some(original) = (*std::ptr::addr_of_mut!(ORIGINAL_TERMIOS)).take() {
            ffi::tcsetattr(ffi::STDIN_FD, ffi::TCSANOW, &original);
        }
    }
}

/// One byte, or `null` if nothing is waiting — sound to call whether or not
/// raw mode is active: without it, `c_cc[VMIN]`/`c_cc[VTIME]` still have
/// their normal (blocking, line-buffered) values, so this just blocks like
/// an ordinary line read until Enter, exactly like `std::io::stdin()`
/// already would.
fn read_byte(span: Span) -> Result<Value, Flow> {
    let mut buf = [0u8; 1];
    // SAFETY: `buf` is a valid 1-byte buffer; fd 0 is always open.
    let n = unsafe { ffi::read(ffi::STDIN_FD, buf.as_mut_ptr(), 1) };
    if n < 0 {
        return Err(RuntimeError::new("readByte: read failed", span).into());
    }
    if n == 0 {
        Ok(Value::Null)
    } else {
        Ok(Value::Int(buf[0] as i64))
    }
}

fn terminal_size(span: Span) -> Result<(u16, u16), Flow> {
    let mut ws = ffi::Winsize::default();
    // SAFETY: `ws` is a valid, correctly-sized `Winsize` for `ioctl` to
    // write into; fd 1 is always open.
    let rc = unsafe { ffi::ioctl(ffi::STDOUT_FD, ffi::TIOCGWINSZ, &mut ws as *mut ffi::Winsize) };
    if rc != 0 {
        return Err(RuntimeError::new("terminal size unavailable (not a terminal?)", span).into());
    }
    Ok((ws.ws_col, ws.ws_row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Flow;
    use crate::interpreter::Interpreter;

    fn call(env: &Env, name: &str, args: Vec<Value>) -> Result<Value, Flow> {
        let f = match env.get(name) {
            Some(Value::Native(n)) => n,
            _ => panic!("'{name}' not registered"),
        };
        let mut sink = Vec::new();
        let mut interp = Interpreter::new(&mut sink);
        (f.func)(&mut interp, args, Span::default())
    }

    /// `cargo test`'s own stdin is normally not a real terminal (piped/
    /// redirected) — `enableRawMode` must reject that cleanly (a
    /// `RuntimeError`) rather than trying the syscalls anyway. Skips itself
    /// rather than asserting anything on the rare chance stdin genuinely is
    /// a terminal when this runs (an interactive shell, not CI) — actually
    /// exercising `enableRawMode` there would mutate *this* test run's own
    /// real terminal mode, which a unit test must never do.
    #[test]
    fn enable_raw_mode_errors_cleanly_off_a_real_terminal() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let env = Env::new();
        populate(&env);
        assert!(call(&env, "enableRawMode", vec![]).is_err());
    }

    /// A no-op, not an error, when raw mode was never enabled.
    #[test]
    fn disable_raw_mode_is_a_harmless_no_op_when_not_active() {
        let env = Env::new();
        populate(&env);
        assert!(call(&env, "disableRawMode", vec![]).is_ok());
    }

    #[test]
    fn sleep_rejects_a_non_int_argument() {
        let env = Env::new();
        populate(&env);
        assert!(call(&env, "sleep", vec![Value::str("nope")]).is_err());
    }

    #[test]
    fn current_time_millis_is_a_plausible_unix_timestamp() {
        let env = Env::new();
        populate(&env);
        let Ok(Value::Int(millis)) = call(&env, "currentTimeMillis", vec![]) else {
            panic!("expected an Int")
        };
        // Any time after ~2020-01-01 in milliseconds — just proves this
        // reads a real clock, not a fixed/zero stub.
        assert!(millis > 1_577_836_800_000, "{millis} doesn't look like a real Unix-epoch-millis timestamp");
    }

    /// Just proves it dispatches and returns a real `Bool` — not asserting
    /// which one, since whether `cargo test`'s own stdin happens to be a
    /// real terminal is environment-dependent (piped in CI, but not
    /// necessarily when run by hand from an interactive shell).
    #[test]
    fn is_terminal_returns_a_real_bool() {
        let env = Env::new();
        populate(&env);
        assert!(matches!(call(&env, "isTerminal", vec![]), Ok(Value::Bool(_))));
    }
}
