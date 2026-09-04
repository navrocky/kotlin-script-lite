//! A small, best-effort static type representation shared by the resolver
//! (static checking, LSP type-aware completion/hover) and the interpreter
//! (runtime enforcement) — see `docs/roadmap.md`'s type-system phase.
//!
//! Deliberately not a full type system: inference is syntax-directed and
//! single-pass (mirroring `resolver.rs`'s existing single-file walk), with
//! `Type::Unknown` as an explicit, named escape hatch. Anything genuinely
//! not statically inferable (a package-qualified call, a stdlib call, a
//! `FieldAccess`/`MethodCall` on an already-`Unknown` receiver, an
//! unrecognized type-hint string) resolves to `Unknown` rather than being
//! rejected — `is_assignable` treats `Unknown` as compatible with anything,
//! on either side. This mirrors `check_type`'s pre-existing behavior (an
//! unrecognized hint was never enforced) rather than introducing new
//! strictness.
//!
//! `Class`/`Generic` are reserved now, ahead of classes/generics actually
//! existing (`docs/roadmap.md`'s later phases), so those phases only need
//! to start populating a registry (`known_classes`/`generic_params`) rather
//! than redesign this enum or `resolve_type_hint`'s signature.

use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::TypeAnnotation;
use crate::value::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int,
    Double,
    Bool,
    String,
    Unit,
    List,
    /// `(A, B) -> R` — parameter types in order, then the return type.
    /// Structural, not nominal: two function types are compatible whenever
    /// their shapes line up (see `is_assignable`), no named function-type
    /// declaration exists to match by identity.
    Function(Vec<Type>, Box<Type>),
    /// A named class/object type (nominal) — unconstructable until classes
    /// exist (no `class` declaration is parsed yet). Compared by exact name
    /// only for now; inheritance-aware subtype compatibility ("Dog is-a
    /// Animal") is the inheritance phase's job to add to `is_assignable`,
    /// not a reason to change this variant's shape.
    Class(Rc<str>),
    /// A generic type parameter's own name (e.g. "T" in `class Box<T>`) —
    /// unconstructable until a declaration has a `type_params` list to draw
    /// from. Distinct from `Unknown` for display purposes (hover can show
    /// "T" instead of a vague "Unknown"), but behaves identically to
    /// `Unknown` in `is_assignable` — erasure means there's no real
    /// constraint to check.
    Generic(Rc<str>),
    /// The type of the `null` literal itself (Kotlin's `Nothing?`,
    /// simplified to its own leaf since this type system has no `Nothing`).
    /// Never appears as a *declared* annotation — only ever produced by
    /// `Type::from_value(&Value::Null)` and `NullLit`'s own inferred type.
    Null,
    /// `T?` — any other type, wrapped as nullable. Composes with every
    /// existing variant including `Function`/`Class`/`Generic`.
    Nullable(Box<Type>),
    /// Kotlin's real top type — supertype of everything. Unlike `Unknown`,
    /// this is a sound, one-directional relationship: anything is
    /// assignable *into* `Any`, but an `Any`-typed value isn't assignable
    /// back to a concrete type without an explicit cast (which the
    /// language doesn't have yet). See `is_assignable`.
    Any,
    /// Kotlin's real bottom type — subtype of everything, the type of
    /// `throw`/a call that never returns normally. Has no values: nothing
    /// in `Type::from_value` ever produces it. Assignable to anything,
    /// including `Nullable(T)`. Also the identity element for `if`/`when`
    /// branch-type joins (`LUB(Nothing, T) = T`) — see `resolver.rs`'s
    /// `ExprKind::If`/`When` handling.
    Nothing,
    /// Statically unknowable → always compatible. See the module doc
    /// comment.
    Unknown,
}

