//! `Exception`/`RuntimeException` — real, native-function constructors for
//! the builtin classes `class::builtin_classes` wires into the
//! `Any -> Exception -> RuntimeException` chain (`throw`/`catch`, `try`'s
//! `find_matching_catch`, all in `interpreter.rs`, dispatch against that
//! same chain). Registered here as ordinary native functions, exactly like
//! `println` — **not** through `Interpreter::construct_object`, since
//! `builtin_classes()`'s entries have no `constructor_params`/
//! `constructor_body` at all (that machinery exists only for AST-declared
//! `class`es — see its own doc comment).

use std::collections::HashMap;

use crate::class::ObjectData;
use crate::environment::Env;
use crate::error::{Flow, Span};
use crate::gc::GcRef;
use crate::interpreter::Interpreter;
use crate::resolver::SymbolKind;
use crate::types::Type;
use crate::value::Value;

use super::{arity_error, register, type_error, BuiltinPackage, BuiltinParam, BuiltinSymbol};

pub(super) const PACKAGE: BuiltinPackage = BuiltinPackage {
    name: "kotlin",
    default_imported: true,
    symbols: &[
        BuiltinSymbol {
            name: "Exception",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "message", ty: Type::String, nullable: false, default: None }],
            variadic_param: None,
            // Really `Type::Class("Exception")` — a `const` table can't
            // build that (see `types::resolve_type_hint`'s own `"Number"`
            // special case for the same barrier). Costs nothing where it
            // matters: `throw Exception(...)`'s own type is always
            // `Type::Nothing` regardless of `Exception(...)`'s own type
            // (`ExprKind::Throw`'s resolver handling), and a `catch (e:
            // Exception)` binding's type comes from the catch annotation
            // itself, not from this return type. Only
            // `val e = Exception("x"); e.message` outside any throw/catch
            // loses static checking — the same accepted gap `List.get()`
            // already has.
            return_type: Type::Any,
            doc: "Constructs an Exception carrying the given message.",
        },
        BuiltinSymbol {
            name: "RuntimeException",
            kind: SymbolKind::Fun,
            params: &[BuiltinParam { name: "message", ty: Type::String, nullable: false, default: None }],
            variadic_param: None,
            return_type: Type::Any,
            doc: "Constructs a RuntimeException (a subtype of Exception) carrying the given message.",
        },
    ],
    populate,
};

fn populate(env: &Env) {
    register(env, "Exception", |interp, args, span| make_exception(interp, "Exception", args, span));
    register(env, "RuntimeException", |interp, args, span| {
        make_exception(interp, "RuntimeException", args, span)
    });
}

/// Builds a `Value::Object` of `class_name` directly, with a single
/// `message` field — `e.message` then works through ordinary field access,
/// no new code needed there. `class_name` is always one of the two names
/// this module just registered above, always present in
/// `Interpreter::builtin_classes` (`class::builtin_classes`), so the
/// index never panics.
fn make_exception(interp: &mut Interpreter, class_name: &'static str, args: Vec<Value>, span: Span) -> Result<Value, Flow> {
    let message = match args.as_slice() {
        [Value::Str(s)] => s.to_string(),
        [_] => return Err(type_error(class_name, "a String message", span)),
        _ => return Err(arity_error(class_name, "1", args.len(), span)),
    };
    let mut fields = HashMap::new();
    fields.insert("message".to_string(), Value::str(message));
    let class = interp.builtin_classes[class_name].clone();
    let obj = GcRef::new(interp.gc.clone(), ObjectData { class, fields });
    Ok(Value::Object(obj))
}
