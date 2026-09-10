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

### Escape sequences

`\n` `\t` `\r` `\\` `\"` `\$`, plus a variable-length Unicode escape
`\u{XXXX}` (1-6 hex digits) for any other codepoint, including control
characters no other escape can produce — `"\u{1b}"` is ESC (`0x1b`),
`"\u{2764}"` is `❤`:

```kotlin
println("\u{1b}[2J")   // ESC [ 2 J — an ANSI "clear screen" sequence
```

## Variables

```kotlin
val x = 10          // immutable
var y: Int = 20      // mutable, with an optional type annotation
y = y + 1
y += 5               // += -= *= /= %= are also supported
y++                  // postfix increment/decrement — evaluates to the old value
++y                  // prefix — evaluates to the new value
```

`++`/`--` are sugar over `y = y + 1`/`y = y - 1` (so they work on `Int`
and `Double` locals the same way `+=`/`-=` do) and, like `=`/`+=`, only
apply to a bare variable name — not yet a `.field` or `[i]` target, since
neither exists as a settable place yet.

Reassigning a `val` is a runtime error. A type annotation (`Int`, `Double`,
`Boolean`, `String`, `Unit`, or `List`) is checked against the
initializer's value, and — for a `var` specifically — against every later
reassignment too. A `var` with **no** annotation is *not* exempt from this:
its type is inferred from the initializer instead (`var y = 20` infers
`Int` exactly as `var y: Int = 20` does) and enforced the same way from
then on. Any other annotation name (a typo, a not-yet-supported type) is
rejected outright as an unknown type. See [Types](#types) below for where
this checking happens and how far it goes.

## Types

ksl's type checking is deliberately strict, matching Kotlin: a value
without an explicit or inferable type is always an error, not a silent
"dynamic" fallback. It runs both ahead of time (static analysis, powering
`ksl-lsp` and gating the `ksl` CLI before a script runs at all) and again
at runtime inside the interpreter itself.

**Type-committing sites — always either a real type or an error:**

- **Every `val`/`var` has a type, annotation or not.** `var count: Int = 0`
  and `var count = 0` end up with the exact same type — `Int` — the second
  form just *infers* it from the initializer instead of reading it off an
  annotation. Either way, that type is then enforced on every later
  reassignment: `count = "hi"` is a type mismatch regardless of which form
  declared `count`. If neither an annotation nor the initializer's type can
  be worked out (e.g. the initializer is itself an uninferable call), that's
  an error — `cannot infer type of 'count'; add an explicit type
  annotation` — never a silent "untyped" fallback.
- **A named `fun`'s parameters always require an explicit annotation** —
  `fun apply(f, x: Int)` is rejected (`parameter 'f' must have an explicit
  type annotation`), even though the parameter's type might seem inferable
  from how it's used in the body. A `fun`'s *return* type, unlike its
  parameters, may be omitted — it's then inferred from the body (every
  `return` plus, when nothing unconditionally returns, the tail
  expression), reconciled into one type; disagreement (`return 1` and
  `return "x"` in the same function) is an error.
- **A lambda's parameters are inferred from context, not required to carry
  an annotation** — `{ x -> x * 2 }` gets `x`'s type from the function type
  its surrounding context expects: a `val`/`var`'s own annotation
  (`val double: (Int) -> Int = { x -> x * 2 }`) or the matching parameter's
  declared type at a call site (`apply({ x -> x * 2 }, 5)`, where `apply`'s
  first parameter is `(Int) -> Int`). If no such context exists, that's an
  error — `cannot infer type of lambda parameter 'x'; add an explicit
  expected function type` — same for the implicit `it` parameter. A
  lambda's return type is always inferred from its body the same way an
  unannotated `fun`'s is; a lambda's own type is always a real function
  type, e.g. `(Int) -> Int`, never left unresolved.

**Immediate errors, regardless of context:**

- A binary/unary operation on two concretely-known but incompatible types
  (`true + false`) — never deferred, even mid-expression.
- Calling a method that doesn't exist for a concretely-known receiver type
  (`5.uppercase()`) or accessing a field on a real local value (nothing has
  fields until classes exist).
