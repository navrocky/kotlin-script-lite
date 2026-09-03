use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::ast::{FunBody, Param, TypeAnnotation};
use crate::environment::Env;
use crate::error::{Flow, Span};
use crate::interpreter::Interpreter;

pub type NativeFn = dyn Fn(&mut Interpreter, Vec<Value>, Span) -> Result<Value, Flow>;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Double(f64),
    Bool(bool),
    Str(Rc<str>),
    Unit,
    List(Rc<RefCell<Vec<Value>>>),
    Function(Rc<FunctionValue>),
    Native(Rc<NativeValue>),
}

pub struct FunctionValue {
    pub name: Option<String>,
    pub params: Vec<Param>,
    /// Raw return-type annotation, if any — always `None` for a lambda
    /// (`ExprKind::Lambda` has no return-type syntax). Resolved into a real
    /// `types::Type` by `check_type`, not here.
    pub return_type: Option<TypeAnnotation>,
    pub body: FunBody,
    pub closure: Env,
}

pub struct NativeValue {
    pub name: String,
    pub func: Box<NativeFn>,
}

impl Value {
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        Value::Str(s.into())
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "Int",
            Value::Double(_) => "Double",
            Value::Bool(_) => "Boolean",
            Value::Str(_) => "String",
            Value::Unit => "Unit",
            Value::List(_) => "List",
            Value::Function(_) | Value::Native(_) => "Function",
        }
    }

    pub fn is_truthy(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    pub fn values_equal(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Double(x), Value::Double(y)) => x == y,
            (Value::Int(x), Value::Double(y)) | (Value::Double(y), Value::Int(x)) => {
                (*x as f64) == *y
            }
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Unit, Value::Unit) => true,
            (Value::List(x), Value::List(y)) => {
                Rc::ptr_eq(x, y) || {
                    let (xb, yb) = (x.borrow(), y.borrow());
                    xb.len() == yb.len()
                        && xb.iter().zip(yb.iter()).all(|(a, b)| Value::values_equal(a, b))
                }
            }
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Double(d) => write!(f, "{d}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Unit => write!(f, "kotlin.Unit"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.borrow().iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Function(func) => {
                write!(f, "<function {}>", func.name.as_deref().unwrap_or("lambda"))
            }
            Value::Native(n) => write!(f, "<function {}>", n.name),
        }
    }
}
