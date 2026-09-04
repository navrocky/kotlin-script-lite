use std::cell::RefCell;
use std::rc::Rc;

use crate::environment::Env;
use crate::error::{Flow, RuntimeError, Span};
use crate::interpreter::Interpreter;
use crate::types::Type;
use crate::value::{NativeValue, Value};

/// `(name, signature, one-line doc)` for every built-in registered by
/// `populate_globals`, mirroring `docs/stdlib.md`. Hand-maintained since
/// `populate_globals` registers functions imperatively (no static,
/// queryable table otherwise) — used for hover/completion in `ksl-lsp` and
/// to tell the resolver a name is a real built-in rather than a typo.
/// Keep in sync with `populate_globals` and `docs/stdlib.md` when adding a
/// stdlib function (see AGENTS.md's feature checklist) — the
/// `stdlib_signatures_match_populate_globals` test below catches this
/// table listing a function that `populate_globals` doesn't (renamed or
/// removed), though not the reverse.
pub const STDLIB_SIGNATURES: &[(&str, &str, &str)] = &[
    ("println", "println(value?)", "Prints value followed by a newline, or just a newline if called with no argument."),
    ("print", "print(value)", "Like println, but without the trailing newline."),
    ("listOf", "listOf(vararg values)", "Builds a List from any number of arguments."),
    ("length", "length(s: String): Int", "Number of characters in s."),
    ("uppercase", "uppercase(s: String): String", "Uppercases s."),
    ("lowercase", "lowercase(s: String): String", "Lowercases s."),
    ("trim", "trim(s: String): String", "Removes leading and trailing whitespace."),
    ("abs", "abs(x: Int | Double): Int | Double", "Absolute value; result type matches the argument's type."),
    ("sqrt", "sqrt(x: Int | Double): Double", "Square root."),
    ("max", "max(a, b): Int | Double", "Returns the larger of a and b."),
    ("min", "min(a, b): Int | Double", "Returns the smaller of a and b."),
    ("assertEquals", "assertEquals(expected, actual, message: String? = null)", "Aborts with a runtime error if expected and actual aren't equal."),
    ("assertTrue", "assertTrue(actual: Boolean, message: String? = null)", "Aborts with a runtime error if actual is false."),
    ("assertFalse", "assertFalse(actual: Boolean, message: String? = null)", "Aborts with a runtime error if actual is true."),
    ("assertNull", "assertNull(actual, message: String? = null)", "Aborts with a runtime error if actual isn't null."),
    ("assertNotNull", "assertNotNull(actual, message: String? = null)", "Aborts with a runtime error if actual is null."),
];

pub fn is_known(name: &str) -> bool {
    STDLIB_SIGNATURES.iter().any(|(n, _, _)| *n == name)
}

/// Real Kotlin's `kotlin.test` package, as far as ksl goes — every one of
/// these names is already a plain global stdlib function (see
/// `populate_globals`), but real Kotlin box tests write `import
/// kotlin.test.*`/`import kotlin.test.assertEquals` expecting a real
/// package to resolve against, not a flat global scope. `Interpreter::new`
/// seeds a synthetic `"kotlin.test"` package env from exactly this list
/// (aliasing the same values already in `globals`, not copies), so both
/// import forms — and a fully-qualified `kotlin.test.assertEquals(...)`
/// call, via the existing package-qualified `FieldAccess`/`MethodCall`
/// path — resolve for real, with no harness-level text rewriting needed.
/// Deliberately narrow (just the assertions this crate actually
/// implements) rather than aliasing all of `globals` under `kotlin.test`,
/// which would make nonsensical calls like `kotlin.test.println(...)`
/// resolve too.
pub const KOTLIN_TEST_MEMBERS: &[&str] =
    &["assertEquals", "assertTrue", "assertFalse", "assertNull", "assertNotNull"];

/// Real, machine-readable `(param types, return type)` for a stdlib
/// function's call-site checking (`STDLIB_SIGNATURES`'s own `signature`
/// field is a free-text display string, not this) — used by
/// `resolver.rs`'s `Call` handling so `println`/`length`/etc. participate
/// in the same argument/return type checking a user-defined `fun` gets,
/// instead of staying permanently `Unknown`. `Type::Unknown` as a
/// parameter's own type means "accepts anything" (never flagged
/// incompatible), matching `println(value: Any?)` in real Kotlin.
///
/// Returns `None` for a name with no checkable signature — `listOf` is
/// deliberately absent (variadic; every arg would need `Type::List`'s
/// still-untracked element type to check meaningfully, and its return
/// type is already just `Type::List` regardless) and so are `abs`/`max`/
/// `min` (numeric-polymorphic — handled directly in `resolver.rs`'s
/// `Call` arm instead, since their return type isn't one fixed `Type`).
pub fn stdlib_call_types(name: &str) -> Option<(Vec<Type>, Type)> {
    match name {
        "println" | "print" => Some((vec![Type::Unknown], Type::Unit)),
        "length" => Some((vec![Type::String], Type::Int)),
        "uppercase" | "lowercase" | "trim" => Some((vec![Type::String], Type::String)),
        "sqrt" => Some((vec![Type::Unknown], Type::Double)),
        "assertEquals" => Some((vec![Type::Unknown, Type::Unknown], Type::Unit)),
        "assertTrue" | "assertFalse" => Some((vec![Type::Bool], Type::Unit)),
        "assertNull" | "assertNotNull" => Some((vec![Type::Unknown], Type::Unit)),
        _ => None,
    }
}

