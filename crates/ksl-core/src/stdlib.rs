//! ksl's standard library — see `STDLIB_PACKAGES` below for the single
//! source of truth on what it provides. Implementations live one file per
//! real Kotlin package (`io.rs` = `kotlin.io`, `collections.rs` =
//! `kotlin.collections`, `text.rs` = `kotlin.text`, `math.rs` =
//! `kotlin.math`, `assertions.rs` = `kotlin.test`) so the stdlib can keep
//! growing — a new package/function is a new file (or a new entry in an
//! existing one) plus one line in `STDLIB_PACKAGES` below, not a bigger and
//! bigger single file. This module itself holds only what's genuinely
//! cross-package: the shared metadata types, the combined package table,
//! the metadata queries every consumer (`is_known`, `stdlib_call_types`,
//! `Interpreter::new`, `ksl-lsp`) derives from it, the handful of
//! registration helpers every `populate_*` fn reuses, and the `.`-method
//! dispatch table (keyed by receiver *type*, not by package — a genuinely
//! different axis from `STDLIB_PACKAGES`, see `BUILTIN_METHOD_SIGNATURES`'s
//! own doc comment).

use std::rc::Rc;

use crate::environment::Env;
use crate::error::{Flow, RuntimeError, Span};
use crate::interpreter::Interpreter;
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::{NativeValue, Value};

mod assertions;
mod collections;
mod io;
mod math;
mod text;

/// One parameter of a `BuiltinSymbol` with a real, checkable type — the
/// single source both `format_signature` (display text) and
/// `stdlib_call_types` (static argument checking) read from, so the two
/// can never drift apart the way a hand-written signature *string* and a
/// hand-written `Type` list once could (see `git log` on this file for
/// the pre-unification shape, if curious).
///
/// `ty` is deliberately the *bare*, non-nullable type, with nullability
/// carried in its own `nullable: bool` instead of `Type::Nullable(Box::new(ty))`
/// directly: `Box::new` isn't a `const fn` in stable Rust (confirmed by
/// compiling a minimal repro — `cannot call non-const associated function
/// 'Box::<T>::new' in constants`), so a `Type::Nullable` value can't
/// appear in a `pub const` array at all. `real_type()` builds the true,
/// possibly-`Nullable`-wrapped `Type` on demand instead — ordinary
/// function calls (not `const` contexts) can call `Box::new` freely.
pub struct BuiltinParam {
    pub name: &'static str,
    pub ty: Type,
    pub nullable: bool,
    /// Display text for a trailing `= default`, e.g. `Some("null")` for
    /// `message: String? = null` — shown verbatim, never evaluated (each
    /// `populate_*` closure's own `match args.len()` is what actually
    /// implements the default).
    pub default: Option<&'static str>,
}

impl BuiltinParam {
    /// The real, checkable `Type` this parameter has — `ty` itself, or
    /// `Type::Nullable(Box::new(ty))` when `nullable` — see this struct's
    /// own doc comment for why `ty` can't just already be the `Nullable`
    /// form in the static table.
    pub fn real_type(&self) -> Type {
        if self.nullable {
            Type::Nullable(Box::new(self.ty.clone()))
        } else {
            self.ty.clone()
        }
    }
}

/// One name ksl's standard library provides — a member of some
/// `BuiltinPackage` (see below), the stdlib's own counterpart of a real
/// declaration's `resolver::Symbol`. Reuses `SymbolKind` rather than
/// inventing a parallel enum: every entry today is `SymbolKind::Fun` (the
/// stdlib has no `val`/`var` constants, and no `Class`/`Interface` variant
/// exists yet because ksl itself doesn't have classes/interfaces yet —
/// this field is what lets either show up later with no shape change
/// here, not a speculative "for future use" addition). Plain
/// `&'static str`/`Type` fields, not an owned `resolver::Symbol` directly:
/// that type's `String`/`Vec`/`Box` fields aren't `const`-constructible,
/// so a `BuiltinSymbol` is converted to a real `Symbol` on demand where
/// one is actually needed (`ksl-lsp`'s `package_index::builtin_symbol`).
///
/// No separate free-text `signature` field — `format_signature` below
/// generates the exact same display text from `params`/`variadic_param`/
/// `return_type` instead, so there's only ever one place describing a
/// given function's shape, not a hand-written string that could silently
/// drift from the real, checked types.
pub struct BuiltinSymbol {
    pub name: &'static str,
    pub kind: SymbolKind,
    /// Every parameter with a real, checkable type. Empty (with
    /// `variadic_param: Some(...)` instead) for a genuinely variadic
    /// function like `listOf`, where there's no fixed parameter list at
    /// all.
    pub params: &'static [BuiltinParam],
    /// `Some("values")` for `listOf`'s `vararg values` — mutually
    /// exclusive with a non-empty `params` in practice (nothing enforces
    /// this at the type level, but every current entry respects it).
    pub variadic_param: Option<&'static str>,
    pub return_type: Type,
    pub doc: &'static str,
}

