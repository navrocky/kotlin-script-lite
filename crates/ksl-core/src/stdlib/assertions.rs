//! `kotlin.test` — `assertEquals`/`assertTrue`/`assertFalse`/`assertNull`/
//! `assertNotNull`. **Not** auto-imported, matching real Kotlin exactly —
//! needs `import kotlin.test.*` (or a single-symbol import, or full
//! `kotlin.test.assertEquals(...)` qualification) — see
//! `docs/syntax.md`'s "Imports". Thin wrappers over `values_equal`/pattern
//! matching that raise the existing `RuntimeError` path on failure (the
//! same mechanism `arity_error`/`type_error` use), not `Flow::Throw` (that
//! doesn't exist until the `try`/`catch` phase). Added so
//! `docs/roadmap.md`'s Phase 0.5 (JetBrains box-test conformance harness)
//! has something to run vendored `kotlin.test.*`-using tests against — see
//! that phase's own "stdlib additions" note.

use crate::environment::Env;
use crate::error::RuntimeError;
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{arity_error, register, type_error, BuiltinPackage, BuiltinParam, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "kotlin.test",
    default_imported: false,
    symbols: &[
        BuiltinSymbol {
            name: "assertEquals",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "expected", ty: Type::Unknown, nullable: false, default: None },
                BuiltinParam { name: "actual", ty: Type::Unknown, nullable: false, default: None },
                BuiltinParam { name: "message", ty: Type::String, nullable: true, default: Some("null") },
            ],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Aborts with a runtime error if expected and actual aren't equal.",
        },
        BuiltinSymbol {
            name: "assertTrue",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "actual", ty: Type::Bool, nullable: false, default: None },
                BuiltinParam { name: "message", ty: Type::String, nullable: true, default: Some("null") },
            ],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Aborts with a runtime error if actual is false.",
        },
        BuiltinSymbol {
            name: "assertFalse",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "actual", ty: Type::Bool, nullable: false, default: None },
                BuiltinParam { name: "message", ty: Type::String, nullable: true, default: Some("null") },
            ],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Aborts with a runtime error if actual is true.",
        },
        BuiltinSymbol {
            name: "assertNull",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "actual", ty: Type::Unknown, nullable: false, default: None },
                BuiltinParam { name: "message", ty: Type::String, nullable: true, default: Some("null") },
            ],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Aborts with a runtime error if actual isn't null.",
        },
        BuiltinSymbol {
            name: "assertNotNull",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "actual", ty: Type::Unknown, nullable: false, default: None },
                BuiltinParam { name: "message", ty: Type::String, nullable: true, default: Some("null") },
            ],
            variadic_param: None,
            return_type: Type::Unit,
            doc: "Aborts with a runtime error if actual is null.",
        },
    ],
    populate,
};