impl Type {
    /// Display name — matches `Value::type_name()`'s exact strings for the
    /// primitive variants, so error/hover text stays consistent with
    /// existing runtime wording. Owned (not `&str`) since a `Function`
    /// type's name is built from its parts, e.g. `(Int, String) -> Result`.
    pub fn name(&self) -> String {
        match self {
            Type::Int => "Int".to_string(),
            Type::Double => "Double".to_string(),
            Type::Bool => "Boolean".to_string(),
            Type::String => "String".to_string(),
            Type::Unit => "Unit".to_string(),
            Type::List => "List".to_string(),
            Type::Function(params, ret) => {
                let params = params.iter().map(Type::name).collect::<Vec<_>>().join(", ");
                format!("({params}) -> {}", ret.name())
            }
            Type::Class(name) | Type::Generic(name) => name.to_string(),
            Type::Null => "Null".to_string(),
            Type::Nullable(inner) => format!("{}?", inner.name()),
            Type::Any => "Any".to_string(),
            Type::Nothing => "Nothing".to_string(),
            Type::Unknown => "Unknown".to_string(),
        }
    }

    /// The exact runtime type of an already-evaluated value. `Function`
    /// reconstructs a best-effort signature from the value's own params/
    /// return-type annotation (`Type::Unknown` for anything unannotated —
    /// consistent with everywhere else an absent annotation means
    /// "genuinely not statically known", not "any type"). `Native` has no
    /// param list to draw from at all (see `value::NativeValue`) — its
    /// signature is represented by the empty/`Unknown` sentinel shape
    /// `Function(vec![], Unknown)`, which `is_assignable` treats as
    /// compatible with any function type rather than checking arity against
    /// it, since there's genuinely nothing to check. No `Value` variant
    /// produces `Class`/`Generic` today (there is no object system yet).
    pub fn from_value(v: &Value) -> Type {
        match v {
            Value::Int(_) => Type::Int,
            Value::Double(_) => Type::Double,
            Value::Bool(_) => Type::Bool,
            Value::Str(_) => Type::String,
            Value::Unit => Type::Unit,
            Value::Null => Type::Null,
            Value::List(_) => Type::List,
            Value::Function(f) => {
                let (param_types, ret_ty) = function_signature_types(f);
                Type::Function(param_types, Box::new(ret_ty))
            }
            Value::Native(_) => Type::Function(Vec::new(), Box::new(Type::Unknown)),
            // No one fixed function type to report — real per-candidate
            // types live in `Interpreter::call_value`'s own overload
            // resolution, which needs the actual argument types anyway
            // and doesn't go through `Type::from_value` for this.
            Value::Overload(_) => Type::Unknown,
        }
    }
}

/// A `FunctionValue`'s real, checkable `(param types, return type)` —
/// shared by `Type::from_value`'s `Function` arm and
/// `Interpreter::call_value`'s runtime overload dispatch (`Value::Overload`),
/// so both derive a function's static shape the same one way.
/// `generic_params`/`known_classes` are always empty here (same as
/// everywhere else a `FunctionValue`'s own annotations are resolved
/// outside of a real enclosing declaration's own generic/class context —
/// see `resolve_type_annotation`'s doc comment).
pub fn function_signature_types(f: &crate::value::FunctionValue) -> (Vec<Type>, Type) {
    let empty_params: Vec<String> = Vec::new();
    let empty_classes: HashSet<Rc<str>> = HashSet::new();
    let param_types = f
        .params
        .iter()
        .map(|p| match &p.type_annotation {
            Some(ann) => resolve_type_annotation(ann, &empty_params, &empty_classes),
            None => Type::Unknown,
        })
        .collect();
    let ret_ty = match &f.return_type {
        Some(ann) => resolve_type_annotation(ann, &empty_params, &empty_classes),
        None => Type::Unknown,
    };
    (param_types, ret_ty)
}