/// Builds the exact display signature a hand-written `signature: &'static
/// str` field used to carry — e.g. `"fun println(value: Any?): Unit"`,
/// `"fun assertEquals(expected, actual, message: String? = null): Unit"` —
/// entirely from `sym`'s own structured fields. A `Type::Unknown` param is
/// printed with **no** type annotation at all (bare name only): that's
/// what already-shipped signatures like `assertEquals(expected, actual,
/// ...)` looked like by hand, because `expected`/`actual` are really a
/// generic `T` in real Kotlin (ksl has no generics yet) — `Unknown` is an
/// internal type-checker escape hatch, not a real Kotlin type name, so it
/// has no business appearing in hover text.
pub fn format_signature(sym: &BuiltinSymbol) -> String {
    let params = if let Some(vararg_name) = sym.variadic_param {
        format!("vararg {vararg_name}")
    } else {
        sym.params
            .iter()
            .map(|p| {
                let mut piece = if p.ty == Type::Unknown {
                    p.name.to_string()
                } else if p.nullable {
                    format!("{}: {}?", p.name, p.ty.name())
                } else {
                    format!("{}: {}", p.name, p.ty.name())
                };
                if let Some(default) = p.default {
                    piece.push_str(" = ");
                    piece.push_str(default);
                }
                piece
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("fun {}({params}): {}", sym.name, sym.return_type.name())
}

/// One real, namable package ksl's standard library provides — always a
/// genuine dotted name (no `""`/flat-scope sentinel: every symbol belongs
/// to a real package, exactly matching real Kotlin, where even `println`
/// is really `kotlin.io.println`). `default_imported` is what makes a
/// package's members callable unqualified with no `import` at all — real
/// Kotlin auto-imports a fixed list of packages (`kotlin.*`,
/// `kotlin.collections.*`, `kotlin.text.*`, `kotlin.io.*`, ...) into every
/// file; `kotlin.math` and `kotlin.test` are deliberately **not** on that
/// list in real Kotlin either, so `abs`/`sqrt`/`max`/`min` and the
/// assertions both need an explicit `import` or a full
/// `kotlin.math.abs(...)`/`kotlin.test.assertEquals(...)` qualification —
/// see `STDLIB_PACKAGES` below for exactly which packages are which.
/// `populate` registers this package's real `NativeFn` closures onto a
/// given `Env` — necessarily hand-written code (a closure isn't data), but
/// keeping a function *pointer* to it right on the table entry is what
/// lets `Interpreter::new` build every package's env, and default-import
/// the right ones into `globals`, with one generic loop instead of a
/// hardcoded call per package (see that function).
pub struct BuiltinPackage {
    pub name: &'static str,
    pub default_imported: bool,
    pub symbols: &'static [BuiltinSymbol],
    pub populate: fn(&Env),
}

/// The single source of truth for "what names does ksl's stdlib provide,
/// under which package, and which packages are auto-imported" — every
/// other stdlib-metadata query in this crate and in `ksl-lsp` (`is_known`,
/// `default_imported_symbols`, `Interpreter::new`'s package/`globals`
/// setup, `package_index::build_package_index`'s built-in seeding,
/// `ksl-lsp::completion`/`hover`'s bare-name branches) derives from this
/// one table rather than keeping its own separately-hand-maintained copy.
/// Adding a stdlib function to an existing package is exactly one entry in
/// that package's own module (`io.rs`/`collections.rs`/`text.rs`/
/// `math.rs`/`assertions.rs`); adding a whole new built-in package is one
/// new file (own `PACKAGE` const + `populate` fn) plus one line here — see
/// AGENTS.md's feature checklist; the `_signatures_match_populate_*` tests
/// below catch a table entry that its `populate` fn doesn't actually
/// register (renamed or removed), though not the reverse.
pub const STDLIB_PACKAGES: &[BuiltinPackage] = &[
    io::PACKAGE,
    collections::PACKAGE,
    text::PACKAGE,
    math::PACKAGE,
    assertions::PACKAGE,
];

/// The combined symbols of every `default_imported` package — every stdlib
/// name visible with no `import`/qualification at all, exactly matching
/// real Kotlin's own default-import list. Used by `is_known` and by
/// `ksl-lsp`'s bare-name completion/hover branches.
pub fn default_imported_symbols() -> impl Iterator<Item = &'static BuiltinSymbol> {
    STDLIB_PACKAGES.iter().filter(|p| p.default_imported).flat_map(|p| p.symbols)
}

pub fn is_known(name: &str) -> bool {
    default_imported_symbols().any(|s| s.name == name)
}

/// Real, machine-readable `(param types, return type)` for a stdlib
/// function's call-site checking — used by `resolver.rs`'s `Call` handling
/// so `println`/`length`/etc. participate in the same argument/return type
/// checking a user-defined `fun` gets, instead of staying permanently
/// `Unknown`. `Type::Unknown` as a parameter's own type means "accepts
/// anything" (never flagged incompatible), matching `println(value: Any?)`
/// in real Kotlin.
///
/// Returns `None` for a name with no single checkable *return* type —
/// `abs`/`max`/`min` (overloads with *different* return types per
/// overload — `resolver.rs` handles any multi-overload stdlib name via
/// real overload resolution instead, see `stdlib_overloads` and
/// `types::resolve_overload`, since there's no one fixed `Type` to report
/// here). A variadic function like `listOf` still gets a real entry here (empty
/// param list, real `Type::List` return): every argument goes unchecked
/// (there's no fixed arity to check against, and `Type::List`'s own
/// element type isn't tracked anyway), but its *return* type is always
/// known regardless of how many arguments were passed, so a hint-less
/// `val l = listOf(1, 2, 3)` can still infer `l: List` instead of
/// incorrectly erroring "cannot infer type" (see `call_types_from_entries`).
///
/// Also covers `kotlin.test`'s names, even though that package isn't
/// `default_imported` (see `STDLIB_PACKAGES`'s doc comment) — this
/// function only supplies argument/return types for a name *already*
/// known to be legitimately in scope (`resolver.rs`'s `Call` arm only
/// reaches this after `walk_expr(callee)` already resolved or rejected
/// the bare name itself, via `is_known`/`imported_names`/
/// `has_wildcard_import`), so listing them here doesn't make an
/// unimported `assertEquals(...)` resolve — it only makes a *properly
/// imported* one get real argument-type checking instead of falling back
/// to `Unknown`.
pub fn stdlib_call_types(name: &str) -> Option<(Vec<Type>, Type)> {
    call_types_from_entries(STDLIB_PACKAGES.iter().flat_map(|p| p.symbols).filter(|s| s.name == name))
}

/// Same as `stdlib_call_types`, but for a fully package-qualified call
/// (`kotlin.test.assertFalse(...)`, parsed as an `ExprKind::MethodCall`
/// whose receiver flattens to a non-local dotted path — see
/// `resolver.rs`'s `MethodCall` arm) — `path`'s last segment is the
/// function name, everything before it is the package name. Validating the
/// package too, not just the trailing name, is what keeps a call qualified
/// under the *wrong* package (or a genuinely unknown qualified name) from
/// incorrectly reusing a same-named symbol from a different package —
/// `stdlib_call_types` alone can't do this since it never sees a package
/// qualifier at all. Returns `None` for anything not resolving to a real
/// package + member, exactly like `stdlib_call_types` does for an unknown
/// bare name.
pub fn stdlib_qualified_call_types(path: &[String]) -> Option<(Vec<Type>, Type)> {
    let (name, package_segs) = path.split_last()?;
    let package_name = package_segs.join(".");
    let pkg = STDLIB_PACKAGES.iter().find(|p| p.name == package_name)?;
    call_types_from_entries(pkg.symbols.iter().filter(|s| s.name == name))
}

/// Every `BuiltinSymbol` overload sharing `name`, across all of
/// `STDLIB_PACKAGES` — the raw candidate list `resolver.rs` runs real,
/// per-argument-type overload resolution over (`types::resolve_overload`)
/// whenever there's more than one, replacing the old numeric-only
/// `is_numeric_polymorphic` special case with something that works for
/// any overloaded stdlib function, of any argument types, not just
/// `Int`/`Double`. A name with zero or one overload has nothing to
/// resolve between — `resolver.rs` uses `stdlib_call_types` for that case
/// instead, unaffected by this function.
pub fn stdlib_overloads(name: &str) -> Vec<&'static BuiltinSymbol> {
    STDLIB_PACKAGES.iter().flat_map(|p| p.symbols).filter(|s| s.name == name && s.variadic_param.is_none()).collect()
}

/// Same as `stdlib_overloads`, but for a fully package-qualified call —
/// see `stdlib_qualified_call_types`'s doc comment for why the package
/// segment must be validated, not just the trailing name.
pub fn stdlib_qualified_overloads(path: &[String]) -> Vec<&'static BuiltinSymbol> {
    let Some((name, package_segs)) = path.split_last() else { return Vec::new() };
    let package_name = package_segs.join(".");
    let Some(pkg) = STDLIB_PACKAGES.iter().find(|p| p.name == package_name) else { return Vec::new() };
    pkg.symbols.iter().filter(|s| s.name == name && s.variadic_param.is_none()).collect()
}