- Mismatched `if`/`when` branch types where every branch is a primitive
  **and there's a real, explicit `else`/`else ->`** (`if (b) 1 else
  "two"`) — there's no common supertype to join to without a class
  hierarchy, so this is an error, not a silent `Unknown`. A branch that's
  a class/generic/unresolvable type still joins leniently for now
  (deferred until classes exist). An **absent** `else` never triggers
  this, no matter what the present branch's type is — `if (true) 10` (no
  `else`, e.g. used as a statement with the value discarded, or assigned
  to a hint-less `val` that then just can't rely on a specific type) is
  not a mismatch: the implicit Unit is only what the whole expression
  evaluates to when no branch is taken (see "Control flow" below), not a
  second real value the present branch is obligated to agree with. This
  hard error is a placeholder for real least-upper-bound inference (e.g.
  `if (b) 1 else "two"` inferring `Comparable<*>`, matching real Kotlin)
  — planned once interfaces and statically-parameterized generics exist,
  see `docs/roadmap.md`'s Phase 6.5/9.
- A named argument whose type doesn't match its parameter's declared type,
  or that names a parameter the callee doesn't have.

**What's still deferred**: anything whose type genuinely can't be worked
out without running the script and isn't at one of the sites above — most
commonly the result of a package-qualified call (`util.math.square(5)`), or
anything derived from one (cross-file type inference is planned but not
implemented yet). These stay `Unknown` and are never a false rejection; but
note this only applies statically — at runtime every value is concrete, so
a `var` initialized from such a call still gets locked to whatever type
that call's result actually turns out to be, checked from then on (see
"Where the check happens" below for why the same script can behave
slightly more leniently in an editor's live diagnostics than it does when
actually run).

**An annotation that doesn't name a real type is itself an error** — `val
x: Sting = 1` (a typo) or naming a feature this interpreter doesn't have
yet (a class, before classes exist) is rejected as `unknown type 'Sting'`,
not silently accepted, at exactly the position of the bad name (a bad name
buried inside a function type, e.g. `(Sting) -> Int`, is flagged the same
way, at its own position).

**`Int`/`Double` mixing is not the same rule as arithmetic.** `1 + 2.0`
freely produces a `Double` (see "Operators" below) — but assigning,
passing, or returning an `Int` where a `Double` is explicitly expected (or
vice versa) *is* a type mismatch. The two rules don't interact.

**Where the check happens, twice**: the same checking runs statically (as
you type, in an editor connected to `ksl-lsp`, and once more when the `ksl`
CLI loads a script, *before* running it — a type problem here means the
script never runs at all, not even its first line) and again at runtime
inside the interpreter itself (so embedding `ksl-core` directly, without
going through the CLI's pre-check, still can't skip it). The runtime check
is strictly more precise (it always has a concrete value to work from,
never "can't tell without running it"), so it can catch a mismatch the
static pass couldn't have. A type-mismatch error looks like `type
mismatch: expected Int, got String`; an unrecognized annotation looks like
`unknown type 'Sting'`.

**Type annotation syntax**: a plain name — one of the 6 recognized
primitives, `Int`, `Double`, `Boolean`, `String`, `Unit`, `List` (no
class/generic types exist yet, see "Not supported" below), plus Kotlin's
real top and bottom types, `Any` (supertype of everything — anything is
assignable *into* `Any`, but not back out to a concrete type without an
explicit cast, which ksl doesn't have yet) and `Nothing` (subtype of
everything — assignable to anything, including a nullable type; the type
of `throw`/a call that never returns normally, once `docs/roadmap.md`'s
Phase 5 lands `throw`) — or a function type, `(ParamType, ...) ->
ReturnType`, e.g. `(Int, String) -> Boolean` or `() -> Unit`. Function
types nest (`((Int) -> Int) -> String`) and are compared structurally:
same parameter count, each parameter and the return type individually
compatible — no named function-type declaration to match by identity.
`Nothing` is also the identity element for `if`/`when` branch-type joins
(`LUB(Nothing, T) = T`) — a never-returning branch (e.g. `throw`, once
Phase 5 lands) never causes a "branches have incompatible types" error,
the whole expression just takes the other branch's type.

Any of the above — a primitive, or a function type — may be suffixed with
`?` to make it nullable, e.g. `String?`, `(Int) -> String?` (nullable
*return* type), `((Int) -> String)?` (the *whole* function type is
nullable, parens required to attach `?` to it rather than just its return
type — the same disambiguation real Kotlin uses). See [Null
safety](#null-safety) below.

## Operators

Listed loosest-to-tightest binding (i.e. `a || b && c` parses as
`a || (b && c)`):

| Precedence | Operators | Notes |
|---|---|---|
| 1 (loosest) | `\|\|` | short-circuits |
| 2 | `&&` | short-circuits |
| 3 | `==` `!=` | structural equality |
| 4 | `<` `>` `<=` `>=` | numeric or lexicographic `String` comparison |
| 5 | `?:` | elvis — short-circuits, doesn't evaluate the right side unless the left is `null`; right-associative |
| 6 | `..` | inclusive `Int` range, e.g. `1..5` |
| 7 | `+` `-` | `+` also concatenates `String` (`"x" + 1`) and `List` |
| 8 | `*` `/` `%` | integer division/remainder by zero is a runtime error |
| — | unary `-` `+` `!` `++` `--` | prefix, tightest, right-associative — `++x`/`--x` mutate `x` in place and evaluate to the *new* value |
| — | `f(...)`, `.field`, `.method(...)`, `?.field`, `?.method(...)`, `!!`, `++` `--` | postfix call/member-access/null-safety/increment/decrement, tightest of all, left-associative (`a.b.c()` is `(a.b).c(...)`) — `x++`/`x--` mutate `x` in place but evaluate to the *old* value |

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

An `if`/`when` branch, or a `while`/`for` loop's own body, may be either a
`{ ... }` block or, without braces, a single **statement** — not just an
expression, so `return`/`break`/`continue` work in this position with no
braces needed, same as real Kotlin:

```kotlin
if (cond) { ... } else { ... }
if (cond) "yes" else "no"
if (x < 0) return "negative"       // a bare `return`, no braces
if (x <= 2) continue               // likewise `break`/`continue`, inside a loop
```

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
while (cond) i = i + 1   // braceless single-statement body, same as if/when
```

