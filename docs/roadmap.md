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
  assuming it exists, in particular Phases 4 (null-safety), 4.5
  (`Any`/`Nothing`), 6 (classes), 6.5 (interfaces), 7 (inheritance), and 9
  (generics), which each have a note on exactly how they build on it.
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
- **No `[1, 2, 3]` list-literal syntax either — this was a mistake in the
  original 9-feature backlog above, corrected during Phase 4's planning.**
  "Rich collection literals" (this document's and `AGENTS.md`'s original
  phrasing) doesn't describe a real Kotlin feature: Kotlin has no bracket
  list-literal expression at all, only `listOf(...)`/`arrayOf(...)`/etc.
  and `[i]` *indexing*. The free `LBracket`/`RBracket` tokens this backlog
  item was going to spend are used for indexing instead — see Phase 3's
  section, which was rewritten and moved later in the dependency order as
  a result (it now needs classes/generics/properties/`operator fun`
  support, none of which existed when it was originally sequenced right
  after Phase 2).
- **`Any`/`Nothing` (Kotlin's real top/bottom types) are added as their
  own Phase 4.5**, ahead of everything except the already-shipped type
  system (Phase 2) and null-safety (Phase 4) — motivated concretely by
  `throw`'s own type (Phase 5's text already says "matches real Kotlin
  where `throw` has type `Nothing`" but never actually wired that up) and
  by Phase 6.5's least-upper-bound branch inference (below), which needs
  `Any` as its fallback when two branches share no common interface. See
  Phase 4.5's own section.
- **Phase 9 (generics)'s original "parsed, fully erased" scope was
  revised** after the concrete `Comparable<*>`-for-`if`/`when`-branches
  example (below, Phase 6.5) surfaced a real need for statically-retained
  generic parameterization — `Box<Int>`/`Box<String>` need to stay
  distinguishable *types* to the resolver, not collapse to the same
  erased placeholder. Erasure survives, but moves to mean only "the
  *runtime* doesn't specialize on a type argument" (exactly Kotlin/JVM's
  own erasure, not an ksl-specific simplification) rather than "the type
  system doesn't track one at all." See Phase 9's own section.
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
- **A Kotlin conformance test harness (JetBrains's own compiler `box`
  tests) is added early, as its own Phase 0.5**, independent of and never
  gated by any feature phase below — the whole point is to start
  exercising already-shipped functionality (Phases 0–4.5 today) rather
  than waiting until the end, and to keep growing opportunistically as
  each later phase lands real syntax the corpus needs. See Phase 0.5's own
  section.
- **ksl's numeric types don't yet match real Kotlin's widths/set** — its
  `Int` is actually 64-bit (documented as `i64` in `docs/syntax.md`, but
  that's what real Kotlin calls `Long`), and `Long`/`Float`/`Byte`/`Short`/
  `Char` don't exist at all. Recorded as its own **Phase 2.3, audit only —
  no implementation** (found via the same session that fixed `kotlin.math`'s
  overload signatures; see that phase's own section for the full findings
  and open questions).

## Dependency order

```
Phase 0: tech-debt (NativeFn span, break/continue fix)
   |
   +--> Phase 0.5: Kotlin box-test conformance harness [initial cut shipped]
   |    (independent side-track — starts here, grows opportunistically as
   |    every later phase lands, never gates or is gated by anything below)
   +--> Phase 0.8: `do { ... } while (cond)` [DONE]
   |    (independent side-track, surfaced by Phase 0.5's audit — same
   |    dependency-graph position `++`/`--` had, no prerequisites beyond
   |    Phase 0, gates nothing below)
   v
Phase 1: `.` postfix member-access syntax  <-- blocks 6, 7, 8
   |
   v
Phase 2: static & runtime type system  <-- needs Phase 1 (type-aware completion hooks into MethodCall)
   |
   +--> Phase 2.3: numeric type width fix + missing primitives
   |    (Byte/Short/Long/Float/Char) [audit only, not implemented —
   |    independent side-track, like Phase 0.5, never gates anything below)
   v
Phase 4: null-safety (Value::Null, ?, ?., ?:, !!)   [DONE]
   |
   v
Phase 4.5: Any & Nothing (top/bottom types) [DONE]  <-- needs only Phase 2 (types.rs); no classes/interfaces required
   |
   v
Phase 5: try/catch (Flow::Throw, minimal Value::Error)  <-- Throw's type is Phase 4.5's Type::Nothing
   |
   v
Phase 6: classes/objects [DONE]  <-- needs Phase 1 (+ Phase 2 reserves Type::Class for it)
   |
   v
Phase 6.5: interfaces  <-- needs Phase 6 (classes to implement them, class.rs to extend)
   |
   v
Phase 7: inheritance  <-- needs Phase 6 (+ extends Phase 2's is_assignable for subtyping,
   |    now also covering Phase 6.5's interface-conformance subtyping)
   v
Phase 8: extension functions  <-- needs Phase 1 (+ benefits from Phase 6's call_method path)
   |
   v
Phase 9: generics (statically parameterized, runtime-erased — revised, see
   Phase 9's own section; Phase 2 reserves Type::Generic for it)
   |
   v
Phase 3 (renumbered, was pre-Phase-4): collection indexing (a[i], a[i]=v)
as real operator overloading on a generic List<T> class  <-- needs Phase 6
(classes), Phase 9 (generics), and properties + `operator fun get`/`set`
support that no phase above defines yet — see the note below and Phase
3's own section
   |
   v
Phase 10: coroutines (sequence{}/yield() only) — separate design spike before committing
```

**Phase 3 was redefined during Phase 4's implementation planning**, after
an audit found its original text specified invented syntax — see Phase
3's own section below (still positioned physically where it originally
was, right after Phase 2, purely so this document doesn't need a large
reshuffle — its *dependency-graph* position, shown above, is what actually
moved) for the full rationale. It's renamed from "collection literals" to
"collection indexing", and now depends on classes (Phase 6), generics
(Phase 9), and properties/`operator fun get`/`set` support that no
existing phase defines yet, instead of sitting right after Phase 2. This
is why Phase 4 was implemented *before* Phase 3 despite the number
ordering — Phase 4 needed nothing Phase 3 (old or new) provides, and the
audit that reordered Phase 3 happened during Phase 4's own planning pass.
Every phase number is kept as originally assigned so cross-references
elsewhere in this document stay valid.

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

## Phase 0.5 — Kotlin conformance test harness (JetBrains box tests) [initial cut shipped]

Independent of every feature phase in this document — started early,
specifically so it can already exercise everything shipped so far (Phases
0–4.5), and grown incrementally as each later phase lands, rather than
gating on any of them or being gated by them. Marked "initial cut shipped"
rather than `[DONE]`, unlike other phases — by design this one never
really finishes; see "Shipped" below for what actually landed and what's
still open.

**What upstream provides.** `JetBrains/kotlin`'s own compiler test suite
(`compiler/testData/codegen/box/`, Apache License 2.0 —
`license/LICENSE.txt` in that repo) has ~179 top-level subdirectories
covering essentially every Kotlin language feature (`basics`, `functions`,
`controlStructures`, `arithmetic`, `classes`, `coroutines`, plus
JVM/JS/klib-backend-specific and IR-dump directories). Each test is a
single `.kt` file with `fun box(): String`; real Kotlin's own compiler
considers a test passed iff `box()` returns the literal string `"OK"`
(most tests just `return "fail ..."` inline on a mismatch; some
`import kotlin.test.*` for `assertEquals`/`assertTrue`/`assertFailsWith`
instead). Files also carry compiler-internal directive comments at the top
(`// WITH_STDLIB`, `// DUMP_IR`, `// IGNORE_BACKEND: ...`, `// FILE:
other.kt` for multi-file tests) — ordinary `//` comments syntactically
(harmless to ksl's lexer), but several describe things ksl has no
equivalent of (backend selection, IR dumps) or change the test's actual
shape (multi-file).

**Audited a sample** (`basics/local_variable.kt`,
`controlStructures/doWhile.kt`, `functions/defaultargs.kt`) against ksl's
current feature set and found three concrete blockers common enough to
matter even for the *simplest* tests, none previously tracked anywhere in
this document:

1. **No `++`/`--` operators at all** (`lexer.rs`/`token.rs` have no
   increment/decrement token) — `do x++ while (...)`
   (`controlStructures/doWhile.kt`) can't even parse. Real Kotlin,
   low-risk, parser+interpreter-only (mirrors how `+=`/`-=`/etc. already
   work) — not part of the original 9-feature backlog this document
   otherwise sequences, so it had no phase of its own until now. **Ships
   as part of this phase** rather than getting its own phase number: it's
   what unlocks a meaningful slice of the corpus to even parse, and this
   is the phase that needs it.
2. **No property-style bare-field access on any value, including
   builtins.** `z.length` (String's real Kotlin property, no parens) is
   legal syntax (`ExprKind::FieldAccess`, Phase 1) but *always* a runtime
   error today, matching `docs/syntax.md`'s documented rule (fields need
   classes/objects, i.e. Phase 6). ksl's own stdlib works around this by
   exposing `length`/`uppercase`/etc. as `.method()` calls instead
   (`docs/stdlib.md#methods`) — a **permanent, deliberate divergence from
   real Kotlin syntax**, not a bug: `s.length()` isn't valid Kotlin (real
   `String.length` is a `val`, no call parens). No upstream test using a
   bare builtin property runs unmodified before Phase 6.
3. **`kotlin.test.*` doesn't exist.** `import kotlin.test.*` is Kotlin
   package-import syntax; ksl's own import is `@file:Import(...)`
   (`docs/syntax.md`'s "Imports" — different keyword, different grammar
   position) and doesn't parse it at all, independent of which assertion
   functions the test calls.

Tests avoiding all three — plain `return "fail"`/`"OK"`, no bare
properties, only `fun`/`val`/`var`/`if`/`when`/`while`/`for`/`return` over
`Int`/`Double`/`Boolean`/`String` — are already runnable today, unmodified.

**Vendoring: a curated, hand-picked subset, not a mirror of the whole
tree.** The upstream corpus is thousands of files covering coroutine
internals, JVM/JS-backend-specific codegen, and IR-dump sidecars — none of
which a tree-walking, no-bytecode-backend interpreter has any use for even
in principle. Pulling the whole tree would mean thousands of permanently
irrelevant fixtures with no path to ever passing, pure noise against the
actual goal of testing shipped functionality. Instead:

- `scripts/fetch-kotlin-box-tests.sh` fetches one **pinned upstream commit
  SHA**, copying a **curated file list** (grouped by subdirectory, kept in
  the script or a sibling `manifest.txt`) content-verbatim into
  `crates/ksl-core/tests/kotlin-box/<category>/<name>.ksl` (upstream's own
  `.kt` extension, renamed to `.ksl` on the way in — see "Shipped" below)
  — never hand-edited, so a re-fetch stays a clean diff against upstream.
  Record the pinned SHA and an Apache-2.0 attribution/`NOTICE` file at the
  vendor root.