pub fn populate_globals(env: &Env) {
    register(env, "println", |interp, args, span| {
        match args.len() {
            0 => write_out(interp, "\n"),
            1 => write_out(interp, &format!("{}\n", args[0])),
            n => return Err(arity_error("println", "0 or 1", n, span)),
        }?;
        Ok(Value::Unit)
    });

    register(env, "print", |interp, args, span| {
        match args.len() {
            1 => write_out(interp, &format!("{}", args[0]))?,
            n => return Err(arity_error("print", "1", n, span)),
        }
        Ok(Value::Unit)
    });

    register(env, "listOf", |_interp, args, _span| {
        Ok(Value::List(Rc::new(RefCell::new(args))))
    });

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

    register(env, "abs", |_interp, args, span| match args.get(0) {
        Some(Value::Int(i)) => Ok(Value::Int(i.wrapping_abs())),
        Some(Value::Double(d)) => Ok(Value::Double(d.abs())),
        _ => Err(type_error("abs", "a number", span)),
    });

    register(env, "sqrt", |_interp, args, span| {
        let x = expect_num("sqrt", &args, 0, span)?;
        Ok(Value::Double(x.sqrt()))
    });

    register(env, "max", |_interp, args, span| {
        let a = args.get(0).ok_or_else(|| arity_error("max", "2", args.len(), span))?;
        let b = args.get(1).ok_or_else(|| arity_error("max", "2", args.len(), span))?;
        numeric_binary(a, b, span, f64::max)
    });

    register(env, "min", |_interp, args, span| {
        let a = args.get(0).ok_or_else(|| arity_error("min", "2", args.len(), span))?;
        let b = args.get(1).ok_or_else(|| arity_error("min", "2", args.len(), span))?;
        numeric_binary(a, b, span, f64::min)
    });

    // `kotlin.test`-shaped assertions — thin wrappers over `values_equal`/
    // `is_truthy` that raise the existing `RuntimeError` path on failure
    // (the same mechanism `arity_error`/`type_error` already use above),
    // not `Flow::Throw` (that doesn't exist until the `try`/`catch` phase).
    // Added so `docs/roadmap.md`'s Phase 0.5 (JetBrains box-test conformance
    // harness) has something to run vendored `kotlin.test.*`-using tests
    // against — see that phase's own "stdlib additions" note.
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

fn register(
    env: &Env,
    name: &str,
    f: impl Fn(&mut Interpreter, Vec<Value>, Span) -> Result<Value, Flow> + 'static,
) {
    env.define(
        name,
        Value::Native(Rc::new(NativeValue {
            name: name.to_string(),
            func: Box::new(f),
        })),
        false,
        Type::Unknown,
    );
}

fn write_out(interp: &mut Interpreter, text: &str) -> Result<(), Flow> {
    write!(interp.out, "{text}").map_err(|e| RuntimeError::new(format!("io error: {e}"), Span::default()).into())
}

fn arity_error(name: &str, expected: &str, got: usize, span: Span) -> Flow {
    RuntimeError::new(format!("{name} expects {expected} argument(s), got {got}"), span).into()
}

fn type_error(name: &str, expected: &str, span: Span) -> Flow {
    RuntimeError::new(format!("{name} expects {expected}"), span).into()
}

fn expect_str(name: &str, args: &[Value], idx: usize, span: Span) -> Result<Rc<str>, Flow> {
    match args.get(idx) {
        Some(Value::Str(s)) => Ok(s.clone()),
        _ => Err(type_error(name, "a String argument", span)),
    }
}

fn expect_num(name: &str, args: &[Value], idx: usize, span: Span) -> Result<f64, Flow> {
    match args.get(idx) {
        Some(Value::Int(i)) => Ok(*i as f64),
        Some(Value::Double(d)) => Ok(*d),
        _ => Err(type_error(name, "a numeric argument", span)),
    }
}

fn numeric_binary(a: &Value, b: &Value, span: Span, f: impl Fn(f64, f64) -> f64) -> Result<Value, Flow> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            let xf = *x as f64;
            let yf = *y as f64;
            let r = f(xf, yf);
            Ok(Value::Int(r as i64))
        }
        _ => {
            let xf = match a {
                Value::Int(x) => *x as f64,
                Value::Double(x) => *x,
                _ => return Err(type_error("max/min", "numeric arguments", span)),
            };
            let yf = match b {
                Value::Int(y) => *y as f64,
                Value::Double(y) => *y,
                _ => return Err(type_error("max/min", "numeric arguments", span)),
            };
            Ok(Value::Double(f(xf, yf)))
        }
    }
}

