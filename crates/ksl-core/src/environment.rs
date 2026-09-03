use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::types::{self, Type};
use crate::value::Value;

struct Binding {
    value: Value,
    mutable: bool,
    /// The variable's declared type, if it has one (`Type::Unknown`
    /// otherwise) — checked on reassignment in `assign`. Only ever set to a
    /// real type at the `val`/`var` declaration site that has an explicit
    /// type-hint annotation (mirrors `resolver.rs`'s `Symbol::declared`
    /// distinction: a hint-less `var`'s reassignments stay fully dynamic,
    /// exactly as before this type system existed).
    declared_type: Type,
}

struct EnvData {
    vars: HashMap<String, Binding>,
    parent: Option<Env>,
}

/// A lexical scope. Cheaply cloneable (`Rc` handle) so closures can capture
/// the environment active at their definition site.
#[derive(Clone)]
pub struct Env(Rc<RefCell<EnvData>>);

pub enum AssignError {
    Undefined,
    NotMutable,
    TypeMismatch(Type, Type),
}

impl Env {
    pub fn new() -> Self {
        Env(Rc::new(RefCell::new(EnvData {
            vars: HashMap::new(),
            parent: None,
        })))
    }

    pub fn child(&self) -> Self {
        Env(Rc::new(RefCell::new(EnvData {
            vars: HashMap::new(),
            parent: Some(self.clone()),
        })))
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

    pub fn get(&self, name: &str) -> Option<Value> {
        let data = self.0.borrow();
        match data.vars.get(name) {
            Some(b) => Some(b.value.clone()),
            None => data.parent.as_ref().and_then(|p| p.get(name)),
        }
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
}