/// Registers `kotlin.test`'s five assertions onto `env` — `default_imported:
/// false` (see `PACKAGE`'s doc comment above), same as `kotlin.math`: needs
/// an explicit `import`/qualification, never mirrored onto `globals`.
fn populate(env: &Env) {
    register(env, "assertEquals", |_interp, args, span| {
        let (expected, actual, message) = match args.len() {
            2 => (&args[0], &args[1], None),
            3 => (&args[0], &args[1], Some(&args[2])),
            n => return Err(arity_error("assertEquals", "2 or 3", n, span)),
        };
        if !Value::values_equal(expected, actual) {
            return Err(RuntimeError::new(
                format!(
                    "assertEquals failed: expected <{expected}>, actual <{actual}>{}",
                    message.map(|m| format!(": {m}")).unwrap_or_default()
                ),
                span,
            )
            .into());
        }
        Ok(Value::Unit)
    });

    register(env, "assertTrue", |_interp, args, span| {
        let (cond, message) = match args.len() {
            1 => (&args[0], None),
            2 => (&args[0], Some(&args[1])),
            n => return Err(arity_error("assertTrue", "1 or 2", n, span)),
        };
        match cond {
            Value::Bool(true) => Ok(Value::Unit),
            Value::Bool(false) => Err(RuntimeError::new(
                format!("assertTrue failed{}", message.map(|m| format!(": {m}")).unwrap_or_default()),
                span,
            )
            .into()),
            _ => Err(type_error("assertTrue", "a Boolean argument", span)),
        }
    });

    register(env, "assertFalse", |_interp, args, span| {
        let (cond, message) = match args.len() {
            1 => (&args[0], None),
            2 => (&args[0], Some(&args[1])),
            n => return Err(arity_error("assertFalse", "1 or 2", n, span)),
        };
        match cond {
            Value::Bool(false) => Ok(Value::Unit),
            Value::Bool(true) => Err(RuntimeError::new(
                format!("assertFalse failed{}", message.map(|m| format!(": {m}")).unwrap_or_default()),
                span,
            )
            .into()),
            _ => Err(type_error("assertFalse", "a Boolean argument", span)),
        }
    });

    register(env, "assertNull", |_interp, args, span| {
        let (actual, message) = match args.len() {
            1 => (&args[0], None),
            2 => (&args[0], Some(&args[1])),
            n => return Err(arity_error("assertNull", "1 or 2", n, span)),
        };
        if !matches!(actual, Value::Null) {
            return Err(RuntimeError::new(
                format!(
                    "assertNull failed: expected null, actual <{actual}>{}",
                    message.map(|m| format!(": {m}")).unwrap_or_default()
                ),
                span,
            )
            .into());
        }
        Ok(Value::Unit)
    });

    register(env, "assertNotNull", |_interp, args, span| {
        let (actual, message) = match args.len() {
            1 => (&args[0], None),
            2 => (&args[0], Some(&args[1])),
            n => return Err(arity_error("assertNotNull", "1 or 2", n, span)),
        };
        if matches!(actual, Value::Null) {
            return Err(RuntimeError::new(
                format!("assertNotNull failed: expected non-null{}", message.map(|m| format!(": {m}")).unwrap_or_default()),
                span,
            )
            .into());
        }
        Ok(Value::Unit)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Flow, Span};
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

    #[test]
    fn assert_equals_passes_on_equal_values_fails_otherwise() {
        let env = Env::new();
        populate(&env);
        assert!(call(&env, "assertEquals", vec![Value::Int(4), Value::Int(4)]).is_ok());
        match call(&env, "assertEquals", vec![Value::Int(4), Value::Int(5)]) {
            Err(Flow::Error(e)) => assert!(e.message.contains("assertEquals failed"), "{}", e.message),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn assert_equals_appends_the_optional_message() {
        let env = Env::new();
        populate(&env);
        match call(&env, "assertEquals", vec![Value::Int(4), Value::Int(5), Value::str("nope")]) {
            Err(Flow::Error(e)) => assert!(e.message.ends_with(": nope"), "{}", e.message),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn assert_true_and_assert_false() {
        let env = Env::new();
        populate(&env);
        assert!(call(&env, "assertTrue", vec![Value::Bool(true)]).is_ok());
        assert!(call(&env, "assertTrue", vec![Value::Bool(false)]).is_err());
        assert!(call(&env, "assertFalse", vec![Value::Bool(false)]).is_ok());
        assert!(call(&env, "assertFalse", vec![Value::Bool(true)]).is_err());
    }

    #[test]
    fn assert_null_and_assert_not_null() {
        let env = Env::new();
        populate(&env);
        assert!(call(&env, "assertNull", vec![Value::Null]).is_ok());
        assert!(call(&env, "assertNull", vec![Value::Int(1)]).is_err());
        assert!(call(&env, "assertNotNull", vec![Value::Int(1)]).is_ok());
        assert!(call(&env, "assertNotNull", vec![Value::Null]).is_err());
    }

    #[test]
    fn assert_functions_report_arity_errors() {
        let env = Env::new();
        populate(&env);
        assert!(call(&env, "assertEquals", vec![Value::Int(1)]).is_err());
        assert!(call(&env, "assertTrue", vec![]).is_err());
    }
}