- Start with hand-picked files (not whole directories) from `basics`,
  `functions`, `controlStructures`, `loops`, `arithmetic`, `elvis`,
  `binaryOp` that use only already-implemented syntax (see the "avoiding
  all three" rule above). Grow the list opportunistically per phase from
  there — Phase 5 unlocks a slice of `exceptions`/`finally`/`try`, Phase 6
  unlocks `classes`/`dataClasses`, Phase 9 unlocks a slice of `generics` —
  rather than pre-selecting the eventual full set now.
- **Explicitly out of scope**: multi-file (`// FILE:`) tests (ksl's own
  file-import mechanism is semantically different — worth a separate
  ksl-native test if ever needed, not a reason to adapt these);
  `coroutines`, `js`, `jvm8`, `klib`, `invokedynamic`,
  `devirtualization`, `escapeAnalysis`, and other backend-optimization
  directories (meaningless with no bytecode backend at all); `.ir.txt`
  sidecar files (compiler-internal, not test input).

**Harness — mirrors the existing example-fixture runner, doesn't invent a
new execution path.** New `crates/ksl-core/tests/kotlin_box_tests.rs`,
structurally parallel to `crates/ksl-core/tests/run_examples.rs` (which
already walks a directory and runs `ksl_core::run_source`, diffing
stdout): for each vendored `.ksl` file, run it with an in-memory-only
trailer (`println(box())`, never written back to the vendored file) and
assert stdout is exactly `"OK\n"`.

**Two-tier layout**, so the suite stays green and useful long before most
of the corpus can pass: `tests/kotlin-box/` (asserted-passing, part of
normal `cargo test`) vs. a parallel `tests/kotlin-box-pending/` (vendored
but not yet promoted). A second, always-green test walks
`kotlin-box-pending` and only asserts each file **parses** — a much weaker
bar, still useful signal — printing a pass/fail-reason summary rather than
a hard assertion, so `cargo test` output shows real progress as more of
the pending tree becomes promotable. Promotion (moving a file from
`kotlin-box-pending` to `kotlin-box`) stays a manual per-file step done as
part of whichever phase's own "Docs/tests" work unblocks it — e.g. Phase
6's own tests section is the natural place to promote the first batch of
`classes`/`dataClasses` files — not a separate cleanup pass.

**stdlib additions, from the same audit.** `assertEquals`/`assertTrue`/
`assertFalse`/`assertNull`/`assertNotNull` can land now — no language
feature required first: each is a thin wrapper comparing values with the
existing `values_equal`/`is_truthy` and raising the existing runtime-error
path on failure (the same pattern `abs`/`sqrt`/etc. already use for
`arity_error`/`type_error`, `stdlib.rs:122-128`) — the failure just aborts
the script the way any other runtime error already does. `assertFailsWith`
specifically needs `Flow::Throw` and waits for Phase 5. Add these to
`STDLIB_SIGNATURES` + `docs/stdlib.md` per the existing checklist (see
"Cross-cutting rules" below) — no exception for being test-only, since an
embedder of `ksl-core` may reasonably want `assertEquals` too.

**Docs:** a short `crates/ksl-core/tests/kotlin-box/README.md` records the
pinned upstream SHA, the fetch script, the two-tier layout, and the
promotion workflow above — living next to the vendored files rather than
duplicated here, so it doesn't go stale.

**Shipped (this initial cut), corrections found during implementation:**

- `++`/`--` landed as planned — both prefix (`++x`/`--x`, evaluates to the
  *new* value) and postfix (`x++`/`x--`, evaluates to the *old* value),
  restricted to a bare-name target (same restriction `=`/`+=` already
  have), sugar over `x = x + 1`/`x = x - 1` reusing the exact
  `apply_binary`/`Env::assign` path compound assignment already goes
  through — so it works on `Int` and `Double` locals identically to
  `+=`/`-=`, with the same mutability/type-mismatch runtime errors.
- `assertEquals`/`assertTrue`/`assertFalse`/`assertNull`/`assertNotNull`
  landed as planned, exactly as scoped above (`assertFailsWith` still
  waits for Phase 5).
- **A real, previously-undiscovered parser bug was found and fixed by this
  audit, independent of the three blockers above**: `parse_branch`
  (`parser.rs`, used for a brace-less `if`/`when` branch) only ever called
  `parse_expr`, never `parse_stmt` — so `if (cond) return "x"` and
  `if (cond) continue`, both with no braces, failed to parse at all
  (`return`/`break`/`continue` are `StmtKind`, not `ExprKind`, in this
  AST). This is completely ordinary Kotlin (`controlStructures/
  bottles.ksl`'s vendored sibling `continueInWhile.ksl` uses exactly this
  shape) and had nothing to do with `++`/`--`/imports — a real, if narrow,
  correctness gap the box-test audit surfaced simply by trying real
  upstream code, which is exactly the kind of bug this phase exists to
  catch early. Fixed by having `parse_branch` delegate to `parse_stmt`
  instead of hand-rolling an expression-only wrapper; see `docs/syntax.md`
  ("Control flow", "if"/"when") and `parser.rs`/`interpreter.rs`'s new
  regression tests.
- **A fourth, larger gap surfaced by the same audit**: ksl had **no
  `do { ... } while (cond)` loop** at all — not a smaller "missing
  operator" fix like `++`/`--`, but a whole new loop construct (new
  keyword, new `StmtKind`, new parser/interpreter/resolver support)
  disproportionate to this phase's own scope. Deliberately left
  unaddressed *here*; shipped later as its own **Phase 0.8** (see below).
- **Vendored so far**: 8 active
  (`controlStructures/{bottles,continueInWhile,factorialTest,ifConst1,kt1899,kt870}.ksl`,
  `elvis/nullNullOk.ksl`, `basics/concatenation.ksl`) + 3 pending
  (`controlStructures/{doWhile,kt2147}.ksl`,
  `basics/local_variable.ksl`), all at pinned SHA
  `8fc606186b9d2be2bce6b390f7bf12f7cdb41adc` — see
  `crates/ksl-core/tests/kotlin-box/README.md`'s own table for exactly
  which gap blocks each pending file.
- **Vendored files carry a `.ksl` extension on disk, not upstream's own
  `.kt`** — the fetch script strips `.kt` and appends `.ksl` on the
  destination path only (the fetch URL still uses real upstream's `.kt`
  path; content is unchanged). Added after `.kt` files sitting inside this
  Rust project caused an editor's Kotlin plugin (VS Code, specifically) to
  pick them up as real Kotlin source and flag every ksl-unsupported
  construct as an error — `.ksl` routes them to ksl's own tooling instead.
  See `crates/ksl-core/tests/kotlin-box/README.md`'s "Layout" section.
