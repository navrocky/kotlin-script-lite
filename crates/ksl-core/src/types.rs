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

use crate::value::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int,
    Double,
    Bool,
    String,
    Unit,
    List,
    Function,
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
    /// Statically unknowable → always compatible. See the module doc
    /// comment.
    Unknown,
}

impl Type {
    /// Display name — matches `Value::type_name()`'s exact strings for the
    /// primitive variants, so error/hover text stays consistent with
    /// existing runtime wording.
    pub fn name(&self) -> &str {
        match self {
            Type::Int => "Int",
            Type::Double => "Double",
            Type::Bool => "Boolean",
            Type::String => "String",
            Type::Unit => "Unit",
            Type::List => "List",
            Type::Function => "Function",
            Type::Class(name) | Type::Generic(name) => name,
            Type::Unknown => "Unknown",
        }
    }

    /// The exact runtime type of an already-evaluated value. `Function` and
    /// `Native` both collapse to `Type::Function`, matching
    /// `Value::type_name()`'s existing collapse. No `Value` variant
    /// produces `Class`/`Generic` today (there is no object system yet).
    pub fn from_value(v: &Value) -> Type {
        match v {
            Value::Int(_) => Type::Int,
            Value::Double(_) => Type::Double,
            Value::Bool(_) => Type::Bool,
            Value::Str(_) => Type::String,
            Value::Unit => Type::Unit,
            Value::List(_) => Type::List,
            Value::Function(_) | Value::Native(_) => Type::Function,
        }
    }
}

/// Resolves a raw type-hint string (`Param.type_hint`/`ValDecl.type_hint`/a
/// function's return-type annotation) to a `Type`, given the generic
/// parameter names in scope (from an enclosing declaration's own `<T, ...>`
/// list) and the set of class names known so far.
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
/// generics or classes) — the source of truth for anything outside this
/// function that needs to list "the built-in type names", e.g. `ksl-lsp`'s
/// completion when the cursor sits in type-annotation position.
pub const PRIMITIVE_TYPE_NAMES: &[&str] = &["Int", "Double", "Boolean", "String", "Unit", "List"];

pub fn resolve_type_hint(hint: &str, generic_params: &[String], known_classes: &HashSet<Rc<str>>) -> Type {
    match hint {
        "Int" => Type::Int,
        "Double" => Type::Double,
        "Boolean" => Type::Bool,
        "String" => Type::String,
        "Unit" => Type::Unit,
        "List" => Type::List,
        _ if generic_params.iter().any(|p| p == hint) => Type::Generic(hint.into()),
        _ if known_classes.contains(hint) => Type::Class(hint.into()),
        _ => Type::Unknown,
    }
}

/// Whether a value of type `from` can be used where `to` is expected.
/// `Unknown` or `Generic` on **either** side is always compatible (the
/// named escape hatch — see the module doc comment); `Class` compares by
/// exact name only (no subtyping yet); everything else requires exact
/// equality. No numeric widening — `Int`/`Double` mixing is an arithmetic
/// result-type rule (see `resolver.rs`'s `infer_type`), not an assignment
/// rule (matches verified current behavior: `val x: Double = 5` is already
/// a type-mismatch error today).
pub fn is_assignable(from: &Type, to: &Type) -> bool {
    match (from, to) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Generic(_), _) | (_, Type::Generic(_)) => true,
        (Type::Class(a), Type::Class(b)) => a == b,
        (a, b) => a == b,
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
    fn primitive_type_names_all_resolve_to_a_known_primitive() {
        // Guards `PRIMITIVE_TYPE_NAMES` against drifting out of sync with
        // `resolve_type_hint`'s own match arms.
        let empty_params: Vec<String> = Vec::new();
        let empty_classes: HashSet<Rc<str>> = HashSet::new();
        for name in PRIMITIVE_TYPE_NAMES {
            let ty = resolve_type_hint(name, &empty_params, &empty_classes);
            assert_ne!(ty, Type::Unknown, "{name} should resolve to a primitive");
            assert!(!matches!(ty, Type::Generic(_) | Type::Class(_)), "{name} resolved to {ty:?}");
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
        assert_eq!(Type::from_value(&Value::Int(1)).name(), Value::Int(1).type_name());
        assert_eq!(Type::from_value(&Value::Bool(true)).name(), Value::Bool(true).type_name());
        assert_eq!(Type::from_value(&Value::str("x")).name(), Value::str("x").type_name());
    }

    #[test]
    fn is_assignable_unknown_is_always_compatible() {
        assert!(is_assignable(&Type::Unknown, &Type::Int));
        assert!(is_assignable(&Type::Int, &Type::Unknown));
        assert!(is_assignable(&Type::Unknown, &Type::Unknown));
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
}
