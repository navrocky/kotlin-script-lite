//! `kotlin.io` — `println`/`print`. Auto-imported, matching real Kotlin.

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
}

fn write_out(interp: &mut Interpreter, text: &str) -> Result<(), Flow> {
    write!(interp.out, "{text}").map_err(|e| RuntimeError::new(format!("io error: {e}"), Span::default()).into())
}
