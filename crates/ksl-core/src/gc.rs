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

use std::cell::{Cell, Ref, RefCell, RefMut};
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

/// How many `register` calls happen between each self-throttled prune (see
/// `GcRegistry::register`'s own doc comment) — small enough that a tight
/// allocation loop (e.g. a `while` loop body, which allocates a fresh
/// `Env` — one `GcRef::new` — every single iteration, see `interpreter.rs`'s
/// `While`/`For`/`DoWhile` handling) never accumulates more than a couple
/// of thousand dead entries before they're swept, large enough that the
/// `retain` scan this triggers (linear in the registry's current size,
/// which this same throttling keeps bounded) stays a rare, cheap event
/// relative to how often `register` itself is called.
const SELF_PRUNE_INTERVAL: usize = 2048;

/// Every live `GcRef` created through a given `Interpreter`'s lifetime —
/// weak handles only, so the registry itself never keeps anything alive
/// (that would defeat the point: nothing could ever become collectible).
/// One instance lives per `Interpreter` (see `Interpreter::gc`) — never
/// global or thread-local, since multiple independent `Interpreter`s can
/// be alive in one process at once, each needing its own.
#[derive(Default)]
pub(crate) struct GcRegistry {
    handles: RefCell<Vec<GcHandle>>,
    /// How many `register` calls have happened since the last prune (self-
    /// throttled here, or a side effect of `live_nodes` resetting it) —
    /// see `register`'s own doc comment for why this exists at all.
    since_prune: Cell<usize>,
}

impl GcRegistry {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(GcRegistry::default())
    }

    /// Registers a freshly-created node, self-throttling a cheap dead-entry
    /// sweep (`prune`) every `SELF_PRUNE_INTERVAL` registrations.
    ///
    /// This is *not* cycle collection (`Interpreter::collect_cycles`,
    /// which needs a real reachability analysis and is only sound to run
    /// between top-level calls — see its own doc comment) — it only ever
    /// drops an entry whose `Weak::upgrade()` already reports `None`, an
    /// objective fact ("no strong references anywhere") rather than a
    /// heuristic about *reachability*, so unlike `collect_cycles` it can
    /// never mistakenly clear something a still-running call still needs,
    /// and is safe to run from literally anywhere, including here, in the
    /// middle of the hottest allocation path there is.
    ///
    /// Without this, `handles` is a pure-append `Vec` for the entire
    /// process lifetime: every `While`/`For`/`DoWhile` loop iteration
    /// allocates a fresh child `Env` (one `GcRef::new` call each —
    /// `interpreter.rs`'s loop handling), and ordinary (non-cyclic) `Rc`
    /// drop already reclaims each iteration's own `Env` correctly the
    /// moment it ends — but with nothing ever pruning this registry's own
    /// *bookkeeping* `Weak` entry for it, a long-running loop (a real-time
    /// game's main loop, in particular) grows this `Vec` completely
    /// unbounded for as long as the process runs, even though the actual
    /// `Env`/`List`/`Object` data it once pointed at was freed correctly
    /// all along. This was a real, reported bug (unbounded memory growth
    /// and progressive slowdown proportional to how long a script had been
    /// running), not a hypothetical one.
    fn register(&self, handle: GcHandle) {
        self.handles.borrow_mut().push(handle);
        let n = self.since_prune.get() + 1;
        if n >= SELF_PRUNE_INTERVAL {
            self.prune();
            self.since_prune.set(0);
        } else {
            self.since_prune.set(n);
        }
    }

    /// Drops every entry whose underlying node no longer has any strong
    /// reference anywhere — shared by `register`'s self-throttling and
    /// `live_nodes`, the two places that need this exact scan.
    fn prune(&self) {
        self.handles.borrow_mut().retain(|h| h.upgrade().is_some());
    }

    /// Every still-live node right now, upgraded once for the duration of
    /// one `collect_cycles` pass — pruning dead entries as a side effect
    /// (and resetting the self-throttle counter, so a `collect_cycles`
    /// call never leaves a stale count behind).
    pub(crate) fn live_nodes(&self) -> Vec<Rc<RefCell<dyn Trace>>> {
        self.prune();
        self.since_prune.set(0);
        self.handles.borrow().iter().filter_map(|h| h.upgrade()).collect()
    }

    /// Test-only: how many still-live nodes the registry currently holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.prune();
        self.handles.borrow().len()
    }

    /// Test-only: the registry's *raw* entry count, with no explicit prune
    /// forced first — unlike `len()` above, this reflects exactly what
    /// `register`'s own self-throttling has (or hasn't) already cleaned up
    /// on its own, which is the specific thing worth a regression test —
    /// see `tests::self_throttled_registry_never_grows_far_past_the_live_count`.
    #[cfg(test)]
    pub(crate) fn raw_len(&self) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Leaf;
    impl Trace for Leaf {
        fn trace_refs(&self, _out: &mut Vec<GcHandle>) {}
        fn clear(&mut self) {}
    }

    /// The exact bug a real user report traced to: a tight allocate-then-
    /// immediately-drop loop — mirroring a `while`/`for`/`do-while` loop
    /// body allocating a fresh `Env` every single iteration
    /// (`interpreter.rs`'s loop handling calls `Env::child`, i.e.
    /// `GcRef::new`, once per iteration), then letting it go out of scope
    /// at the end of that iteration, exactly like a real long-running
    /// script's main loop — must not grow the registry's own bookkeeping
    /// without bound. Before `register`'s self-throttling existed, this
    /// grew a real process's memory unbounded and slower with every
    /// iteration (confirmed against `tests/demos/snake.ksl`, whose render
    /// loop has exactly this shape, before this fix: ~277MB RSS after
    /// under 7 seconds of a tight nested-loop benchmark with the same
    /// allocation pattern this test uses).
    #[test]
    fn self_throttled_registry_never_grows_far_past_the_live_count() {
        let registry = GcRegistry::new();
        for _ in 0..10_000 {
            let node = GcRef::new(registry.clone(), Leaf);
            drop(node);
        }
        assert!(
            registry.raw_len() <= SELF_PRUNE_INTERVAL,
            "registry grew to {} entries after 10,000 allocate-then-drop cycles — self-throttling isn't working",
            registry.raw_len()
        );
    }

    /// A node that's still actually referenced must never be pruned,
    /// self-throttled or not — dead-entry pruning only ever removes an
    /// entry whose `Weak::upgrade()` already reports `None`.
    #[test]
    fn self_throttled_pruning_never_drops_a_still_live_node() {
        let registry = GcRegistry::new();
        let alive = GcRef::new(registry.clone(), Leaf);
        for _ in 0..(SELF_PRUNE_INTERVAL * 2) {
            drop(GcRef::new(registry.clone(), Leaf));
        }
        assert_eq!(registry.len(), 1, "the still-alive node should be the only survivor");
        drop(alive);
    }
}
