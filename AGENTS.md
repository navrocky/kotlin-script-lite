# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

A tree-walking interpreter for a Kotlin-syntax scripting language, written
in Rust, with no compilation/bytecode step. See `docs/syntax.md` and
`docs/stdlib.md` for the language/stdlib reference, and
`crates/ksl-core/src/` for the pipeline (`lexer.rs` -> `parser.rs` ->
`ast.rs` -> `interpreter.rs`). Read `README.md`'s "Project layout" table
before making changes — it's short and saves re-deriving the module map.

This is a Cargo **workspace** with three crates: `ksl-core` (the
interpreter library, zero dependencies — most changes happen here),
`ksl-cli` (the `ksl` binary, a thin wrapper over `ksl-core`), and
`ksl-lsp` (the language server binary for editor support, may depend on
`tower-lsp`/`tokio` — keep those dependencies out of `ksl-core`/`ksl-cli`).

## Build / test / run

```sh
cargo build                          # dev build, whole workspace
cargo test                           # unit tests (lexer/parser/interpreter) + run_examples.rs, whole workspace
cargo run -p ksl-cli -- script.ksl   # run a script (binary is named `ksl`)
cargo build --release -p ksl-cli --target x86_64-unknown-linux-musl   # static release binary
```

Always run `cargo test` after a change to `src/`. If you touch lexer,
parser, or interpreter semantics, add or update a test in that module's
`#[cfg(test)] mod tests` block rather than only relying on the example
scripts — the unit tests pin down structural details (AST shape, token
sequence) that the end-to-end examples don't check.

## Conventions specific to this codebase

- **No parser-generator dependency.** The lexer and parser are hand-written
  (see the plan history / commit log for why `pest` was dropped). Don't
  reintroduce a grammar-file-based parser.
- **`Span` on every `Expr`/`Stmt`.** Runtime errors need a source location.
  When adding a new `ExprKind`/`StmtKind` variant, thread a real `Span`
  through it (from the token that starts the construct), not
  `Span::default()`. `NativeFn` (`value.rs`) receives the call-site `Span`
  as its third argument — stdlib functions in `stdlib.rs` must thread it
  into their error helpers (`arity_error`/`type_error`/etc.) rather than
  falling back to `Span::default()`, which remains reserved for cases with
  genuinely no call site (e.g. an internal io error in `write_out`).
- **Int arithmetic wraps**, matching Kotlin's `Int` overflow semantics —
  use `wrapping_add`/`wrapping_sub`/`wrapping_mul`/`wrapping_neg`, never
  bare `+`/`-`/`*` on `i64` values in `interpreter.rs`. Integer division
  and remainder by zero are runtime errors (`checked_div`/`checked_rem`),
  not panics.
- **Control flow is threaded through `Result<_, Flow>`.** `return`,
  `break`, and `continue` are `Err(Flow::Return/Break/Continue)`, unwound
  with `?`. Loops catch `Break`/`Continue`; function calls catch `Return`.
  Don't add a side-channel (thread-local, panic/catch_unwind, etc.) for
  this — extend `Flow` instead if a new non-local transfer is needed.
- **Closures capture the environment at definition time.** `FunctionValue`
  stores the `Env` alive when the function/lambda literal was evaluated;
  calling it creates `closure.child()`, never the caller's env. Preserve
  this when touching `call_function`.
- **`.` member-call syntax is deliberately ambiguous at parse time.**
  `ExprKind::FieldAccess`/`MethodCall` (`parser.rs`'s `parse_postfix`) are
  produced for *any* `receiver.member[(...)]`, including what's really a
  package-qualified name (`a.b.c`) — the parser has no scope info to tell
  a local receiver from a package apart. `eval_expr` resolves this at
  evaluation time (see `Interpreter::dotted_path_if_unresolved`): try the
  receiver as a real local first, and only fall back to
  `resolve_qualified` over the flattened dotted chain once its base name
  is confirmed not to be one. `resolver.rs` mirrors this exact logic
  (`Resolver::dotted_path_if_not_local`) so hover/goto-def keep working for
  both cases. Only a handful of builtin `String` methods exist so far
  (`stdlib::call_builtin_method`); real per-type members need classes
  (Phase 5 of `docs/roadmap.md`).
- **Values are `Rc`-based, not `Arc`.** This interpreter is single-threaded
  by design; don't introduce `Send`/`Sync`/threading to `Value`,
  `Env`, or `Interpreter`.

## Adding a feature checklist

1. Token(s) in `token.rs` if new syntax is needed (`TokenKind` + lexing in
   `lexer.rs`).
2. AST node(s) in `ast.rs`.
3. Parsing in `parser.rs`, at the correct precedence level if it's an
   operator (see the precedence table in `docs/syntax.md` — loosest-to-
   tightest: `||`, `&&`, `==`/`!=`, comparisons, `..`, `+`/`-`, `*`/`/`/`%`,
   unary, postfix call).
4. Evaluation in `interpreter.rs`.
5. Unit test(s) colocated in the relevant module.
6. If it's a user-visible language feature (not an internal refactor),
   consider adding or extending a script under `tests/examples/` (`.ksl`
   extension) with a matching `.out` fixture — generate the `.out` by
   actually running the script through the built binary
   (`cargo run -- tests/examples/foo.ksl`), don't hand-compute output for
   anything involving floating-point formatting (Rust's `f64` `Display`
   differs from Kotlin's — e.g. `2.0` prints as `2`, not `2.0`).
7. Adding a stdlib function (`stdlib.rs`'s `register` calls) also needs an
   entry in `STDLIB_SIGNATURES` (same file) and `docs/stdlib.md` — skipping
   the table entry means `ksl-lsp` won't offer it in hover/completion and
   the resolver will flag calls to it as an undefined-name diagnostic.

## Explicitly out of scope (don't add without being asked)

Classes/objects, inheritance, generics, null-safety operators, rich
collection literals beyond `listOf`, `try`/`catch`, coroutines, and
extension functions. These were deliberately deferred — see
`docs/syntax.md`.

Local file imports (`@file:Import("path.ksl")`, loading another file's
top-level code into the shared global scope, or a package's own scope —
see below) *are* supported, as is `package`/`import` namespacing
(`package a.b`; `import a.b.c` aliasing a qualified name into bare scope;
`a.b.c` qualified-name expressions work inline without `import`) — see
`docs/syntax.md#imports`, `src/importer.rs`, `Interpreter::package_env`/
`resolve_qualified` in `src/interpreter.rs`. Network imports remain
unsupported (`@file:Import("https://...")` is a clear error).

`ksl_core::resolver` (used for hover/goto-def/completion) stays
single-file, same limitation it has for `@file:Import` — it doesn't try to
resolve `a.b.c` itself. Cross-file resolution for package-qualified names
lives entirely in `ksl-lsp`'s own `package_index` module instead, which
walks a document's `@file:Import` graph read-only to index every
`package`-declaring file it finds (mirroring `importer.rs`'s own path
resolution, via the now-`pub` `resolve_path`/`is_url`). Keep this layering
if extending it further — `resolver.rs` shouldn't grow filesystem access
or multi-file state.
