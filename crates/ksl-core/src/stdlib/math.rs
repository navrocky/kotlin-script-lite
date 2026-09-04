//! `kotlin.math` — `abs`/`sqrt`/`max`/`min`. **Not** auto-imported, matching
//! real Kotlin exactly — needs `import kotlin.math.*` or full qualification.
//!
//! Real Kotlin has no `Int | Double` union-type syntax at all — this is real
//! *overloading* instead: several `BuiltinSymbol`s sharing one `name`, each a
//! distinct, single-type signature, exactly the shape `symbols:
//! &[BuiltinSymbol]` already supports (it's an array, nothing about it
//! assumes unique names). No runtime change needed to go with it —
//! `populate`'s single closure per name already dispatches on the real
//! `Value` variant it's given, which *is* the overload resolution; only the
//! metadata was ever collapsed into one fake union-typed entry. Two of these
//! overload sets intentionally go a little beyond what real Kotlin itself
//! overloads, both pre-existing ksl simplifications kept as-is here (not a
//! behavior change, just finally represented honestly instead of hidden
//! behind `|`): `sqrt(x: Int)` — real Kotlin's `kotlin.math.sqrt` has no
//! `Int` overload at all (needs an explicit `.toDouble()` first); ksl's
//! accepts one directly. `max`/`min` also still accept a *mixed*
//! `(Int, Double)` call at runtime, widening to `Double` — real Kotlin has no
//! such overload either — see `docs/stdlib.md`'s Math section.

use crate::environment::Env;
use crate::error::{Flow, Span};
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{arity_error, register, type_error, BuiltinPackage, BuiltinParam, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "kotlin.math",
    default_imported: false,
    symbols: &[
        BuiltinSymbol {
            name: "abs",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "x", ty: Type::Int, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Int,
            doc: "Absolute value of an Int.",
        },
        BuiltinSymbol {
            name: "abs",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "x", ty: Type::Double, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Double,
            doc: "Absolute value of a Double.",
        },
        BuiltinSymbol {
            name: "sqrt",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "x", ty: Type::Int, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Double,
            doc: "Square root of an Int, as a Double.",
        },
        BuiltinSymbol {
            name: "sqrt",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "x", ty: Type::Double, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Double,
            doc: "Square root.",
        },
        BuiltinSymbol {
            name: "max",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "a", ty: Type::Int, nullable: false, default: None },
                BuiltinParam { name: "b", ty: Type::Int, nullable: false, default: None },
            ],
            variadic_param: None,
            return_type: Type::Int,
            doc: "Returns the larger of a and b.",
        },
        BuiltinSymbol {
            name: "max",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "a", ty: Type::Double, nullable: false, default: None },
                BuiltinParam { name: "b", ty: Type::Double, nullable: false, default: None },
            ],
            variadic_param: None,
            return_type: Type::Double,
            doc: "Returns the larger of a and b.",
        },
        BuiltinSymbol {
            name: "min",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "a", ty: Type::Int, nullable: false, default: None },
                BuiltinParam { name: "b", ty: Type::Int, nullable: false, default: None },
            ],
            variadic_param: None,
            return_type: Type::Int,
            doc: "Returns the smaller of a and b.",
        },
        BuiltinSymbol {
            name: "min",
            kind: SymbolKind::Fun,
            params: &[
                BuiltinParam { name: "a", ty: Type::Double, nullable: false, default: None },
                BuiltinParam { name: "b", ty: Type::Double, nullable: false, default: None },
            ],
            variadic_param: None,
            return_type: Type::Double,
            doc: "Returns the smaller of a and b.",
        },
    ],
    populate,
};

/// Registers `kotlin.math`'s members (`abs`/`sqrt`/`max`/`min`) onto
/// `env` — `default_imported: false` (see `PACKAGE`'s doc comment above),
/// unlike `kotlin.io`/`kotlin.collections`/`kotlin.text`: these need
/// `import kotlin.math.*` or full qualification, matching real Kotlin
/// exactly.
fn populate(env: &Env) {
    register(env, "abs", |_interp, args, span| match args.first() {
        Some(Value::Int(i)) => Ok(Value::Int(i.wrapping_abs())),
        Some(Value::Double(d)) => Ok(Value::Double(d.abs())),
        _ => Err(type_error("abs", "a number", span)),
    });

    register(env, "sqrt", |_interp, args, span| {
        let x = expect_num("sqrt", &args, 0, span)?;
        Ok(Value::Double(x.sqrt()))
    });

    register(env, "max", |_interp, args, span| {
        let a = args.first().ok_or_else(|| arity_error("max", "2", args.len(), span))?;
        let b = args.get(1).ok_or_else(|| arity_error("max", "2", args.len(), span))?;
        numeric_binary(a, b, span, f64::max)
    });

    register(env, "min", |_interp, args, span| {
        let a = args.first().ok_or_else(|| arity_error("min", "2", args.len(), span))?;
        let b = args.get(1).ok_or_else(|| arity_error("min", "2", args.len(), span))?;
        numeric_binary(a, b, span, f64::min)
    });
}

fn expect_num(name: &str, args: &[Value], idx: usize, span: Span) -> Result<f64, Flow> {
    match args.get(idx) {
        Some(Value::Int(i)) => Ok(*i as f64),
        Some(Value::Double(d)) => Ok(*d),
        _ => Err(type_error(name, "a numeric argument", span)),
    }
}

/// A mixed `(Int, Double)`/`(Double, Int)` pair still widens to `Double`
/// here at runtime — a lenient fallback for a call the static checker
/// couldn't fully type (e.g. an `Unknown`-typed argument), not a
/// documented feature: `max`/`min`'s declared overloads
/// (`(Int,Int):Int`/`(Double,Double):Double`, see `PACKAGE` above) no
/// longer include a mixed shape, so a *statically*-typed mixed call like
/// `min(3.5, 2)` is now a real type error before this ever runs — see
/// `docs/stdlib.md`'s Math section.
fn numeric_binary(a: &Value, b: &Value, span: Span, f: impl Fn(f64, f64) -> f64) -> Result<Value, Flow> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            let xf = *x as f64;
            let yf = *y as f64;
            let r = f(xf, yf);
            Ok(Value::Int(r as i64))
        }
        _ => {
            let xf = match a {
                Value::Int(x) => *x as f64,
                Value::Double(x) => *x,
                _ => return Err(type_error("max/min", "numeric arguments", span)),
            };
            let yf = match b {
                Value::Int(y) => *y as f64,
                Value::Double(y) => *y,
                _ => return Err(type_error("max/min", "numeric arguments", span)),
            };
            Ok(Value::Double(f(xf, yf)))
        }
    }
}
