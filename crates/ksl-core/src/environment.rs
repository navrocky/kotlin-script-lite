use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::value::Value;

struct Binding {
    value: Value,
    mutable: bool,
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

    pub fn define(&self, name: &str, value: Value, mutable: bool) {
        self.0
            .borrow_mut()
            .vars
            .insert(name.to_string(), Binding { value, mutable });
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
