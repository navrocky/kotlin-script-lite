# kotlin-script-lite (`ksl`)

A small tree-walking interpreter, written in Rust, for a scripting language
whose syntax matches a subset of Kotlin. There is no compilation step and no
bytecode — scripts are lexed, parsed into an AST, and evaluated directly.
Builds to a single, fully static Linux binary named `ksl`. Script files use
the `.ksl` extension.

## Documentation

- [`docs/syntax.md`](docs/syntax.md) — supported language syntax
  (variables, operators, control flow, functions, lambdas, and what's
  deliberately out of scope)
- [`docs/stdlib.md`](docs/stdlib.md) — built-in functions

See also `tests/examples/*.ksl` for runnable sample scripts.

## Building

Requires a stable Rust toolchain (install via [rustup](https://rustup.rs)
if you don't have one).

```sh
cargo build --release -p ksl-cli
```

### Static Linux binary

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release -p ksl-cli --target x86_64-unknown-linux-musl
```

The `musl` target links statically by default — no extra `RUSTFLAGS`
needed. Verify with:

```sh
file target/x86_64-unknown-linux-musl/release/ksl   # "statically linked"
ldd target/x86_64-unknown-linux-musl/release/ksl    # "not a dynamic executable"
```

## Usage

```sh
cargo run -p ksl-cli -- script.ksl              # run a script file
echo 'println("hi")' | cargo run -p ksl-cli -- - # run a script from stdin
cargo run -p ksl-cli -- --help
```

A script can also be made directly executable with a shebang:

```sh
#!/usr/bin/env ksl
println("hi")
```

## Testing

```sh
cargo test
```

This runs:
- Unit tests colocated with `lexer`, `parser` and `interpreter` (in
  `crates/ksl-core/src/`), covering tokenization, precedence/AST shape,
  and evaluation semantics.
- `crates/ksl-core/tests/run_examples.rs`, which runs every
  `crates/ksl-core/tests/examples/*.ksl` script through the interpreter
  and diffs its stdout against the matching `*.out` fixture.

## Project layout

This is a Cargo workspace with three crates:

| Path | Purpose |
|---|---|
| `crates/ksl-core/` | The interpreter, as a library (`ksl-core`). Zero external dependencies. |
| `crates/ksl-core/src/token.rs`, `lexer.rs` | Tokenizer, incl. the mode-stack lexer for string templates |
| `crates/ksl-core/src/ast.rs` | AST node definitions |
| `crates/ksl-core/src/parser.rs` | Recursive-descent statement parser + Pratt expression parser |
| `crates/ksl-core/src/value.rs` | Runtime `Value` representation |
| `crates/ksl-core/src/environment.rs` | Lexical scope chain (`Env`), backs closures |
| `crates/ksl-core/src/interpreter.rs` | Tree-walking evaluator |
| `crates/ksl-core/src/stdlib.rs` | Built-in global functions |
| `crates/ksl-core/src/importer.rs` | Resolves and runs `@file:Import(...)` local file imports |
| `crates/ksl-core/src/error.rs` | `ParseError` / `RuntimeError` / `Flow` (control transfer) / `InterpError` |
| `crates/ksl-core/src/lib.rs` | `run_source()` — the single entry point used by both the CLI and tests; resolves imports before running the script itself |
| `crates/ksl-core/tests/examples/` | End-to-end `.ksl` scripts + expected `.out` output |
| `crates/ksl-cli/src/main.rs` | The `ksl` CLI binary — a thin wrapper over `ksl-core` |
| `crates/ksl-lsp/` | The `ksl-lsp` language server binary for editor support (in progress) |
| `docs/` | Syntax and stdlib reference documentation |