/// Shared derivation both `stdlib_call_types` and `stdlib_qualified_call_types`
/// build on, given whichever `BuiltinSymbol`s share the name being called
/// (already filtered to the right package, for the qualified case). Several
/// overloads sharing one name (abs/sqrt/max/min): without the call site's
/// real argument types there's no way to pick one, so this only synthesizes
/// a single permissive signature when every overload agrees on arity and
/// return type (true for `sqrt`'s two overloads, both returning Double) —
/// otherwise (abs/max/min, where Int vs Double overloads return different
/// types) there's no safe single signature, so this returns `None`.
/// `resolver.rs`'s own real overload resolution (`stdlib_overloads` +
/// `types::resolve_overload`, picking a candidate by the call's actual
/// argument types) is unaffected by this and handles any name with more
/// than one overload instead — this function (and its qualified
/// counterpart) is only ever consulted by `resolver.rs` for a name with
/// zero or one overload.
///
/// A variadic entry (`listOf`) has no fixed arity to check individual
/// arguments against, but its return type is still real and known
/// regardless — reported as `(vec![], return_type)` (an empty param list,
/// so every argument goes unchecked rather than being falsely flagged as
/// "too many arguments"), not `None`. `None` would make the whole call's
/// type `Unknown`, which used to make a hint-less `val l = listOf(1, 2,
/// 3)` incorrectly fail resolver.rs's "cannot infer type" check — a real,
/// user-reported bug, since `listOf(...)` obviously has a real type
/// (`List`) regardless of how many arguments it's given. No stdlib name
/// is both variadic and overloaded today, so this case is only ever
/// reached with exactly one entry.
fn call_types_from_entries<'a>(entries: impl Iterator<Item = &'a BuiltinSymbol>) -> Option<(Vec<Type>, Type)> {
    let entries: Vec<&BuiltinSymbol> = entries.collect();
    let first = *entries.first()?;
    if entries.len() == 1 {
        if first.variadic_param.is_some() {
            return Some((Vec::new(), first.return_type.clone()));
        }
        return Some((first.params.iter().map(BuiltinParam::real_type).collect(), first.return_type.clone()));
    }
    if entries.iter().any(|s| s.variadic_param.is_some()) {
        return None;
    }
    let arity = first.params.len();
    let ret = first.return_type.clone();
    if entries.iter().all(|s| s.params.len() == arity && s.return_type == ret) {
        Some((vec![Type::Unknown; arity], ret))
    } else {
        None
    }
}