### `do` / `while`

```kotlin
do {
    ...
} while (cond)
do i = i + 1 while (cond)   // braceless single-statement body, same as while
```

The body always runs at least once — `cond` is only checked *after* it,
unlike `while`. `cond` is resolved/evaluated in the body's own scope, not
the enclosing one, matching real Kotlin/Java: `do { val x = f() } while (x
> 0)` can see `x`.

### `for`

Iterates an inclusive `Int` range or a `List` (e.g. from `listOf(...)`):

```kotlin
for (i in 1..5) { println(i) }
for (item in listOf("a", "b")) { println(item) }
for (i in 1..5) println(i)   // braceless single-statement body
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

## Null safety

```kotlin
val name: String? = null       // T? — a nullable type annotation
println(name?.uppercase())     // ?. safe call — null instead of erroring
println(name?.uppercase() ?: "NONE")   // ?: elvis — fallback when the left side is null
println(name!!)                // !! not-null assertion — errors on null
```

- **`null`** is a literal, of its own type (`Null`). `x == null` is always
  legal and never a type error, whatever `x`'s type is.
- **`T?`** — any type suffixed with `?` — accepts either a `T` value or
  `null`. Enforced both statically and at runtime, the same way every
  other type rule is (see [Types](#types)): `val x: String = null` is
  rejected as a type mismatch; `val x: String? = null` is fine.
- **`?.`** (safe call) evaluates to `null`, without evaluating the member
  or any of its arguments, when the receiver is `null`; otherwise it
  behaves exactly like `.`. A chain of safe calls (`a?.b?.c`) short-circuits
  as soon as any link is `null`, with no special chain-wide handling needed
  — each `?.` step only needs its own immediate receiver.
- **`?:`** (elvis) evaluates its right side only when the left side is
  `null` — the right side is a plain fallback value or expression,
  evaluated lazily (`x ?: computeDefault()` never calls `computeDefault()`
  when `x` isn't null).
- **`!!`** (not-null assertion) is a postfix operator: `x!!` passes `x`
  through unchanged if it isn't `null`, and errors with `null cannot be
  cast to non-null type` if it is.

**Unsafe access on a nullable receiver is a compile-time error, unless
smart-cast applies.** `val x: String? = null; x.length()` is rejected
outright — `only safe (?.) or non-null asserted (!!) calls are allowed on
a nullable receiver of type 'String?'` — matching Kotlin's own compiler
error, and never a confusing runtime "has no method" message.

**Smart-cast narrows a `val`/parameter's type after a null check**, the
same way Kotlin's does, for these condition shapes:

```kotlin
fun describe(s: String?): String {
    if (s == null) {
        return "unknown"          // early exit narrows `s` for the rest of the function
    }
    return s.uppercase()          // `s` is smart-cast to `String` here — no ?./!! needed
}

