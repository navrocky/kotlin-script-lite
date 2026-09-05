use std::collections::HashMap;
use std::rc::Rc;

use crate::gc::{GcHandle, GcRef, GcRegistry, Trace};
use crate::types::{self, Type};
use crate::value::{self, Value};

struct Binding {
    value: Value,
    mutable: bool,
    /// The variable's type, checked on reassignment in `assign`
    /// (`Type::Unknown` is never enforced — see `types`'s module doc
    /// comment). Set from an explicit type-hint annotation if the `val`/
    /// `var` declaration has one, else inferred from the initial value
    /// itself (`Type::from_value`) — matching Kotlin, where `var x = 5`
    /// locks `x` to `Int` just as surely as `var x: Int = 5` does (see
    /// `resolver.rs`'s identical `ValDecl` handling, which mirrors this at
    /// the static-checking layer).
    declared_type: Type,
}

struct EnvData {
    vars: HashMap<String, Binding>,
    parent: Option<Env>,
}

impl Trace for EnvData {
    fn trace_refs(&self, out: &mut Vec<GcHandle>) {
        if let Some(p) = &self.parent {
            out.push(p.handle());
        }
        for binding in self.vars.values() {
            value::value_refs(&binding.value, out);
        }
    }

    fn clear(&mut self) {
        self.vars.clear();
        self.parent = None;
    }
}

/// A lexical scope. Cheaply cloneable (a `GcRef` handle) so closures can
/// capture the environment active at their definition site. Backed by
/// `GcRef<EnvData>`, not a bare `Rc<RefCell<EnvData>>` — see `gc.rs`'s
/// module doc comment for why every such container needs to be a `GcRef`.
#[derive(Clone)]
pub struct Env(GcRef<EnvData>);

pub enum AssignError {
    Undefined,
    NotMutable,
    TypeMismatch(Type, Type),
}

impl Env {
    pub fn new() -> Self {
        Env(GcRef::new(GcRegistry::new(), EnvData { vars: HashMap::new(), parent: None }))
    }

    pub fn child(&self) -> Self {
        Env(GcRef::new(self.0.registry(), EnvData { vars: HashMap::new(), parent: Some(self.clone()) }))
    }

    /// `declared_type` is `Type::Unknown` for every binding except a
    /// `val`/`var` with an explicit type-hint annotation — see
    /// `Binding::declared_type`'s doc comment.
    pub fn define(&self, name: &str, value: Value, mutable: bool, declared_type: Type) {
        self.0
            .borrow_mut()
            .vars
            .insert(name.to_string(), Binding { value, mutable, declared_type });
    }

    /// Every name defined directly on this scope — not inherited from a
    /// parent — as `(name, value)` pairs. Used for wildcard `import a.b.*`
    /// (`importer.rs`): a package's own `Env` is always a direct child of
    /// `globals` (see `Interpreter::package_env`), so this returns exactly
    /// that package's own top-level declarations, nothing from `globals`
    /// itself or any sibling package.
    pub fn own_bindings(&self) -> Vec<(String, Value)> {
        self.0.borrow().vars.iter().map(|(k, b)| (k.clone(), b.value.clone())).collect()
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let data = self.0.borrow();
        match data.vars.get(name) {
            Some(b) => Some(b.value.clone()),
            None => data.parent.as_ref().and_then(|p| p.get(name)),
        }
    }

    /// Like `get`, but only ever looks at this exact scope — never a
    /// parent. Used by `StmtKind::FunDecl` execution to tell "a sibling
    /// `fun` already declared this name *in this same scope*" (real
    /// overloading — group with it, see `Value::Overload`) apart from "a
    /// same-named binding exists somewhere further out" (irrelevant
    /// shadowing, not a reason to group).
    pub fn get_own(&self, name: &str) -> Option<Value> {
        self.0.borrow().vars.get(name).map(|b| b.value.clone())
    }

    pub fn assign(&self, name: &str, value: Value) -> Result<(), AssignError> {
        let mut data = self.0.borrow_mut();
        if let Some(b) = data.vars.get_mut(name) {
            if !b.mutable {
                return Err(AssignError::NotMutable);
            }
            let actual = Type::from_value(&value);
            if b.declared_type != Type::Unknown && !types::is_assignable(&actual, &b.declared_type) {
                return Err(AssignError::TypeMismatch(actual, b.declared_type.clone()));
            }
            b.value = value;
            return Ok(());
        }
        let parent = data.parent.clone();
        drop(data);
        match parent {
            Some(p) => p.assign(name, value),
            None => Err(AssignError::Undefined),
        }
    }

    /// Forcibly drops every binding (and the parent link) this scope holds,
    /// breaking any `Rc` cycle running back through it (e.g. a named
    /// function's own `closure` pointing back at the scope that defines
    /// it — see `Interpreter`'s `Drop` impl, the only caller). Clearing
    /// `parent` too, not just `vars`, is deliberate: a cleared `Env` should
    /// be predictably, fully dead rather than silently falling through to a
    /// parent chain that may itself be mid-teardown for an unrelated
    /// reason.
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// Test-only: how many strong `Rc` handles point at this scope right
    /// now — used to prove a cycle exists (count stays above what's held
    /// externally) and that `clear`/`Interpreter`'s `Drop` actually breaks
    /// it. `collect_cycles` itself reads the same count generically, via
    /// `Rc::strong_count` on the type-erased node — see `gc::GcRef`.
    #[cfg(test)]
    pub(crate) fn strong_count(&self) -> usize {
        self.0.strong_count()
    }

    /// A non-owning, type-erased handle for `Interpreter::gc`'s registry
    /// and for `EnvData::trace_refs`'s own output — see `gc::GcHandle`.
    pub(crate) fn handle(&self) -> GcHandle {
        self.0.handle()
    }

    /// The registry this scope's chain of child scopes will register into
    /// — same as whatever `Interpreter` created the root of this chain,
    /// since `child` always derives it from `self` rather than taking one
    /// from the caller.
    pub(crate) fn registry(&self) -> Rc<GcRegistry> {
        self.0.registry()
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}
