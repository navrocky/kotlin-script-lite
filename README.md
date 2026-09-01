# kotlin-script-lite (`ksl`)

A small tree-walking interpreter, written in Rust, for a scripting language
whose syntax matches a subset of Kotlin. There is no compilation step and no
bytecode — scripts are lexed, parsed into an AST, and evaluated directly.
Builds to a single, fully static Linux binary named `ksl`. Script files use
the `.ksl` extension.

## Language features (MVP)

- `val` / `var` declarations, with optional `: Type` annotations
- Arithmetic, comparison and logical expressions with standard precedence
- String templates: `"hello $name"` and `"sum = ${a + b}"`, including nesting
- `if` / `when` / blocks as **expressions** (they produce a value), as well
  as statements
- `while`, and `for (x in <range or list>)`, with `break` / `continue`
- Functions with default parameter values and named arguments at call sites
- Lambdas / closures as first-class values, including implicit `it` for
  single-argument lambdas (`{ it * 2 }`)
- A small standard library: `println`, `print`, `listOf`, `length`,
  `uppercase`, `lowercase`, `trim`, `abs`, `sqrt`, `max`, `min`

Explicitly out of scope for now: classes/objects, inheritance, generics,
null-safety operators (`?`, `?.`, `?:`, `!!`), rich collection literals,
`try`/`catch`, coroutines, extension functions, packages/imports, and `.`
method-call syntax (stdlib functions are called directly, e.g. `length(s)`
rather than `s.length`).

See `tests/examples/*.ksl` for runnable sample scripts.

## Building

Requires a stable Rust toolchain (install via [rustup](https://rustup.rs)
if you don't have one).

```sh
cargo build --release
```

### Static Linux binary

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The `musl` target links statically by default — no extra `RUSTFLAGS`
needed. Verify with:

```sh
file target/x86_64-unknown-linux-musl/release/ksl   # "statically linked"
ldd target/x86_64-unknown-linux-musl/release/ksl    # "not a dynamic executable"
```

## Usage

```sh
ksl script.ksl                        # run a script file
echo 'println("hi")' | ksl -          # run a script from stdin
ksl --help
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
  `src/`), covering tokenization, precedence/AST shape, and evaluation
  semantics.
- `tests/run_examples.rs`, which runs every `tests/examples/*.ksl` script
  through the interpreter and diffs its stdout against the matching
  `*.out` fixture.

## Project layout

| Path | Purpose |
|---|---|
| `src/token.rs`, `src/lexer.rs` | Tokenizer, incl. the mode-stack lexer for string templates |
| `src/ast.rs` | AST node definitions |
| `src/parser.rs` | Recursive-descent statement parser + Pratt expression parser |
| `src/value.rs` | Runtime `Value` representation |
| `src/environment.rs` | Lexical scope chain (`Env`), backs closures |
| `src/interpreter.rs` | Tree-walking evaluator |
| `src/stdlib.rs` | Built-in global functions |
| `src/error.rs` | `ParseError` / `RuntimeError` / `Flow` (control transfer) / `InterpError` |
| `src/lib.rs` | `run_source()` — the single entry point used by both the CLI and tests |
| `src/main.rs` | Thin CLI wrapper |
| `tests/examples/` | End-to-end `.ksl` scripts + expected `.out` output |