fun greet(s: String?) {
    if (s != null) {
        println(s.uppercase())    // narrowed inside the `then` branch
    }
    // `s` is back to `String?` here — the narrowing doesn't outlive the `if`
}

println(s != null && s.length() > 0)   // narrows `s` for the right side of `&&`
println(s == null || s.length() > 0)   // narrows `s` for the right side of `||`
while (s != null) { println(s.length()) }   // narrows `s` inside the loop body
```

Recognized: `x != null`/`x == null` (either operand order), combined
through `&&`, `||`, and unary `!`; `if`/`else` branches; carry-forward past
an `if` whose taken branch always `return`s/`break`s/`continue`s; `&&`/`||`
short-circuit (the right operand sees the left operand's implied
narrowing); `while` loop bodies; and a subject-less `when` arm with a
single condition (`when { s != null -> ... }`).

**Restricted to `val`s and parameters** — never `var`, matching Kotlin's
own rule (a mutable local could change between the check and the use, so
narrowing it wouldn't be sound), and never a method-call result or a field
(only a bare name is a "stable reference"). This is still best-effort, not
full Kotlin-grade flow analysis: **hover doesn't show the narrowed type**
(it shows the declaration's own `T?`, not the smart-cast `T`), and
`val y = x ?: return` (Kotlin's own idiom for narrowing `x` via an early
return through elvis) isn't recognized — moot for now, since ksl's grammar
has no `return`/`break`/`continue` in expression position at all, so that
exact form can't be written yet either.

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

- A named `fun`'s parameters always require a `: Type` annotation (see
  [Types](#types)) — `fun apply(f, x: Int)` is rejected, `f` needs one too.
  A parameter may also have a default value.
- Both positional and named arguments are checked against their matching
  parameter's declared type; a named argument that names a parameter the
  function doesn't have is also an error.
- Positional and named arguments can be mixed; a named argument must
  match a declared parameter name.
- A declared `: ReturnType` after the parameter list is checked against
  what the function actually returns; if omitted, the return type is
  inferred from the body instead (see [Types](#types)) — never left
  unresolved.
- Functions declared with `fun` inside another function's body are
  closures over that enclosing scope, and can be recursive.

### Overloading

```kotlin
fun describe(x: Int): String = "int $x"
fun describe(x: String): String = "string $x"

println(describe(5))     // "int 5"
println(describe("hi"))  // "string hi"
```

Several `fun`s sharing one name are legal as long as their parameter types
make them distinguishable — matching real Kotlin, this is real overload
resolution by the call's actual argument types (arity first, then an exact
type match, then ordinary assignability), not name-mangling or a
first-match-wins list; picking a stdlib function (like `abs`/`max`/`min`)
between its own overloads uses the exact same mechanism. A call whose
argument types don't match *any* declared overload — wrong arity, or no
overload accepts the given types — is a compile-time error, same as any
other type mismatch. Two `fun`s sharing a name with the *exact* same
parameter types (not just the same arity) is a compile-time
"conflicting overload" error, distinct from redeclaring a plain `val`/`var`
name (still "already declared").

## Lambdas / closures

```kotlin
val double: (Int) -> Int = { x -> x * 2 }
val add: (Int, Int) -> Int = { a, b -> a + b }
val square: (Int) -> Int = { it * it }   // implicit `it` — only when called with exactly one argument

fun apply(f: (Int) -> Int, x: Int): Int = f(x)
apply(square, 5)                   // 25
```

A lambda's parameter types (and the implicit `it`'s) are inferred from the
function type its surrounding context expects — the enclosing `val`/`var`'s
own annotation, as above, or the matching parameter's declared type at a
call site (`apply({ x -> x * 2 }, 5)` needs no annotation on `double`
itself, since `apply`'s first parameter already says `(Int) -> Int`). With
no such context, an unannotated parameter's type can't be inferred and is
an error (see [Types](#types)). A lambda captures the environment that was
active where it was written (true lexical closures), not the caller's
environment.

**Trailing-lambda call syntax** — `foo { ... }`/`obj.method { ... }` — a
lambda argument written right after the callee, with no comma before it.
With no other arguments, the parentheses can be omitted entirely; with
other arguments already in `(...)` (empty or not), the lambda is appended
to that same call as one more argument, same as real Kotlin:

```kotlin
fun withResult(body: () -> Int): Int = body()
withResult { 41 + 1 }              // 42, same as withResult({ 41 + 1 })