/// Resolves a raw type-hint name (`Param.type_annotation`/`ValDecl.type_annotation`/a
/// function's return-type annotation, in its `TypeAnnotation::Named` form)
/// to a `Type`, given the generic parameter names in scope (from an
/// enclosing declaration's own `<T, ...>` list) and the set of class names
/// known so far.
///
/// Both context arguments are always empty today — no declaration has a
/// `type_params` list yet, and no `class` declaration is parsed yet — they
/// exist now purely so the classes/generics phases only need to start
/// passing real data in, never change this function's signature or add a
/// new resolution kind.
///
/// Resolution order: a known primitive name resolves directly; else a name
/// matching `generic_params` resolves to `Type::Generic`; else a name
/// matching `known_classes` resolves to `Type::Class`; else `Type::Unknown`
/// (formalizes `check_type`'s old whitelist-plus-fallback as one named
/// function instead of a duplicated match).
/// Every primitive type name `resolve_type_hint` recognizes directly (not
/// generics or classes), paired with the `Type` it resolves to — the
/// single source of truth both `resolve_type_hint` and anything outside
/// this function that needs to list "the built-in type names" (e.g.
/// `ksl-lsp`'s completion when the cursor sits in type-annotation
/// position) derive from, so the two can never drift apart the way a
/// separate hand-written name-only list once could (it was missing
/// `"Nothing"`, present here but never added to that list by hand when
/// `Nothing` was added — see `git log` on this file for the pre-
/// unification shape, if curious).
pub const PRIMITIVE_TYPES: &[(&str, Type)] = &[
    ("Int", Type::Int),
    ("Double", Type::Double),
    ("Boolean", Type::Bool),
    ("String", Type::String),
    ("Unit", Type::Unit),
    ("List", Type::List),
    ("Any", Type::Any),
    ("Nothing", Type::Nothing),
];

fn resolve_type_hint(hint: &str, generic_params: &[String], known_classes: &HashSet<Rc<str>>) -> Type {
    if let Some((_, ty)) = PRIMITIVE_TYPES.iter().find(|(name, _)| *name == hint) {
        return ty.clone();
    }
    if generic_params.iter().any(|p| p == hint) {
        return Type::Generic(hint.into());
    }
    if known_classes.contains(hint) {
        return Type::Class(hint.into());
    }
    Type::Unknown
}

/// Resolves a full `TypeAnnotation` tree (a plain name, or a function type
/// like `(Int, String) -> Result`) to a `Type`, recursing into a function
/// type's parameter/return annotations. An unresolvable name anywhere in
/// the tree resolves to `Type::Unknown` at that position only — it doesn't
/// collapse the whole annotation to `Unknown` (a caller wanting to flag
/// each bad name individually, like the resolver's own diagnostics, walks
/// the `TypeAnnotation` tree itself; see `resolver.rs`'s
/// `resolve_type_annotation_here`).
pub fn resolve_type_annotation(
    ann: &TypeAnnotation,
    generic_params: &[String],
    known_classes: &HashSet<Rc<str>>,
) -> Type {
    match ann {
        TypeAnnotation::Named(name, _) => resolve_type_hint(name, generic_params, known_classes),
        TypeAnnotation::Function(params, ret, _) => {
            let param_types =
                params.iter().map(|p| resolve_type_annotation(p, generic_params, known_classes)).collect();
            let ret_ty = resolve_type_annotation(ret, generic_params, known_classes);
            Type::Function(param_types, Box::new(ret_ty))
        }
        TypeAnnotation::Nullable(inner, _) => {
            Type::Nullable(Box::new(resolve_type_annotation(inner, generic_params, known_classes)))
        }
    }
}

/// Whether a value of type `from` can be used where `to` is expected.
/// `Nothing` on the `from` side is always compatible (it has no values, so
/// there's nothing that could fail); `Unknown` or `Generic` on **either**
/// side is always compatible (the named escape hatch — see the module doc
/// comment); anything is assignable **into** `Any` on the `to` side, but
/// (unlike `Unknown`) not the other way around — an `Any`-typed value isn't
/// assignable back to a concrete type without an explicit cast; `Class`
/// compares by exact name only (no subtyping yet); `Function` compares
/// structurally,
/// element-wise (no variance — matching the approved plan's "exact
/// equality in v1"), except for the `Value::Native` sentinel shape (empty
/// params, `Unknown` return — see `Type::from_value`), which has nothing
/// real to check against and so is always compatible; everything else
/// requires exact equality. No numeric widening — `Int`/`Double` mixing is
/// an arithmetic result-type rule (see `resolver.rs`'s `infer_type`), not
/// an assignment rule (matches verified current behavior: `val x: Double =
/// 5` is already a type-mismatch error today).
pub fn is_assignable(from: &Type, to: &Type) -> bool {
    match (from, to) {
        (Type::Nothing, _) => true,
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Generic(_), _) | (_, Type::Generic(_)) => true,
        (_, Type::Any) => true,
        (Type::Null, Type::Nullable(_)) => true,
        (Type::Nullable(a), Type::Nullable(b)) => is_assignable(a, b),
        (a, Type::Nullable(b)) => is_assignable(a, b),
        (Type::Class(a), Type::Class(b)) => a == b,
        (Type::Function(fp, fr), Type::Function(tp, tr)) => {
            if fp.is_empty() && **fr == Type::Unknown {
                true
            } else {
                fp.len() == tp.len()
                    && fp.iter().zip(tp.iter()).all(|(a, b)| is_assignable(a, b))
                    && is_assignable(fr, tr)
            }
        }
        (a, b) => a == b,
    }
}

