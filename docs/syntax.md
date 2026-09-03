# Syntax reference

`ksl` scripts use a subset of Kotlin syntax, interpreted directly (no
compilation step). This document describes everything the parser and
interpreter currently support. For built-in functions, see
[`stdlib.md`](stdlib.md).

## Comments and shebang

```kotlin
#!/usr/bin/env ksl        // optional, only recognized as the very first line

// line comment
/* block
   comment */
```

A leading `#!...` line is skipped by the lexer, so scripts can be made
directly executable.

## Statement separators

Statements are separated by a newline or a `;`. A newline only ends a
statement if the previous token could legally end one (an identifier,
literal, `)`, `]`, `}`, or `return`/`break`/`continue`) — so this is fine:

```kotlin
val total = 1 +
    2 +
    3
```

## Literals

| Kind | Examples |
|---|---|
| `Int` (`i64`) | `42`, `-7` |
| `Double` (`f64`) | `3.14`, `1e3`, `2.5e-2` |
| `Boolean` | `true`, `false` |
| `String` | `"hello"`, with templates (below) |

There is no list literal syntax (`[1, 2, 3]`) — build a list with
[`listOf(...)`](stdlib.md#listof).

### String templates

```kotlin
val name = "World"
println("Hello, $name!")            // bare-identifier shorthand
println("sum = ${1 + 2}")           // arbitrary expression
println("${"nested ${name}"}")      // templates can nest
```

`$name` only accepts a bare identifier (no `.member` or `[index]`); for
anything else use `${ ... }`.

## Variables

```kotlin
val x = 10          // immutable
var y: Int = 20      // mutable, with an optional type annotation
y = y + 1
y += 5               // += -= *= /= %= are also supported
```

Reassigning a `val` is a runtime error. A type annotation (`Int`, `Double`,
`Boolean`, `String`, `Unit`, or `List`) is checked against the
initializer's value, and — for a `var` specifically — against every later
reassignment too; a `var` with **no** annotation keeps its full dynamic
flexibility (reassignable to any type, exactly as if this checking didn't
exist). Any other annotation name (a typo, a not-yet-supported type) is
accepted but not enforced. See [Types](#types) below for where this
checking happens and how far it goes.

## Types

ksl has a small, best-effort type checker — not a full static type
system, but real checking, both ahead of time and while a script runs:

- **What's checked**: a `val`/`var`'s initializer against its type
  annotation; a `var`'s reassignment against that same annotation (only
  when one was given — see "Variables" above); a function call's
  positional arguments against the callee's declared parameter types;
  and a function's actual return value against its declared return type.
- **What's never checked**: named arguments (checked positionally only,
  for now); anything without a checkable type on both sides — an
  annotation the checker doesn't recognize (a typo, or a real feature
  this interpreter doesn't have yet, like a class name), a value whose
  type can't be worked out without running the script (e.g. the result of
  a package-qualified call), or a plain, unannotated `val`/`var`/parameter
  used somewhere its type can't be inferred. These are always accepted,
  never a false rejection.
- **`Int`/`Double` mixing is not the same rule as arithmetic.** `1 + 2.0`
  freely produces a `Double` (see "Operators" below) — but assigning,
  passing, or returning an `Int` where a `Double` is explicitly expected
  (or vice versa) *is* a type mismatch. The two rules don't interact.
- **Where the check happens, twice**: the same checking runs statically
  (as you type, in an editor connected to `ksl-lsp`, and once more when
  the `ksl` CLI loads a script, *before* running it — a type problem here
  means the script never runs at all, not even its first line) and again
  at runtime inside the interpreter itself (so embedding `ksl-core`
  directly, without going through the CLI's pre-check, still can't skip
  it). A type-mismatch error looks like `type mismatch: expected Int, got
  String`.
- Only 6 annotation names are recognized today — `Int`, `Double`,
  `Boolean`, `String`, `Unit`, `List` — no class/generic/nullable types
  exist yet (see "Not supported" below). A lambda's own type is always
  inferred as a plain "function" internally, but there's no `Function`
  annotation to write down for it.

## Operators

Listed loosest-to-tightest binding (i.e. `a || b && c` parses as
`a || (b && c)`):

| Precedence | Operators | Notes |
|---|---|---|
| 1 (loosest) | `\|\|` | short-circuits |
| 2 | `&&` | short-circuits |
| 3 | `==` `!=` | structural equality |
| 4 | `<` `>` `<=` `>=` | numeric or lexicographic `String` comparison |
| 5 | `..` | inclusive `Int` range, e.g. `1..5` |
| 6 | `+` `-` | `+` also concatenates `String` (`"x" + 1`) and `List` |
| 7 | `*` `/` `%` | integer division/remainder by zero is a runtime error |
| — | unary `-` `+` `!` | tightest, right-associative |
| — | `f(...)`, `.field`, `.method(...)` | postfix call/member-access, tightest of all, left-associative (`a.b.c()` is `(a.b).c(...)`) |

`Int`/`Double` operands mix freely (`1 + 2.5`); the result is `Double`
whenever either operand is. `Int` arithmetic wraps on overflow, matching
Kotlin's `Int` semantics.

## Control flow