class Counter(var n: Int) {
    fun update(f: (Int) -> Int) { n = f(n) }
}
Counter(5).update { it + 10 }      // 15

fun withLog(label: String, body: () -> Unit) { println(label); body() }
withLog("start") { println("ran") }   // same as withLog("start", { println("ran") })
```

Purely parser sugar for `foo({ ... })`/`foo(a, b, { ... })` — identical
checking and evaluation either way. A trailing lambda only merges with a
call immediately to its left, on the same line (or right after that
call's own closing `)`) — `foo()\n{ ... }`, with a real newline in
between, parses as two separate statements instead, same as without this
syntax at all.

A lambda passed where a `() -> Unit` (or any `Unit`-returning function
type) is expected doesn't need its own body to actually produce `Unit` —
`runIt { someCall() }` against `fun runIt(block: () -> Unit)` works even
when `someCall()` returns something else; the value is simply discarded,
matching real Kotlin's function-literal Unit-coercion.

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
- `import a.b.*` (wildcard) brings in *every* top-level name package `a.b`
  declares, instead of just one — otherwise the same rules as above
  (must appear before any other statement, the package must already be
  loaded/known, `import a.b.*` before that is `unknown package 'a.b'`).
- `a.b.c` qualified-name syntax (package `a.b`, member `c`) works in any
  expression position without needing `import` first.

**Every stdlib name lives in a real, named package** — `println` is really
`kotlin.io.println`, `assertEquals` is really `kotlin.test.assertEquals`,
and so on (see [`stdlib.md`](stdlib.md) for the full list) — even though
no file ever declares any of these `package`s; they're synthetic, seeded
directly rather than parsed from a `.ksl` file, but otherwise behave
exactly like a real user-declared package: both `import` forms above work
on them, and so does a fully-qualified call
(`kotlin.io.println(...)`/`kotlin.test.assertEquals(...)`) with no import
at all.

The only thing that varies per package is whether it's **auto-imported**:
`kotlin.io`, `kotlin.collections`, and `kotlin.text` are (matching real
Kotlin's own default-import list), so their members also work completely
unqualified, no `import` needed. `kotlin.math` and `kotlin.test` are
**not** auto-imported (matching real Kotlin exactly, where neither is on
that list either) — calling one of their members bare, with no
`import kotlin.math.*`/`import kotlin.test.*` (or a per-name import) in
scope and no `kotlin.math.`/`kotlin.test.` qualification, is `undefined
name`, the same as referencing any other name that needs an import first.
`kotlin.test` in particular is what lets real Kotlin code that writes
`import kotlin.test.*` (a common convention, including in JetBrains's own
compiler test suite — see `docs/roadmap.md`'s Phase 0.5) run under ksl
unmodified. Importing a member ksl doesn't actually implement (e.g.
`kotlin.test.assertFailsWith`, which needs `throw` — see "Not supported")
is likewise a plain `undefined name` error.

`a.b.c` in expression position is genuinely ambiguous between a
package-qualified name and `.` member access on a value (see "Methods"
below) — there's no parser-level way to tell `a` is a package rather than
a local, so this is resolved at evaluation time: `a` is tried as a local
first, and only treated as a package name if it isn't one.

Every form above (`import a.b.c`, `import a.b.*`, `@file:Import`'s
package-less flat namespace, and a bare qualified call) is checked
**statically** too, not just at runtime, with the exact same rigor a local
declaration gets: an unresolved package or member is a real diagnostic at
the import/call site (`unknown package '...'`/`undefined name '...'`), and
a resolved one gets real argument-count/type checking against its actual
signature — an editor sees this live, before the script ever runs.

## Classes

```kotlin
class Point(val x: Int, val y: Int) {
    var visits: Int = 0

    init {
        visits = visits + 1
    }

    fun distanceSquared(other: Point): Int {
        val dx = x - other.x
        val dy = y - other.y
        return dx * dx + dy * dy
    }

    fun toString(): String = "(" + x + ", " + y + ")"
}