/// Defines a native function `name` on `env` — the one primitive every
/// package's own `populate` fn builds on.
fn register(
    env: &Env,
    name: &str,
    f: impl Fn(&mut Interpreter, Vec<Value>, Span) -> Result<Value, Flow> + 'static,
) {
    env.define(
        name,
        Value::Native(Rc::new(NativeValue {
            name: name.to_string(),
            func: Box::new(f),
        })),
        false,
        Type::Unknown,
    );
}

fn arity_error(name: &str, expected: &str, got: usize, span: Span) -> Flow {
    RuntimeError::new(format!("{name} expects {expected} argument(s), got {got}"), span).into()
}

fn type_error(name: &str, expected: &str, span: Span) -> Flow {
    RuntimeError::new(format!("{name} expects {expected}"), span).into()
}

/// One builtin `.`-method — the `.`-syntax counterpart of a
/// `BuiltinPackage` member, dispatched by the *receiver's type* (walking
/// its real builtin-class chain, see `class::builtin_classes`/
/// `builtin_type_chain`) rather than by package/namespace, a fundamentally
/// different lookup axis from `STDLIB_PACKAGES`. Kept as its own table for
/// exactly that reason. `call` is a plain `fn` pointer (not a closure) so
/// this can live in a `pub const` array, and doubles as the real
/// `MethodImpl::Native` implementation once `class::builtin_classes` wraps
/// it — the same function backs both static metadata (hover/completion/
/// resolver) and runtime dispatch, so they can never drift apart.
pub struct BuiltinMethod {
    pub receiver: &'static str,
    pub symbol: BuiltinSymbol,
    pub call: fn(&mut Interpreter, Value, Vec<Value>, Span) -> Result<Value, Flow>,
}

/// Every builtin `.`-method ksl provides — `String`'s own methods (also
/// reachable as plain `kotlin.text` functions, see `text::populate`) plus
/// `Any`'s three universal methods (`toString`/`hashCode`/`equals`),
/// inherited by every other builtin type and every user class via the real
/// superclass chain (`class::builtin_classes`, `exec_class_decl`'s
/// `superclass: Some(Any)`) — neither `Int`/`Double`/`Boolean`/`List`/
/// `Unit` nor a plain user class needs its own override entry here: `Any`'s
/// implementations are written to branch correctly on any `Value` they're
/// given (structural for a primitive/list, identity-based for an
/// `Object`).
pub const BUILTIN_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        receiver: "String",
        symbol: BuiltinSymbol {
            name: "length",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Int,
            doc: "Number of characters in the receiver.",
        },
        call: string_length,
    },
    BuiltinMethod {
        receiver: "String",
        symbol: BuiltinSymbol {
            name: "uppercase",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::String,
            doc: "Uppercases the receiver.",
        },
        call: string_uppercase,
    },
    BuiltinMethod {
        receiver: "String",
        symbol: BuiltinSymbol {
            name: "lowercase",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::String,
            doc: "Lowercases the receiver.",
        },
        call: string_lowercase,
    },
    BuiltinMethod {
        receiver: "String",
        symbol: BuiltinSymbol {
            name: "trim",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::String,
            doc: "Removes leading and trailing whitespace from the receiver.",
        },
        call: string_trim,
    },
    BuiltinMethod {
        receiver: "Any",
        symbol: BuiltinSymbol {
            name: "toString",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::String,
            doc: "A string representation of the receiver.",
        },
        call: any_to_string,
    },
    BuiltinMethod {
        receiver: "Any",
        symbol: BuiltinSymbol {
            name: "hashCode",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Int,
            doc: "A hash code consistent with equals: equal values always report the same hashCode.",
        },
        call: any_hash_code,
    },
    BuiltinMethod {
        receiver: "Any",
        symbol: BuiltinSymbol {
            name: "equals",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "other", ty: Type::Any, nullable: true, default: None }],
            variadic_param: None,
            return_type: Type::Bool,
            doc: "Structural equality for primitives/lists, identity for objects — the same rule '==' uses.",
        },
        call: any_equals,
    },
];

