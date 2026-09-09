//! `kotlin.random` — `random()`/`randomInt(bound)`. **Not** auto-imported,
//! matching real Kotlin (which doesn't auto-import `kotlin.random` either) —
//! needs `import kotlin.random.*` or full qualification.
//!
//! Real Kotlin's own `kotlin.random.Random` is an *object* (`Random.nextInt(...)`,
//! `Random.nextDouble()`) — ksl has no `object`/singleton declarations yet
//! (`docs/roadmap.md`'s classes phase), so these are plain top-level
//! functions instead, a deliberate, documented shape divergence, the same
//! kind `kotlin.math`'s own module doc comment already carries for
//! `sqrt(Int)`.
//!
//! Not cryptographically secure — a hand-rolled splitmix64 generator (no
//! `rand` crate: `ksl-core` stays zero-dependency, `AGENTS.md`), seeded once
//! per `Interpreter` from the system clock (`Interpreter::new`). Good enough
//! for its actual purpose (game logic — food placement, etc.), not a
//! security primitive.

use crate::environment::Env;
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{arity_error, type_error, BuiltinPackage, BuiltinParam, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "kotlin.random",
    default_imported: false,
    symbols: &[
        BuiltinSymbol {
            name: "random",
            kind: SymbolKind::Fun,
            params: &[],
            variadic_param: None,
            return_type: Type::Double,
            doc: "A random Double in [0, 1).",
        },
        BuiltinSymbol {
            name: "randomInt",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "bound", ty: Type::Int, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Int,
            doc: "A random Int in [0, bound). bound must be positive.",
        },
    ],
    populate,
};

/// One splitmix64 step — advances `state` and returns the next 64-bit
/// output. Public-domain algorithm (Vigna), chosen for being small enough to
/// hand-write correctly and fast-mixing enough that a plain top-level
/// `state += const; state = mix(state)` needs no separate warm-up.
fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Registers `kotlin.random`'s members onto `env` — `default_imported:
/// false` (see `PACKAGE`'s doc comment above), matching `kotlin.math`/
/// `kotlin.test`'s precedent: needs `import kotlin.random.*` or full
/// qualification.
fn populate(env: &Env) {
    super::register(env, "random", |interp, args, span| {
        if !args.is_empty() {
            return Err(arity_error("random", "0", args.len(), span));
        }
        let bits = splitmix64_next(&mut interp.rng_state);
        // Top 53 bits -> a Double uniformly in [0, 1), the standard
        // technique (a Double's mantissa is exactly 53 bits, so this uses
        // every bit of precision it has with no bias).
        let frac = (bits >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
        Ok(Value::Double(frac))
    });

    super::register(env, "randomInt", |interp, args, span| {
        let bound = match args.as_slice() {
            [Value::Int(b)] => *b,
            [_] => return Err(type_error("randomInt", "an Int bound", span)),
            _ => return Err(arity_error("randomInt", "1", args.len(), span)),
        };
        if bound <= 0 {
            return Err(type_error("randomInt", "a positive bound", span));
        }
        let bits = splitmix64_next(&mut interp.rng_state);
        Ok(Value::Int((bits % bound as u64) as i64))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Flow, Span};
    use crate::interpreter::Interpreter;

    /// Same helper shape `assertions.rs`'s own tests already use, except
    /// `interp` is threaded through by the caller (not created fresh per
    /// call) — needed here so `rng_state` actually advances across repeated
    /// calls in one test, the same way a real script's own successive
    /// `random()`/`randomInt()` calls would.
    fn call(env: &Env, interp: &mut Interpreter, name: &str, args: Vec<Value>) -> Result<Value, Flow> {
        let f = match env.get(name) {
            Some(Value::Native(n)) => n,
            _ => panic!("'{name}' not registered"),
        };
        (f.func)(interp, args, Span::default())
    }

    #[test]
    fn random_stays_within_zero_one() {
        let env = Env::new();
        populate(&env);
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        for _ in 0..200 {
            let Ok(Value::Double(d)) = call(&env, &mut interp, "random", vec![]) else { panic!("expected a Double") };
            assert!((0.0..1.0).contains(&d), "{d} not in [0, 1)");
        }
    }

    #[test]
    fn random_int_stays_within_bound_and_varies() {
        let env = Env::new();
        populate(&env);
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let Ok(Value::Int(i)) = call(&env, &mut interp, "randomInt", vec![Value::Int(10)]) else {
                panic!("expected an Int")
            };
            assert!((0..10).contains(&i), "{i} not in [0, 10)");
            seen.insert(i);
        }
        assert!(seen.len() > 1, "200 draws from [0, 10) landed on only one value — generator looks broken");
    }

    #[test]
    fn random_int_rejects_a_non_positive_bound() {
        let env = Env::new();
        populate(&env);
        let mut out = Vec::new();
        let mut interp = Interpreter::new(&mut out);
        assert!(call(&env, &mut interp, "randomInt", vec![Value::Int(0)]).is_err());
    }
}
