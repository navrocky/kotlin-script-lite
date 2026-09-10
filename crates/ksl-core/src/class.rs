//! Runtime representation of classes and objects (`docs/roadmap.md`'s
//! Phase 6) — the value-level counterpart of `ast.rs`'s `ClassParam`/
//! `ClassMember`/`StmtKind::ClassDecl`. See the plan this phase was built
//! from for the full design rationale (Any/Number builtin hierarchy,
//! `GcRef` integration); this module doc comment only covers the shapes.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{ClassParam, Expr, Stmt};
use crate::environment::Env;
use crate::error::{Flow, Span};
use crate::gc::{GcHandle, Trace};
use crate::interpreter::Interpreter;
use crate::types::Type;
use crate::value::{value_refs, Value};

/// The payload a `Value::Object`'s `GcRef` wraps — named to mirror
/// `environment.rs`'s `EnvData` (both name "the thing a `GcRef` wraps",
/// not a user-facing type; there is no separate `pub struct ObjectValue`).
///
/// Kept as a `HashMap`, unlike `environment.rs`'s `EnvData::vars` (which
/// switched to a linearly-scanned `Vec` after profiling showed it as the
/// dominant cost) — a *scope* is reliably tiny (a handful of locals), but
/// a user class's field count has no such guarantee (could reasonably run
/// into the tens/hundreds for a large class), where linear scan would
/// lose to hashing. `get_field`/`set_field` below keep call sites
/// decoupled from which container this actually is.
pub struct ObjectData {
    pub class: Rc<ClassValue>,
    pub fields: HashMap<String, Value>,
}

impl ObjectData {
    pub fn get_field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    pub fn set_field(&mut self, name: impl Into<String>, value: Value) {
        self.fields.insert(name.into(), value);
    }
}

/// An object's field map is itself a GC-tracked node — for exactly the
/// same reason `List`'s backing store is (see `value.rs`'s `impl Trace for
/// Vec<Value>`, right next to this one in spirit): a user script can write
/// `a.friend = b; b.friend = a`, a genuine reference cycle running through
/// two objects' own fields, and only a real tracked node lets
/// `Interpreter::collect_cycles`'s trial-deletion see it. `self.class`
/// deliberately contributes no edge here — a plain `Rc<ClassValue>` holds
/// no per-instance live state of its own; the one live field a class value
/// carries (`ClassValue.closure`) is traced at the `Value::Class` level
/// instead (see `value.rs`'s `value_refs`), not per-instance.
impl Trace for ObjectData {
    fn trace_refs(&self, out: &mut Vec<GcHandle>) {
        for v in self.fields.values() {
            value_refs(v, out);
        }
    }

    fn clear(&mut self) {
        self.fields.clear();
    }
}

pub struct FieldDecl {
    pub name: String,
    pub ty: Type,
    pub mutable: bool,
}

/// A concrete implementation a class's method name maps to.
pub enum MethodImpl {
    UserDefined(Rc<crate::value::FunctionValue>),
    Native(Box<NativeMethodFn>),
}

pub type NativeMethodFn = dyn Fn(&mut Interpreter, Value, Vec<Value>, Span) -> Result<Value, Flow>;

/// One step of a constructor's body, in source order — a promoted-or-not
/// property initializer, or an `init { ... }` block.
pub enum ConstructorStep {
    Property { name: String, mutable: bool, ty: Type, init: Expr },
    Init(Vec<Stmt>),
}

