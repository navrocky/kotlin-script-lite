use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{FunBody, Param, TypeAnnotation};
use crate::environment::Env;
use crate::error::{Flow, Span};
use crate::gc::{GcHandle, GcRef, Trace};
use crate::interpreter::Interpreter;

/// Invariant: a native function must never capture an `Env` or a `Value`
/// in its own Rust closure — `Interpreter::collect_cycles`'s reachability
/// trace (`value_refs`, `Trace::trace_refs`) can only see through `Value`'s
/// own variants, not into an opaque `Box<dyn Fn>`'s captures. An `Env`
/// captured that way would be invisible to the trace, so a collection pass
/// could wrongly treat it as unreachable garbage and clear it while the
/// closure still silently holds it — corrupting state, not just leaking
/// it. Every registration in `stdlib.rs` operates only on its own
/// `Vec<Value>` arguments and the `&mut Interpreter` passed in; keep it
/// that way.
pub type NativeFn = dyn Fn(&mut Interpreter, Vec<Value>, Span) -> Result<Value, Flow>;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Double(f64),
    Bool(bool),
    Str(Rc<str>),
    Unit,
    Null,
    // `GcRef` is deliberately `pub(crate)`, not part of `ksl-core`'s public
    // API — external code should only ever reach a `List`'s contents
    // through `Value`'s own methods (`Display`, `values_equal`, ...), the
    // same way it already can't construct/inspect `FunctionValue.closure`
    // directly beyond what `Value`'s own API exposes.
    #[allow(private_interfaces)]
    List(GcRef<Vec<Value>>),
    Function(Rc<FunctionValue>),
    Native(Rc<NativeValue>),
    /// Real user-defined function overloading: a second (or later) `fun`
    /// sharing a name already bound to a `Function`/`Overload` in the same
    /// `Env` groups with it here instead of overwriting it (see
    /// `Env::define`'s doc comment) — resolved to one specific candidate
    /// by real argument type at the call site (`Interpreter::call_value`),
    /// the exact same `types::resolve_overload` core the *static* checker
    /// already uses for this. Never produced for a lambda (anonymous,
    /// never shares a name with anything to group with).
    Overload(Rc<Vec<Rc<FunctionValue>>>),
}

/// A `List`'s backing store is itself a GC-tracked node (not just something
/// `Env`/`Object` transparently see through) — necessary, not incidental:
/// if two different tracked nodes each held a reference to the *same*
/// shared list, and the list's own contents were only walked transparently
/// through each holder's own trace, `Interpreter::collect_cycles` would
/// double-count the list's single real edge to whatever it contains once
/// per holder, and — worse — would have no way to notice the list itself
/// being held from *outside* the tracked graph entirely (e.g. handed back
/// to an embedder from a call and kept there between calls). Giving `List`
/// its own node, exactly like `Env`, is what lets the algorithm's real
/// `Rc::strong_count` check see that correctly.
impl Trace for Vec<Value> {
    fn trace_refs(&self, out: &mut Vec<GcHandle>) {
        for v in self {
            value_refs(v, out);
        }
    }

    fn clear(&mut self) {
        Vec::clear(self);
    }
}

/// Every GC-tracked node (`Env`, `List`, and — once they exist — a class
/// instance's fields, a `Map`'s entries, ...) this value directly (one
/// level) holds. This exhaustive match — no wildcard arm — is the one
/// place in the whole mechanism that can't be automated away: it's what
/// makes adding a new `Value` variant that can hold a `GcRef` a compile
/// error (`non-exhaustive patterns`) until this function is updated, so
/// forgetting to wire a new container into cycle collection isn't a
/// silent bug, it's a build failure.
pub(crate) fn value_refs(v: &Value, out: &mut Vec<GcHandle>) {
    match v {
        Value::Function(f) => out.push(f.closure.handle()),
        Value::Overload(group) => out.extend(group.iter().map(|f| f.closure.handle())),
        Value::List(l) => out.push(l.handle()),
        Value::Int(_)
        | Value::Double(_)
        | Value::Bool(_)
        | Value::Str(_)
        | Value::Unit
        | Value::Null
        | Value::Native(_) => {}
    }
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
    /// Which file this function was declared in — `Interpreter`'s
    /// `current_file` at the moment the `fun`/lambda was evaluated. Used
    /// for stack-trace frames (`error::StackFrame`) and, once a call site
    /// is statically pre-resolved, to find the right runtime function
    /// again by matching `decl_span` regardless of which file the call
    /// happens to run from.
    pub file: Arc<Path>,
    /// The span of the `fun`/lambda declaration itself (not a call site) —
    /// stable identity for "this exact declared function", used to match
    /// a statically pre-resolved overload decision back to the concrete
    /// `FunctionValue` built for it at runtime.
    pub decl_span: Span,
    /// This function's own file's resolver-computed overload decisions —
    /// `Interpreter::current_resolved` at the moment this `fun`/lambda was
    /// evaluated (same moment `file`/`decl_span` are captured). Restored
    /// onto `Interpreter::current_resolved` for the duration of a call to
    /// this function (see `call_function`), so a call site *inside* this
    /// function's own body resolves against *its* file's decisions even
    /// when called from a different file's code — without this, `Span`
    /// having no file component would let two different files' call-site
    /// spans collide in one shared table.
    pub resolved_calls: Rc<HashMap<Span, Span>>,
}

/// A human-readable rendering of a function's call signature for stack
/// traces, e.g. `"add(a: Int, b: Int)"` or `"<lambda>(x)"` for an
/// unnamed/unannotated one — independent of `resolver.rs`'s own
/// (differently-scoped) signature text, since `value.rs` must not depend
/// on the resolver (see the interpreter/resolver independence this crate
/// otherwise relies on).
pub fn function_signature(f: &FunctionValue) -> String {
    let name = f.name.as_deref().unwrap_or("<lambda>");
    let params = f
        .params
        .iter()
        .map(|p| match &p.type_annotation {
            Some(ann) => format!("{}: {}", p.name, type_annotation_text(ann)),
            None => p.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({params})")
}

fn type_annotation_text(ann: &TypeAnnotation) -> String {
    match ann {
        TypeAnnotation::Named(name, _) => name.clone(),
        TypeAnnotation::Function(params, ret, _) => {
            let params = params.iter().map(type_annotation_text).collect::<Vec<_>>().join(", ");
            format!("({params}) -> {}", type_annotation_text(ret))
        }
        TypeAnnotation::Nullable(inner, _) => format!("{}?", type_annotation_text(inner)),
    }
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
            Value::Null => "Null",
            Value::List(_) => "List",
            Value::Function(_) | Value::Native(_) | Value::Overload(_) => "Function",
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
            (Value::Null, Value::Null) => true,
            (Value::List(x), Value::List(y)) => {
                GcRef::ptr_eq(x, y) || {
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
            Value::Null => write!(f, "null"),
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
            Value::Overload(group) => {
                write!(f, "<function {}>", group[0].name.as_deref().unwrap_or("lambda"))
            }
        }
    }
}
