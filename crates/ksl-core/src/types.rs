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
                Type::Function(param_types, Box::new(ret_ty))
            }
            Value::Native(_) => Type::Function(Vec::new(), Box::new(Type::Unknown)),
        }
    }
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
/// generics or classes) — the source of truth for anything outside this
/// function that needs to list "the built-in type names", e.g. `ksl-lsp`'s
/// completion when the cursor sits in type-annotation position.
pub const PRIMITIVE_TYPE_NAMES: &[&str] = &["Int", "Double", "Boolean", "String", "Unit", "List", "Any"];

fn resolve_type_hint(hint: &str, generic_params: &[String], known_classes: &HashSet<Rc<str>>) -> Type {
    match hint {
        "Int" => Type::Int,
        "Double" => Type::Double,
        "Boolean" => Type::Bool,
        "String" => Type::String,
        "Unit" => Type::Unit,
        "List" => Type::List,
        "Any" => Type::Any,
        "Nothing" => Type::Nothing,
        _ if generic_params.iter().any(|p| p == hint) => Type::Generic(hint.into()),
        _ if known_classes.contains(hint) => Type::Class(hint.into()),
        _ => Type::Unknown,
    }
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
}