`if`, `when`, and bare `{ }` blocks are **expressions** — they evaluate to
the value of their last expression statement (or `Unit` if their last
statement isn't a bare expression) — so they can be used as statements or
assigned directly:

```kotlin
val label = if (score > 50) "pass" else "fail"
```

An `if`/`when` branch may be either a `{ ... }` block or, without braces,
a single expression.

### `if`

```kotlin
if (cond) { ... } else { ... }
if (cond) "yes" else "no"
```

### `when`

Subject form (`==` against the subject) and subject-less form (each
condition is a plain `Boolean` expression):

```kotlin
when (x) {
    1 -> "one"
    2, 3 -> "two or three"
    else -> "other"
}

when {
    x < 0 -> "negative"
    x == 0 -> "zero"
    else -> "positive"
}
```

If no arm matches and there's no `else`, the `when` evaluates to `Unit`.

### `while`

```kotlin
while (cond) {
    ...
}
```

### `for`

Iterates an inclusive `Int` range or a `List` (e.g. from `listOf(...)`):

```kotlin
for (i in 1..5) { println(i) }
for (item in listOf("a", "b")) { println(item) }
```

### `break` / `continue` / `return`

`break` and `continue` behave as expected inside the body of the nearest
`while`/`for` loop. Using either outside a loop, or inside a `fun`/lambda
body that is merely lexically nested inside a loop (they can't unwind
through a function boundary to a loop in the caller), is a parse error,
matching Kotlin's compile-time rejection of the same cases.

`return` exits the enclosing function with a value (`return expr`) or
`Unit` (bare `return`). At the top level of a script, `return` simply
stops the script.

## Functions

```kotlin
fun add(a: Int, b: Int = 10): Int = a + b        // expression body
fun greet(name: String): String {                 // block body
    return "Hello, $name!"
}

add(1)                 // 11 (uses the default for b)
add(1, b = 2)           // 3, named argument
add(a = 1, b = 2)       // named arguments can be given in any order
```

- Parameters may have a `: Type` annotation (see [Types](#types)) and a
  default value. A positional argument's value is checked against its
  parameter's annotation, when both are of a checkable type.
- Positional and named arguments can be mixed; a named argument must
  match a declared parameter name. **Named arguments are not
  type-checked** (checked positionally only, for now — see
  [Types](#types)).
- A declared `: ReturnType` after the parameter list is checked against
  what the function actually returns.
- Functions declared with `fun` inside another function's body are
  closures over that enclosing scope, and can be recursive.

## Lambdas / closures

```kotlin
val double = { x -> x * 2 }
val add = { a, b -> a + b }
val square = { it * it }          // implicit `it` — only when called with exactly one argument

fun apply(f, x: Int): Int = f(x)
apply(square, 5)                   // 25
```

A lambda captures the environment that was active where it was written
(true lexical closures), not the caller's environment. There is no
trailing-lambda call syntax (`list.map { ... }`) — lambdas are passed and
invoked like any other value: `f(x)`.

## Imports

```kotlin
@file:Import("helpers/greeter.ksl")

println(greet("World"))
```

`@file:Import("path.ksl")` loads another local `.ksl` file and runs its
top-level code — its `fun`/`val` declarations become available to the
importing script, as if the imported file's contents ran first in the same
global scope (or, if the imported file declares a `package`, in that
package's own scope — see below).

- File annotations must appear before any other statement, at the very top
  of the file. Only `Import` is recognized (`@file:Import(...)`) — any other
  `@file:...` form, or an `@` anywhere else, is a parse error.
- The import path must be a plain string literal — no `${...}` templates.
- A relative path resolves against the *importing file's own* directory,
  not the top-level script's directory. Absolute paths are used as-is.
- The same file imported multiple times (directly or transitively) only
  runs once. An import cycle is a runtime error naming the cycle.
- Only local files are supported right now — `@file:Import("https://...")`
  is a clear "not yet supported" error rather than a working network fetch.

### Packages

A file can optionally declare a package as the very first thing in it:

```kotlin
package util.math

fun square(x: Int): Int = x * x
```

A file with no `package` declaration behaves exactly as before: its
top-level declarations land in one flat global scope, shared with every
other package-less file (via `@file:Import`) and the entry script itself.

A file *with* a `package` declaration namespaces its top-level declarations
under that dotted name instead — invisible as bare names to code outside
the package, but visible (as bare names) to any other file sharing the same
`package`, the same way package-less files already share the global scope.

Two ways to use something from another package, once the file declaring it
has been `@file:Import`ed:

```kotlin
@file:Import("mathutil.ksl")   // declares `package util.math`

println(util.math.square(5))   // qualified access, no import needed
```

```kotlin
@file:Import("mathutil.ksl")
import util.math.square        // brings `square` into this file's own scope

println(square(5))
```

- `import a.b.c` (like `@file:Import`, only legal before any other
  statement) never loads a file by itself — it only aliases a name that
  some already-`@file:Import`ed file's `package` declaration put there.
  Using it, or a bare `a.b.c` reference, before that file has actually been
  imported is a runtime error (`unknown package 'a.b'`).
- `a.b.c` qualified-name syntax (package `a.b`, member `c`) works in any
  expression position without needing `import` first.

`a.b.c` in expression position is genuinely ambiguous between a
package-qualified name and `.` member access on a value (see "Methods"
below) — there's no parser-level way to tell `a` is a package rather than
a local, so this is resolved at evaluation time: `a` is tried as a local
first, and only treated as a package name if it isn't one.

## Methods

Some values support `.` method-call syntax, e.g. `"hi".uppercase()` —
equivalent to, and currently implemented in terms of, the same-named
plain stdlib function (`uppercase("hi")`). See
[`stdlib.md`](stdlib.md#methods) for the full list. `.field` access with
no call is also legal syntax, but no value has real fields yet (that
needs classes/objects, not yet implemented) — it's always a runtime
error until then.

## Not supported

Deliberately out of scope for now: classes/objects, inheritance,
generics, null-safety operators (`?`, `?.`, `?:`, `!!`), list/map
literals, `try`/`catch`, coroutines, extension functions, and network
imports. `.` method syntax only covers the small builtin set in
[`stdlib.md`](stdlib.md#methods) — arbitrary field/method access on a
value needs classes/objects first.
