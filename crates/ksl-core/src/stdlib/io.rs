//! `kotlin.io` — `println`/`print`/`flush`. Auto-imported, matching real
//! Kotlin (`flush` is a ksl-specific addition — real Kotlin has no portable
//! one in common `kotlin.io`, only JVM's `System.out.flush()`).

use crate::environment::Env;
use crate::error::{Flow, RuntimeError, Span};
use crate::interpreter::Interpreter;
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{arity_error, register, BuiltinPackage, BuiltinParam, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "kotlin.io",
    default_imported: true,
    symbols: &[
        BuiltinSymbol {
            name: "println",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "value", ty: Type::Any, nullable: true, default: None }],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Prints value followed by a newline, or just a newline if called with no argument.",
        },
        BuiltinSymbol {
            name: "print",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "value", ty: Type::Any, nullable: true, default: None }],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Like println, but without the trailing newline.",
        },
        BuiltinSymbol {
            name: "flush",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Forces any buffered output from print()/println() calls out to the terminal right now. Needed after print() calls with no trailing newline — stdout is line-buffered, so output can otherwise sit unseen until a later println() (or process exit) finally flushes it.",
        },
        BuiltinSymbol {
            name: "readLine",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            // Really `String?` — see `list_get`'s own doc comment in
            // `stdlib.rs` for why `BuiltinSymbol::return_type` can't
            // express `Nullable` in this `const` table. Unlike `List.get`
            // (where the element usually isn't actually null), this one
            // really can return `Value::Null` on every call at EOF, so
            // `Type::String` would be an actual soundness gap (a caller
            // assigning the "known non-null" result to a `val`, then
            // calling a String method on an unchecked, possibly-null
            // value) — `Type::Any` is the only sound static type here,
            // same as `.get`'s own reasoning, just for a stronger reason.
            return_type: Type::Any,
            doc: "Reads one line from stdin (without the trailing newline), or null at end of input.",
        },
    ],
    populate,
};

/// Registers `kotlin.io`'s members (`println`/`print`) onto `env` —
/// `default_imported: true`, so `Interpreter::new` also mirrors these onto
/// `globals`.
fn populate(env: &Env) {
    register(env, "println", |interp, args, span| {
        match args.len() {
            0 => write_out(interp, "\n"),
            1 => {
                let s = interp.value_to_display_string(&args[0], span)?;
                write_out(interp, &format!("{s}\n"))
            }
            n => return Err(arity_error("println", "0 or 1", n, span)),
        }?;
        Ok(Value::Unit)
    });

    register(env, "print", |interp, args, span| {
        match args.len() {
            1 => {
                let s = interp.value_to_display_string(&args[0], span)?;
                write_out(interp, &s)?
            }
            n => return Err(arity_error("print", "1", n, span)),
        }
        Ok(Value::Unit)
    });

    register(env, "flush", |interp, args, span| {
        if !args.is_empty() {
            return Err(arity_error("flush", "0", args.len(), span));
        }
        interp.out.flush().map_err(|e| RuntimeError::new(format!("io error: {e}"), span))?;
        Ok(Value::Unit)
    });

    register(env, "readLine", |_interp, args, span| {
        if !args.is_empty() {
            return Err(arity_error("readLine", "0", args.len(), span));
        }
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Ok(Value::Null), // EOF, nothing read at all
            Ok(_) => {
                // `read_line` keeps the trailing newline (and a preceding
                // '\r' on a CRLF input) — real Kotlin's `readLine()` strips
                // it, matching every other line-oriented readLine in
                // common use.
                let trimmed = line.trim_end_matches(['\n', '\r']);
                Ok(Value::str(trimmed.to_string()))
            }
            Err(e) => Err(RuntimeError::new(format!("io error: {e}"), span).into()),
        }
    });
}

fn write_out(interp: &mut Interpreter, text: &str) -> Result<(), Flow> {
    write!(interp.out, "{text}").map_err(|e| RuntimeError::new(format!("io error: {e}"), Span::default()).into())
}