pub struct ClassValue {
    pub name: Rc<str>,
    /// Every field this class declares directly (constructor `val`/`var`
    /// params, promoted, plus body `Property` members) — used to look up a
    /// field's `mutable` flag for `FieldAssign`. Flat, not walking a
    /// superclass chain: no user-visible field-adding inheritance syntax
    /// exists yet (Phase 7).
    pub fields: Vec<FieldDecl>,
    pub methods: HashMap<String, MethodImpl>,
    pub constructor_params: Vec<ClassParam>,
    pub constructor_body: Vec<ConstructorStep>,
    /// Real, always `Some(..)` for a user class (implicitly `Any`), `None`
    /// only for `Any` itself. See `lookup_method`.
    pub superclass: Option<Rc<ClassValue>>,
    /// Interfaces this class documents itself as implementing — metadata
    /// only (shown on hover), no method dispatch or type-check consequence
    /// yet; real interface support is Phase 6.5.
    pub interfaces: Vec<&'static str>,
    /// The scope this class was declared in — deliberately the *same*
    /// `Env` every one of its methods' own `FunctionValue.closure` is set
    /// to (never a `child()` of it): `Env::trace` only ever looks
    /// *outward* via `parent`, so a distinct per-method closure would need
    /// its own separate edge from `Value::Class` to stay correctly
    /// reachable — keeping every method's closure identical to this field
    /// means the single edge `value.rs`'s `value_refs` already gives
    /// `Value::Class` covers all of them. Do not give a method its own
    /// `child()` scope at declaration time — only a transient one per
    /// *call* (`call_function_inner`'s existing `f.closure.child()`),
    /// exactly like an ordinary function.
    pub closure: Env,
    pub file: Arc<Path>,
    pub decl_span: Span,
}

/// Builds the real `Any`/`Number`/`Int`/`Double`/`Boolean`/`String`/`List`/
/// `Unit` hierarchy as actual `ClassValue`s — `Interpreter::new` calls this
/// once and keeps the result in `Interpreter.builtin_classes`. This is what
/// lets both `stdlib::call_builtin_method` (primitive `.`-method dispatch)
/// and `exec_class_decl` (every user class's implicit `superclass: Some(Any)`)
/// use the exact same `lookup_method` walk that `Value::Object` dispatch
/// already does, rather than a separate mechanism for builtins vs. user
/// classes. Each class's own method map is populated straight from
/// `stdlib::BUILTIN_METHODS`, filtered to that receiver name — `Any` gets
/// `toString`/`hashCode`/`equals`, `String` gets its four text methods,
/// everything else gets none of its own and relies entirely on inheriting
/// `Any`'s (see `BUILTIN_METHODS`'s own doc comment for why that's
/// correct, not just economical).
pub fn builtin_classes() -> HashMap<&'static str, Rc<ClassValue>> {
    fn methods_for(receiver: &'static str) -> HashMap<String, MethodImpl> {
        crate::stdlib::BUILTIN_METHODS
            .iter()
            .filter(|m| m.receiver == receiver)
            .map(|m| (m.symbol.name.to_string(), MethodImpl::Native(Box::new(m.call))))
            .collect()
    }

    fn make(name: &'static str, superclass: Option<Rc<ClassValue>>, interfaces: Vec<&'static str>) -> Rc<ClassValue> {
        Rc::new(ClassValue {
            name: Rc::from(name),
            fields: Vec::new(),
            methods: methods_for(name),
            constructor_params: Vec::new(),
            constructor_body: Vec::new(),
            superclass,
            interfaces,
            closure: Env::new(),
            file: Arc::from(Path::new("<builtin>")),
            decl_span: Span::default(),
        })
    }

    let mut map: HashMap<&'static str, Rc<ClassValue>> = HashMap::new();
    let any = make("Any", None, Vec::new());
    map.insert("Any", any.clone());
    let number = make("Number", Some(any.clone()), Vec::new());
    map.insert("Number", number.clone());
    map.insert("Int", make("Int", Some(number.clone()), Vec::new()));
    map.insert("Double", make("Double", Some(number), Vec::new()));
    map.insert("Boolean", make("Boolean", Some(any.clone()), Vec::new()));
    map.insert("String", make("String", Some(any.clone()), vec!["CharSequence", "Comparable"]));
    map.insert("List", make("List", Some(any.clone()), Vec::new()));
    map.insert("Unit", make("Unit", Some(any.clone()), Vec::new()));
    // `Exception`/`RuntimeException` — see `stdlib::exceptions` for their
    // real, native-function constructors (not `construct_object`, since
    // neither has a `constructor_params`/`constructor_body` — see this
    // function's own doc comment) and `interpreter.rs`'s `ExprKind::Try`
    // handling for how `catch` walks this same `superclass` chain to match
    // a thrown value against a catch clause's declared type.
    let exception = make("Exception", Some(any), Vec::new());
    map.insert("Exception", exception.clone());
    map.insert("RuntimeException", make("RuntimeException", Some(exception), Vec::new()));
    map
}

