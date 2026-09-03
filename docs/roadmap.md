# Roadmap: remaining language features

`docs/syntax.md` ("Not supported") and `AGENTS.md` ("Explicitly out of scope")
both list the same backlog: `.` member-call syntax, rich collection literals,
classes/objects, inheritance, generics, null-safety operators, `try`/`catch`,
coroutines, and extension functions. These were deliberately deferred during
the initial build-out. This document sequences those 9 features plus 2 small,
already-documented tech-debt items (`Span::default()` in stdlib errors, the
`break`/`continue` unwind-through-function-calls quirk) into dependency-ordered
phases, grounded in the actual current shape of `Value`, `ExprKind`/`StmtKind`,
`Flow`, and the parser.

Decisions already made that shape this plan:
- **A static + runtime type system was added as Phase 2**, ahead of every
  feature phase below except the already-shipped Phases 0/1. It wasn't part
  of the original 9-feature backlog — it surfaced from a completion bug
  (LSP `.`-completion offering `String` methods after a non-`String`
  receiver) that traced back to a real gap: type hints were parsed but
  never resolved into anything checkable, function arguments/return values/
  `var` reassignment were never type-checked at all, and there was no
  structured type for the resolver or `ksl-lsp` to reason about. See Phase
  2 below; every later phase in this document was written (or rewritten)
  assuming it exists, in particular Phases 4 (null-safety), 6 (classes), 7
  (inheritance), and 9 (generics), which each have a note on exactly how
  they build on it.