/// The static ancestor chain (own type name first, then each supertype in
/// order, ending at `"Any"`) for a builtin type name — the resolver/
/// `ksl-lsp`'s side of the same `Int`/`Double` -> `Number` -> `Any`, other
/// builtins -> `Any` hierarchy `class::builtin_classes` builds as real
/// `ClassValue`s at runtime, needed here too since neither the resolver nor
/// `ksl-lsp` ever constructs an `Interpreter`. Returns an empty `Vec` for a
/// name that isn't a builtin type at all (a user class name, or garbage) —
/// callers already treat "not found anywhere in the chain" as `Unknown`/
/// "no method", so an empty chain naturally falls out the same way.
pub fn builtin_type_chain(receiver_type: &str) -> Vec<&'static str> {
    let canonical: &'static str = match receiver_type {
        "Int" => "Int",
        "Double" => "Double",
        "Boolean" => "Boolean",
        "String" => "String",
        "List" => "List",
        "Unit" => "Unit",
        "Number" => "Number",
        "Any" => "Any",
        _ => return Vec::new(),
    };
    let mut chain = vec![canonical];
    let mut current = canonical;
    while current != "Any" {
        current = match current {
            "Int" | "Double" => "Number",
            _ => "Any",
        };
        chain.push(current);
    }
    chain
}

/// The static return type of a builtin `.`-method call, given its
/// receiver's own statically known type — used by `resolver.rs`'s
/// `walk_expr` (`MethodCall` inference) to type-check chained/nested
/// method calls and their results. Walks `builtin_type_chain` so e.g.
/// `"x".toString()` (a `String`, inheriting `Any`'s `toString`) resolves
/// correctly rather than only an exact-receiver match. `(type, method)`
/// pairs not found anywhere in the chain resolve to `Type::Unknown`, same
/// as an unrecognized name anywhere else in this type system.
pub fn builtin_method_return_type(receiver_type: &str, method: &str) -> Type {
    builtin_method_signature(receiver_type, method).map(|(ret, ..)| ret).unwrap_or(Type::Unknown)
}

/// `(return type, param types, param names, param docs)` — the same shape
/// `resolver.rs`'s own private `CallSignature` alias names, exposed here
/// too since `builtin_method_signature` needs to return it across the
/// crate boundary.
pub type MethodSignature = (Type, Vec<Type>, Vec<String>, Vec<Option<String>>);

/// Every parameter's real type/name/doc for a builtin method — the
/// `.`-method counterpart of `stdlib_call_types`, giving `resolver.rs` a
/// full checkable signature (not just a return type), so a method call's
/// arguments — including named ones — get exactly the same checking a
/// qualified/plain stdlib call already gets (see `resolver.rs`'s
/// `class_method_signature`, which builds on this for both a real builtin
/// receiver and a user class falling through to `Any`). Walks
/// `builtin_type_chain` exactly like `builtin_method_return_type`, which is
/// now just this function's return-type projection.
pub fn builtin_method_signature(receiver_type: &str, method: &str) -> Option<MethodSignature> {
    for rt in builtin_type_chain(receiver_type) {
        if let Some(m) = BUILTIN_METHODS.iter().find(|m| m.receiver == rt && m.symbol.name == method) {
            let param_types = m.symbol.params.iter().map(BuiltinParam::real_type).collect();
            let param_names = m.symbol.params.iter().map(|p| p.name.to_string()).collect();
            let param_docs = vec![None; m.symbol.params.len()];
            return Some((m.symbol.return_type.clone(), param_types, param_names, param_docs));
        }
    }
    None
}

/// Dispatches a `.`-call (`receiver.method(args)`) once `receiver` has
/// already evaluated to a real value — the counterpart of `parse_postfix`
/// producing `ExprKind::MethodCall` for `receiver.method(...)` syntax.
/// Walks `interp.builtin_classes`' real superclass chain via
/// `class::lookup_method`, exactly like `Value::Object` method dispatch
/// does in `interpreter.rs` — this is what lets `5.toString()`/
/// `"x".equals("x")`/etc. find `Any`'s methods without every builtin type
/// needing its own duplicate entry in `BUILTIN_METHODS`.
pub fn call_builtin_method(interp: &mut Interpreter, receiver: Value, method: &str, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    let receiver_type = receiver.type_name();
    if let Some(class) = interp.builtin_classes.get(receiver_type).cloned() {
        if let Some((_, m)) = crate::class::lookup_method(&class, method) {
            return match m {
                crate::class::MethodImpl::Native(native) => (native)(interp, receiver, args, span),
                crate::class::MethodImpl::UserDefined(_) => {
                    unreachable!("builtin classes only ever register Native methods")
                }
            };
        }
    }
    Err(RuntimeError::new(format!("'{receiver_type}' has no method '{method}'"), span).into())
}

fn string_length(_interp: &mut Interpreter, this: Value, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    expect_no_args("length", &args, span)?;
    let Value::Str(s) = &this else { unreachable!("dispatched only for String receivers") };
    Ok(text::str_length(s))
}

fn string_uppercase(_interp: &mut Interpreter, this: Value, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    expect_no_args("uppercase", &args, span)?;
    let Value::Str(s) = &this else { unreachable!("dispatched only for String receivers") };
    Ok(text::str_uppercase(s))
}

fn string_lowercase(_interp: &mut Interpreter, this: Value, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    expect_no_args("lowercase", &args, span)?;
    let Value::Str(s) = &this else { unreachable!("dispatched only for String receivers") };
    Ok(text::str_lowercase(s))
}

fn string_trim(_interp: &mut Interpreter, this: Value, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    expect_no_args("trim", &args, span)?;
    let Value::Str(s) = &this else { unreachable!("dispatched only for String receivers") };
    Ok(text::str_trim(s))
}