- **Superseded**: this bullet originally described stripping a leading
  `import kotlin.test.*`/`import kotlin.test.<name>` line at the harness
  level (text-rewriting the vendored file's *effective* content in
  memory, never the file on disk). Reconsidered almost immediately after
  shipping — a text-rewrite is a workaround, not real import support, and
  the vendored corpus needs it too often to leave as a permanent hack. It
  was replaced outright by real language support:
  - **`import a.b.*` (wildcard import) is now real syntax**, a general
    language feature independent of `kotlin.test` — `parser.rs`'s
    `parse_import_use` grew a `.`-then-`*` lookahead (`Star` is never a
    legal path segment otherwise), and `ast::ImportUse` gained a
    `wildcard: bool` (when set, every segment of `path` is the package
    name — no member position to store, since `*` isn't one). Resolves
    via a new `Interpreter::resolve_package_members` (the
    all-members counterpart of the existing single-member
    `resolve_qualified`) and a new `Env::own_bindings()` (every name
    defined directly on a scope, not inherited — exactly a package env's
    own top-level declarations, since a package env is always a direct
    child of `globals`); `importer.rs`'s `run_program` defines each
    returned `(name, value)` pair into the importing file's scope.
    `resolver.rs` can't know a wildcard's actual member set statically
    (no access to the interpreter's runtime package envs), so — same
    reasoning as the pre-existing `has_file_imports` leniency — a new
    `has_wildcard_import` flag suppresses the undefined-name diagnostic
    file-wide when any wildcard import is present, rather than false-flag
    every name it legitimately provides.
  - **`kotlin.test` is now a real, resolvable package** — `Interpreter::new`
    seeds a synthetic `"kotlin.test"` package env (a child of `globals`,
    populated by a new `stdlib::populate_kotlin_test` from a new
    `stdlib::KOTLIN_TEST_SIGNATURES` table — currently exactly the five
    `assert*` functions this crate implements) *without* any file ever
    declaring `package kotlin.test`. This makes all three forms real:
    `import kotlin.test.*`, `import kotlin.test.assertEquals`
    (single-symbol import already worked once the package env existed —
    no parser change needed for that form), and a fully-qualified
    `kotlin.test.assertEquals(...)` call with no import at all (via the
    pre-existing `FieldAccess`/`MethodCall` → `resolve_qualified`
    fallback, unchanged). Importing a `kotlin.test` member not in
    `KOTLIN_TEST_SIGNATURES` (e.g. `assertFailsWith`, which needs
    `Flow::Throw`/Phase 5) is a plain `undefined name` error — the
    package is deliberately narrow, not an alias for all of `globals`.
  - **The harness (`kotlin_box_tests.rs`) went back to doing nothing but
    append the `println(box())` driver** — `adapt`/its import-stripping
    logic was deleted outright (renamed to `append_box_driver`), and
    `basics/concatenation.ksl`'s vendored `import kotlin.test.*` line now
    runs as genuinely unmodified Kotlin, real import and all.
  - See `docs/syntax.md`'s "Imports" section for the user-facing
    documentation of both wildcard import and the `kotlin.test` special
    case.
- **Corrected after a user report, right after the above shipped**: the
  first cut of the `kotlin.test` package env aliased values already
  registered on `globals` (`populate_globals` itself called
  `register(env, "assertEquals", ...)` etc.) — meaning the five assertions
  were *also* plain unqualified globals, callable as bare
  `assertEquals(...)` with **no** `import kotlin.test.*`/qualification at
  all. Flagged as wrong: real Kotlin's `kotlin.test` needs an explicit
  import (it isn't one of the packages Kotlin auto-imports), so ksl
  silently accepting the unqualified form both polluted the root scope
  with test-only names and didn't actually exercise the new import
  machinery the way a real vendored box test does. Fixed by moving the
  five `register(...)` calls out of `populate_globals` into a new
  `stdlib::populate_kotlin_test(env: &Env)`, called only on the synthetic
  `"kotlin.test"` package env, never on `globals` — and moving their
  entries out of `STDLIB_SIGNATURES` (which feeds `is_known`, the "is this
  bare name legitimately global" check `resolver.rs`'s `resolve_name`
  uses) into the new, separate `KOTLIN_TEST_SIGNATURES`. A bare
  `assertEquals(...)` with nothing importing it now correctly surfaces
  `undefined name 'assertEquals'`, both at runtime (`Env::get` finds
  nothing on `globals`) and statically (`is_known` no longer matches it,
  so `resolve_name` falls through to the same diagnostic any other typo
  gets, unless `imported_names`/`has_wildcard_import` says otherwise).
  `stdlib_call_types` (argument/return-type checking) still recognizes
  these five names unconditionally — safe to leave as-is, since it only
  ever runs *after* `walk_expr(callee)` has already resolved-or-rejected
  the bare name itself, so it can't make an unimported call resolve, only
  give a *properly imported* one real argument-type checking instead of
  falling back to `Unknown`. `docs/stdlib.md`'s "Assertions" section and
  `docs/syntax.md`'s `kotlin.test` paragraph corrected to match.
- **A second, separate user-reported bug, this time in `ksl-lsp` (not
  `ksl-core`) — also found and fixed the same session**: `ksl-lsp`'s
  cross-file `PackageIndex` (`crates/ksl-lsp/src/package_index.rs`) only
  ever indexed real `package`-declaring `.ksl` files, with no concept of
  `kotlin.test` at all — so completion/hover/goto-definition on
  `kotlin.test.*` all silently failed in an editor, independent of
  `ksl-core`'s own (correct) runtime resolution. A first attempt patched
  this by special-casing the literal strings `"kotlin"`/`"kotlin.test"`
  directly inside `completion.rs` — rejected by the user as non-systemic,
  and for good reason: it also surfaced a **real, general, pre-existing**
  bug unrelated to `kotlin.test` — `completion.rs`'s dotted-prefix branch
  unconditionally appended every `BUILTIN_METHOD_SIGNATURES` entry
  (`length`/`uppercase`/...) after *any* multi-segment prefix, even one
  that exactly matched a fully-known real package (confirmed by the user
  against arbitrary package names, not just `kotlin.test`). Fixed
  generically instead:
  - New `stdlib::BUILTIN_PACKAGES: &[(&str, stdlib::Signatures)]` — the
    single, data-only source of truth for "which packages ksl's stdlib
    itself provides" (currently one entry, `("kotlin.test",
    KOTLIN_TEST_SIGNATURES)"`); a future built-in package needs only a new
    entry here, nothing in `ksl-lsp`.
  - `package_index::build_package_index` now generically seeds every
    `BUILTIN_PACKAGES` entry into the same `PackageIndex` real files
    populate — iterating the table, never matching a name — via a new
    `builtin_symbol` helper synthesizing a `resolver::Symbol` with no real
    declaration site. `PackageMember::file` became `Option<Url>` (`None`
    for a built-in member — "no source location", the same thing a plain
    stdlib function already means for `goto_definition.rs`'s pre-existing
    fallback); `goto_definition.rs` updated to treat that as "nothing to
    jump to" rather than a broken URI.
  - `completion.rs`'s ad-hoc `import_use_completions` was deleted and
    replaced by one generic `package_path_completions`, shared by both
    `import_use_context` and the general expression-position dotted-prefix
    branch: real package members (`analysis.packages.members_of`, real or
    built-in, no distinction) plus a new `next_segment_completions`
    (generic hierarchical prefix matching — `kotlin` finds `kotlin.test`
    and offers `test`; an empty prefix offers every package's first
    segment, so "nothing typed after `import `" needs no special case
    either) plus `BUILTIN_METHOD_SIGNATURES` *only* when nothing
    package-shaped matched at all (or a concretely resolved local's own
    type — preserves the original single-segment-local behavior exactly).
    This is the general fix for the pre-existing bug above, for *every*
    package, not just `kotlin.test`.
  - Regression coverage deliberately includes a real, declared
    multi-segment user package (`a.b`) exercising the identical code path,
    proving the fix isn't `kotlin.test`-specific.
- **A third round of user feedback, same session, on `ksl-core::stdlib`'s
  own internal shape rather than `ksl-lsp`**: after the fix above, the
  user asked why new stdlib functions don't automatically show up
  wherever `kotlin.test` does, and why `is_known` only consulted one of
  several separately hand-maintained lists (`STDLIB_SIGNATURES` for the
  flat root scope, `KOTLIN_TEST_SIGNATURES` for `kotlin.test`,
  `BUILTIN_PACKAGES` wrapping only the latter — the root scope wasn't in
  it at all). Their proposed shape: one list of `Package { packageName,
  [Symbol { name, type, ... }] }`. Adopted, with the flat root scope
  itself modeled as `""`-named package inside the same list rather than a
  separate parallel const:
  - `stdlib.rs` gained `BuiltinSymbol { name, kind: SymbolKind, signature,
    doc }` (reusing `resolver::SymbolKind` rather than inventing a
    parallel one — every entry is `SymbolKind::Fun` today, since ksl has
    no stdlib `val`/`var` constants and no classes/interfaces yet, but
    needs no shape change whenever that stops being true) and
    `BuiltinPackage { name, symbols }`, and one `STDLIB_PACKAGES: &[BuiltinPackage]`
    replacing all three old consts and the `Signatures` type alias
    outright. `resolver::Symbol` itself isn't `const`-constructible
    (owned `String`/`Vec`/`Box` fields), which is why `BuiltinSymbol`
    stays plain `&'static str` fields rather than literally being a
    `Symbol` — converted to one on demand where a real `Symbol` is
    actually needed (`package_index::builtin_symbol`, which now takes the
    `kind` from the table instead of hardcoding `SymbolKind::Fun`).
  - `is_known` and a new `stdlib::root_symbols()` (for `ksl-lsp`'s two
    former direct `STDLIB_SIGNATURES` consumers) both read *only* the
    `""`-named package — preserves the previous round's namespace-
    pollution fix exactly (a bare `assertEquals(...)` must still be
    `undefined name`, not suddenly "known" just because the tables merged).
  - `package_index.rs`'s built-in seeding now iterates
    `STDLIB_PACKAGES.iter().filter(|p| !p.name.is_empty())` — the `""`
    entry is deliberately excluded there (it isn't a real navigable
    package to `import`), one filter instead of a second hand-maintained
    list.
  - `BUILTIN_METHOD_SIGNATURES` (the `.`-method table, `"x".uppercase()`
    etc.) was deliberately **not** folded in — it dispatches by the
    *receiver's type*, not by package/namespace, a genuinely different
    lookup axis from everything else here (already documented as a
    deliberate duplication in `docs/stdlib.md#methods`).
  - At the time, described as a pure internal refactor with no behavior
    change — **superseded almost immediately by a fourth round of
    feedback, same session**, which corrected that framing itself: see
    below.
- **A fourth round, on the `""` root-package sentinel the previous round
  introduced**: the user rejected it outright — real Kotlin has no such
  thing as an unqualified "root scope" distinct from packages; `println`
  is genuinely `kotlin.io.println`, and it's callable bare only because
  `kotlin.io` is one of several packages the compiler **auto-imports**
  into every file (`kotlin.io`, `kotlin.collections`, `kotlin.text`, and
  others ksl has no stdlib symbols in yet — `kotlin`, `kotlin.annotation`,
  `kotlin.comparisons`, `kotlin.sequences`); `kotlin.math` and
  `kotlin.test` are real packages too, just **not** on that auto-import
  list, so `kotlin.math.abs(...)`/`kotlin.io.println(...)` should both be
  valid fully-qualified calls either way. Adopted in full — this is *not*
  a pure internal refactor, it's a deliberate behavior change:
  - `BuiltinPackage` gained `default_imported: bool` and a `populate: fn(&Env)`
    field (a function *pointer* right on the table entry, so
    `Interpreter::new` can build every package's env and default-import
    the right ones with one generic loop, instead of a hardcoded call per
    package); the `""` sentinel is gone — `STDLIB_PACKAGES` now has five
    entries, all real names: `kotlin.io`/`kotlin.collections`/`kotlin.text`
    (`default_imported: true`) and `kotlin.math`/`kotlin.test`
    (`default_imported: false`). `populate_globals` (one function for
    everything) was split into `populate_kotlin_io`/`_collections`/`_text`/
    `_math` (plus the already-existing `populate_kotlin_test`) — same
    `register(...)` calls, just grouped by package, moved verbatim.
  - `Interpreter::new` now loops over `STDLIB_PACKAGES`: builds each
    package's own child `Env` via its `populate` fn (so
    `kotlin.math.abs(...)`/`kotlin.io.println(...)`-style full
    qualification works for *every* package, not just `kotlin.test` as
    before), and for `default_imported` ones only, additionally mirrors
    the same `Value`s onto `globals` (`Env::own_bindings()`, already added
    earlier this session) — one `Rc`-cheap clone per name, not a second
    copy of any closure.
  - `is_known`/former `root_symbols()` (renamed `default_imported_symbols()`)
    now union every `default_imported` package instead of reading one
    special-cased entry.
  - `package_index.rs`'s built-in seeding **lost** its `.filter(|p|
    !p.name.is_empty())` entirely — every package is real now, so every
    one is indexed identically, no exclusion needed. Net simplification,
    not added complexity.
  - **Real, deliberate behavior change, confirmed with the user in
    Plan Mode before implementing**: `kotlin.math` not being
    auto-imported means `abs`/`sqrt`/`max`/`min` stop working unqualified
    — exactly like real Kotlin. `crates/ksl-core/tests/examples/closures.ksl`
    (the one place in this repo actually calling them bare) got an `import
    kotlin.math.*` added; `docs/stdlib.md`'s Math section documents the
    same requirement.
  - Regression tests added generically over the whole table (loop over
    `STDLIB_PACKAGES`, not one hand-written test per package): every
    declared symbol is actually registered by its own package's
    `populate`; `default_imported_symbols()`/`is_known` matches
    `default_imported` exactly; a real `Interpreter`'s `globals` mirrors
    exactly the default-imported packages, nothing else.
- **A fifth round, on `kotlin.math`'s own signatures specifically**: the
  user (having started hand-editing `stdlib.rs`'s signature strings
  directly, adding `fun`/explicit return types elsewhere) pointed out
  `"fun abs(x: Int | Double): Int | Double"` uses a union-type syntax that
  doesn't exist in real Kotlin — real Kotlin solves exactly this with
  **overloading** (several same-named functions, each with one concrete
  signature). Fixed by splitting each of `abs`/`sqrt`/`max`/`min` into two
  separate `BuiltinSymbol` entries (one `Int`, one `Double`) sharing one
  `name` — needed no shape change to `BuiltinSymbol`/`BuiltinPackage`
  at all: `symbols: &'static [BuiltinSymbol]` never assumed unique names
  to begin with, and `populate_kotlin_math`'s actual runtime closures
  already dispatch on the real `Value` variant they're given (that dynamic
  dispatch *is* the overload resolution, at the one layer that actually
  executes code) — only the display metadata was ever collapsed into one
  fake union-typed entry. Confirmed every consumer tolerates a repeated
  `name` within one package's `symbols` (audited, not just assumed):
  `is_known`/`default_imported_symbols` (`.any()`/iteration, unaffected by
  duplicates), `package_index.rs`'s built-in seeding (both overloads get
  indexed as separate `PackageMember`s — correct for completion, which
  should show both; `find()`'s first-match-only behavior means
  hover/goto-def on a `kotlin.math` qualified call shows only the first
  overload — an accepted, pre-existing-style imprecision, not a new
  regression), `resolver.rs`/`stdlib_call_types` (doesn't read signature
  strings at all — already has separate, non-string-based numeric-
  polymorphic handling for these four names, untouched). Two of the eight
  resulting overloads intentionally go beyond what real Kotlin itself
  overloads — `sqrt(x: Int): Double` (real `kotlin.math.sqrt` has no `Int`
  overload; needs `.toDouble()` first) and the still-accepted *mixed*
  `max(anInt, aDouble)`/`min(...)` call (widens to `Double` at runtime; no
  such overload exists in real Kotlin either) — both pre-existing ksl
  conveniences, not new behavior, now just honestly documented instead of
  hidden behind `|` — see `docs/stdlib.md`'s Math section.
  - **Immediately followed by a related roadmap-only request** (no
    implementation asked for): audit and record what's still missing for
    real Kotlin numeric-type fidelity — see Phase 2.3 below, added for
    exactly this.
