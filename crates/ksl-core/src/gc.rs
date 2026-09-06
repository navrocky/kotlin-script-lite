//! Generic machinery for detecting and breaking `Rc` reference cycles that
//! plain reference counting can never reclaim on its own — see
//! `Interpreter::collect_cycles`'s doc comment for the actual algorithm and
//! exactly when it's safe to run it. This module only provides the generic
//! mechanism (`Trace`, `GcRef`, the registry); it has no notion of `Env`,
//! `Value`, `List`, or any other concrete type in this crate — those live
//! in `environment.rs`/`value.rs`, which depend on this module, not the
//! other way around.
//!
//! Every container that can hold `Value`s in a way that might reach back
//! into the graph (`Env`'s bindings, a `List`'s elements, and — once they
//! exist — a class instance's fields, a `Map`'s entries, ...) should be a
//! `GcRef<T>` where `T: Trace`, never a bare `Rc<RefCell<T>>`. Because
//! `GcRef::new` is the *only* way to construct one, and it always
//! registers itself, there is no code path that can create such a
//! container without `collect_cycles` being able to see it — unlike a
//! hand-rolled `Rc<RefCell<T>>`, which nothing tracks unless every call
//! site separately remembers to register it. The one thing that can't be
//! automated this way: `value.rs`'s exhaustive match from a `Value` to the
//! `GcRef`(s) it directly holds (`value_refs`) must be written with no
//! wildcard arm, so the compiler — not a convention — refuses to build
//! when a new `Value` variant is added without updating it.

use std::cell::{Ref, RefCell, RefMut};
use std::rc::{Rc, Weak};

/// Anything that can hold other GC-tracked nodes through interior
/// mutability. Implemented once per container *shape* (`EnvData`,
/// `Vec<Value>`, ...) — never per `Value` variant.
pub(crate) trait Trace {
    /// Every GC-tracked node this container directly (one level) holds.
    fn trace_refs(&self, out: &mut Vec<GcHandle>);
    /// Forcibly drop everything this container holds, breaking any cycle
    /// running through it.
    fn clear(&mut self);
}

/// A non-owning, type-erased handle to a `GcRef`'s data — erased because
/// the registry and the trial-deletion algorithm in
/// `Interpreter::collect_cycles` need to treat every kind of node
/// uniformly, regardless of its concrete payload type.
#[derive(Clone)]
pub(crate) struct GcHandle(Weak<RefCell<dyn Trace>>);

impl GcHandle {
    pub(crate) fn upgrade(&self) -> Option<Rc<RefCell<dyn Trace>>> {
        self.0.upgrade()
    }
}

/// Every live `GcRef` created through a given `Interpreter`'s lifetime —
/// weak handles only, so the registry itself never keeps anything alive
/// (that would defeat the point: nothing could ever become collectible).
/// One instance lives per `Interpreter` (see `Interpreter::gc`) — never
/// global or thread-local, since multiple independent `Interpreter`s can
/// be alive in one process at once, each needing its own.
#[derive(Default)]
pub(crate) struct GcRegistry {
    handles: RefCell<Vec<GcHandle>>,
}

impl GcRegistry {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(GcRegistry::default())
    }

    fn register(&self, handle: GcHandle) {
        self.handles.borrow_mut().push(handle);
    }

    /// Every still-live node right now, upgraded once for the duration of
    /// one `collect_cycles` pass — pruning dead entries as a side effect.
    pub(crate) fn live_nodes(&self) -> Vec<Rc<RefCell<dyn Trace>>> {
        let mut handles = self.handles.borrow_mut();
        handles.retain(|h| h.upgrade().is_some());
        handles.iter().filter_map(|h| h.upgrade()).collect()
    }

    /// Test-only: how many still-live nodes the registry currently holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.handles.borrow_mut().retain(|h| h.upgrade().is_some());
        self.handles.borrow().len()
    }
}

/// A shared, interior-mutable, cycle-collectible container — the generic
/// replacement for a bare `Rc<RefCell<T>>` for anything that can hold
/// `Value`s reaching back into the graph. Carries its own registry handle
/// so a domain-specific *derived* constructor (like `Env::child`) never
/// needs a registry threaded through by its caller — only a genuinely
/// fresh, rootless `GcRef` (`Env::new`, a brand-new `List`) needs one
/// supplied explicitly, and that's always available wherever one is
/// needed (an existing `Env`'s own `.registry()`, or `Interpreter::gc`).
pub(crate) struct GcRef<T: Trace> {
    data: Rc<RefCell<T>>,
    registry: Rc<GcRegistry>,
}

impl<T: Trace> Clone for GcRef<T> {
    fn clone(&self) -> Self {
        GcRef { data: self.data.clone(), registry: self.registry.clone() }
    }
}

/// Downgrades to a type-erased `Weak` — a separate function, not inlined at
/// each call site, because the unsizing coercion (`Weak<RefCell<T>>` to
/// `Weak<RefCell<dyn Trace>>`) only happens automatically at a return-type
/// coercion site; `Rc::downgrade`'s own generic parameter can't be inferred
/// directly against the erased target type.
fn downgrade_dyn<T: Trace + 'static>(data: &Rc<RefCell<T>>) -> Weak<RefCell<dyn Trace>> {
    let weak: Weak<RefCell<T>> = Rc::downgrade(data);
    weak
}

impl<T: Trace + 'static> GcRef<T> {
    pub(crate) fn new(registry: Rc<GcRegistry>, value: T) -> Self {
        let data = Rc::new(RefCell::new(value));
        registry.register(GcHandle(downgrade_dyn(&data)));
        GcRef { data, registry }
    }

    pub(crate) fn registry(&self) -> Rc<GcRegistry> {
        self.registry.clone()
    }

    pub(crate) fn borrow(&self) -> Ref<'_, T> {
        self.data.borrow()
    }

    pub(crate) fn borrow_mut(&self) -> RefMut<'_, T> {
        self.data.borrow_mut()
    }

    pub(crate) fn handle(&self) -> GcHandle {
        GcHandle(downgrade_dyn(&self.data))
    }

    /// Test-only: `Env::strong_count`'s real underlying implementation —
    /// `collect_cycles` itself reads the same count directly via
    /// `Rc::strong_count` on the type-erased node, not through this.
    #[cfg(test)]
    pub(crate) fn strong_count(&self) -> usize {
        Rc::strong_count(&self.data)
    }

    pub(crate) fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(&a.data, &b.data)
    }

    /// A stable-for-this-value's-lifetime identity address — used for the
    /// default, identity-based `Display`/`hashCode` every class instance
    /// gets unless it overrides `toString`/`hashCode` itself (real
    /// Kotlin's own `Any` default: `ClassName@<hex address>`-shaped).
    pub(crate) fn addr(&self) -> usize {
        Rc::as_ptr(&self.data) as usize
    }
}
