//! `kotlin.text` — `length`/`uppercase`/`lowercase`/`trim`. Auto-imported,
//! matching real Kotlin. `str_*` helpers are also reused by
//! `super::call_builtin_method` for the `.`-method calling convention on a
//! `String` receiver (`"x".uppercase()` etc. — see `docs/stdlib.md#methods`).

use std::rc::Rc;

use crate::environment::Env;
use crate::error::{Flow, Span};
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{register, type_error, BuiltinPackage, BuiltinParam, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "kotlin.text",
    default_imported: true,
    symbols: &[
        BuiltinSymbol {
            name: "length",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "s", ty: Type::String, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Int,
            doc: "Number of characters in s.",
        },
        BuiltinSymbol {
            name: "uppercase",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "s", ty: Type::String, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::String,
            doc: "Uppercases s.",
        },
        BuiltinSymbol {
            name: "lowercase",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "s", ty: Type::String, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::String,
            doc: "Lowercases s.",
        },
        BuiltinSymbol {
            name: "trim",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "s", ty: Type::String, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::String,
            doc: "Removes leading and trailing whitespace.",
        },
    ],
    populate,
};

/// Registers `kotlin.text`'s members (`length`/`uppercase`/`lowercase`/
/// `trim`) onto `env`.
fn populate(env: &Env) {
    register(env, "length", |_interp, args, span| {
        let s = expect_str("length", &args, 0, span)?;
        Ok(str_length(&s))
    });

    register(env, "uppercase", |_interp, args, span| {
        let s = expect_str("uppercase", &args, 0, span)?;
        Ok(str_uppercase(&s))
    });

    register(env, "lowercase", |_interp, args, span| {
        let s = expect_str("lowercase", &args, 0, span)?;
        Ok(str_lowercase(&s))
    });

    register(env, "trim", |_interp, args, span| {
        let s = expect_str("trim", &args, 0, span)?;
        Ok(str_trim(&s))
    });
}

fn expect_str(name: &str, args: &[Value], idx: usize, span: Span) -> Result<Rc<str>, Flow> {
    match args.get(idx) {
        Some(Value::Str(s)) => Ok(s.clone()),
        _ => Err(type_error(name, "a String argument", span)),
    }
}

pub(super) fn str_length(s: &str) -> Value {
    Value::Int(s.chars().count() as i64)
}

pub(super) fn str_uppercase(s: &str) -> Value {
    Value::str(s.to_uppercase())
}

pub(super) fn str_lowercase(s: &str) -> Value {
    Value::str(s.to_lowercase())
}

pub(super) fn str_trim(s: &str) -> Value {
    Value::str(s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Flow, Span};
    use crate::interpreter::Interpreter;

    /// A stdlib type error must report the call-site span passed to the
    /// native function, not a default `0:0` — regression test for the
    /// `NativeFn` span threading fix.
    #[test]
    fn stdlib_error_reports_call_site_span() {
        let env = Env::new();
        populate(&env);
        let mut sink = Vec::new();
        let mut interp = Interpreter::new(&mut sink);
        let f = match env.get("length") {
            Some(Value::Native(n)) => n,
            _ => panic!("length not registered"),
        };
        let span = Span::new(7, 3);
        match (f.func)(&mut interp, vec![Value::Int(1)], span) {
            Err(Flow::Error(e)) => assert_eq!(e.span, span),
            _ => panic!("expected Flow::Error"),
        }
    }
}