fn str_length(s: &str) -> Value {
    Value::Int(s.chars().count() as i64)
}

fn str_uppercase(s: &str) -> Value {
    Value::str(s.to_uppercase())
}

fn str_lowercase(s: &str) -> Value {
    Value::str(s.to_lowercase())
}

fn str_trim(s: &str) -> Value {
    Value::str(s.trim().to_string())
}

/// `(receiver type, method name, signature, one-line doc)` for every
/// builtin method `call_builtin_method` dispatches on — the `.`-syntax
/// counterpart of `STDLIB_SIGNATURES`, used the same way for hover/
/// completion in `ksl-lsp` (now filtered by receiver type — see
/// `ksl-lsp`'s `completion.rs`/`hover.rs`). Every entry here is currently
/// also reachable as a plain stdlib function (`"x".length()` and
/// `length("x")` both work) — see `call_builtin_method`, which reuses the
/// same `str_*` helpers as `populate_globals`'s registrations.
pub const BUILTIN_METHOD_SIGNATURES: &[(&str, &str, &str, &str)] = &[
    ("String", "length", "String.length(): Int", "Number of characters in the receiver."),
    ("String", "uppercase", "String.uppercase(): String", "Uppercases the receiver."),
    ("String", "lowercase", "String.lowercase(): String", "Lowercases the receiver."),
    ("String", "trim", "String.trim(): String", "Removes leading and trailing whitespace from the receiver."),
];

/// The static return type of a builtin `.`-method call, given its
/// receiver's own statically known type — used by `resolver.rs`'s
/// `walk_expr` (`MethodCall` inference) to type-check chained/nested
/// method calls and their results. Hand-maintained separately from
/// `BUILTIN_METHOD_SIGNATURES` (whose `signature` field is a free-text
/// display string, not machine-readable); `(type, method)` pairs not
/// listed here resolve to `Type::Unknown`, same as an unrecognized name
/// anywhere else in this type system.
pub fn builtin_method_return_type(receiver_type: &str, method: &str) -> Type {
    match (receiver_type, method) {
        ("String", "length") => Type::Int,
        ("String", "uppercase" | "lowercase" | "trim") => Type::String,
        _ => Type::Unknown,
    }
}

/// Dispatches a `.`-call (`receiver.method(args)`) once `receiver` has
/// already evaluated to a real value — the counterpart of `parse_postfix`
/// producing `ExprKind::MethodCall` for `receiver.method(...)` syntax. Only
/// a handful of `String` methods exist so far (mirroring the equivalent
/// plain stdlib functions); every other `(type, method)` pair is a runtime
/// error until Phase 5 (classes) and Phase 7 (extension functions) give
/// this real per-type dispatch.
pub fn call_builtin_method(receiver: Value, method: &str, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    match (&receiver, method) {
        (Value::Str(s), "length") => {
            expect_no_args(method, &args, span)?;
            Ok(str_length(s))
        }
        (Value::Str(s), "uppercase") => {
            expect_no_args(method, &args, span)?;
            Ok(str_uppercase(s))
        }
        (Value::Str(s), "lowercase") => {
            expect_no_args(method, &args, span)?;
            Ok(str_lowercase(s))
        }
        (Value::Str(s), "trim") => {
            expect_no_args(method, &args, span)?;
            Ok(str_trim(s))
        }
        _ => Err(RuntimeError::new(
            format!("'{}' has no method '{method}'", receiver.type_name()),
            span,
        )
        .into()),
    }
}

