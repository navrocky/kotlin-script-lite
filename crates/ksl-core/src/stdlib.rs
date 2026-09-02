use std::cell::RefCell;
use std::rc::Rc;

use crate::environment::Env;
use crate::error::{Flow, RuntimeError, Span};
use crate::interpreter::Interpreter;
use crate::value::{NativeValue, Value};

pub fn populate_globals(env: &Env) {
    register(env, "println", |interp, args| {
        match args.len() {
            0 => write_out(interp, "\n"),
            1 => write_out(interp, &format!("{}\n", args[0])),
            n => return Err(arity_error("println", "0 or 1", n)),
        }?;
        Ok(Value::Unit)
    });

    register(env, "print", |interp, args| {
        match args.len() {
            1 => write_out(interp, &format!("{}", args[0]))?,
            n => return Err(arity_error("print", "1", n)),
        }
        Ok(Value::Unit)
    });

    register(env, "listOf", |_interp, args| {
        Ok(Value::List(Rc::new(RefCell::new(args))))
    });

    register(env, "length", |_interp, args| {
        let s = expect_str("length", &args, 0)?;
        Ok(Value::Int(s.chars().count() as i64))
    });

    register(env, "uppercase", |_interp, args| {
        let s = expect_str("uppercase", &args, 0)?;
        Ok(Value::str(s.to_uppercase()))
    });

    register(env, "lowercase", |_interp, args| {
        let s = expect_str("lowercase", &args, 0)?;
        Ok(Value::str(s.to_lowercase()))
    });

    register(env, "trim", |_interp, args| {
        let s = expect_str("trim", &args, 0)?;
        Ok(Value::str(s.trim().to_string()))
    });

    register(env, "abs", |_interp, args| match args.get(0) {
        Some(Value::Int(i)) => Ok(Value::Int(i.wrapping_abs())),
        Some(Value::Double(d)) => Ok(Value::Double(d.abs())),
        _ => Err(type_error("abs", "a number")),
    });

    register(env, "sqrt", |_interp, args| {
        let x = expect_num("sqrt", &args, 0)?;
        Ok(Value::Double(x.sqrt()))
    });

    register(env, "max", |_interp, args| {
        let a = args.get(0).ok_or_else(|| arity_error("max", "2", args.len()))?;
        let b = args.get(1).ok_or_else(|| arity_error("max", "2", args.len()))?;
        numeric_binary(a, b, f64::max)
    });

    register(env, "min", |_interp, args| {
        let a = args.get(0).ok_or_else(|| arity_error("min", "2", args.len()))?;
        let b = args.get(1).ok_or_else(|| arity_error("min", "2", args.len()))?;
        numeric_binary(a, b, f64::min)
    });
}

fn register(
    env: &Env,
    name: &str,
    f: impl Fn(&mut Interpreter, Vec<Value>) -> Result<Value, Flow> + 'static,
) {
    env.define(
        name,
        Value::Native(Rc::new(NativeValue {
            name: name.to_string(),
            func: Box::new(f),
        })),
        false,
    );
}

fn write_out(interp: &mut Interpreter, text: &str) -> Result<(), Flow> {
    write!(interp.out, "{text}").map_err(|e| RuntimeError::new(format!("io error: {e}"), Span::default()).into())
}

fn arity_error(name: &str, expected: &str, got: usize) -> Flow {
    RuntimeError::new(format!("{name} expects {expected} argument(s), got {got}"), Span::default()).into()
}

fn type_error(name: &str, expected: &str) -> Flow {
    RuntimeError::new(format!("{name} expects {expected}"), Span::default()).into()
}

fn expect_str(name: &str, args: &[Value], idx: usize) -> Result<Rc<str>, Flow> {
    match args.get(idx) {
        Some(Value::Str(s)) => Ok(s.clone()),
        _ => Err(type_error(name, "a String argument")),
    }
}

fn expect_num(name: &str, args: &[Value], idx: usize) -> Result<f64, Flow> {
    match args.get(idx) {
        Some(Value::Int(i)) => Ok(*i as f64),
        Some(Value::Double(d)) => Ok(*d),
        _ => Err(type_error(name, "a numeric argument")),
    }
}

fn numeric_binary(a: &Value, b: &Value, f: impl Fn(f64, f64) -> f64) -> Result<Value, Flow> {
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
                _ => return Err(type_error("max/min", "numeric arguments")),
            };
            let yf = match b {
                Value::Int(y) => *y as f64,
                Value::Double(y) => *y,
                _ => return Err(type_error("max/min", "numeric arguments")),
            };
            Ok(Value::Double(f(xf, yf)))
        }
    }
}