/// Walks `class`, then its superclass chain, returning the first class
/// that actually *defines* `method` (not merely inherits it) together with
/// its implementation — the single point of lookup both the interpreter
/// (call dispatch) and the resolver (static method-signature lookup) use,
/// so both agree on exactly the same rule.
pub fn lookup_method<'c>(class: &'c Rc<ClassValue>, method: &str) -> Option<(&'c Rc<ClassValue>, &'c MethodImpl)> {
    let mut current = class;
    loop {
        if let Some(m) = current.methods.get(method) {
            return Some((current, m));
        }
        current = current.superclass.as_ref()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::Env;

    fn minimal_class(name: &str, superclass: Option<Rc<ClassValue>>, methods: HashMap<String, MethodImpl>) -> Rc<ClassValue> {
        Rc::new(ClassValue {
            name: Rc::from(name),
            fields: Vec::new(),
            methods,
            constructor_params: Vec::new(),
            constructor_body: Vec::new(),
            superclass,
            interfaces: Vec::new(),
            closure: Env::new(),
            file: Arc::from(Path::new("<test>")),
            decl_span: Span::default(),
        })
    }

    fn dummy_native() -> MethodImpl {
        MethodImpl::Native(Box::new(|_interp, _receiver, _args, _span| Ok(Value::Unit)))
    }

    #[test]
    fn lookup_method_finds_a_method_on_the_class_itself() {
        let mut methods = HashMap::new();
        methods.insert("bark".to_string(), dummy_native());
        let dog = minimal_class("Dog", None, methods);

        let (owner, _) = lookup_method(&dog, "bark").expect("bark should be found");
        assert_eq!(owner.name.as_ref(), "Dog");
    }

    #[test]
    fn lookup_method_walks_up_the_superclass_chain() {
        let mut any_methods = HashMap::new();
        any_methods.insert("toString".to_string(), dummy_native());
        let any = minimal_class("Any", None, any_methods);
        let dog = minimal_class("Dog", Some(any), HashMap::new());

        let (owner, _) = lookup_method(&dog, "toString").expect("toString should be found via Any");
        assert_eq!(owner.name.as_ref(), "Any", "toString is defined on Any, not Dog");
    }

    #[test]
    fn lookup_method_prefers_the_classs_own_override_over_the_superclass() {
        let mut any_methods = HashMap::new();
        any_methods.insert("toString".to_string(), dummy_native());
        let any = minimal_class("Any", None, any_methods);
        let mut dog_methods = HashMap::new();
        dog_methods.insert("toString".to_string(), dummy_native());
        let dog = minimal_class("Dog", Some(any), dog_methods);

        let (owner, _) = lookup_method(&dog, "toString").expect("toString should be found");
        assert_eq!(owner.name.as_ref(), "Dog", "Dog's own override must win over Any's");
    }

    #[test]
    fn lookup_method_returns_none_when_nothing_in_the_chain_defines_it() {
        let any = minimal_class("Any", None, HashMap::new());
        let dog = minimal_class("Dog", Some(any), HashMap::new());
        assert!(lookup_method(&dog, "bark").is_none());
    }

    #[test]
    fn builtin_classes_wires_the_real_any_number_hierarchy() {
        let classes = builtin_classes();
        assert!(classes["Any"].superclass.is_none(), "Any is the root");
        assert!(Rc::ptr_eq(classes["Number"].superclass.as_ref().unwrap(), &classes["Any"]));
        assert!(Rc::ptr_eq(classes["Int"].superclass.as_ref().unwrap(), &classes["Number"]));
        assert!(Rc::ptr_eq(classes["Double"].superclass.as_ref().unwrap(), &classes["Number"]));
        for leaf in ["Boolean", "String", "List", "Unit"] {
            assert!(Rc::ptr_eq(classes[leaf].superclass.as_ref().unwrap(), &classes["Any"]), "{leaf} -> Any directly");
        }
    }

    #[test]
    fn builtin_classes_gives_every_type_its_own_and_inherited_methods() {
        let classes = builtin_classes();
        assert!(classes["String"].methods.contains_key("length"), "String has its own methods");
        assert!(!classes["Int"].methods.contains_key("toString"), "Int has no own override");
        let (owner, _) = lookup_method(&classes["Int"], "toString").expect("Int inherits toString via Number -> Any");
        assert_eq!(owner.name.as_ref(), "Any");
    }
}