/// `Any.toString()`'s default — reuses `Value`'s own `Display` impl
/// directly, which already does the right thing for every kind of value:
/// plain formatting for a primitive/list, `"ClassName@addr"` identity
/// formatting for a `Value::Object` (see `value.rs`'s `Display` impl).
fn any_to_string(_interp: &mut Interpreter, this: Value, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    expect_no_args("toString", &args, span)?;
    Ok(Value::str(this.to_string()))
}

/// `Any.hashCode()`'s default — identity (the object's own `GcRef` address)
/// for a `Value::Object`, a real structural hash (see `value_hash`) for
/// everything else. Consistent with `any_equals`: equal values (by
/// `values_equal`) always report the same hash code.
fn any_hash_code(_interp: &mut Interpreter, this: Value, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    expect_no_args("hashCode", &args, span)?;
    Ok(Value::Int(value_hash(&this)))
}

/// `Any.equals(other)`'s default — reuses `Value::values_equal` directly
/// (structural for primitives/lists, identity via `GcRef::ptr_eq` for
/// objects), the exact same rule `==` already uses. A distinct user
/// override of `equals` is a real, separate `MethodImpl::UserDefined` on
/// that class, taking precedence via `class::lookup_method` — this is only
/// ever reached as the inherited fallback.
fn any_equals(_interp: &mut Interpreter, this: Value, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    if args.len() != 1 {
        return Err(arity_error("equals", "1", args.len(), span));
    }
    Ok(Value::Bool(Value::values_equal(&this, &args[0])))
}

/// A real, deterministic (within one process) structural hash for any
/// value — the shared basis for `any_hash_code`'s non-`Object` case. Not
/// required to match real Kotlin's own `hashCode` bit-for-bit (nothing in
/// ksl reads a hash across processes/runs), only to be internally
/// consistent with `values_equal`.
fn value_hash(v: &Value) -> i64 {
    use std::hash::{Hash, Hasher};
    match v {
        Value::Int(i) => *i,
        Value::Double(d) => d.to_bits() as i64,
        Value::Bool(b) => *b as i64,
        Value::Str(s) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut hasher);
            hasher.finish() as i64
        }
        Value::List(l) => l.borrow().iter().fold(1i64, |acc, item| acc.wrapping_mul(31).wrapping_add(value_hash(item))),
        Value::Object(o) => o.addr() as i64,
        // Unit/Null/Function/Native/Class/Overload: no meaningful
        // structural content to hash — a fixed constant is still
        // consistent with `values_equal` (nothing compares equal to a
        // different one of these that would need a different hash).
        _ => 0,
    }
}