- **A sixth round, on the remaining duplication between `BuiltinSymbol`'s
  own free-text `signature: &'static str` (hover/completion display) and
  `stdlib_call_types`'s separate, hand-written `match name { ... }` (call-
  site argument/return-type checking)**: the user pointed out these were
  two independently hand-maintained descriptions of the same fact — a
  signature string could drift from its own type-checking entry with
  nothing to catch it. First proposal (keep both, framed as "different
  axes") was rejected in favor of full unification: derive *both* the
  display text and the checked types from one canonical structure.
  Landed as `BuiltinParam { name, ty: Type, nullable: bool, default:
  Option<&'static str> }` plus `BuiltinSymbol { params: &'static
  [BuiltinParam], variadic_param: Option<&'static str>, return_type: Type,
  ... }` — `signature` is gone entirely; `format_signature(sym)` now
  generates the exact same display text on demand (verified byte-for-byte
  against every one of the 20 previously hand-written strings, across all
  15 distinct names including the `abs`/`sqrt`/`max`/`min` overload pairs,
  in a dedicated test), and `stdlib_call_types` derives from the same
  `params`/`return_type` instead of its own separate match. One real,
  empirically-confirmed constraint shaped the design: `Box::new(...)` is
  **not** a `const fn` in stable Rust (`rustc 1.98.0` — confirmed by
  compiling a minimal repro: `cannot call non-const associated function
  'Box::<T>::new' in constants`), so a `Type::Nullable(Box<Type>)` can't
  appear directly inside `pub const STDLIB_PACKAGES`; `BuiltinParam` works
  around this by storing the bare, non-nullable `ty` plus a `nullable:
  bool` flag, with `real_type()` building the true `Type::Nullable(Box::
  new(ty))` on demand in ordinary (non-`const`) function calls, both in
  `format_signature` and in `stdlib_call_types`. `stdlib_call_types`'s own
  overload-collapsing heuristic (several `BuiltinSymbol`s sharing a name —
  safe to represent as one permissive signature only when every overload
  agrees on arity *and* return type, e.g. `sqrt`'s two `Double`-returning
  overloads; otherwise, e.g. `abs`/`max`/`min` with diverging `Int`/
  `Double` return types, `None`) reproduces today's exact name-by-name
  behavior with no hardcoded names anywhere — `resolver.rs`'s own,
  separate `abs`/`max`/`min` special case (which picks a type from the
  actual call-site argument, real overload resolution rather than a static
  table lookup) is untouched, a deliberately different and more capable
  mechanism for exactly those three names. `ksl-lsp`'s three direct
  `sym.signature` readers (`package_index::builtin_symbol`, `completion.rs`,
  `hover.rs`) all switched to `stdlib::format_signature(sym)` — no other
  changes needed there, since none of them ever matched on stdlib names
  specifically. One accepted, small behavior expansion falls out of giving
  `assertEquals`/`assertTrue`/`assertFalse`/`assertNull`/`assertNotNull`'s
  `message` parameter a real, checkable `params` entry (needed so
  `format_signature` can print it at all) instead of it being invisible to
  `stdlib_call_types` as before: `assertEquals(1, 2, 3)` is now a genuine
  static type error (`argument 3 has type Int, expected String?`) — it
  previously type-checked fine and only failed, if at all, when the
  message was later formatted at runtime. Confirmed via `cargo test
  --workspace` (all green) and manual `ksl-cli` runs that: `abs`/`max`/
  `min`/`sqrt` still execute correctly at runtime; `length(5)` and
  `assertTrue("x")` are still rejected exactly as before; `assertEquals(1,
  2, 3)` is the one newly-caught case, as expected.

---

## Phase 0.6 — Real overload resolution, function overloading, resolver/interpreter unification, and stack traces [DONE]

Not part of the original 9-feature backlog — grew out of a user objection
to `resolver.rs`'s `is_numeric_polymorphic` special case (a hardcoded
`"abs" | "max" | "min"` match picking a call's return type from its first
argument's numeric type): a patch, not real overload resolution, unable to
handle non-numeric types, wrong arity, or any future overload shaped
differently. The user's ask, generalized: overload resolution must work
identically whether the function is a stdlib builtin or user-declared —
which meant first *building* user-defined function overloading as a real
language feature (it didn't exist at all before this phase: a second `fun
foo` with the same name was unconditionally "already declared"). A
follow-up question about avoiding redundant per-call dispatch cost then
surfaced a deeper architectural fact: **the resolver and interpreter were
two fully independent passes**, each re-parsing the same source separately
(`run_source` never called `resolver::resolve` at all; the old
`static_check` did, but never touched `Interpreter`). Real once-per-call-
site resolution needed those two passes bridged for real — parse once,
resolve once, and hand execution the resolver's own decisions — which is
also exactly the infrastructure (each function value knowing its own home
file) needed for real stack traces, so both landed together.

**1. `types::resolve_overload`** — one generic, source-agnostic overload-
resolution core (`OverloadResolution::{Unique, Ambiguous, ArityMismatch,
NoMatch}`), taking only `candidate_params: &[&[Type]]` and `arg_types:
&[Type]`. Filters by arity first (always a hard error if none match,
`Unknown` or not), then prefers an *exact* type match at every position
over plain assignability (so a concrete `Int` overload wins over a same-
arity `Any` overload), and reports `Ambiguous` (not `Unknown`-flattened)
when more than one candidate remains possible — letting each caller decide
whether ambiguous-but-agreeing candidates are still safe to report a
return type for. This one function is reused, unmodified, by every layer
below.

**2. Stdlib overload resolution** (resolver-only — stdlib native closures
already dispatched correctly at runtime, e.g. `abs`'s closure pattern-
matching on the real `Value`; only the *static* checker was wrong).
`stdlib::is_numeric_polymorphic` is deleted; `stdlib_overloads`/
`stdlib_qualified_overloads` (new) list every `BuiltinSymbol` sharing a
name, and `resolver.rs`'s `Call`/`MethodCall` arms run `resolve_overload`
over them whenever there's more than one — a genuinely new capability for
the qualified form too (`kotlin.math.abs(-5)` gets real resolution for the
first time; previously only the bare, imported form did). Concretely fixed
gaps the old special case had: `abs("hello")` is now a real type error
(`no overload of 'abs' matches argument types (String)`) instead of
silently `Unknown`/no error; `abs(1, 2)` (wrong arity) is now flagged too.
**One accepted behavior change**: ksl's own `max(anInt, aDouble)`/`min(...)`
mixed-type convenience (documented as a ksl-only extra beyond real Kotlin)
no longer type-checks — a mixed call needs an explicit conversion now,
exactly like real Kotlin; `docs/stdlib.md`'s Math section and
`tests/examples/closures.ksl` updated accordingly. The runtime's own
`numeric_binary` widening fallback is left in place as defense-in-depth for
a genuinely `Unknown`-typed dynamic call, documented as no longer a
user-facing feature.

**3. File identity + call stacks** (foundation for 4/5). `FunctionValue`
gains `file: Arc<Path>` (`Arc`, not `Rc` — `ksl-lsp`'s runtime-diagnostics
pass runs `run_source` on a background thread and needs the error type to
stay `Send`) and `decl_span: Span` (the `fun`/lambda declaration's own
span — must exactly match `resolver.rs`'s own `Symbol.def_span`, i.e. the
*name's* span, not the whole statement's; got this wrong once during
implementation — using the statement's span — and it silently broke fast-
path matching in 5 below until caught by a dedicated counter-based test).
`Interpreter` gains `current_file` (stamped onto every function/lambda
created while it's the active file — swapped, save-and-restore, around
`call_function` so a nested declaration inside a cross-file-called
function gets *that* function's file, not the caller's) and `call_stack:
Vec<StackFrame>` (pushed on entry, popped only on success, so an error in
progress leaves the whole chain in place until `run_in_env` reads it off
into the error itself and clears it). `RuntimeError` gains `stack:
Vec<StackFrame>`; `InterpError::Runtime`'s `Display` prints one `    at
<signature> (<file>:<line>:<col>)` line per frame, innermost first. Each
frame's printed position is that frame's *own* file's coordinates (the
span of whatever call *it* made further in) — not the caller's coordinates
paired with the callee's file, a real bug caught and fixed during
implementation by reasoning through a cross-file example by hand.

**4. Real user-defined function overloading.** `resolver.rs`:
`ScopeData::names` becomes `HashMap<String, Vec<usize>>` (was `HashMap<String,
usize>`); `define()` now only flags a same-name `Fun` redeclaration as a
conflict when either something non-`Fun` shares the name, or an existing
`Fun` has the *exact* same parameter types (worded `"conflicting overload
of '{name}'"`, distinct from plain `"already declared"`) — otherwise it's
legal, appended to the group. New `resolve_symbols` (plural) returns the
whole group for `Call` to run `resolve_overload` over; `resolve_symbol`
(singular) keeps returning just the last-defined entry for every non-call
use (hover, `Assign`, ...), a documented simplification. Runtime:
`environment.rs`/`value.rs`/`interpreter.rs` gain `Value::Overload(Rc<Vec<Rc<FunctionValue>>>)`
— a second `fun` sharing a name in the same `Env` groups with the first
(`Env::get_own`, parent-blind on purpose) instead of overwriting it;
`call_overload` (the runtime fallback — see 5) runs the same
`resolve_overload` core against real argument types
(`Type::from_value`)/real parameter types (new shared
`types::function_signature_types`, also now used by `Type::from_value`'s
own `Function` arm, removing a small pre-existing duplication).

**5. The resolver → interpreter bridge** — what makes 4's runtime search
the *fallback*, not the default. `ResolveResult` gains `resolved_calls:
HashMap<Span, Span>` (call-site span → the one local-overload candidate's
declaration span), populated whenever `Call` resolves a local overload
group to exactly one candidate. `importer.rs`'s `run_top_level`/
`load_import` now parse **and** resolve each file exactly once (new
`parse_and_resolve`, replacing the old separate `static_check` — which did
its own independent re-parse+re-resolve — with one shared analysis feeding
both "reject before running" and execution); a `Program` that fails
resolution never executes a single statement (`InterpError::TypeErrors`,
new). `FunctionValue` additionally carries `resolved_calls: Rc<HashMap<Span,
Span>>`, captured from `Interpreter::current_resolved` at the same moment
`file`/`decl_span` are (and swapped the same way, around `call_function`) —
this is what lets a call site *inside* a cross-file-called function
resolve against *its own* file's decisions, not the caller's (`Span` has
no file field, so two files' call-site spans could otherwise collide in
one shared table). `eval_expr`'s `Call` handling checks
`current_resolved.get(&expr.span)` first when the callee is a
`Value::Overload`, and calls the named candidate directly, skipping
`call_overload`'s search entirely — verified by a test-only call counter
that a fully statically-typed overloaded call never invokes the runtime
search at all, while a call bypassing resolution (or one the resolver left
ambiguous) still falls back to it correctly.

**Fallout fixed along the way**: `ksl-cli`'s separate `static_check` call
is removed (`run_source` now does this itself); several `interpreter.rs`
unit tests that deliberately exercised the *runtime* type-checking layer
in isolation (wrongly-typed call arguments, incompatible `var`
reassignment, unsafe nullable method/field access, untyped lambda
parameters) now go through a new `run_bypassing_static_check` test helper
(parses and runs directly against a bare `Interpreter`, skipping
`resolver::resolve` entirely) since `run_source` itself now rejects those
same scripts before ever reaching the runtime checks — preserving direct
test coverage of that layer as defense-in-depth, independent of whether
the resolver also happens to catch the same mistake. One vendored Kotlin
box test (`elvis/nullNullOk.ksl`, `fun box() = null ?: null ?: "OK"`) hit
a real, pre-existing resolver gap — inferring a hint-less `fun`'s return
type through a chained elvis whose operands are `null` literals — that had
simply never been exercised before `run_source` started statically
checking; demoted to `kotlin-box-pending/` per the harness's own
documented procedure (`kotlin-box/README.md`) rather than hand-fixed here,
since it's an unrelated, separate resolver gap.

**Docs:** `docs/syntax.md` gains a "Functions → Overloading" subsection.
`docs/stdlib.md`'s error-format note now mentions the stack trace.

---

## Phase 0.7 — Precise cross-file `import`/`@file:Import` resolution [DONE]

Triggered by a user question ("why does a typo inside an uncalled function
only error once the function is called?"), which surfaced something much
bigger: `resolver.rs`'s undefined-name check was **blanket-suppressed for
the whole file** the moment it had *any* wildcard `import a.b.*`
(`has_wildcard_import`) or *any* `@file:Import(...)` (`has_file_imports`) —
a precise single-name `import a.b.c` was also just blindly trusted
(`imported_names`), never checked against what `a.b` actually exports. An
initial narrower fix (make only *stdlib* wildcard imports precise, keep
blanket leniency for user/file-backed packages) was explicitly rejected:
the requirement is that **every** import — stdlib or user, wildcard or
named, `import` or bare `@file:Import` — resolves for real, with an exact
recorded reference to the symbol it names, and zero special-case leniency
anywhere; a qualified call into an unresolved package (`util.math.square(2)`)
is now a hard diagnostic too, not silent `Type::Unknown`.

**The mechanism already existed, just in the wrong place.** `ksl-lsp`'s
`package_index.rs` already built exactly this — a `PackageIndex` recursively
walking `@file:Import` (never `import`/`ImportUse`, which carries no file
path — see `ast::ImportUse`'s own doc comment), seeding every
`stdlib::STDLIB_PACKAGES` entry unconditionally and reading each
transitively-reachable file's `resolver::resolve(...).table.top_level_symbols()`
— but purely for hover/completion/goto-definition, entirely disconnected
from `resolver.rs`'s own diagnostics. Moved verbatim to
`crates/ksl-core/src/package_index.rs` (`PackageMember::file` becomes
`Option<PathBuf>` instead of `tower_lsp`'s `Option<Url>`, since `ksl-core`
can't depend on it — `ksl-lsp`'s own copy is now a thin re-export plus a
`path_to_url` conversion at its two call sites in `goto_definition.rs`),
since `resolver::resolve_with_imports` (new) needs `PackageIndex` as an
input parameter and `ksl-lsp` depends on `ksl-core`, never the reverse.
`PackageIndex::new()` seeds the stdlib unconditionally (always fully known,
regardless of filesystem context) — `resolve(program)` stays a thin
zero-argument wrapper over `resolve_with_imports(program, &PackageIndex::new())`,
so every one of the resolver's ~100 pre-existing single-file unit tests
keeps compiling unchanged (a handful whose *assertions* encoded the old
leniency needed real fixes — see below).

**`resolver.rs`**: `imported_names`/`has_file_imports`/`has_wildcard_import`
removed entirely, replaced by a new `CrossFileImports` (built once up
front from `program.imports_use`, `program.imports`'s flat/package-less
namespace, and the passed-in `PackageIndex`) — every `import` is checked
for real: an unresolved package or member is a diagnostic at the import's
own span (`"unknown package '...'"` / `"undefined name '...'"`, matching
`Interpreter::resolve_qualified`'s existing runtime wording exactly), a
resolved one is recorded into a new `ResolveResult::cross_file_refs:
HashMap<Span, CrossFileRef>` (the cross-file counterpart of
`resolved_calls`, since a reference needs a source *file*, not just a
same-file span). `resolve_name`'s fallback chain is now: local scope →
`stdlib::is_known` → `CrossFileImports` → real diagnostic, no exceptions.
`Call`'s existing three-tier overload dance (local → stdlib) gained a
fourth tier, consulted only when neither of the first two found anything —
a cross-file name with more than one overload goes through the same
`types::resolve_overload` core via a new `resolve_cross_file_overload`
(mirroring `resolve_user_overload`/`resolve_stdlib_overload`); a single
candidate feeds `check_call_signature` exactly like a local `fun`, so a
wildcard-imported function now gets real argument-count/type checking it
never had before. `MethodCall`'s qualified-path handling
(`kotlin.test.assertFalse(...)`) gained the same treatment for a *user*
package (`Resolver` now holds `packages: &'a PackageIndex` directly,
consulted whenever the path isn't a stdlib member) — full strictness means
an unresolved qualified call is now a diagnostic too, not the old lenient
`Type::Unknown` fallback.

**`importer.rs`**: gained a `packages: PackageIndex` field, seeded via
`PackageIndex::new()` and extended (`add_module_to_index`) right after each
file is resolved — one shared, ever-growing index across the whole run,
not a leniency but the *correct* model: a package-less `@file:Import`ed
file's names are genuinely global at runtime (`run_program`'s single shared
`globals` env, `Interpreter::packages`'s single shared per-package env
regardless of which file provided a name), so one shared index mirrors
that exactly. `run_top_level`/`load_import`'s order inverted: a file's own
`@file:Import`s are now fully loaded, resolved, and run *before* that file
itself is resolved (previously resolution happened first, then imports) —
otherwise `self.packages` wouldn't yet reflect what those imports export
by the time the importing file needs it. Since each canonical path is
still only ever parsed+resolved once (the pre-existing `loaded` dedup, now
also gating `self.packages`'s extension), a file imported from several
places is genuinely resolved exactly once and its exports reused, not
reprocessed — no separate `ksl-lsp`-style re-walk of the import graph is
needed at execution time at all.

**Fallout fixed**: three resolver.rs tests whose assertions were the exact
leniency being removed (`import util.math.square` with no real backing
package, `@file:Import` with no real backing file) were rewritten against
a new `package_index_with_util_math()` test helper (a `PackageIndex` with
one hand-inserted fake `util.math.square` member, no temp files needed) or
flipped to expect the new diagnostic. New tests cover: a wildcard/named
import of a real package (temp-file-backed, `importer.rs`'s existing
`TempDir` pattern) with a genuine unrelated typo still flagged, a
type-mismatched argument to a wildcard-imported function now rejected
*before* the script runs, an unknown package/member at both the `import`
site and a qualified call site, and `cross_file_refs` actually getting
populated. The full vendored Kotlin box-test corpus
(`cargo test -p ksl-core --test kotlin_box_tests`) and the whole `ksl-lsp`
suite (which now threads a real `PackageIndex` through `diagnostics.rs`'s
`static_diagnostics`, doing real filesystem I/O on every `didChange` — a
deliberate trade-off so live diagnostics never lag behind what `ksl-cli`
would actually reject) both pass unchanged.

**Still deliberately deferred, not part of this pass**: folding
`diagnostics` (undefined-name, duplicate-declaration) into
`importer.rs::parse_and_resolve`'s fatal check alongside `type_errors` —
this is what originally motivated the investigation, but was meaningless
to do before `import`/`@file:Import` resolution was actually precise (it
would have rejected almost every multi-file script on a false positive).
Now that it's precise, this is a small, focused follow-up.

---

## Phase 0.8 — `do { ... } while (cond)` [DONE]

Surfaced by Phase 0.5's box-test audit as a real, previously-untracked gap
(`kotlin-box-pending/controlStructures/{doWhile,factorialTest}.ksl`),
deliberately deferred there since it's a whole new loop construct, not a
smaller fix. Independent of every other phase — no prerequisites beyond
Phase 0, same dependency-graph position `++`/`--` had.

**Shipped**: new `TokenKind::Do`/keyword, `StmtKind::DoWhile { body, cond
}`. Parser: `parse_do_while` mirrors `parse_while` (braceless
single-statement body via the existing `parse_branch`, `loop_depth`
bumped the same way so `break`/`continue` are legal inside), with an
unconditional `skip_newlines()` between the body and the mandatory
`while (cond)` — unlike `if`/`else`'s own lookahead
(`skip_separators_if_else_follows`), no lookahead-then-maybe-rollback is
needed here since `while` isn't optional the way `else` is, so there's no
"next unrelated statement" a newline-skip could wrongly swallow instead.

**The one real semantic subtlety, not just new syntax**: matching real
Kotlin/Java, `cond` is resolved (`resolver.rs`) and evaluated
(`interpreter.rs`) in the loop body's *own* scope, not the enclosing
one — `do { val x = f() } while (x > 0)` sees `x`, since the body always
runs at least once before `cond` is ever checked (unlike `while`, where
`cond` gates *whether* the body runs at all, so it's evaluated in the
outer scope, before the body's own scope even opens). Interpreter detail:
`continue` inside a `do`-body must still evaluate `cond` (against that
same iteration's env) before deciding whether to loop again — it only
skips the rest of the body, not the condition check; `break` skips the
condition check entirely and exits.

**Box tests**: `controlStructures/factorialTest.ksl` promoted to
`kotlin-box/` (real `do {...} while(...)` loop, verified against real
Kotlin's own `box()` convention). `controlStructures/doWhile.ksl` stays
pending — it also uses a bare `z.length` property access, a separate,
still-open gap (Phase 6) unrelated to `do`/`while` itself.

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
yet) is a clear runtime error until Phase 6. This table is a deliberate,
temporary stopgap, not a permanent design — Phase 6 retires it entirely
once real class-method dispatch exists (see that phase's "Built-in
primitive types as classes" subsection), replacing it with primitives
dispatching through the same mechanism a real `class`'s methods use.

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

**`Symbol` (`resolver.rs`) gains**: `ty: Type` (inferred or declared — for
`Fun`, always `Type::Function`, not its return type), `return_type: Type`
and `param_types: Vec<Type>` (`Fun` only).

**Enforcement, both ways, per explicit product decision** (not a warning —
a hard rejection, as close to Kotlin's compile-time behavior as a
no-compile-step interpreter can get):
- **No dynamic-by-default carve-out for a hint-less `val`/`var`** — this
  was tried during implementation and reverted: `var x = 5` infers `Int`
  from the initializer exactly as `var x: Int = 5` does, and both are
  enforced on every later reassignment identically, matching Kotlin. There
  is no "explicit annotation only" gate anywhere in the final design — a
  type is a type, whether written down or inferred.
- **An annotation that doesn't resolve to a known type is itself an
  error** — `resolve_type_hint` returning `Unknown` for an *absent* hint
  (`None`) is fine (genuinely dynamic), but returning `Unknown` for an
  *explicitly given* one (a typo, or a name that would be valid once
  classes/generics exist) is not: it's reported as `unknown type '<name>'`
  at the hint's own span. This needed threading a `type_hint_span`/
  `return_type_span: Option<Span>` alongside the existing raw hint strings
  on `Param`/`ValDecl`/`FunDecl` (parser captures both together, mirroring
  the existing `name`/`name_span` pattern), so the diagnostic can point at
  the annotation itself rather than the whole declaration. Resolve each
  hint exactly once per declaration site to avoid double-reporting the
  same typo (a real bug hit during implementation: computing a function's
  `param_types` *and* separately walking its params both naively called
  the resolving/diagnosing function on the same hint).
- *Runtime* (`interpreter.rs`/`environment.rs`, always on): `call_function`
  checks each bound argument against `Param.type_hint` and the result
  against a new `FunDecl.return_type`/`FunctionValue.return_type` field
  (the return-type annotation used to be parsed and thrown away —
  `parse_fun_decl` now keeps it); `Env::Binding` gains a `declared_type`
  field — set from the hint if there is one, else `Type::from_value` of
  the initializer itself, never `Unknown` for a real local — checked in
  `Env::assign` (`AssignError::TypeMismatch`, mapped to a `RuntimeError` in
  `interpreter.rs`'s `Assign` handler); `check_type` itself now rejects an
  unresolvable hint instead of treating it as always-compatible. Note
  there's no "Unknown" escape hatch at this layer the way there is
  statically — every runtime value is concrete — so a `var` initialized
  from a statically-`Unknown` expression (e.g. a package-qualified call)
  still ends up type-locked once actually run, even though the static
  checker couldn't have predicted that; documented as an accepted,
  inherent gap between the two layers' precision, not a bug.
- *Static* (`resolver.rs`): `ResolveResult` gains a `type_errors:
  Vec<RuntimeError>` field, kept *separate* from the pre-existing
  `diagnostics` (undefined-name, duplicate-declaration) — checked at `val`/
  `var` initialization, `var` reassignment, positional call arguments
  (named arguments are out of scope for v1), `return` vs. a function's
  declared return type, and any unresolvable explicit annotation.
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
yet; and both a static and a runtime test for a typo'd annotation being
rejected as unknown); new `tests/examples/type_checking.ksl` + `.out` (a
passing-only script — the harness treats any `Err` as failure —
demonstrating correctly typed `val`/`var`/`fun` end-to-end, including a
hint-less `var`'s inferred type still being enforced on a compatible
reassignment; negative cases — a mismatch, an unknown annotation — belong
in the unit tests above, not this fixture, since it can only ever
demonstrate scripts that succeed); `completion.rs`/`hover.rs`/
`diagnostics.rs` tests for the type-aware filtering.

### Phase 2.1/2.2 — Strict typing, function types, lambda inference (addendum)

The section above describes Phase 2 as it originally shipped, built around
one central rule: **`Type::Unknown` is always compatible, on either side**.
Hands-on use immediately after showed that rule was too lenient for what a
Kotlin-like language's users actually expect (a hint-less `var` not
enforced on reassignment, a typo'd annotation silently accepted, a named
`fun` parameter allowed with no type at all), so the design was
deliberately reversed: **a value without an explicit or inferable type is
now always an error**, not a silently-accepted `Unknown` — matching
Kotlin's own strictness. Everything below supersedes the equivalent claims
above; the rest of Phase 2's description (module layout, `Symbol` fields,
LSP wiring) still holds.

- **`Type::Function` is no longer a bare marker** — it's
  `Function(Vec<Type>, Box<Type>)`, a real structural signature
  (`(Int, String) -> Result`). `is_assignable`'s new `Function`/`Function`
  arm compares element-wise (same arity, each parameter and the return
  individually `is_assignable`) rather than by variant alone — except the
  exact sentinel shape `Function(vec![], Unknown)`, reserved for a
  `Value::Native`'s unknowable-at-runtime signature (see
  `Type::from_value`), which is always compatible with any function type
  since there's nothing real to check.
- **New AST node** `TypeAnnotation { Named(String, Span), Function(Vec<TypeAnnotation>,
  Box<TypeAnnotation>, Span) }` (`ast.rs`) replaces the old bare
  `Option<String>` + `Option<Span>` pair on `Param`/`ValDecl`/`FunDecl`'s
  return type. `parser.rs`'s `parse_type_annotation` implements
  `TypeAnnotation ::= Ident | '(' (TypeAnnotation (',' TypeAnnotation)*)? ')' '->' TypeAnnotation`,
  called only from type-annotation position (right after a `:`) so a
  leading `(` there is unambiguous. `types::resolve_type_hint` (a plain
  name) is joined by `types::resolve_type_annotation` (the full tree,
  recursing into a function type's parameter/return annotations).
- **"Type-committing sites" now error on `Unknown` instead of accepting
  it**: a hint-less `val`/`var` whose initializer's type can't be inferred
  either; a named `fun`'s parameters (**always** mandatory now, no
  exception — `walk_params` in `resolver.rs`); a lambda parameter (or the
  implicit `it`) with no explicit annotation *and* no expected function
  type available from context (see below); an unannotated `fun`'s or
  lambda's return type when nothing in the body could be reconciled to one
  type (`ReturnTypeContext::Collect` + `reconcile_types`, shared machinery
  for both).
- **Immediate errors** (not deferred, regardless of context) were added
  for: a binary/unary operator on two concretely-known incompatible types;
  a method call on a concretely-known receiver type with no matching entry
  in `stdlib::BUILTIN_METHOD_SIGNATURES`; a `.field` access on a real local
  value; mismatched `if`/`when` branch types when every branch is a
  primitive (`is_primitive` in `resolver.rs` — `Class`/`Generic`/`Unknown`
  branches still join leniently, deferred to Phases 6/7's common-supertype
  work); a named argument whose type doesn't match, or that names a
  parameter that doesn't exist (`Symbol` gained `param_names: Vec<String>`
  for this, matched by the same two-pass claim-named-then-positional
  algorithm `call_function` already used at runtime).
  **Correction (found via a user report after shipping):** the
  branch-mismatch check above originally fired even with no `else`/
  `else ->` at all, by comparing the present branch's type against the
  implicit-Unit fallback substituted for the missing one — so `if (true)
  10` used as a statement (value discarded) or assigned to a hint-less
  `val` wrongly errored `branches have incompatible types: Int and Unit`,
  even though this is valid Kotlin (an else-less `if`/`when`'s implicit
  Unit is only what the *runtime* falls back to when no branch is taken,
  never a second real value the present branch must agree with — see
  `eval_expr`/`eval_when`'s own matching rule, unchanged). Fixed by gating
  the error on a real, explicit `else` being present (`else_branch.is_some()`
  for `If`, `has_else` for `When`) — an absent one still joins to
  `Type::Unknown` on any mismatch, same as before, just without the false
  positive.
- **Lambda type inference** (`resolver.rs`'s `walk_lambda`/
  `walk_lambda_params`): a lambda's own type is always a real
  `Type::Function`, never left unresolved. Parameter types come from (in
  order) an explicit annotation, else the "expected type" propagated in
  from the lambda's use site — a `val`/`var`'s own function-type
  annotation, or the matching parameter's declared type at a call site —
  else an error. This needed a new `walk_expr_expected` (an `Option<&Type>`-
  carrying sibling of `walk_expr`, used only at the exact expression
  position a lambda might sit in — a `val`'s initializer, a call argument —
  never threaded into recursive subexpression calls) so the `Call` arm
  could resolve the callee's signature *before* walking its arguments,
  instead of after. Return type inference reuses the same
  `ReturnTypeContext::Collect`/`reconcile_types`/`always_returns` machinery
  Phase 2.1 built for an unannotated `fun`'s return type.
- **Breaking, by explicit product decision**: every named `fun` parameter
  needs a real type now (no bare `fun apply(f, x: Int)`); a lambda needs
  either an explicit parameter annotation or a real expected-type context
  (no more bare `val double = { x -> x * 2 }` with nothing to infer `x`
  from) — `tests/examples/closures.ksl` was rewritten to
  `fun apply(f: (Int) -> Int, x: Int): Int = f(x)` /
  `val double: (Int) -> Int = { x -> x * 2 }` accordingly.
- **Since fixed** (was: "still pending, not part of this addendum"):
  cross-file type inference through `@file:Import`/`package` — a
  package-qualified call (`util.math.square(5)`) now resolves against a
  real, filesystem-backed `PackageIndex` and gets real argument checking,
  same as a local call — see Phase 0.7.

---

## Phase 2.3 — Numeric type width fix + missing primitive types (audit only, not implemented)

Extends Phase 2 directly (the type system this touches), independent of
everything after it — no later phase currently depends on ksl's numeric
types being exactly `Int`/`Double`, so this can land any time once picked
up. Added as a roadmap entry only, at the user's explicit request, right
after that same session's `kotlin.math` overload-signature fix (see Phase
0.5's own "fifth round" note) surfaced the underlying gap — **no
implementation work done here**, this section is the audit/scoping pass
alone, the same way Phase 10 (coroutines) is deliberately left as "needs a
design spike before implementation" rather than a concrete plan.

**Finding, confirmed by grep across `value.rs`/`types.rs`/`lexer.rs`/
`token.rs`**: ksl's `Value::Int`/`types::Type::Int` is backed by `i64` —
**64-bit**, already documented as such in `docs/syntax.md`'s Literals
table (`Int (i64)`) — a known, existing, *documented* divergence, not a
hidden bug. But real Kotlin's `Int` is 32-bit; what ksl calls `Int` today
actually behaves like real Kotlin's `Long`, and there's no real `Long` at
all. Real Kotlin's full primitive set, for reference: `Byte` (8-bit),
`Short` (16-bit), `Int` (32-bit), `Long` (64-bit), `Float` (32-bit),
`Double` (64-bit), `Boolean`, `Char` (16-bit, a UTF-16 code unit — a real
distinct type in Kotlin, not just a length-1 `String`). Cross-checked
against ksl's actual `Value`/`Type` enums and `lexer.rs`'s `lex_number` /
identifier-lexing:

| Kotlin primitive | ksl today |
|---|---|
| `Boolean` | ✅ `Value::Bool` — correct, no width concept applies |
| `Double` (64-bit) | ✅ `Value::Double(f64)` — correct width |
| `Int` (32-bit) | ⚠️ exists as `Value::Int(i64)` — **wrong width**, behaves like `Long` |
| `Long` (64-bit) | ❌ missing entirely — no variant, no `10L` literal suffix in `lex_number` |
| `Float` (32-bit) | ❌ missing entirely — no variant, no `3.14f`/`3.14F` literal suffix |
| `Byte` (8-bit) | ❌ missing entirely |
| `Short` (16-bit) | ❌ missing entirely |
| `Char` | ❌ missing entirely — no `'x'` single-quoted literal token anywhere in `lexer.rs`/`token.rs` |

**Open questions for whoever picks this up** (deliberately not decided
here — this is scoping, not design):
- Whether to actually **resize** `Int` to `i32` — a real, breaking
  behavior change: `docs/syntax.md`'s Operators section already promises
  "`Int` arithmetic wraps on overflow, matching Kotlin's `Int` semantics"
  — that wrap is currently `i64` wraparound; becoming `i32` wraparound
  changes the result of any script relying on today's wider range, not
  just an additive fix.
- Whether `Byte`/`Short`/`Char` belong in this phase at all, or are a
  distinct, lower-priority follow-up — recommend scoping an initial pass
  to the `Int`-width fix plus real `Long`/`Float` first (the three the
  user actually asked about), since a scripting language's programs
  reach for `Byte`/`Short`/`Char` far less often than `Int`/`Long`/
  `Double`/`Float`.
- Literal suffix grammar (`10L`, `3.14f`/`3.14F`) — new to `lexer.rs`'s
  `lex_number`, matching real Kotlin's own suffix rules; real Kotlin also
  has `0x`/`0b` hex/binary integer literals and `_` digit separators,
  worth deciding whether those ride along or are their own separate item.
- **Arithmetic promotion stops being a 2×2 table.** Today `apply_binary`
  only ever widens `Int`↔`Double` (one rule, `docs/syntax.md`: "`Int`/
  `Double` operands mix freely... result is `Double` whenever either
  operand is"). With `Byte`/`Short`/`Int`/`Long`/`Float`/`Double` all
  real, this becomes a real promotion lattice matching Kotlin's own rules
  (`Byte < Short < Int < Long`, `Float < Double`, an integral type mixed
  with `Float`/`Double` promotes to that floating type) — needs its own
  careful design, not a mechanical extension of today's single rule.
  `types::is_assignable`'s current "no numeric widening" assignment rule,
  `resolver.rs`'s `is_numeric`, and value formatting/`Display` all need
  re-deriving for N numeric types instead of 2 as well.
- **stdlib impact, concretely, following directly from this same
  session's `kotlin.math` overload work**: real Kotlin's own
  `kotlin.math.abs`/`max`/`min` each have `Int`/`Long`/`Float`/`Double`
  overloads (`sqrt` has `Float`/`Double` only, no integral overload at
  all — see Phase 0.5's note on ksl's `Int` overload already being a
  deliberate ksl-only convenience beyond real Kotlin there). Growing
  `STDLIB_PACKAGES`'s `kotlin.math` entries from today's 2-overloads-per-
  name to the real 3–4 is a natural, incremental follow-up once the types
  themselves exist — no redesign of the overload-representation mechanism
  just built (`symbols: &[BuiltinSymbol]` already supports it).

---

## Phase 3 — Collection indexing (redefined; see below)

**This phase's original text (below the line) proposed `[1, 2, 3]` as
list-literal syntax and a `mapOf`-backed `Value::Map`, and was audited and
corrected before Phase 4 was implemented.** Two problems were found:

1. **`[1, 2, 3]` is not real Kotlin syntax.** Real Kotlin has no bracket
   list-literal expression at all — `[` only ever means indexing
   (`list[i]`, sugar over `operator fun get`/`set`) or, narrowly, an
   array-valued annotation argument. Reusing the already-lexed-but-unused
   `LBracket`/`RBracket` tokens for a literal would have been inventing
   grammar, not implementing Kotlin. **Decision: use those tokens for real
   indexing instead** — `a[i]` (read) and `a[i] = v` / `a[i] += v` (write),
   both genuinely missing from ksl today and both real Kotlin, once the
   type system has somewhere real to hang them: a generic `List<T>` with
   actual `operator fun get(index: Int): T` / `operator fun set(index:
   Int, value: T)` methods. That needs classes (Phase 6), generics (Phase
   9), and object properties/`operator fun` declaration support that
   **no phase in this document defines yet** — none of Phase 6/7/8/9 as
   written scopes in `operator fun` overloading, only ordinary named
   methods. Treat "properties + `operator fun get`/`set`" as an unscoped
   extension of Phase 6 to design when this phase is actually picked up,
   not something to design speculatively now. Until then, indexing stays
   unimplemented — a builtin special-case over today's untyped
   `Value::List` was considered and rejected, since it would need
   reimplementing anyway once real operator overloading exists.
2. **`mapOf`'s proposed pair representation doesn't match real Kotlin
   either.** Real `mapOf(vararg pairs: Pair<K, V>)` takes actual `Pair`
   objects built via the infix `to` function (`"a" to 1`) — ksl has none
   of `Pair`, generics, or infix functions. **Decision: defer
   `Value::Map`/`mapOf` entirely**, for the same reason as indexing —
   build it properly once `Pair`/generics/infix functions exist, rather
   than shipping a workaround (e.g. raw 2-element `List`s standing in for
   pairs) now. Infix functions aren't even a scoped phase in this document
   yet — flagged here as a known gap, not designed now.

Phase 4 (null-safety) was independently audited at the same time and found
to already match real Kotlin syntax exactly (including elvis's precedence
placement, corrected — see Phase 4's own section) — it needed no content
changes and was implemented first, since it has no dependency on this
phase either way.

**Original text** (kept for context on what was rejected; superseded by
the above):

~~Independent of Phase 1; can land any time after Phase 0.~~

~~**List literals** `[1, 2, 3]`: tokens already exist (`LBracket`/
`RBracket` are lexed, `token.rs:40-41`). New `ExprKind::ListLit(Vec<Expr>)`
parsed in `parse_primary` alongside the existing `(`/`{` arms. Evaluates
to the existing `Value::List` — no new `Value` variant, pure sugar over
`listOf(...)`.~~

~~**Maps** (per the decision above, no bracket syntax): new
`Value::Map(Rc<RefCell<Vec<(Value, Value)>>>)` — association-list backed
since `Value` has no `Hash`/`Eq`, only a custom `values_equal`. New
stdlib: `mapOf(pairs...)`, `pair(a, b)` (or accept `listOf(k,v)` pairs
directly, needing no new stdlib entry beyond `mapOf`), `get`,
`containsKey`, `keys`, `values`. Needs `type_name`, `Display`
(`{k=v, ...}`), `values_equal`, and an `iterate()` arm (yielding 2-elem
lists, matching Kotlin's `Map.Entry` iteration) added alongside `List`'s
existing handling in `value.rs`/`interpreter.rs:401-406`.~~

---

## Phase 4 — Null-safety [DONE]

Independent of Phase 1 and (the redefined) Phase 3; requires Phase 2 (its
`Type`/`is_assignable` machinery is what makes nullability checkable both
statically and at runtime — see below). No `null` concept exists at all
today (confirmed: no `Value::Null`, no nullable-hint handling in
`check_type`) — this is from-scratch, not an extension.

**Implemented as planned below, with a few corrections found during
implementation** (kept here rather than silently editing the plan out from
under itself):
- **Nullable type hints are a `TypeAnnotation::Nullable(Box<TypeAnnotation>,
  Span)` wrapper node**, not the `nullable: bool` field sketched below —
  `TypeAnnotation` is already a recursive tree, so a wrapper composes with
  `Named`/`Function` for free (`(Int) -> String?`, `((Int) -> String)?`)
  without touching every construction/consumption site the way a flag
  would have. `types::Type` gained the matching `Null`/`Nullable(Box<Type>)`
  pair. Elvis's precedence, described below as "like `&&`/`||`", actually
  needed its own grammar level: real Kotlin's `?:` binds looser than `<`/
  `>`/`==`/`&&`/`||` but tighter than `..`/`+`/`-`, so it's a new
  `parse_elvis()` between `parse_comparison` and `parse_range`, not handled
  inside `parse_and`/`parse_or`.
- **Two `ksl-lsp` bugs, not anticipated by the LSP note below**: once a
  receiver's static type can be `Type::Nullable(T)`, both `completion.rs`'s
  and `hover.rs`'s exact-string match against `BUILTIN_METHOD_SIGNATURES`
  (comparing a receiver's `Type::name()`, e.g. `"String?"`, against the
  table's bare `"String"` entries) silently produced no matches — so a
  `T?`-typed local got no completion/hover for its own builtin methods.
  Both were fixed by unwrapping one `Nullable` layer before the name
  comparison.
- **`parse_type_annotation`'s existing grammar had no support for grouping
  parens around a non-function type annotation** — `((Int) -> String)?`
  needs `(X)` to sometimes mean "just `X`, redundantly parenthesized" (to
  attach the trailing `?` to the whole function type) rather than always
  meaning "the start of a function type's own parameter list, `-> Ret`
  mandatory afterward". Fixed: a parenthesized annotation list with no
  following `->` and exactly one element is now that one element,
  unwrapped, instead of a hard parse error.
- **Reversed after initial shipping: real flow-sensitive smart-casting was
  added, and an unsafe call on an un-narrowed nullable receiver is now a
  static error, not silently accepted.** The bullet below ("best-effort,
  not full Kotlin-grade flow-sensitive null tracking... no smart-casting
  after an `if (x != null)` check") described the *shipped* v1 behavior
  accurately at the time, but real usage immediately surfaced two
  problems: an unsafe `.` call on a `T?` local was never rejected even
  with no null check at all, and the only failure mode was a confusing
  runtime `'Null' has no method 'X'` (the value's runtime type, not the
  variable's declared nullable type). Fixed in `resolver.rs`: a new
  `Resolver::narrowed: HashSet<usize>` field (symbol indices currently
  proven non-null by the surrounding control flow) is read in the single
  place a `Name`'s type is resolved (`resolve_name` — returns the
  unwrapped inner type when narrowed, so every consumer benefits
  uniformly with no special-casing) and written wherever Kotlin itself
  narrows: `if`/`else` branches (plus carry-forward past an `if` whose
  taken branch always `return`s/`break`s/`continue`s), `&&`/`||`
  short-circuit's right operand, `while` loop bodies, and a subject-less
  `when` arm with one condition — recognizing `x != null`/`x == null`
  (either operand order) combined through `&&`/`||`/`!`. Deliberately
  restricted, matching Kotlin's own rule, to `val`/parameter bindings
  (never `var`) and bare-name receivers (never a method-call result).
  `MethodCall`/`FieldAccess` now reject an unsafe access on a
  `Type::Nullable` receiver outright with Kotlin's own wording (`only safe
  (?.) or non-null asserted (!!) calls are allowed on a nullable receiver
  of type 'X?'`); the interpreter's runtime message for the residual
  dynamic-only case (a genuinely `Unknown`-typed value that turns out
  `Value::Null`) was also clarified. See `docs/syntax.md`'s "Null safety"
  section for the exact recognized patterns and remaining limits (no
  narrowed-type hover, no `x ?: return`-style narrowing — moot today since
  ksl has no `return`/`break`/`continue` in expression position at all).

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
  sides statically known" gate as everything else. ~~This is still
  best-effort, not full Kotlin-grade flow-sensitive null tracking (e.g. no
  smart-casting after an `if (x != null)` check) — a real, documented
  divergence from actual Kotlin null-safety, just a narrower one than "no
  static checking at all."~~ **Superseded** — see the "Reversed after
  initial shipping" correction above; real smart-casting was added.
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

**Docs/tests:** new "Null safety" section in `docs/syntax.md`, now
describing the real smart-cast support (recognized condition shapes, the
`val`/parameter-only restriction) rather than a "no smart-casting" caveat
— see the "Reversed after initial shipping" correction above; remove from
"Not supported." Tests: `x?.length()` on null receiver, `?:` fallback and
short-circuit, `!!` error + span, non-nullable assigned `null` errors both
statically and at runtime, `null == null`, chained `a?.b?.c`, an unsafe
call on an un-narrowed nullable receiver rejected with the new message,
and one test per smart-cast source (`if`/`else`, early-exit carry-forward,
`&&`/`||`, `while`, subject-less `when`, `var` never narrows, narrowing
doesn't leak past a non-exiting `if`). New `tests/examples/null_safety.ksl`.

---

## Phase 4.5 — `Any` and `Nothing` (top and bottom types) [DONE]

Needs only Phase 2 (`types.rs`) — no classes, no interfaces, nothing else
below this line. Added early, deliberately ahead of `try`/`catch` and
classes, because both need it: `throw` (Phase 5) is only correctly typed
once `Nothing` exists, and least-upper-bound branch inference (Phase 6.5)
needs `Any` as its fallback supertype. Real Kotlin has no type literally
named `Unknown` — this phase is specifically about `Any`/`Nothing`, kept
distinct from ksl's own, unrelated `Type::Unknown` (an internal "couldn't
statically determine a type" escape hatch, not a real declarable type;
see below for exactly how they differ).

- **`Type::Any`** — new leaf in `types.rs`'s `Type`, alongside `Unknown`/
  `Null` (not part of `is_primitive()`'s set — like `Unknown`/`Generic`/
  `Class`, not a "concrete primitive" that can genuinely disagree with
  another one). `resolve_type_hint`/`PRIMITIVE_TYPE_NAMES` gain `"Any"`.
  **The key difference from `Type::Unknown`**: `is_assignable` for `Any`
  is a **one-directional** rule, `(_, Type::Any) => true` (anything is
  assignable *into* `Any`) — deliberately **not** `(Type::Any, _) =>
  true` (an `Any`-typed value is *not* assignable back to a concrete type
  without an explicit cast, which the language doesn't have yet, so `val
  x: Int = someAnyTypedVal` stays a type error, correctly). This is the
  opposite of `Unknown`'s bidirectional leniency ("can't tell, so don't
  block either direction") — `Any` isn't an escape hatch, it's a real,
  sound top type, exactly like Kotlin's.
- **`Type::Nothing`** — new leaf alongside. `resolve_type_hint` gains
  `"Nothing"`. `is_assignable`: `(Type::Nothing, _) => true`,
  unconditionally — `Nothing` is assignable to anything, including a
  `Nullable(T)` (already covered by the same general rule, no special
  nullable case needed) — placed near the existing `Unknown`/`Generic`
  rules at the top of the match.
- **New join rule for `if`/`when` branches** (`resolver.rs`'s
  `ExprKind::If`/`When`, the same code the recent "else-less is never a
  mismatch" fix touched): when either branch's type is `Type::Nothing`,
  the whole join's result is simply the *other* branch's type, no further
  comparison — `Nothing` is the identity element of least-upper-bound
  (`LUB(Nothing, T) = T`, always, in any type lattice). This is what
  makes `if (cond) 5 else throw Foo()` type-check as `Int`, not an error
  or `Unknown` — the concrete motivating case for `Nothing`, and exactly
  what Phase 5's own text below already promises ("`throw` has type
  `Nothing`") without yet having anything to back it up.
- **No runtime changes at all** — `Nothing` has no values (`throw` never
  returns control flow normally, only via `Flow::Throw`), so
  `Type::from_value` never produces it; `Any` doesn't need a
  `Type::from_value` arm either (that function already returns the most
  specific type a value has — `Any` only ever appears on the annotation
  side, never as an inferred result).
- **Optional future cleanup, not required now**: once `Nothing` exists,
  the already-shipped (Phase 4) `Type::Null` could in principle be
  redefined as `Type::Nullable(Box::new(Type::Nothing))` — exactly
  matching real Kotlin, where `null : Nothing?`, instead of keeping its
  own separate `Null` leaf. Not blocking anything and not required —
  `Type::Null` already works correctly for what it does today; this is a
  purely cosmetic/theoretical convergence with Kotlin, left to whoever
  eventually touches this code.

**Docs/tests:** `docs/syntax.md`'s "Types" section gains `Any`/`Nothing`
in the recognized-type-names list. Tests in `types.rs` for the
`is_assignable` rules above (including `Any`'s one-directionality,
explicitly distinguishing it from `Unknown`'s two-directionality); once
Phase 5 lands `Throw`, an end-to-end test that `if (cond) 5 else throw
Foo()` type-checks as `Int` with no errors.

---

## Phase 5 — `try`/`catch`

Requires Phase 4.5 (`Type::Nothing` is `throw`'s real static type — see
below; without it this phase would have nowhere correct to put that type
and would either fall back to `Unknown` or reintroduce the exact
branch-mismatch false-positive the `if`/`when` fix and Phase 4.5 both
already address).

**`Flow::Throw(Value)`** — new variant on `Flow` (`error.rs:90-95`),
exactly the extension point AGENTS.md calls for. `throw expr` is an
**expression** (`ExprKind::Throw(Box<Expr>)`, not a statement), matching
real Kotlin where `throw` has type `Nothing` and is usable in expression
position (`if (cond) 1 else throw Foo()`) — consistent with ksl already
treating `if`/`when`/blocks as expressions. **Concretely**: `resolver.rs`'s
new `ExprKind::Throw(inner)` arm walks `inner` (for diagnostics) and
unconditionally returns `Type::Nothing` (Phase 4.5) — this, combined with
Phase 4.5's new `If`/`When` join rule ("either branch `Nothing` → use the
other branch's type"), is what makes `if (cond) 1 else throw Foo()`
type-check as `Int` cleanly. `ExprKind::Try`'s own join (the `try` body's
value vs. each `catch`'s, when used as an expression) reuses the same
Nothing-is-the-join-identity rule.

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

## Phase 6 — Classes/objects [DONE]

The largest phase. Requires Phase 1.

**New module** `crates/ksl-core/src/class.rs` (this feature is big enough
to warrant its own file rather than growing `value.rs`/`interpreter.rs`
further, matching the existing one-concern-per-file layout).

- **As actually shipped** (this sketch predates the generic `GcRef<T:
  Trace>` cycle-collector built just before this phase, and is kept below
  as historical planning context, not the real shape): `Value::Object(
  GcRef<ObjectData>)`, `ObjectData { class: Rc<ClassValue>, fields:
  HashMap<String, Value> }` — a single `GcRef`-tracked node (mirroring
  `Value::List(GcRef<Vec<Value>>)`) rather than a plain `Rc` wrapping a
  separately-`RefCell`'d field map; see `class.rs`'s own doc comments for
  why a bare `RefCell<HashMap<...>>` here would silently leak (no `GcRef`
  tracking, so a genuine object reference cycle would never be found by
  `Interpreter::collect_cycles`'s trial-deletion). Original sketch, for
  reference: ~~`Value::Object(Rc<ObjectValue>)`, `ObjectValue { class:
  Rc<ClassValue>, fields: RefCell<HashMap<String, Value>> }` — mirrors the
  existing `Rc`-wrapped-struct pattern (`FunctionValue`) and
  interior-mutability pattern (`List`'s `Rc<RefCell<Vec<Value>>>`)~~.
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
  class; a receiver of a built-in primitive type looks up on *its* class the
  same way — see "Built-in primitive types as classes" below, which is what
  retires Phase 1's separate builtin-method table rather than keeping it as
  a permanent second path; if still not found, (Phase 8) extension
  functions.
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

**Built-in primitive types as classes.** Real Kotlin has no split between
"real classes" and "primitives with special-cased methods" — `String`,
`Int`, `List`, etc. genuinely *are* classes in Kotlin's own standard
library, with real member functions dispatched the same way any other
class's methods are. ksl's `stdlib.rs` currently implements `.`-method
calls (`"x".uppercase()`) via three free-standing items —
`BUILTIN_METHOD_SIGNATURES` (a flat `&[(receiver_type, method, signature,
doc)]` table), `call_builtin_method` (a direct `match (&receiver, method)`
over runtime `Value`s), and `builtin_method_return_type` (a separate
hand-written `match` for the resolver's static return-type inference) —
which is exactly the "dangling functions, not real class methods" shape
this phase should retire, not preserve as a permanent parallel mechanism
alongside real class dispatch:

- A fixed, small set of types get a builtin `Rc<ClassValue>` at interpreter
  startup: `Int`, `Double`, `Boolean`, `String`, `List`. `Unit`/`Nothing`/
  `Any` are eligible receivers too but start with empty method tables — the
  same externally-visible "no methods yet" outcome as today, just expressed
  structurally (a lookup miss) instead of via a separate hand-written
  catch-all error case.
- `ClassValue.methods` needs to hold either an interpreted or a native
  implementation, not just `Rc<FunctionValue>` as sketched above — e.g.
  `HashMap<String, MethodImpl>` with `enum MethodImpl {
  UserDefined(Rc<FunctionValue>), Native(NativeMethodFn) }`, mirroring how
  `Value::Function` vs `Value::Native` already coexist for plain functions.
  Each native method's metadata should follow the same "one canonical
  signature feeds both runtime dispatch and static type-checking"
  principle the stdlib signature-unification work already established for
  package-level functions (`BuiltinParam`/`BuiltinSymbol`/
  `format_signature`/`stdlib_call_types` in `stdlib.rs`) — not a repeat of
  today's split between `call_builtin_method`'s hand-written `match` and
  `builtin_method_return_type`'s separate hand-written `match`.
- `interpreter.rs`'s `MethodCall` dispatch unifies on one lookup path:
  receiver evaluates, then its class is looked up — `Value::Object`'s real
  class, or a builtin class keyed by `Value::type_name()` — then `method`
  is looked up in that class's `methods`. The direct value-pattern match
  used today (`(Value::Str(s), "length") => ...`) goes away.
- `resolver.rs`'s `MethodCall` arm mirrors this: it already derives a
  `dispatch_name` from `Type::name()` for its static check — instead of
  consulting `stdlib::BUILTIN_METHOD_SIGNATURES`/`builtin_method_return_type`,
  it consults the same builtin-class method registry, mirrored resolver-side
  the same way `Resolver::known_classes` mirrors `class.rs`'s user-class
  registry above.
- **Explicitly NOT changed by this**: `resolve_type_hint`'s primitive arms
  stay exactly as they are — `"String"` still resolves straight to
  `Type::String`, never `Type::Class("String")`. `known_classes` also stays
  exactly what this phase already plans for it above (populated only from
  user `class` declarations). The builtin-class method registry is a
  separate, code-only table keyed by type name for *method lookup only* —
  it does not feed into `Type::Class`/`known_classes`/`is_assignable`.
  Conflating "this receiver's methods are looked up via the class
  mechanism" (true for every primitive under this design) with "this
  receiver's static type is nominal `Type::Class`" (only ever true for
  user-declared classes) would be a real, unintended type-system behavior
  change, not just an implementation refactor — worth watching for during
  implementation, not doing by accident.
- Precedence with Phase 8 (extension functions) simplifies from three tiers
  to two once this lands — see Phase 8's own "Dispatch precedence" bullet.

Open questions, deliberately left unresolved here (same convention as Phase
2.3's "audit only" list and Phase 3's "unscoped extension... to design when
this phase is actually picked up"): whether `List`'s eventual real methods
(Phase 3's deferred `operator fun get`/`set`, or Kotlin's real `size`/
`get`/`contains`/etc.) go through this same mechanism once Phase 9
(generics) exists, or need generic-aware handling of their own — revisit
once Phase 9 is picked up. Exact Rust-level shape of `MethodImpl`/native-
method metadata — left to implementation time, matching how Phase 9
already defers its own open design point the same way. Whether `Any`/
`Unit`/`Nothing` need real method entries at all in this first cut (real
Kotlin's `Any` has `toString()`/`equals()`/`hashCode()`) — out of scope
for the initial cut.

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

## Phase 6.5 — Interfaces

Requires Phase 6 (needs a class to implement one; extends `class.rs`).
Not part of the original 9-feature backlog — added to make real
least-upper-bound (LUB) branch inference possible (see below), the
concrete motivating example being real Kotlin's own `val v = if (true) 10
else "hello"`, which infers `v: Comparable<*>` rather than erroring or
losing precision, because `Int` and `String` both implement
`Comparable<T>`. Today's roadmap has nothing like this at all — Phase 6
(classes) and Phase 7 (inheritance) only ever cover a single `superclass`,
never multiple-interface implementation.

- **Grammar**: `interface Comparable<T> { fun compareTo(other: T): Int }`
  — new `interface` keyword; a method inside is either a bare signature
  (abstract, no body) or has a body (a default implementation, same as
  Kotlin). `class`'s supertype list after `:` (already reserved by Phase
  7) generalizes to **one optional superclass with constructor-call
  syntax, plus any number of comma-separated interfaces**, e.g. `class
  Dog : Animal(name), Comparable<Dog> { override fun compareTo(...) =
  ... }`.
- **Runtime**: new `InterfaceValue { name: Rc<str>, methods:
  HashMap<String, Option<Rc<FunctionValue>>> }` in `class.rs` (already
  planned by Phase 6); `ClassValue` gains `interfaces:
  Vec<Rc<InterfaceValue>>`. Method dispatch (class → superclass chain,
  already Phase 6/7) gains a third step — an implemented interface's
  default methods.
- **Interface conformance is accepted syntactically but not fully
  enforced** in this first cut — the same already-established pattern
  this document uses for `open`/`override` in Phase 7 ("accepted but not
  enforced"): a class not implementing one of an interface's abstract
  methods isn't caught by the resolver yet; that's separate, explicitly
  deferred work.
- **Built-in primitives formally implement standard interfaces** — the
  new piece that specifically exists to make LUB/`Comparable<*>` work:
  `Int`/`Double`/`String`/`Boolean` are registered as implementing
  `Comparable<Self>` (a deliberately minimal starting set — exactly what
  the motivating example needs; other stdlib interfaces like
  `Iterable<T>` can be added to the registry later with no redesign). The
  registry is a new table (e.g. `stdlib.rs`'s `builtin_implements: &[(&str,
  &str)]`, mirrored into the resolver's own `known_interfaces` the same
  way `Resolver::known_classes` already works).
- **Type-system integration — real LUB for `if`/`when`**: the exact code
  the recent "else-less `if`/`when` is never a mismatch" fix touched
  (`resolver.rs`'s `ExprKind::If`/`When`, the `is_primitive`-gated
  "branches have incompatible types" error). Instead of an immediate
  error/`Unknown` on a primitive mismatch: intersect the set of
  interfaces each branch's type implements (directly, or via Phase 7's
  superclass chain) — a non-empty intersection's result is a real type
  representing the shared interface (star-projected in the type-argument
  slot when the branches' own arguments differ — see Phase 9's revision,
  below, for how that's represented); an empty intersection falls back to
  `Type::Any` (Phase 4.5 — already exists by this point, nothing new to
  add, already assignable-target-for-everything). `is_assignable` gains
  the mirror rule: type `X` is assignable to interface `I` when `X`
  implements `I` (directly or via inheritance).

**LSP:** `resolver.rs` gets an interface table alongside Phase 6's own
class table; `completion.rs`/`hover.rs` surface interface methods
alongside class methods; `semantic_tokens.rs` classifies `interface` as a
keyword.

**Docs/tests:** "Interfaces" section in `docs/syntax.md`. Tests: multiple
interface implementation, default methods, class-vs-interface dispatch
precedence, and — the actual payoff — an end-to-end test that `val v =
if (true) 10 else "hello"` type-checks as `Comparable<*>` with no error.

---

## Phase 7 — Inheritance

Requires Phase 6 (`superclass` field already reserved on `ClassValue`).
Doesn't strictly require Phase 6.5 (interfaces) to land first, but the
`is_assignable` subtyping work below ends up shared: once both exist,
subtyping covers the `superclass` chain *and* interface conformance
through the same lookup.

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
- **Dispatch precedence** (document explicitly): class methods > extension
  functions — matches Kotlin's rule that members always beat extensions
  with the same name. This collapses what used to look like a three-tier
  split ("real object methods > builtin methods > extension functions")
  into two: once Phase 6's "Built-in primitive types as classes"
  subsection lands, a built-in primitive's methods (`String`/`Int`/`List`/
  etc.) are looked up through the exact same class-method mechanism a
  user-declared `class`'s methods are — there is no separate "builtin
  methods" tier left to rank against extension functions by the time this
  phase is implemented.
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

## Phase 9 — Generics (statically parameterized, runtime-erased)

Requires Phase 6 (class headers are the primary place `<T>` appears).

**Revised** from this phase's original "parsed, fully erased" scope (kept
below, in the "Grammar-ambiguity constraint"/`type_params` bullets, as
still-valid groundwork) after Phase 6.5's motivating example
(`Comparable<*>` as the join of `Int`/`String`) showed that erasing
parameterization *statically* throws away exactly the information LUB
inference needs — the resolver must actually know that `Int` implements
`Comparable<Int>` specifically (not just "implements `Comparable` of
something") to compute that both sides converge on `Comparable<*>`. The
fix keeps erasure exactly where it was always legitimate — **erasure is
now a property of the runtime, not the type system** — matching how real
Kotlin/JVM generics actually work: `List<Int>` and `List<String>` are
statically distinct, fully checked types to the compiler, and only
collapse to the same erased runtime class at the bytecode level. This was
never an ksl-specific simplification to begin with; it's just where the
original text drew the line one layer too high.

- **Static parameterization is retained, not erased.** `Type::Class`/a
  new `Type::Interface` (Phase 6.5) carry a real type-argument list
  (`Vec<Type>`), not just a bare name — `Box<Int>` and `Box<String>` are
  distinct, distinguishable static types, and `is_assignable` compares
  their argument lists element-wise (invariantly, in this first pass —
  real variance for generic parameters/joins is an open question, left
  for whoever implements this to resolve against the actual code by
  then, not decided speculatively here). This also needs a way to
  represent Kotlin's star projection (`Comparable<*>` — "argument
  intentionally unconstrained," not "argument `Unknown` because
  something went wrong") — a dedicated marker, or a reserved slot value
  in that `Vec<Type>`; exact Rust-level shape left to implementation
  time.
- **Runtime erasure is unchanged** — `interpreter.rs` still never
  specializes on a type argument, preserving the original "zero
  interpreter changes needed" property, just now correctly scoped to
  mean "the interpreter," not "the whole type system."
- **Call-site type arguments in expression position remain unsupported,
  unchanged** — `identity<Int>(5)` still isn't parsed, for the same real
  reason as before: `<`/`>` collide with `Lt`/`Gt` in expression position
  or a backtracking-free recursive-descent parser. This doesn't conflict
  with retaining parameterization above: `Box<Int>` in **type-annotation**
  position (`val b: Box<Int>`, `val c: Comparable<Int>`) is already
  grammatically unambiguous today, for the same reason function types
  `(A, B) -> C` already parse fine there — that's the position this
  revision actually needs to parse and keep, not expression position.

- **Grammar-ambiguity constraint** (unchanged): `<`/`>` are already
  `Lt`/`Gt` comparison tokens — `Box<T>` vs. `a < b` is ambiguous in a
  backtracking-free recursive-descent parser. Resolve by **restricting
  `<...>` type-parameter parsing to declaration headers and type-annotation
  position** (immediately after `class Name`/`fun` before the parameter
  list, or wherever a type annotation is already being parsed — see the
  revision above), never in general expression position.
- `ClassDecl`/`FunDecl` gain an optional `type_params: Vec<String>` (names
  + spans). Push this list onto `Resolver::generic_params` (Phase 2,
  always empty until now) while walking the declaration's body, so a
  type-hint like `x: T` resolves to `types::Type::Generic("T")` —
  reserved back in Phase 2 for exactly this, and still distinct from a
  *parameterized* `Type::Class`/`Type::Interface` above (`Generic("T")` is
  an unbound type-parameter reference inside a generic declaration's own
  body; the parameterized-class shape is a real, instantiated argument
  list at a use site) — instead of an accidental `Unknown`. `Generic`
  still behaves like `Unknown` in `is_assignable` (erasure inside a
  generic body itself is unaffected by this revision — there's genuinely
  nothing more specific to check there without real specialization).

**LSP:** `resolver.rs`/`hover.rs` show the declared `type_params` names in
hover text for `class`/`fun` declarations, and now also show real
parameterized types correctly (`Box<Int>`, not just `Box`) wherever hover
already shows a `Type::name()`; `semantic_tokens.rs` classifies
type-parameter names distinctly from ordinary identifiers so they read as
generic placeholders in the editor.

**Docs/tests:** `docs/syntax.md` must be explicit about the revised split
— statically checked parameterization, runtime erasure, no call-site type
arguments. Tests: `class Box<T>(val value: T)` works with any value type;
`fun <T> identity(x: T): T = x` works interchangeably; regression test
that `a < b` in ordinary expression position is unaffected; `Box<Int>` and
`Box<String>` are rejected as mismatched when one is used where the other
is declared; the Phase 6.5 end-to-end `Comparable<*>` test.

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
- `crates/ksl-core/src/class.rs` — new module for Phase 6/6.5/7.
- `crates/ksl-lsp/src/*.rs` — LSP-side updates, per phase, in lockstep.
- `docs/syntax.md`, `docs/stdlib.md` — spec updates per phase.
- `AGENTS.md` — update "Explicitly out of scope" as each item lands.