- **Coroutines are scoped down** to cooperative `sequence {}`/`yield()`
  generators only — no `launch`/`async`/`Deferred`/dispatchers. Real
  async/await-style concurrency conflicts with the interpreter's
  `Rc`-based, explicitly single-threaded design (AGENTS.md: "don't
  introduce `Send`/`Sync`/threading") and would require a from-scratch
  resumable-evaluator rearchitecture disproportionate to everything else
  here.
- **`try`/`catch` ships with a minimal `Value::Error{message, class_name}`**
  now, ahead of classes, rather than waiting for a general object system.
  Phase 6 (classes) later reimplements it as sugar over `Value::Object`
  without changing the `Flow::Throw` contract.
- **No `{k: v}` map-literal syntax** — real Kotlin doesn't have one either
  (`{ }` is already lambdas/blocks in ksl, so reusing it would be
  ambiguous). Maps are built via `mapOf(...)` stdlib sugar instead.
- **Tech-debt Phase 0 includes** the `NativeFn` span fix and the
  `break`/`continue` function-boundary fix (both parser/interpreter-only,
  low-risk). The `Double` formatting mismatch (`2.0` prints as `2`) is
  **excluded** from this roadmap — fixing it regenerates every existing
  `.out` fixture and isn't needed by any of the 9 features, so it's left
  as a separate future cleanup.
- **`ksl-lsp` is updated in lockstep with every phase, not as a trailing
  cleanup pass.** Every phase below has its own explicit `LSP:` note for
  exactly this reason — treat a phase as unfinished until its `LSP:` note
  is done, the same way it'd be unfinished without tests.

## Dependency order

```
Phase 0: tech-debt (NativeFn span, break/continue fix)
   |
   v
Phase 1: `.` postfix member-access syntax  <-- blocks 6, 7, 8
   |
   v
Phase 2: static & runtime type system  <-- needs Phase 1 (type-aware completion hooks into MethodCall)
   |
   +--> Phase 3: collection literals (list [1,2,3] + mapOf/pair)  [independent]
   +--> Phase 4: null-safety (Value::Null, ?, ?., ?:, !!)          [independent]
   |
   v
Phase 5: try/catch (Flow::Throw, minimal Value::Error)
   |
   v
Phase 6: classes/objects  <-- needs Phase 1 (+ Phase 2 reserves Type::Class for it)
   |
   v
Phase 7: inheritance  <-- needs Phase 6 (+ extends Phase 2's is_assignable for subtyping)
   |
   v
Phase 8: extension functions  <-- needs Phase 1 (+ benefits from Phase 6's call_method path)
   |
   v
Phase 9: generics (parsed, fully erased — Phase 2 reserves Type::Generic for it)
   |
   v
Phase 10: coroutines (sequence{}/yield() only) — separate design spike before committing
```

Phases 3 and 4 can be built in either order or interleaved — they don't
depend on each other. Every other phase after Phase 2 does, directly or
indirectly.

---

## Phase 0 — Tech-debt cleanup (do first)

**0a. `NativeFn` call-site spans.** `NativeFn` (`value.rs:10`) doesn't
receive a `Span`, so stdlib errors (`stdlib.rs:122-128`) report `0:0`
instead of the call site. Change the type alias to accept a `Span`
parameter, thread it from `call_value`'s `Value::Native` arm
(`interpreter.rs:329-332`, which already has `span` in scope), update all
11 stdlib registrations and the `arity_error`/`type_error`/`expect_str`/
`expect_num` helpers. Do this before Phases 2/4 add more stdlib functions,
so new code isn't migrated twice.

**0b. `break`/`continue` escaping through function/lambda boundaries.**
Currently unwinds via bare `Flow::Break`/`Continue` with no barrier at
`call_function` (only `Flow::Return` is caught there), so a `break` inside
a lambda or nested `fun` body silently escapes to the *caller's* enclosing
loop — unlike real Kotlin, which rejects this at compile time. Fix at
**parse time**: give `Parser` a `loop_depth: u32` counter, incremented
around `while`/`for` bodies, saved-and-reset-to-0 around `parse_fun_decl`
and lambda-body parsing. Reject `break`/`continue` with `loop_depth == 0`
as a `ParseError` with an accurate span. Self-contained in `parser.rs`, no
`Flow`/AST changes needed.

**LSP:** neither change adds a new `ExprKind`/`StmtKind`, so no
`resolver.rs`/semantic-token/completion arms are needed. Do confirm the
`break`/`continue` `ParseError` surfaces correctly through `ksl-lsp`'s
existing parse-error diagnostics path (`diagnostics.rs`, on `didOpen`/
`didChange`) with a sensible message and span — it's a new *kind* of
parse error, worth a quick manual check even though no code there
changes.

**Tests:** unit tests in `parser.rs`/`stdlib.rs` for both; update AGENTS.md
to drop these from its "known gap" callouts once fixed.

---

## Phase 1 — `.` postfix member-access syntax

The linchpin: classes, inheritance, and extension functions are all
inexpressible without `receiver.method(...)`.

**Parsing rework.** Currently `parse_primary` eagerly slurps an entire
dotted chain into `ExprKind::QualifiedName` the moment it sees `Ident`
followed by `.` (`parser.rs:647-661`) — `parse_postfix`
(`parser.rs:589-607`) has no `.` handling at all today. This must change:
`parse_primary` on `Ident` returns a bare `ExprKind::Name`; `parse_postfix`
grows a `.`-handling loop that, after consuming `.` + member name, checks
for a following `(` to decide between `ExprKind::MethodCall { receiver,
method, method_span, args }` and `ExprKind::FieldAccess { receiver, field,
field_span }`.

**Package-qualified names must keep working without a parser-level
type/scope check** (the parser can't know whether `a` is a local or a
package name). Resolve this by **deferring to evaluation**: the parser
never produces `QualifiedName` for expressions anymore (only for
`package`/`import` declaration parsing, which is a separate grammar
position); `eval_expr` on `FieldAccess`/`MethodCall` tries local
resolution first, and if the base `Name` is genuinely undefined, flattens
the chain and falls back to `resolve_qualified` (`interpreter.rs:62-71`),
exactly as `QualifiedName` already does today. This needs explicit test
coverage for both `a.b` (local object field) and `a.b` (package-qualified)
resolving correctly, plus the "neither resolves" error case.

**Interpreter.** New `eval_expr` arms for `FieldAccess`/`MethodCall`. Ship
this phase with real value: convert a handful of existing stdlib functions
into a builtin-method dispatch table (`call_builtin_method(receiver,
method, args, span)` matching on `(type_name(), method)`), e.g.
`"x".length()`, `"x".uppercase()`. `FieldAccess` on primitives (no fields
yet) is a clear runtime error until Phase 6.

**LSP (do in the same phase, not a follow-up — matches existing repo
pattern):** `resolver.rs` needs `FieldAccess`/`MethodCall` arms (recurse
into receiver/args at minimum); `semantic_tokens.rs` classifies the new
`method_span`/`field_span` spans; `completion.rs` offers the builtin-method
table after `.` (document that it over-suggests until Phase 6 gives it
real per-type member info — Phase 2 later narrows this to the receiver's
statically known primitive type, but a class/object receiver still shows
everything until Phase 6); `hover.rs`/`goto_definition.rs` keep
package-qualified hover/goto-def working, add hover for builtin methods.

**Docs/tests:** update `docs/syntax.md`'s precedence table and "Not
supported" list, `docs/stdlib.md`'s new "methods" section. Parser + eval
unit tests per above; new `tests/examples/method_calls.ksl` + `.out`.

---

## Phase 2 — Static & runtime type system

Requires Phase 1 (`MethodCall`/`FieldAccess` are what a receiver's static
type needs to hook into for type-aware completion — the concrete bug this
phase fixes). Best-effort and syntax-directed, deliberately not full
whole-program unification — mirrors how this roadmap already scopes down
coroutines and generics elsewhere. The rule that makes this tractable:
**`Type::Unknown` is always compatible, on either side** — anything not
statically inferable is never rejected, only left unchecked.

**New module** `crates/ksl-core/src/types.rs`: `Type { Int, Double, Bool,
String, Unit, List, Function, Class(Rc<str>), Generic(Rc<str>), Unknown }`.
`Class`/`Generic` are reserved *now*, unconstructable until Phases 6/9
exist, so those phases only ever need to populate a registry
(`known_classes`/`generic_params`, both threaded through
`resolve_type_hint` already, always empty until then) rather than change
this enum's shape later. `resolve_type_hint(hint, generic_params,
known_classes) -> Type` is the one place a raw annotation string becomes a
`Type` (known primitive → that type; matches a generic param in scope →
`Type::Generic`; matches a known class → `Type::Class`; else `Unknown`).
`is_assignable(from, to) -> bool`: `Unknown`/`Generic` on either side ⇒
`true`; `Class(a)`/`Class(b)` ⇒ exact name match only (no subtyping until
Phase 7); everything else ⇒ exact equality — deliberately **no** numeric
`Int`→`Double` widening here (that stays exactly where it already lived,
arithmetic result-type inference, not assignment compatibility).

**Inference** (`resolver.rs`'s existing `walk_expr`/`walk_stmt`/`walk_stmts`
made to return the expression/statement-list's inferred `Type` alongside
their existing side effects, rather than a separate second traversal):
literals type themselves; a `Name` looks up its resolved `Symbol.ty`; a
builtin `MethodCall`/`FieldAccess` on a concretely-typed receiver looks up
`stdlib::builtin_method_return_type`; arithmetic follows the existing
`Int`/`Double` widening rule, comparisons/`&&`/`||` are always `Bool`; a
`Call` to a bare-`Name` `Fun` symbol yields its declared return type;
`if`/`when`/`{}` join same-typed branches (a missing `else`/`else ->`
implicitly joins with `Unit`, matching the runtime's own "no branch taken"
rule) or fall back to `Unknown` on a mismatch. Everything else (package-
qualified calls, stdlib calls, anything touching an already-`Unknown`
value) is `Unknown`.

**`Symbol` (`resolver.rs`) gains**: `ty: Type` (inferred/declared — for
`Fun`, always `Type::Function`, not its return type), `declared: bool`
(true only when an explicit type-hint annotation was present — see next
paragraph), `return_type: Type` and `param_types: Vec<Type>` (`Fun` only).

**Enforcement, both ways, per explicit product decision** (not a warning —
a hard rejection, as close to Kotlin's compile-time behavior as a
no-compile-step interpreter can get):
- *Runtime* (`interpreter.rs`/`environment.rs`, always on): `call_function`
  checks each bound argument against `Param.type_hint` and the result
  against a new `FunDecl.return_type`/`FunctionValue.return_type` field
  (the return-type annotation used to be parsed and thrown away —
  `parse_fun_decl` now keeps it); `Env::Binding` gains a `declared_type`
  field, checked in `Env::assign` (`AssignError::TypeMismatch`, mapped to a
  `RuntimeError` in `interpreter.rs`'s `Assign` handler). **Critical
  scoping rule**: only a `val`/`var` with an *explicit* type-hint sets
  `declared_type`/`Symbol.declared` — a hint-less `var`'s type is still
  inferred (useful for argument/join checks elsewhere) but never enforced
  on reassignment, so untyped locals keep their full pre-existing dynamic
  flexibility. Getting this wrong (enforcing inferred-but-undeclared types)
  would silently make ordinary untyped scripts stricter than documented.
- *Static* (`resolver.rs`): `ResolveResult` gains a `type_errors:
  Vec<RuntimeError>` field, kept *separate* from the pre-existing
  `diagnostics` (undefined-name, duplicate-declaration) — checked at `val`/
  `var` initialization, `var` reassignment (same declared-only rule as
  above), positional call arguments (named arguments are out of scope for
  v1), and `return` vs. a function's declared return type.
- *CLI* (`ksl-cli`): new `ksl-core::static_check(src)` (parse +
  `resolver::resolve`, no execution) runs before `run_source`; any
  `type_errors` abort the script with a non-zero exit and nothing from the
  script body ever printed — this is the "reject before running" behavior
  explicitly asked for. The pre-existing `diagnostics` list stays an
  editor-only (LSP) concern for this phase, unchanged.

**LSP:** `stdlib::BUILTIN_METHOD_SIGNATURES` grows a leading receiver-type
field (3-tuple → 4-tuple); `completion.rs` filters it to the receiver's
statically known type when the `.`-prefix is a single bare identifier
(falls back to showing everything when the type is `Unknown`, unresolved,
or the prefix looks package-shaped — never under-suggests); `hover.rs`
requires a new `method_receiver_type_at` match (a new `resolver.rs`
side-table, `method_receiver_types`) in addition to the method-name match,
so a nonsensical `5.uppercase()` correctly shows no hover; `diagnostics.rs`
merges `type_errors` into the diagnostics it already returns.

**Docs/tests:** unit tests in `types.rs`/`resolver.rs`/`interpreter.rs`
(including forward-compat contract tests for `Type::Class`/`Generic` via
direct calls with synthetic context, since no real syntax reaches them
yet); new `tests/examples/type_checking.ksl` + `.out` (a passing-only
script — the harness treats any `Err` as failure — demonstrating correctly
typed code end-to-end, plus proof the existing dynamic-typing behaviors
are unchanged); `completion.rs`/`hover.rs`/`diagnostics.rs` tests for the
type-aware filtering.

---

## Phase 3 — Collection literals

Independent of Phase 1; can land any time after Phase 0.

- **List literals** `[1, 2, 3]`: tokens already exist (`LBracket`/
  `RBracket` are lexed, `token.rs:40-41`). New `ExprKind::ListLit(Vec<Expr>)`
  parsed in `parse_primary` alongside the existing `(`/`{` arms. Evaluates
  to the existing `Value::List` — no new `Value` variant, pure sugar over
  `listOf(...)`.
- **Maps** (per the decision above, no bracket syntax): new
  `Value::Map(Rc<RefCell<Vec<(Value, Value)>>>)` — association-list backed
  since `Value` has no `Hash`/`Eq`, only a custom `values_equal`. New
  stdlib: `mapOf(pairs...)`, `pair(a, b)` (or accept `listOf(k,v)` pairs
  directly, needing no new stdlib entry beyond `mapOf`), `get`,
  `containsKey`, `keys`, `values`. Needs `type_name`, `Display`
  (`{k=v, ...}`), `values_equal`, and an `iterate()` arm (yielding 2-elem
  lists, matching Kotlin's `Map.Entry` iteration) added alongside `List`'s
  existing handling in `value.rs`/`interpreter.rs:401-406`.

**LSP:** `resolver.rs` gets a `ListLit` arm (recurse into elements,
mechanical); new stdlib functions flow through the existing
`STDLIB_SIGNATURES`-driven LSP machinery automatically.

**Docs/tests:** `docs/syntax.md` literals section + remove from "Not
supported"; `docs/stdlib.md` entries + `STDLIB_SIGNATURES`. Tests: empty/
nested list literals, map round-trip (`get`/`containsKey`/`keys`/`values`),
map equality, `for (entry in map)`. New `tests/examples/collections.ksl`.

---

## Phase 4 — Null-safety

Independent of Phase 1 and Phase 3; requires Phase 2 (its `Type`/
`is_assignable` machinery is what makes nullability checkable both
statically and at runtime — see below). No `null` concept exists at all
today (confirmed: no `Value::Null`, no nullable-hint handling in
`check_type`) — this is from-scratch, not an extension.

- **`Value::Null`** — new variant, touches every exhaustive match over
  `Value` (`type_name`, `is_truthy`, `values_equal`, `Display` in
  `value.rs`; `apply_binary`, `compare_values`, `check_type`, `iterate` in
  `interpreter.rs`; `expect_str`/`expect_num`/`numeric_binary` in
  `stdlib.rs`). Rust's exhaustiveness checking catches every missed arm at
  compile time (a real safety net here) — but watch for any `_ => {}`
  wildcard arms in `ksl-lsp` modules, which would swallow it silently.
  `values_equal(Null, Null) == true`; `Null` vs. anything else `== false`
  (never a type error — `== null` must always be legal); arithmetic/
  ordering ops on `Null` are runtime type errors.
- **Nullable type hints** (`String?`): add a `nullable: bool` field
  alongside existing `type_hint: Option<String>` on `Param`/`ValDecl`
  rather than string-suffix-hacking `"String?"` everywhere it's read.
  `check_type` allows `Null` only when `nullable` is set. Unlike when this
  phase was originally scoped, this is now enforced **both ways**, same as
  every other type rule since Phase 2 landed: at runtime (the existing
  `check_type` extension) *and* statically, by `resolver.rs`'s inference —
  a `val x: String = null` (or a provably-`Null`-typed expression assigned
  to a non-nullable hint) is flagged in `type_errors` under the same "both
  sides statically known" gate as everything else. This is still
  best-effort, not full Kotlin-grade flow-sensitive null tracking (e.g. no
  smart-casting after an `if (x != null)` check) — a real, documented
  divergence from actual Kotlin null-safety, just a narrower one than "no
  static checking at all."
- **`?.` safe call**: `safe: bool` flag on `FieldAccess`/`MethodCall`
  (Phase 1), set by a dedicated `?.` token. Short-circuits the whole chain
  to `Value::Null` if receiver is `Null`, without evaluating the member.
- **`?:` elvis**: short-circuit like `&&`/`||` (handled specially in
  `eval_expr`, not via eager `apply_binary`) — RHS must not evaluate if LHS
  is non-null.
- **`!!` not-null assertion**: postfix (unlike prefix `-`/`!`), so it's a
  new arm in `parse_postfix`, not `parse_unary`. Lex `!!` as its own token
  (matches how `&&`/`||` are single tokens) to avoid adjacency reasoning.
  Errors on `Null` with "null cannot be cast to non-null type"; passes
  through otherwise.
- **`null` literal**: new keyword token, `ExprKind::NullLit`.

**LSP:** `resolver.rs` arms for `NullLit` and the `safe`-flag variants;
`semantic_tokens.rs` classifies `null`/`?.`/`?:`/`!!`.

**Docs/tests:** new "Null safety" section in `docs/syntax.md` with the
best-effort (not full flow-sensitive) caveat spelled out explicitly;
remove from "Not supported." Tests: `x?.length()` on null receiver, `?:`
fallback and short-circuit, `!!` error + span, non-nullable assigned
`null` errors both statically and at runtime, `null == null`, chained
`a?.b?.c`. New `tests/examples/null_safety.ksl`.

---

## Phase 5 — `try`/`catch`

**`Flow::Throw(Value)`** — new variant on `Flow` (`error.rs:90-95`),
exactly the extension point AGENTS.md calls for. `throw expr` is an
**expression** (`ExprKind::Throw(Box<Expr>)`, not a statement), matching
real Kotlin where `throw` has type `Nothing` and is usable in expression
position (`if (cond) 1 else throw Foo()`) — consistent with ksl already
treating `if`/`when`/blocks as expressions.

**`try { } catch (e: T) { } finally { }`** as `ExprKind::Try { body,
catches: Vec<CatchClause>, finally }`, usable as an expression like
`if`/`when`. On `Err(Flow::Throw(v))`, walk `catches` in order matching by
`class_name`; bind the catch param in a child env; evaluate that catch
body as the result. **Critical correctness detail**: `finally` must run
even when `Flow::Return`/`Break`/`Continue`/another `Throw` is
propagating through the `try` body — catch *every* `Err(Flow)` variant,
run `finally` for side effects only (its own value is always discarded),
then re-raise whatever was caught. Easy to get wrong by special-casing
only `Throw`.

**What gets thrown**: per the decision above, a minimal
`Value::Error { message: Rc<str>, class_name: Rc<str> }` (constructed via
a stdlib `Exception(message)`/`RuntimeException(message)`), with
`class_name` defaulting to something like `"Exception"`. `catch (e: T)`
matches by string-comparing `class_name` (`"Exception"`/`"Throwable"`
matching everything, hard-coded, non-extensible until Phase 6).
**Existing `RuntimeError`/`Flow::Error` stays fatal and uncatchable** in
this phase — only explicitly-`throw`n values are catchable. Making
built-in runtime errors catchable is a separate, bigger semantic decision
left for later. Design `Value::Error`'s shape so Phase 6 can later
reimplement it as sugar over `Value::Object` (an instance of a builtin
`Exception` class) without changing the `Flow::Throw(Value)` contract.

**LSP:** `resolver.rs` arms for `Try`/`Throw`, entering a child scope per
`catch` clause (same pattern as existing `If`/`When`/`For` handling);
`semantic_tokens.rs` keywords for `try`/`catch`/`finally`/`throw`.

**Docs/tests:** new "Exceptions" section in `docs/syntax.md`; remove
`try`/`catch` from "Not supported"; document `Exception`/`RuntimeException`
in `docs/stdlib.md`. Tests: catch matching, `finally` always running
(caught / uncaught / no exception / non-Throw Flow passing through),
`try` as an expression with a fallback value, multiple catch clauses,
uncaught exception aborting the script with a sensible top-level message.
New `tests/examples/exceptions.ksl`.

---

## Phase 6 — Classes/objects

The largest phase. Requires Phase 1.

**New module** `crates/ksl-core/src/class.rs` (this feature is big enough
to warrant its own file rather than growing `value.rs`/`interpreter.rs`
further, matching the existing one-concern-per-file layout).

- `Value::Object(Rc<ObjectValue>)`, `ObjectValue { class: Rc<ClassValue>,
  fields: RefCell<HashMap<String, Value>> }` — mirrors the existing
  `Rc`-wrapped-struct pattern (`FunctionValue`) and interior-mutability
  pattern (`List`'s `Rc<RefCell<Vec<Value>>>`).
- `Value::Class(Rc<ClassValue>)` — the class itself as a callable value;
  `ClassValue { name, fields: Vec<FieldDecl>, methods:
  HashMap<String, Rc<FunctionValue>>, constructor_params: Vec<ClassParam>,
  superclass: Option<Rc<ClassValue>> }` (superclass field reserved now for
  Phase 7).
- **`this` binding**: new `call_method` path (parallel to
  `call_function`, sharing its body-execution tail) that additionally
  binds `this` in the call env before parameter binding.
- **Parsing**: `class Foo(val x: Int, var y: String = "d") { fun m() { } }`
  — Kotlin's primary-constructor-in-header shorthand, reusing existing
  `Param` parsing plus a `val`/`var` promotion marker (a thin
  `ClassParam { param, is_field }` wrapper rather than polluting plain
  function `Param`). `init { }` blocks for constructor logic beyond simple
  field promotion. `this` as `ExprKind::This`, a dedicated node (not a bare
  `Name` lookup) so "used outside a method" gets a clear diagnostic.
- **Construction**: `Foo(1, "x")` is an ordinary `Call` whose callee
  evaluates to `Value::Class` — new arm in `call_value`
  (`interpreter.rs:322-336`) builds a fresh `ObjectValue`, runs
  field-promotion + `init` blocks, returns `Value::Object`.
- **Method dispatch**: pure runtime `HashMap` lookup on the value's actual
  class — still no static-dispatch complexity, but now because ksl has no
  method *overloading* for a type system to disambiguate between, not
  because there's no type system at all (Phase 2 added one). `MethodCall`
  (Phase 1) gains a branch: if receiver is `Value::Object`, look up on its
  class; if not found, fall through to Phase 1's builtin-method table, then
  (Phase 8) extension functions.
- **Equality**: `values_equal` needs a `Value::Object` arm — reference
  equality (`Rc::ptr_eq`) by default, matching Kotlin's default `==`
  without an `equals()` override.
- **Type system integration**: `types::Type::Class(Rc<str>)` is already
  reserved (Phase 2) for exactly this — this phase's job is only to
  populate `Resolver::known_classes` by scanning top-level `class`
  declarations (mirroring how top-level `fun`/`val` names are already
  collected) and give `ObjectValue`/`ClassValue` a way to report their
  class name to `Type::from_value`. No changes to `types.rs`'s enum shape
  or `resolve_type_hint`'s signature are needed. `is_assignable`'s
  `(Class, Class)` arm still only does exact-name equality — real
  inheritance-aware subtyping (`Dog` assignable to `val x: Animal`) is
  Phase 7's job, not this one's.

**LSP — substantial, not mechanical**: `resolver.rs` needs a new
class/member side-table (`HashMap<String, ClassInfo>`) so `this.field`/
`obj.method()` resolve for goto-def/hover; `completion.rs` needs the same
table to finally offer real post-`.` member completion (resolving Phase
1's "over-suggests" limitation, and superseding Phase 2's type-name-based
filtering for object receivers specifically); `semantic_tokens.rs` needs
token categories for class names/`this`/field/method names if not already
present. Budget real time here.

**Docs/tests:** big "Classes" section in `docs/syntax.md`, explicitly
noting `open`/`override`/`:` superclass syntax isn't supported yet if
Phase 7 hasn't landed. Tests: field read/write, constructor defaults,
method-calling-method via `this`, builder-style chaining, object identity.
New `tests/examples/classes.ksl`.

---

## Phase 7 — Inheritance

Requires Phase 6 (`superclass` field already reserved on `ClassValue`).

- `class Dog : Animal(name) { override fun speak() { } }` — new `:` class
  header syntax (reuses existing `Colon` token), superclass name +
  `super(...)` constructor delegation args.
- **`open`/`override` accepted syntactically but not enforced** in this
  first cut (consistent with the project's existing pattern of accepting-
  but-not-enforcing unknown type annotations) — document as a deliberate
  simplification.
- Method lookup walks `class.superclass` when not found on the immediate
  class — small, mechanical extension of Phase 6's lookup helper.
- `super.method()`: new `ExprKind::Super`, legal only as a
  `MethodCall`/`FieldAccess` receiver, skips straight to
  `class.superclass.methods` regardless of the object's actual runtime
  class.
- Constructor chaining: superclass fields/constructor run before the
  subclass's own field init/`init` blocks (matches Kotlin's init order).
- **Type system integration**: extend `types::is_assignable`'s
  `(Class(a), Class(b))` arm to walk `a`'s superclass chain looking for
  `b` (using whatever class-hierarchy side table this phase already builds
  for method-override lookup), instead of Phase 6's exact-name-only
  equality — this is what makes `val x: Animal = Dog()` correctly
  type-check. No `types.rs`/`resolve_type_hint` signature changes.

**LSP:** `resolver.rs`'s Phase 6 class-info table gains a `superclass`
link so goto-def/hover resolve inherited fields and overridden methods;
`completion.rs` walks the superclass chain too, so post-`.` completion
includes inherited members; `semantic_tokens.rs` classifies `open`/
`override`/`super`/`:` tokens.

**Docs/tests:** inheritance section in `docs/syntax.md`; tests for
override dispatch (always the *runtime* class's method wins),
`super.method()`, multi-level inheritance, subtype-compatible assignment
(`val x: Animal = Dog()` type-checks; the reverse doesn't).

---

## Phase 8 — Extension functions

Requires Phase 1 (dot-call syntax is the extension-function call site);
benefits from Phase 6's `call_method`-style `this`-binding path, which is
reused as-is here.

- `fun String.shout(): String = uppercase(this) + "!"` — new optional
  receiver-type prefix in function-declaration parsing (`Ident` + `.`
  lookahead right after `fun`).
- Storage: a side-table (or, better for LSP reuse, lexically scoped
  alongside normal `Env` bindings) keyed by `(receiver_type_name,
  function_name)`.
- **Dispatch precedence** (document explicitly): real object methods
  (Phase 6) > builtin methods (Phase 1) > extension functions — matches
  Kotlin's rule that members always beat extensions with the same name.
- `this` inside the extension body refers to the receiver, via the same
  binding mechanism Phase 6 built for methods.

**LSP:** `resolver.rs` needs to track in-scope extension-function
declarations (keyed by receiver type) the same way it already tracks
plain functions, so goto-def/hover work on an extension-function call;
`completion.rs` needs the same table to offer extensions after `.`
alongside builtin/object methods, respecting the real-method-wins
precedence when ranking suggestions; `semantic_tokens.rs` classifies the
receiver-type prefix in `fun String.shout()` declarations.

**Docs/tests:** extension-functions section spelling out the precedence
rule. Tests: extensions on `String`/`Int`/`List`/user classes; a test
confirming real-method-over-extension precedence.

---

## Phase 9 — Generics (parsed, fully erased)

Requires Phase 6 (class headers are the primary place `<T>` appears).
Erasure here is a **deliberate, sound design choice**, not an accident of
there being no type system to hook into (Phase 2 added one) — `<T>` is
parsed and stored (for hover text) but never checked or specialized.

- **Grammar-ambiguity constraint**: `<`/`>` are already `Lt`/`Gt`
  comparison tokens — `Box<T>` vs. `a < b` is ambiguous in a
  backtracking-free recursive-descent parser. Resolve by **restricting
  `<...>` type-parameter parsing to declaration headers only**
  (immediately after `class Name` or `fun`, before the parameter list),
  never in general expression position. **Do not support call-site type
  arguments** (`identity<Int>(5)`) — they'd reintroduce the ambiguity in
  expression position for zero runtime benefit, since there's nothing to
  specialize against anyway.
- `ClassDecl`/`FunDecl` gain an optional `type_params: Vec<String>` (names
  + spans, for hover only). Push this list onto `Resolver::generic_params`
  (Phase 2, always empty until now) while walking the declaration's body,
  so a type-hint like `x: T` resolves to `types::Type::Generic("T")` —
  reserved back in Phase 2 for exactly this — instead of an accidental
  `Unknown`. `Generic` behaves identically to `Unknown` in `is_assignable`,
  so **zero interpreter changes are needed** — a useful confirmation that
  erased generics fit the type system's existing rules; only `resolver.rs`
  needs the `type_params` plumbing.

**LSP:** `resolver.rs`/`hover.rs` show the declared `type_params` names in
hover text for `class`/`fun` declarations (documentation value only, no
type-checking implied — hover for a `Type::Generic("T")`-typed symbol
should read as "T", not "Unknown"); `semantic_tokens.rs` classifies
type-parameter names distinctly from ordinary identifiers so they read as
generic placeholders in the editor.

**Docs/tests:** `docs/syntax.md` must be explicit that this is purely
syntactic — no checking, no specialization. Tests: `class Box<T>(val
value: T)` works with any value type; `fun <T> identity(x: T): T = x`
works interchangeably; regression test that `a < b` in ordinary expression
position is unaffected.

---

## Phase 10 — Coroutines (`sequence {}` / `yield()` only)

Scoped down per the decision above — no `launch`/`async`/dispatchers, only
cooperative single-threaded generators, matching what Kotlin itself calls
"restricted suspension" (`sequence {}` builder).

**This needs a design spike before implementation**, because it's the one
feature that doesn't fit the "add an ExprKind + eval arm" shape of every
other phase — it requires the tree-walking evaluator to become
*resumable* at `yield` points, which Rust's natural recursive call stack
doesn't support without either:
(a) a dedicated OS thread per active generator with a synchronous
hand-off channel (technically introduces thread machinery even though
logically cooperative — needs explicit sign-off since it's in tension with
AGENTS.md's `Rc`-not-`Arc`, no-`Send`/`Sync` constraint), or
(b) a genuine CPS/state-machine rewrite of the evaluator's execution
strategy for resumability (architecturally a different interpreter, not
an incremental feature).

**Recommendation**: treat this phase as a standalone prototype/spike
*after* Phases 0–9 land, to pick between (a)/(b) with actual code in hand,
rather than committing to an approach up front. Do not block any other
phase on it — it's placed last and nothing else in this roadmap depends
on it.

**LSP:** deferred until the resumability approach is picked (the LSP
surface depends on it — e.g. whether `yield` is a special form only legal
inside a `sequence { }` block affects what `resolver.rs`/diagnostics can
validate). At minimum, once implemented: `resolver.rs` arms for
`sequence`/`yield`, `semantic_tokens.rs` keyword classification — same
lockstep rule as every other phase, just necessarily decided after the
spike rather than up front.

---

## Cross-cutting rules for every phase

- Follow AGENTS.md's existing "Adding a feature checklist" literally:
  tokens → AST → parser (correct precedence) → interpreter eval → unit
  tests colocated in the module → `tests/examples/*.ksl` + `.out` fixture
  → (stdlib) `STDLIB_SIGNATURES` + `docs/stdlib.md` entry.
- **Update `ksl-lsp` (`resolver.rs` and whichever of `semantic_tokens.rs`/
  `completion.rs`/`hover.rs`/`goto_definition.rs`/`diagnostics.rs` pattern-
  match the touched `ExprKind`/`StmtKind`) in the same change as the
  interpreter work**, not as a follow-up. Rust's exhaustive `match` over
  these enums forces every miss to a compile error *unless* a module uses
  a wildcard `_ => {}` arm, which would silently swallow new variants —
  grep for that pattern specifically when touching `ksl-lsp/src/*.rs`.
- **Never hand-compute `.out` fixtures involving floating-point output** —
  generate via `cargo run -- tests/examples/foo.ksl` and capture real
  output (Rust's `f64` `Display` differs from Kotlin's).
- `cargo test` (whole workspace) must pass after every phase.

## Verification

After each phase: `cargo build` (whole workspace), `cargo test` (whole
workspace — unit tests + `run_examples.rs` + any `ksl-lsp` tests), and
manually run the new example script(s) via `cargo run -p ksl-cli --
tests/examples/<new>.ksl` to confirm output before hand-writing (or
better, capturing) the `.out` fixture.

## Critical files

- `crates/ksl-core/src/parser.rs`, `ast.rs`, `interpreter.rs`, `value.rs`,
  `error.rs`, `stdlib.rs`, `resolver.rs` — core language work.
- `crates/ksl-core/src/types.rs` — the static/runtime type system (Phase 2).
- `crates/ksl-core/src/class.rs` — new module for Phase 6/7.
- `crates/ksl-lsp/src/*.rs` — LSP-side updates, per phase, in lockstep.
- `docs/syntax.md`, `docs/stdlib.md` — spec updates per phase.
- `AGENTS.md` — update "Explicitly out of scope" as each item lands.