val p = Point(1, 2)
println(p)             // (1, 2) -- via the overridden toString()
println(p.x)            // 1
println(p.distanceSquared(Point(4, 6)))
```

`class Name(...)` declares a **primary constructor** right in the header,
same as real Kotlin — parentheses (and the body's own `{ }`) are optional:
`class Empty` and `class Empty()` are both legal, empty classes. Each
constructor parameter is either:

- **promoted to a real field** by writing `val` or `var` in front of it
  (`val x: Int`) — readable (and, for `var`, writable) via `this.x`/
  `obj.x` from anywhere the object is visible;
- a **plain constructor parameter** (no `val`/`var`) — only a real binding
  *inside the constructor itself* (property initializers and `init`
  blocks), not a field; it doesn't exist on the object afterward.

A class body can contain, in any order, any number of:

- **properties** — `val`/`var name: Type = initializer` (the initializer
  is required; every property is either a promoted ctor parameter or has
  its own, with no third option);
- **`init { ... }` blocks** — arbitrary statements, run as part of
  construction;
- **methods** — ordinary `fun` declarations.

Properties and `init` blocks run **in the order they're written**,
interleaved — matching real Kotlin's initialization order — so a later
property's initializer (or an `init` block) can read an earlier one's
already-assigned value via `this.`.

**Implicit `this`.** Matching real Kotlin: a bare name inside a property
initializer, `init` block, or method body — anywhere in the class — that
isn't a local variable/parameter resolves to `this`'s own field or method
of that name, no `this.` prefix needed. `this.` is only ever required to
disambiguate, exactly like real Kotlin's own rule: **a local variable or
parameter always shadows a same-named field** — inside `fun f(x: Int) {
... }`, a bare `x` means the parameter, and reaching the field (if there
even is one, and it's a different name) needs an explicit `this.x`. The
**primary constructor's own (non-promoted) parameters** are visible bare
*only inside the constructor's own scope* (property initializers, `init`
blocks) — they're not fields, so there's no `this.`-qualified way to reach
them from an ordinary method at all; only a promoted (`val`/`var`)
parameter is visible everywhere, via the same implicit-`this` rule as any
other field.

**Construction** is an ordinary call — `Point(1, 2)`, or with named
arguments (`Point(y = 2, x = 1)`), same rules as calling a function.

**Every class implicitly extends `Any`** — the real root of the type
hierarchy — and inherits three real, overridable methods from it:
`toString(): String`, `equals(other: Any?): Boolean`, and
`hashCode(): Int`. Their default behavior (when a class doesn't override
them) is **identity-based**, matching Kotlin: `toString()` shows a
`ClassName@address`-shaped string, `equals()`/`hashCode()` compare/hash by
object identity, not by field contents. Overriding `toString()` (as
`Point` does above) is picked up automatically by `println`/string
interpolation (`"$p"`), not just an explicit `p.toString()` call.

The `==` operator itself is unaffected by an `equals()` override — for an
object, `==` always compares identity directly (same object or not),
mirroring Kotlin's default `==` behavior for a class with no
`equals()`/`hashCode()` pair overridden. Numbers, strings, booleans, and
`Int`/`Double` (via `Number`) also support `.toString()`/`.equals()`/
`.hashCode()` explicitly (`5.toString()`, `"x".equals("x")`), with the
expected structural (not identity) behavior for those.

**Not supported yet**: `:` superclass syntax, `open`/`override`,
`interface`, generic classes (`class Box<T>`), and collection indexing
syntax (`obj[i]`) even on a user class — all separate, later phases.

## Methods

Some values support `.` method-call syntax, e.g. `"hi".uppercase()` —
equivalent to, and currently implemented in terms of, the same-named
plain stdlib function (`uppercase("hi")`) — plus every class's own
methods (see [Classes](#classes) above) and every value's inherited
`toString()`/`equals()`/`hashCode()`. See [`stdlib.md`](stdlib.md#methods)
for the full builtin list. `.field` access with no call is also legal
syntax — real on a class instance (again, see [Classes](#classes)); a
runtime error on anything else, which has no fields.

## Not supported

Deliberately out of scope for now: interfaces, inheritance (`:`
superclass syntax, `open`/`override`), generics (including generic
classes), list/map literal syntax (`[1, 2, 3]` isn't real Kotlin syntax
either — build a list with [`listOf(...)`](stdlib.md#listof); a `{k: v}`
map literal doesn't exist in real Kotlin at all), collection indexing
(`list[i]`, including on a user class) — deferred until generics,
properties, and `operator fun get`/`set` exist, so it can be real
operator overloading rather than a special case — `try`/`catch`,
coroutines, extension functions, and network imports.