fn expect_no_args(method: &str, args: &[Value], span: Span) -> Result<(), Flow> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(arity_error(method, "0", args.len(), span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name listed in `STDLIB_SIGNATURES` must actually be
    /// registered by `populate_globals` — catches the table listing a
    /// stdlib function that was renamed or removed. (`Env` doesn't expose
    /// enumeration, so this can't also check the other direction — a new
    /// `register` call with no matching table entry has to be caught by
    /// the "adding a feature" checklist in AGENTS.md instead.)
    #[test]
    fn stdlib_signatures_match_populate_globals() {
        let env = Env::new();
        populate_globals(&env);
        for (name, _, _) in STDLIB_SIGNATURES {
            assert!(
                matches!(env.get(name), Some(Value::Native(_))),
                "'{name}' is in STDLIB_SIGNATURES but not registered by populate_globals"
            );
        }
    }

    /// A stdlib type error must report the call-site span passed to the
    /// native function, not a default `0:0` — regression test for the
    /// `NativeFn` span threading fix.
    #[test]
    fn stdlib_error_reports_call_site_span() {
        let env = Env::new();
        populate_globals(&env);
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

    /// Every name in `BUILTIN_METHOD_SIGNATURES` must actually be
    /// dispatchable by `call_builtin_method` on some receiver — mirrors
    /// `stdlib_signatures_match_populate_globals` for the `.`-method table.
    #[test]
    fn builtin_method_signatures_are_all_dispatchable_on_a_string() {
        for (receiver_type, name, _, _) in BUILTIN_METHOD_SIGNATURES {
            assert_eq!(*receiver_type, "String", "add a receiver-type-appropriate dispatch below if this changes");
            let result = call_builtin_method(Value::str("x"), name, vec![], Span::default());
            assert!(result.is_ok(), "'{name}' listed in BUILTIN_METHOD_SIGNATURES but not dispatchable: {:?}", result.err().map(|_| ()));
        }
    }

    #[test]
    fn builtin_method_return_type_matches_signatures() {
        for (receiver_type, name, _, _) in BUILTIN_METHOD_SIGNATURES {
            assert_ne!(
                builtin_method_return_type(receiver_type, name),
                Type::Unknown,
                "'{name}' listed in BUILTIN_METHOD_SIGNATURES but builtin_method_return_type doesn't know it"
            );
        }
        assert_eq!(builtin_method_return_type("Int", "length"), Type::Unknown);
        assert_eq!(builtin_method_return_type("String", "shout"), Type::Unknown);
    }

    #[test]
    fn call_builtin_method_dispatches_string_methods() {
        assert!(matches!(call_builtin_method(Value::str("abc"), "length", vec![], Span::default()), Ok(Value::Int(3))));
        assert!(matches!(call_builtin_method(Value::str("abc"), "uppercase", vec![], Span::default()), Ok(Value::Str(s)) if &*s == "ABC"));
    }

    #[test]
    fn call_builtin_method_unknown_method_is_an_error() {
        match call_builtin_method(Value::str("x"), "shout", vec![], Span::default()) {
            Err(Flow::Error(e)) => assert!(e.message.contains("has no method 'shout'"), "{}", e.message),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn call_builtin_method_arity_error_reports_the_given_span() {
        let span = Span::new(4, 2);
        match call_builtin_method(Value::str("x"), "length", vec![Value::Int(1)], span) {
            Err(Flow::Error(e)) => assert_eq!(e.span, span),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }

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
        populate_globals(&env);
        assert!(call(&env, "assertEquals", vec![Value::Int(4), Value::Int(4)]).is_ok());
        match call(&env, "assertEquals", vec![Value::Int(4), Value::Int(5)]) {
            Err(Flow::Error(e)) => assert!(e.message.contains("assertEquals failed"), "{}", e.message),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn assert_equals_appends_the_optional_message() {
        let env = Env::new();
        populate_globals(&env);
        match call(&env, "assertEquals", vec![Value::Int(4), Value::Int(5), Value::str("nope")]) {
            Err(Flow::Error(e)) => assert!(e.message.ends_with(": nope"), "{}", e.message),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn assert_true_and_assert_false() {
        let env = Env::new();
        populate_globals(&env);
        assert!(call(&env, "assertTrue", vec![Value::Bool(true)]).is_ok());
        assert!(call(&env, "assertTrue", vec![Value::Bool(false)]).is_err());
        assert!(call(&env, "assertFalse", vec![Value::Bool(false)]).is_ok());
        assert!(call(&env, "assertFalse", vec![Value::Bool(true)]).is_err());
    }

    #[test]
    fn assert_null_and_assert_not_null() {
        let env = Env::new();
        populate_globals(&env);
        assert!(call(&env, "assertNull", vec![Value::Null]).is_ok());
        assert!(call(&env, "assertNull", vec![Value::Int(1)]).is_err());
        assert!(call(&env, "assertNotNull", vec![Value::Int(1)]).is_ok());
        assert!(call(&env, "assertNotNull", vec![Value::Null]).is_err());
    }

    #[test]
    fn assert_functions_report_arity_errors() {
        let env = Env::new();
        populate_globals(&env);
        assert!(call(&env, "assertEquals", vec![Value::Int(1)]).is_err());
        assert!(call(&env, "assertTrue", vec![]).is_err());
    }
}