fn expect_no_args(method: &str, args: &[Value], span: Span) -> Result<(), Flow> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(arity_error(method, "0", args.len(), span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every symbol every package in `STDLIB_PACKAGES` claims must
    /// actually be registered by that same package's own `populate` fn —
    /// catches a table entry out of sync with the real `register(...)`
    /// calls (renamed or removed), generically across every package at
    /// once rather than one hand-written test per package. (`Env` doesn't
    /// expose enumeration, so this can't also check the reverse direction
    /// — a new `register` call with no matching table entry has to be
    /// caught by the "adding a feature" checklist in AGENTS.md instead.)
    #[test]
    fn every_declared_symbol_is_registered_by_its_own_package() {
        for pkg in STDLIB_PACKAGES {
            let env = Env::new();
            (pkg.populate)(&env);
            for sym in pkg.symbols {
                assert!(
                    matches!(env.get(sym.name), Some(Value::Native(_))),
                    "'{}' is declared in STDLIB_PACKAGES's '{}' package but its populate fn doesn't register it",
                    sym.name,
                    pkg.name
                );
            }
        }
    }

    /// `format_signature` must reproduce, byte-for-byte, exactly the text
    /// every `BuiltinSymbol` carried by hand before `signature: &'static
    /// str` was replaced with `params`/`variadic_param`/`return_type` (see
    /// this module's own doc comments) — this is what actually catches a
    /// future edit to `params`/`return_type` silently changing displayed
    /// hover/completion text, since there's no longer a separate string to
    /// eyeball against.
    #[test]
    fn format_signature_matches_every_symbols_original_hand_written_text() {
        let expected: &[(&str, &str)] = &[
            ("println", "fun println(value: Any?): Unit"),
            ("print", "fun print(value: Any?): Unit"),
            ("listOf", "fun listOf(vararg values): List"),
            ("length", "fun length(s: String): Int"),
            ("uppercase", "fun uppercase(s: String): String"),
            ("lowercase", "fun lowercase(s: String): String"),
            ("trim", "fun trim(s: String): String"),
            ("abs", "fun abs(x: Int): Int"),
            ("abs", "fun abs(x: Double): Double"),
            ("sqrt", "fun sqrt(x: Int): Double"),
            ("sqrt", "fun sqrt(x: Double): Double"),
            ("max", "fun max(a: Int, b: Int): Int"),
            ("max", "fun max(a: Double, b: Double): Double"),
            ("min", "fun min(a: Int, b: Int): Int"),
            ("min", "fun min(a: Double, b: Double): Double"),
            ("assertEquals", "fun assertEquals(expected, actual, message: String? = null): Unit"),
            ("assertTrue", "fun assertTrue(actual: Boolean, message: String? = null): Unit"),
            ("assertFalse", "fun assertFalse(actual: Boolean, message: String? = null): Unit"),
            ("assertNull", "fun assertNull(actual, message: String? = null): Unit"),
            ("assertNotNull", "fun assertNotNull(actual, message: String? = null): Unit"),
        ];
        let actual: Vec<(&str, String)> = STDLIB_PACKAGES
            .iter()
            .flat_map(|p| p.symbols)
            .map(|s| (s.name, format_signature(s)))
            .collect();
        assert_eq!(actual.len(), expected.len(), "STDLIB_PACKAGES entry count changed — update this test's expected table too");
        for ((actual_name, actual_sig), (expected_name, expected_sig)) in actual.iter().zip(expected.iter()) {
            assert_eq!(actual_name, expected_name);
            assert_eq!(actual_sig, expected_sig);
        }
    }

    /// `stdlib_qualified_call_types` must agree with `stdlib_call_types` for
    /// a correctly-qualified path, but — unlike `stdlib_call_types`, which
    /// only ever sees the bare trailing name — must reject a path qualified
    /// under the *wrong* package, or a genuinely unknown package/member,
    /// rather than silently reusing a same-named symbol from elsewhere.
    /// Regression test for the bug where `kotlin.test.assertFalse(true,
    /// 123)` (a fully-qualified call) skipped argument type-checking
    /// entirely, while the bare `assertFalse(true, 123)` (after `import
    /// kotlin.test.*`) was already correctly flagged.
    #[test]
    fn stdlib_qualified_call_types_validates_the_package_too() {
        let path = |segs: &[&str]| segs.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            stdlib_qualified_call_types(&path(&["kotlin", "test", "assertFalse"])),
            stdlib_call_types("assertFalse"),
        );
        assert_eq!(
            stdlib_qualified_call_types(&path(&["kotlin", "io", "println"])),
            stdlib_call_types("println"),
        );
        // Wrong package for a real member name.
        assert_eq!(stdlib_qualified_call_types(&path(&["kotlin", "math", "assertFalse"])), None);
        // Unknown package.
        assert_eq!(stdlib_qualified_call_types(&path(&["kotlin", "nope", "assertFalse"])), None);
        // Unknown member of a real package.
        assert_eq!(stdlib_qualified_call_types(&path(&["kotlin", "test", "nope"])), None);
        // Too short to have both a package and a member.
        assert_eq!(stdlib_qualified_call_types(&path(&["assertFalse"])), None);
    }

    #[test]
    fn stdlib_overloads_lists_every_overload_sharing_a_name() {
        assert_eq!(stdlib_overloads("abs").len(), 2);
        assert_eq!(stdlib_overloads("max").len(), 2);
        assert_eq!(stdlib_overloads("min").len(), 2);
        assert_eq!(stdlib_overloads("sqrt").len(), 2);
        assert_eq!(stdlib_overloads("println").len(), 1);
        assert_eq!(stdlib_overloads("nope").len(), 0);
        // listOf is variadic, not overloaded — excluded, matching
        // `stdlib_call_types`'s own single-entry handling for it.
        assert_eq!(stdlib_overloads("listOf").len(), 0);
    }

    #[test]
    fn stdlib_qualified_overloads_validates_the_package_too() {
        let path = |segs: &[&str]| segs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(stdlib_qualified_overloads(&path(&["kotlin", "math", "abs"])).len(), 2);
        assert_eq!(stdlib_qualified_overloads(&path(&["kotlin", "test", "abs"])).len(), 0);
        assert_eq!(stdlib_qualified_overloads(&path(&["kotlin", "nope", "abs"])).len(), 0);
    }

    /// Regression test: `listOf` (variadic) used to be entirely excluded
    /// from `stdlib_call_types`, returning `None` — which made a
    /// hint-less `val l = listOf(1, 2, 3)` resolve to `Type::Unknown` and
    /// incorrectly fail `resolver.rs`'s "cannot infer type" check, even
    /// though `listOf(...)`'s return type (`List`) is always known
    /// regardless of its arguments.
    #[test]
    fn stdlib_call_types_reports_list_of_s_return_type_despite_being_variadic() {
        assert_eq!(stdlib_call_types("listOf"), Some((Vec::new(), Type::List)));
        assert_eq!(
            stdlib_qualified_call_types(&["kotlin".to_string(), "collections".to_string(), "listOf".to_string()]),
            Some((Vec::new(), Type::List)),
        );
    }

    /// `is_known`/`default_imported_symbols` must track `default_imported`
    /// exactly — including that `kotlin.math`/`kotlin.test` are correctly
    /// *not* on that list, matching real Kotlin (neither is one of the
    /// packages it auto-imports either).
    #[test]
    fn default_imported_symbols_matches_the_table() {
        let names: Vec<&str> = default_imported_symbols().map(|s| s.name).collect();
        for pkg in STDLIB_PACKAGES.iter().filter(|p| p.default_imported) {
            for sym in pkg.symbols {
                assert!(names.contains(&sym.name), "'{}' ({}) should be in default_imported_symbols()", sym.name, pkg.name);
                assert!(is_known(sym.name), "'{}' ({}) should be is_known", sym.name, pkg.name);
            }
        }
        for pkg in STDLIB_PACKAGES.iter().filter(|p| !p.default_imported) {
            for sym in pkg.symbols {
                assert!(!names.contains(&sym.name), "'{}' ({}) should NOT be in default_imported_symbols()", sym.name, pkg.name);
                assert!(!is_known(sym.name), "'{}' ({}) should NOT be is_known", sym.name, pkg.name);
            }
        }
    }

    /// The point of the whole `default_imported` split: a real
    /// `Interpreter` mirrors exactly the default-imported packages' members
    /// onto `globals` — never a non-default-imported one (`kotlin.math`,
    /// `kotlin.test`), which must stay reachable only via
    /// import/qualification. End-to-end (via `Interpreter::new`, not a
    /// bare `Env`), since this is the whole point of that constructor's
    /// loop over `STDLIB_PACKAGES`.
    #[test]
    fn interpreter_globals_mirror_exactly_the_default_imported_packages() {
        let mut sink = Vec::new();
        let interp = Interpreter::new(&mut sink);
        for pkg in STDLIB_PACKAGES {
            for sym in pkg.symbols {
                let visible = interp.globals.get(sym.name).is_some();
                assert_eq!(
                    visible, pkg.default_imported,
                    "'{}' ({}, default_imported={}) visibility on globals doesn't match",
                    sym.name, pkg.name, pkg.default_imported
                );
            }
        }
    }

    /// Every entry in `BUILTIN_METHODS` must actually be dispatchable by
    /// `call_builtin_method` on a real receiver of its own declared type —
    /// mirrors `every_declared_symbol_is_registered_by_its_own_package` for
    /// the `.`-method table. Needs a real `Interpreter` now (for
    /// `builtin_classes`), unlike the old free-function version.
    #[test]
    fn every_builtin_method_is_dispatchable_on_its_receiver_type() {
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        for m in BUILTIN_METHODS {
            let receiver = match m.receiver {
                "String" => Value::str("x"),
                "Int" => Value::Int(1),
                "Double" => Value::Double(1.0),
                "Boolean" => Value::Bool(true),
                "List" => Value::List(crate::gc::GcRef::new(interp.gc.clone(), Vec::new())),
                "Any" => Value::Int(1),
                other => panic!("add a receiver-type-appropriate value above for '{other}'"),
            };
            let args = if m.symbol.name == "equals" { vec![Value::Unit] } else { vec![] };
            let result = call_builtin_method(&mut interp, receiver, m.symbol.name, args, Span::default());
            assert!(result.is_ok(), "'{}' on '{}' listed in BUILTIN_METHODS but not dispatchable: {:?}", m.symbol.name, m.receiver, result.err().map(|_| ()));
        }
    }

    #[test]
    fn builtin_method_return_type_walks_the_hierarchy() {
        for m in BUILTIN_METHODS {
            assert_ne!(
                builtin_method_return_type(m.receiver, m.symbol.name),
                Type::Unknown,
                "'{}' listed in BUILTIN_METHODS but builtin_method_return_type doesn't know it",
                m.symbol.name
            );
        }
        // Inherited from Any, not listed directly under these receivers.
        assert_eq!(builtin_method_return_type("Int", "toString"), Type::String);
        assert_eq!(builtin_method_return_type("String", "toString"), Type::String);
        assert_eq!(builtin_method_return_type("String", "hashCode"), Type::Int);
        assert_eq!(builtin_method_return_type("Int", "length"), Type::Unknown);
        assert_eq!(builtin_method_return_type("String", "shout"), Type::Unknown);
    }

    #[test]
    fn call_builtin_method_dispatches_string_methods() {
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        assert!(matches!(call_builtin_method(&mut interp, Value::str("abc"), "length", vec![], Span::default()), Ok(Value::Int(3))));
        assert!(matches!(call_builtin_method(&mut interp, Value::str("abc"), "uppercase", vec![], Span::default()), Ok(Value::Str(s)) if &*s == "ABC"));
    }

    /// The whole point of unifying dispatch through `builtin_classes`'
    /// hierarchy: a primitive that declares no method of its own still
    /// finds `Any`'s inherited `toString`/`hashCode`/`equals`.
    #[test]
    fn call_builtin_method_dispatches_inherited_any_methods() {
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        assert!(matches!(
            call_builtin_method(&mut interp, Value::Int(5), "toString", vec![], Span::default()),
            Ok(Value::Str(s)) if &*s == "5"
        ));
        assert!(matches!(
            call_builtin_method(&mut interp, Value::Int(5), "equals", vec![Value::Int(5)], Span::default()),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            call_builtin_method(&mut interp, Value::Int(5), "hashCode", vec![], Span::default()),
            Ok(Value::Int(5))
        ));
    }

    #[test]
    fn call_builtin_method_unknown_method_is_an_error() {
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        match call_builtin_method(&mut interp, Value::str("x"), "shout", vec![], Span::default()) {
            Err(Flow::Error(e)) => assert!(e.message.contains("has no method 'shout'"), "{}", e.message),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn call_builtin_method_arity_error_reports_the_given_span() {
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        let span = Span::new(4, 2);
        match call_builtin_method(&mut interp, Value::str("x"), "length", vec![Value::Int(1)], span) {
            Err(Flow::Error(e)) => assert_eq!(e.span, span),
            other => panic!("expected an error, got {:?}", other.is_ok()),
        }
    }
}
