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
];

pub fn is_known(name: &str) -> bool {
    STDLIB_SIGNATURES.iter().any(|(n, _, _)| *n == name)
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
}
