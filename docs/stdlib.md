# Standard library reference

Every built-in lives in a real, named package, exactly matching real
Kotlin — `println` is really `kotlin.io.println`, `listOf` is really
`kotlin.collections.listOf`, and so on. A handful of packages
(`kotlin.io`, `kotlin.collections`, `kotlin.text`) are **auto-imported**
into every script, so their members work unqualified with no `import` at
all, e.g. `length(s)` — a handful of those also have a `.` method-call
form (`s.length()`) — see [Methods](#methods) below. `kotlin.math` and
`kotlin.test` are **not** auto-imported (matching real Kotlin exactly),
so their members need an explicit `import` or a fully-qualified call —
see [Math](#math) and [Assertions](#assertions) below. A fully-qualified
call (`kotlin.io.println(...)`) always works too, imported or not. See
[`syntax.md`](syntax.md) for the language itself.

A small number of ksl-only extensions with **no real Kotlin stdlib
equivalent** (terminal/game utilities) live under a `ksl.*` namespace
instead of `kotlin.*` — flagged as such in [Terminal](#terminal) below,
not real Kotlin.

Errors thrown by these functions are runtime errors like any other — they
stop the script and print `runtime error: <message> at <line:col>` to
stderr, `<line:col>` being the actual call site, followed by a stack
trace — one `    at <function>(<params>) (<file>:<line>:<col>)` line per
active call, innermost first, each crediting the file the function
printed there was actually declared in (not necessarily the file that
called it).

## Output

Lives in `kotlin.io`, auto-imported (see the intro above) — also callable
as `kotlin.io.println(...)`/`kotlin.io.print(...)`/`kotlin.io.flush()`.

### `println(value?)`

Prints `value` followed by a newline, or just a newline if called with no
argument. Any value type is accepted; it's converted via its default
string representation (see [Value formatting](#value-formatting) below).

```kotlin
println("hi")     // hi
println(42)        // 42
println()          // (blank line)
```

### `print(value)`

Like `println`, but without the trailing newline. Requires exactly one
argument.

### `flush()`

Forces any buffered `print()`/`println()` output out to the terminal
right now. Not part of real Kotlin's portable `kotlin.io` (the JVM has
`System.out.flush()`, but common Kotlin has no cross-platform
equivalent) — added here because it's needed: stdout is line-buffered,
so a `print()` with no trailing newline (drawing a game frame with no
final `println()`, say) can otherwise sit unseen until something else
later happens to flush it.

```kotlin
print("Loading")
flush()          // shows immediately, even with no newline
```

### `readLine(): String?`

Reads one line from stdin (without the trailing newline), or `null` at
end of input — exactly real Kotlin's own `readLine()`.

```kotlin
val name = readLine()
println("Hello, " + name)
```

## Lists

Lives in `kotlin.collections`, auto-imported — also callable as
`kotlin.collections.listOf(...)`.

### `listOf(vararg values)`

Builds a `List` from any number of arguments (including zero).

```kotlin
val xs = listOf(1, 2, 3)
for (x in xs) { println(x) }
```

Lists are also produced by an inclusive `Int` range, e.g. `1..5` — see
[`syntax.md`](syntax.md#for). `+` concatenates two lists.

### Methods

`List` is mutable — these are `.`-only, matching real Kotlin's own
`MutableList` (no bare-function `kotlin.collections.get(...)` form exists,
same as real Kotlin):

```kotlin
val xs = listOf(1, 2, 3)
xs.add(4)              // [1, 2, 3, 4]
xs.set(0, 99)           // [99, 2, 3, 4]
println(xs.get(0))      // 99
println(xs.size())      // 4
println(xs.removeAt(0)) // 99 — and xs is now [2, 3, 4]
```

- `get(index: Int): Any` — the element at `index`. Throws if out of bounds.
- `set(index: Int, value: Any?)` — replaces the element at `index`. Throws if out of bounds. Returns `Unit`, unlike real Kotlin's `MutableList.set` (which returns the replaced element) — a ksl-only simplification.
- `add(value: Any?)` — appends `value`, growing the list by one. Returns `Unit`, unlike real Kotlin's `MutableList.add` (which returns `Boolean`) — a ksl-only simplification.
- `size(): Int` — the element count. A method (`xs.size()`), not a bare property (`xs.size`) — same divergence `String.length()` already has, see [Methods](#methods-1) below.
- `removeAt(index: Int): Any` — removes and returns the element at `index`, shifting later elements down by one. Throws if out of bounds.

Not `[]` indexing syntax (`xs[0]`) — that's a separate, larger, still-unimplemented feature (see `docs/roadmap.md`'s Phase 3); these are ordinary method calls only, for now.

## Strings

Live in `kotlin.text`, auto-imported — also callable as
`kotlin.text.length(...)`/`kotlin.text.uppercase(...)`/etc.

### `length(s: String): Int`

Number of characters in `s`.

```kotlin
length("hello")   // 5
```

### `uppercase(s: String): String` / `lowercase(s: String): String`

```kotlin
uppercase("kotlin")   // "KOTLIN"
lowercase("KOTLIN")   // "kotlin"
```

### `trim(s: String): String`

Removes leading and trailing whitespace.

```kotlin
trim("  hi  ")   // "hi"
```

## Methods

`length`, `uppercase`, `lowercase`, and `trim` above are also callable as
`.` methods on a `String` receiver — `s.uppercase()` is exactly
`uppercase(s)`, just written the other way round:

```kotlin
println("hello".length())     // 5
println("kotlin".uppercase())  // "KOTLIN"
println("  Loud  ".trim().uppercase())   // "LOUD" — methods chain
```

`List` has its own `.`-only methods too — see [Lists → Methods](#methods)
above, which unlike these have no bare-function form at all (`get`/`set`/
`add`/`size()` are never callable as plain `get(xs, 0)`, matching real
Kotlin's own `MutableList`). Every user-declared `class` has real
fields/methods too (`docs/syntax.md`'s "Classes"). Nothing else has
methods — a bare `receiver.name` is otherwise
ambiguous with a package-qualified reference (`util.math.square`, see
`syntax.md`'s "Packages"); this is resolved by trying `receiver` as a
local value first and only falling back to a package lookup if it isn't
one — so a local variable always wins over a same-named package.

## Math

Lives in `kotlin.math`, which — matching real Kotlin exactly — is **not**
auto-imported: needs `import kotlin.math.*` (or a per-name import, or a
fully-qualified `kotlin.math.abs(...)` call) before any of the functions
below work:

```kotlin
abs(-5)              // undefined name 'abs'

import kotlin.math.*
abs(-5)               // OK
kotlin.math.abs(-5)   // also OK, with or without the import
```

Kotlin has no `Int | Double`-style union-type syntax — each function below
is really **overloaded** (two separate signatures sharing one name, same
as real Kotlin), not one signature with a union type:

### `abs(x: Int): Int` / `abs(x: Double): Double`

Absolute value; the result type matches the argument's type.

```kotlin
abs(-5)      // 5
abs(-3.5)    // 3.5
```

### `sqrt(x: Int): Double` / `sqrt(x: Double): Double`

```kotlin
sqrt(16)     // 4.0 (printed as "4", see below)
```

The `Int` overload is a ksl-only convenience — real Kotlin's
`kotlin.math.sqrt` has no `Int` overload at all (`sqrt(16)` there needs
`sqrt(16.toDouble())`).

### `max(a: Int, b: Int): Int` / `max(a: Double, b: Double): Double` (and `min`, same shape)

```kotlin
max(3, 7)       // 7
min(3.5, 2.0)   // 2.0 (printed as "2")
```

Matching real Kotlin exactly — `Int` and `Double` never mix in one call
without an explicit conversion, so `min(3.5, 2)` (an `Int` where the
`Double` overload needs a `Double`) is a type error, exactly like calling
any other function with the wrong argument type. (An earlier ksl-only
convenience silently widened a mixed call to `Double` at runtime instead;
it was retired once overload resolution started picking a real, declared
overload by actual argument type — same mechanism for every stdlib
function, not a numeric-specific special case.)

## Random

Lives in `kotlin.random`, same story as `kotlin.math` above — **not**
auto-imported: needs `import kotlin.random.*` (or a per-name import, or a
fully-qualified `kotlin.random.randomInt(...)` call).

Real Kotlin's `kotlin.random.Random` is an object (`Random.nextInt(...)`,
`Random.nextDouble()`) — ksl has no `object`/singleton declarations yet,
so these are plain top-level functions instead, a deliberate shape
divergence (same kind `sqrt(Int)` above already has). Not cryptographically
secure — good enough for game logic (food placement, etc.), not a security
primitive.

### `random(): Double`

A random value in `[0, 1)`.

### `randomInt(bound: Int): Int`

A random value in `[0, bound)`. `bound` must be positive.

```kotlin
import kotlin.random.*

println(randomInt(6) + 1)   // a random die roll, 1..6
```

## Terminal

Lives in `ksl.term` — **not** auto-imported, and, unlike every package
above, **not real Kotlin at all**: there is no `kotlin.term` in the real
language. This is the one ksl-only namespace in the standard library (see
the intro above), added for terminal/game scripts.

Deliberately minimal: only functions that genuinely need a real OS
syscall live here. Screen/cursor control (clearing the screen, moving the
cursor, colors) is **not** provided — those are just ANSI escape-sequence
strings, buildable entirely in ordinary ksl code via the `\u{XXXX}` string
escape (`docs/syntax.md`'s "Strings") with no syscall involved, so they
belong in an ordinary ksl library instead (see
`crates/ksl-core/tests/demos/tui.ksl` for a small one, built on exactly
these primitives).

### `enableRawMode()` / `disableRawMode()`

Turns off line buffering and character echo on stdin ("cbreak" mode), so
`readByte()` sees each keypress immediately instead of waiting for Enter.
`enableRawMode()` throws if stdin isn't a real terminal — check
`isTerminal()` first to fail gracefully instead. `disableRawMode()` is
always safe to call, even if raw mode isn't active.

Ctrl+C still works as normal (sends `SIGINT`) even while raw mode is
active, and the terminal is always restored to normal automatically —
whether the script ends normally, errors out, or is interrupted with
Ctrl+C — so a script never needs its own cleanup-on-error logic for this.

### `readByte(): Int?`

The next byte waiting on stdin (its code, `0`-`255`), or `null` if
nothing is available right now. Only non-blocking after `enableRawMode()`
— without it, this blocks like an ordinary read. Multi-byte input (e.g.
an arrow key, which sends an `ESC [ A`-style sequence) arrives as
separate `readByte()` calls — assembling those into one keypress is a
library's job, not this function's.

```kotlin
import ksl.term.*

enableRawMode()
val b = readByte()
if (b != null) {
    println("got byte " + b)
}
disableRawMode()
```

### `isTerminal(): Boolean`

Whether stdin is connected to a real terminal.

### `terminalWidth(): Int` / `terminalHeight(): Int`

The terminal's current size, in columns/rows.

### `sleep(millis: Int)`

Pauses execution for the given number of milliseconds.

### `currentTimeMillis(): Int`

Milliseconds since the Unix epoch — for frame timing, or seeding your own
generator instead of `kotlin.random`.

## Assertions

Live in `kotlin.test`, same story as `kotlin.math` above — **not**
auto-imported (exactly matching real Kotlin). Calling one bare, with no
`import kotlin.test.*`/`import kotlin.test.<name>` in scope and no
`kotlin.test.` qualification, is `undefined name`, same as referencing any
other name that needs an import:

```kotlin
assertEquals(4, 2 + 2)              // undefined name 'assertEquals'

import kotlin.test.*
assertEquals(4, 2 + 2)              // OK

import kotlin.test.assertEquals
assertEquals(4, 2 + 2)              // OK, single-symbol import

kotlin.test.assertEquals(4, 2 + 2)  // OK, no import needed — fully qualified
```

Each aborts the script with a runtime error on failure, exactly like any
other stdlib error.

### `assertEquals(expected, actual, message: String? = null)`

```kotlin
import kotlin.test.*

assertEquals(4, 2 + 2)
assertEquals(4, 2 + 3, "arithmetic is broken")   // fails: "... : arithmetic is broken"
```

### `assertTrue(actual: Boolean, message: String? = null)` / `assertFalse(actual: Boolean, message: String? = null)`

```kotlin
assertTrue(1 < 2)
assertFalse(1 > 2)
```

### `assertNull(actual, message: String? = null)` / `assertNotNull(actual, message: String? = null)`

```kotlin
assertNull(null)
assertNotNull(5)
```

## Value formatting

`println`/`print` and string templates format values as follows:

| Type | Formatting |
|---|---|
| `Int` | plain decimal, e.g. `42` |
| `Double` | Rust's default `f64` formatting — **note this differs from Kotlin**: a whole-number `Double` like `2.0` prints as `2`, not `2.0` |
| `Boolean` | `true` / `false` |
| `String` | as-is |
| `Unit` | `kotlin.Unit` |
| `List` | `[a, b, c]`, elements formatted the same way, recursively |
| `Null` | `null` |
| Function/lambda | `<function name>` / `<function lambda>` |
