//! `kotlin.collections` — `listOf`. Auto-imported, matching real Kotlin.

use crate::environment::Env;
use crate::gc::GcRef;
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{register, BuiltinPackage, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "kotlin.collections",
    default_imported: true,
    symbols: &[BuiltinSymbol {
        name: "listOf",
        kind: SymbolKind::Fun,
        params: &[],
        variadic_param: Some("values"),
        return_type: Type::List,
        doc: "Builds a List from any number of arguments.",
    }],
    populate,
};

/// Registers `kotlin.collections`'s members (`listOf`) onto `env`.
fn populate(env: &Env) {
    register(env, "listOf", |interp, args, _span| Ok(Value::List(GcRef::new(interp.gc.clone(), args))));
}