/// The outcome of resolving a call's real argument types against a set of
/// candidate overloads' parameter types — see `resolve_overload`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverloadResolution {
    /// Exactly one candidate (by index into the input slice) is compatible.
    Unique(usize),
    /// More than one candidate is still possible — typically because some
    /// argument's own type is `Unknown`, so it can't rule any of them out.
    /// The caller decides what to do with this (e.g. use a shared return
    /// type if every still-possible candidate agrees on one, else treat
    /// the whole call as `Unknown`).
    Ambiguous(Vec<usize>),
    /// No candidate accepts this many arguments at all — arity is a
    /// syntactic fact, so this is always a real mismatch, regardless of
    /// whether any argument's own type is `Unknown`.
    ArityMismatch,
    /// At least one candidate accepts this many arguments, but none of
    /// those is type-compatible with the given argument types.
    NoMatch,
}

/// Real, Kotlin-style overload resolution by arity and argument type —
/// generic over *any* set of candidate signatures (stdlib functions, user-
/// declared `fun` overloads, or anything else with a parameter-type list),
/// so the exact same mechanism applies whether the function being called is
/// a builtin or user-defined; callers just need to line their own candidate
/// list up with `candidate_params`.
///
/// `candidate_params[i]` is candidate `i`'s parameter types, in declaration
/// order. Resolution: filter to the candidates whose arity matches
/// `arg_types.len()` (`ArityMismatch` if none do — a real error regardless
/// of `Unknown`, since argument *count* is never ambiguous); among those,
/// prefer any candidate that's an **exact** type match at every position
/// (so a specific `Int` overload wins over a same-arity `Any`/generic
/// overload when called with a concrete `Int`, without needing a full
/// Kotlin-style most-specific-overload ranking); otherwise fall back to
/// plain assignability (`is_assignable`, with `Type::Unknown` — on either
/// side — treated as compatible with anything, same as everywhere else
/// `Unknown` appears). Exactly one compatible candidate is `Unique`; more
/// than one is `Ambiguous` (letting the caller decide, e.g. by checking
/// whether they all agree on a return type anyway); none is `NoMatch`.
pub fn resolve_overload(candidate_params: &[&[Type]], arg_types: &[Type]) -> OverloadResolution {
    let arity_matches: Vec<usize> = candidate_params
        .iter()
        .enumerate()
        .filter(|(_, params)| params.len() == arg_types.len())
        .map(|(i, _)| i)
        .collect();
    if arity_matches.is_empty() {
        return OverloadResolution::ArityMismatch;
    }

    let exact: Vec<usize> = arity_matches
        .iter()
        .copied()
        .filter(|&i| candidate_params[i].iter().zip(arg_types).all(|(p, a)| p == a))
        .collect();
    let compatible = if !exact.is_empty() {
        exact
    } else {
        arity_matches
            .into_iter()
            .filter(|&i| {
                candidate_params[i]
                    .iter()
                    .zip(arg_types)
                    .all(|(p, a)| *a == Type::Unknown || *p == Type::Unknown || is_assignable(a, p))
            })
            .collect()
    };

    match compatible.len() {
        1 => OverloadResolution::Unique(compatible[0]),
        0 => OverloadResolution::NoMatch,
        _ => OverloadResolution::Ambiguous(compatible),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_type_hint_known_primitives() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        for (hint, expected) in [
            ("Int", Type::Int),
            ("Double", Type::Double),
            ("Boolean", Type::Bool),
            ("String", Type::String),
            ("Unit", Type::Unit),
            ("List", Type::List),
        ] {
            assert_eq!(resolve_type_hint(hint, &empty_params, &empty_classes), expected);
        }
    }

    #[test]
    fn primitive_types_all_resolve_to_the_type_they_declare() {
        // `resolve_type_hint` now reads its primitive arms directly from
        // `PRIMITIVE_TYPES`, so this also structurally guarantees the
        // reverse direction (every primitive `resolve_type_hint` arm is
        // listed here) — not just that every listed name resolves to
        // *some* primitive, which is all the pre-unification version of
        // this test could check (and which is exactly how `"Nothing"`
        // went missing from the old name-only list without being caught).
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        for (name, expected_ty) in PRIMITIVE_TYPES {
            let ty = resolve_type_hint(name, &empty_params, &empty_classes);
            assert_eq!(&ty, expected_ty, "{name} should resolve to {expected_ty:?}");
        }
    }

    #[test]
    fn resolve_type_hint_unrecognized_name_is_unknown() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        assert_eq!(resolve_type_hint("Frobnicator", &empty_params, &empty_classes), Type::Unknown);
    }

    /// Forward-compat contract for the not-yet-implemented classes/generics
    /// phases: locked in now via direct calls with synthetic context, since
    /// no real syntax can produce these paths yet.
    #[test]
    fn resolve_type_hint_generic_param_in_scope() {
        let params = vec!["T".to_string()];
        let classes: HashSet<Rc<str>> = HashSet::new();
        assert_eq!(resolve_type_hint("T", &params, &classes), Type::Generic("T".into()));
    }

    #[test]
    fn resolve_type_hint_known_class_name() {
        let params: Vec<String> = Vec::new();
        let mut classes: HashSet<Rc<str>> = HashSet::new();
        classes.insert("Dog".into());
        assert_eq!(resolve_type_hint("Dog", &params, &classes), Type::Class("Dog".into()));
    }

    #[test]
    fn from_value_matches_value_type_name() {
        // Every `Value` variant whose `Type::from_value` result names
        // itself the same way `Value::type_name()` does — i.e. every
        // variant except `Function`/`Native`, covered separately below,
        // since those two are a deliberate exception, not an oversight.
        assert_eq!(Type::from_value(&Value::Int(1)).name(), Value::Int(1).type_name());
        assert_eq!(Type::from_value(&Value::Double(1.5)).name(), Value::Double(1.5).type_name());
        assert_eq!(Type::from_value(&Value::Bool(true)).name(), Value::Bool(true).type_name());
        assert_eq!(Type::from_value(&Value::str("x")).name(), Value::str("x").type_name());
        assert_eq!(Type::from_value(&Value::Unit).name(), Value::Unit.type_name());
        let list = Value::List(Rc::new(std::cell::RefCell::new(Vec::new())));
        assert_eq!(Type::from_value(&list).name(), list.type_name());
    }

    /// `Function`/`Native` are the one deliberate exception to the rule
    /// `from_value_matches_value_type_name` checks: `Value::type_name()`
    /// reports the literal word `"Function"` for both, but
    /// `Type::from_value` gives each a real structural
    /// `Type::Function(params, ret)`, whose `.name()` renders as a
    /// signature like `"() -> Unknown"`, not the word `"Function"` — so
    /// these two are *expected* to disagree, not a gap in the test above.
    #[test]
    fn from_value_function_and_native_intentionally_diverge_from_type_name() {
        let f = Value::Function(Rc::new(crate::value::FunctionValue {
            name: Some("f".to_string()),
            params: Vec::new(),
            return_type: None,
            body: crate::ast::FunBody::Block(Vec::new()),
            closure: crate::environment::Env::new(),
            file: std::sync::Arc::from(std::path::Path::new("<test>")),
            decl_span: crate::error::Span::default(),
            resolved_calls: std::rc::Rc::new(std::collections::HashMap::new()),
        }));
        assert_eq!(f.type_name(), "Function");
        assert_ne!(Type::from_value(&f).name(), f.type_name());

        let native = Value::Native(Rc::new(crate::value::NativeValue {
            name: "n".to_string(),
            func: Box::new(|_, _, _| Ok(Value::Unit)),
        }));
        assert_eq!(native.type_name(), "Function");
        assert_ne!(Type::from_value(&native).name(), native.type_name());
    }

    #[test]
    fn from_value_null_is_type_null() {
        assert_eq!(Type::from_value(&Value::Null), Type::Null);
        assert_eq!(Type::from_value(&Value::Null).name(), Value::Null.type_name());
    }

    #[test]
    fn nullable_type_name_appends_question_mark() {
        assert_eq!(Type::Nullable(Box::new(Type::String)).name(), "String?");
        assert_eq!(Type::Null.name(), "Null");
    }

    #[test]
    fn is_assignable_null_to_nullable_is_true() {
        assert!(is_assignable(&Type::Null, &Type::Nullable(Box::new(Type::String))));
    }

    #[test]
    fn is_assignable_bare_type_widens_to_nullable() {
        assert!(is_assignable(&Type::String, &Type::Nullable(Box::new(Type::String))));
    }

    #[test]
    fn is_assignable_nullable_does_not_narrow_to_bare_type() {
        assert!(!is_assignable(&Type::Nullable(Box::new(Type::String)), &Type::String));
    }

    #[test]
    fn is_assignable_null_to_non_nullable_is_false() {
        assert!(!is_assignable(&Type::Null, &Type::String));
    }

    #[test]
    fn is_assignable_nullable_to_nullable_recurses() {
        assert!(is_assignable(&Type::Nullable(Box::new(Type::Int)), &Type::Nullable(Box::new(Type::Int))));
        assert!(!is_assignable(&Type::Nullable(Box::new(Type::Int)), &Type::Nullable(Box::new(Type::String))));
    }

    #[test]
    fn is_assignable_unknown_is_always_compatible() {
        assert!(is_assignable(&Type::Unknown, &Type::Int));
        assert!(is_assignable(&Type::Int, &Type::Unknown));
        assert!(is_assignable(&Type::Unknown, &Type::Unknown));
    }

    #[test]
    fn resolve_type_hint_any_and_nothing() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        assert_eq!(resolve_type_hint("Any", &empty_params, &empty_classes), Type::Any);
        assert_eq!(resolve_type_hint("Nothing", &empty_params, &empty_classes), Type::Nothing);
    }

    #[test]
    fn any_and_nothing_type_names() {
        assert_eq!(Type::Any.name(), "Any");
        assert_eq!(Type::Nothing.name(), "Nothing");
    }

    #[test]
    fn is_assignable_anything_widens_to_any() {
        assert!(is_assignable(&Type::Int, &Type::Any));
        assert!(is_assignable(&Type::String, &Type::Any));
        assert!(is_assignable(&Type::Null, &Type::Any));
        assert!(is_assignable(&Type::Class("Dog".into()), &Type::Any));
    }

    #[test]
    fn is_assignable_any_does_not_narrow_back() {
        // Distinguishes `Any` (a sound, one-directional top type) from
        // `Unknown` (bidirectionally permissive) — an `Any`-typed value
        // can't flow back into a concrete type without an explicit cast.
        assert!(!is_assignable(&Type::Any, &Type::Int));
        assert!(!is_assignable(&Type::Any, &Type::String));
    }

    #[test]
    fn is_assignable_nothing_is_assignable_to_anything() {
        assert!(is_assignable(&Type::Nothing, &Type::Int));
        assert!(is_assignable(&Type::Nothing, &Type::String));
        assert!(is_assignable(&Type::Nothing, &Type::Any));
        assert!(is_assignable(&Type::Nothing, &Type::Nullable(Box::new(Type::Int))));
        assert!(is_assignable(&Type::Nothing, &Type::Nothing));
    }

    #[test]
    fn is_assignable_generic_is_always_compatible() {
        let t: Rc<str> = "T".into();
        assert!(is_assignable(&Type::Generic(t.clone()), &Type::Int));
        assert!(is_assignable(&Type::String, &Type::Generic(t)));
    }

    #[test]
    fn is_assignable_no_numeric_widening() {
        assert!(!is_assignable(&Type::Int, &Type::Double));
        assert!(!is_assignable(&Type::Double, &Type::Int));
    }

    #[test]
    fn is_assignable_exact_match_required() {
        assert!(is_assignable(&Type::String, &Type::String));
        assert!(!is_assignable(&Type::String, &Type::Bool));
    }

    #[test]
    fn is_assignable_class_by_exact_name_only() {
        let dog: Rc<str> = "Dog".into();
        let animal: Rc<str> = "Animal".into();
        assert!(is_assignable(&Type::Class(dog.clone()), &Type::Class(dog)));
        assert!(!is_assignable(&Type::Class("Dog".into()), &Type::Class(animal)));
    }

    #[test]
    fn function_type_name_renders_like_kotlin() {
        let f = Type::Function(vec![Type::Int, Type::String], Box::new(Type::Bool));
        assert_eq!(f.name(), "(Int, String) -> Boolean");
        let no_params = Type::Function(Vec::new(), Box::new(Type::Unit));
        assert_eq!(no_params.name(), "() -> Unit");
    }

    #[test]
    fn is_assignable_function_types_structural_same_shape() {
        let a = Type::Function(vec![Type::Int], Box::new(Type::Int));
        let b = Type::Function(vec![Type::Int], Box::new(Type::Int));
        assert!(is_assignable(&a, &b));
    }

    #[test]
    fn is_assignable_function_types_reject_arity_mismatch() {
        let one_param = Type::Function(vec![Type::Int], Box::new(Type::Int));
        let two_params = Type::Function(vec![Type::Int, Type::Int], Box::new(Type::Int));
        assert!(!is_assignable(&one_param, &two_params));
    }

    #[test]
    fn is_assignable_function_types_reject_param_or_return_mismatch() {
        let int_to_int = Type::Function(vec![Type::Int], Box::new(Type::Int));
        let string_to_int = Type::Function(vec![Type::String], Box::new(Type::Int));
        let int_to_string = Type::Function(vec![Type::Int], Box::new(Type::String));
        assert!(!is_assignable(&int_to_int, &string_to_int));
        assert!(!is_assignable(&int_to_int, &int_to_string));
    }

    #[test]
    fn is_assignable_function_types_unknown_slot_is_a_wildcard() {
        // A lambda param inferred from context, or a return type that
        // couldn't be pinned down, is `Unknown` at that slot only — still
        // compatible element-wise, same as `Unknown` everywhere else.
        let partially_known = Type::Function(vec![Type::Unknown], Box::new(Type::Int));
        let fully_known = Type::Function(vec![Type::Int], Box::new(Type::Int));
        assert!(is_assignable(&partially_known, &fully_known));
    }

    #[test]
    fn is_assignable_native_sentinel_matches_any_function_type() {
        // `Type::from_value(&Value::Native(_))`'s exact shape (empty params,
        // `Unknown` return) — nothing to check structurally, so it's always
        // compatible with any declared function type, unlike a real
        // zero-param function (which would have a concrete, non-`Unknown`
        // return type).
        let native_sentinel = Type::Function(Vec::new(), Box::new(Type::Unknown));
        let declared = Type::Function(vec![Type::Int, Type::String], Box::new(Type::Bool));
        assert!(is_assignable(&native_sentinel, &declared));
    }

    #[test]
    fn resolve_type_annotation_named_resolves_like_resolve_type_hint() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        let ann = TypeAnnotation::Named("Int".to_string(), crate::error::Span::default());
        assert_eq!(resolve_type_annotation(&ann, &empty_params, &empty_classes), Type::Int);
    }

    #[test]
    fn resolve_type_annotation_function_resolves_structurally() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        let span = crate::error::Span::default();
        let ann = TypeAnnotation::Function(
            vec![TypeAnnotation::Named("Int".to_string(), span), TypeAnnotation::Named("String".to_string(), span)],
            Box::new(TypeAnnotation::Named("Boolean".to_string(), span)),
            span,
        );
        let resolved = resolve_type_annotation(&ann, &empty_params, &empty_classes);
        assert_eq!(resolved, Type::Function(vec![Type::Int, Type::String], Box::new(Type::Bool)));
    }

    #[test]
    fn resolve_type_annotation_unresolvable_name_is_unknown_only_at_its_own_position() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        let span = crate::error::Span::default();
        let ann = TypeAnnotation::Function(
            vec![TypeAnnotation::Named("Frobnicator".to_string(), span)],
            Box::new(TypeAnnotation::Named("Int".to_string(), span)),
            span,
        );
        let resolved = resolve_type_annotation(&ann, &empty_params, &empty_classes);
        assert_eq!(resolved, Type::Function(vec![Type::Unknown], Box::new(Type::Int)));
    }

    #[test]
    fn resolve_type_annotation_nullable_wraps_the_inner_type() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        let span = crate::error::Span::default();
        let ann =
            TypeAnnotation::Nullable(Box::new(TypeAnnotation::Named("String".to_string(), span)), span);
        let resolved = resolve_type_annotation(&ann, &empty_params, &empty_classes);
        assert_eq!(resolved, Type::Nullable(Box::new(Type::String)));
    }

    #[test]
    fn resolve_type_annotation_nullable_function_type() {
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        let span = crate::error::Span::default();
        let fn_ann = TypeAnnotation::Function(
            vec![TypeAnnotation::Named("Int".to_string(), span)],
            Box::new(TypeAnnotation::Named("String".to_string(), span)),
            span,
        );
        let ann = TypeAnnotation::Nullable(Box::new(fn_ann), span);
        let resolved = resolve_type_annotation(&ann, &empty_params, &empty_classes);
        assert_eq!(
            resolved,
            Type::Nullable(Box::new(Type::Function(vec![Type::Int], Box::new(Type::String))))
        );
    }

    // ---- resolve_overload ----

    #[test]
    fn resolve_overload_no_candidate_accepts_this_arity() {
        let candidates: &[&[Type]] = &[&[Type::Int], &[Type::Int, Type::Int]];
        assert_eq!(resolve_overload(candidates, &[]), OverloadResolution::ArityMismatch);
    }

    #[test]
    fn resolve_overload_arity_matches_but_no_type_matches() {
        let candidates: &[&[Type]] = &[&[Type::Int], &[Type::Double]];
        assert_eq!(resolve_overload(candidates, &[Type::String]), OverloadResolution::NoMatch);
    }

    #[test]
    fn resolve_overload_unique_exact_match() {
        let candidates: &[&[Type]] = &[&[Type::Int], &[Type::Double]];
        assert_eq!(resolve_overload(candidates, &[Type::Int]), OverloadResolution::Unique(0));
        assert_eq!(resolve_overload(candidates, &[Type::Double]), OverloadResolution::Unique(1));
    }

    #[test]
    fn resolve_overload_unknown_argument_is_ambiguous_across_disagreeing_candidates() {
        let candidates: &[&[Type]] = &[&[Type::Int], &[Type::Double]];
        match resolve_overload(candidates, &[Type::Unknown]) {
            OverloadResolution::Ambiguous(mut is) => {
                is.sort();
                assert_eq!(is, vec![0, 1]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn resolve_overload_specific_type_beats_a_same_arity_any_overload() {
        // A concrete `Int` overload must win over a same-arity `Any`
        // overload when called with a real `Int` — real Kotlin picks the
        // most specific applicable overload, not an arbitrary one.
        let candidates: &[&[Type]] = &[&[Type::Any], &[Type::Int]];
        assert_eq!(resolve_overload(candidates, &[Type::Int]), OverloadResolution::Unique(1));
    }

    #[test]
    fn resolve_overload_not_limited_to_numeric_types() {
        // The whole point of replacing the old numeric-only special case:
        // this must work for any argument types, not just Int/Double.
        let candidates: &[&[Type]] = &[&[Type::String], &[Type::Bool], &[Type::String, Type::Bool]];
        assert_eq!(resolve_overload(candidates, &[Type::String]), OverloadResolution::Unique(0));
        assert_eq!(resolve_overload(candidates, &[Type::Bool]), OverloadResolution::Unique(1));
        assert_eq!(resolve_overload(candidates, &[Type::String, Type::Bool]), OverloadResolution::Unique(2));
        assert_eq!(resolve_overload(candidates, &[Type::Int]), OverloadResolution::NoMatch);
    }
}
