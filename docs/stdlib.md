# Standard library reference

Built-ins are plain global functions, e.g. `length(s)`. A handful also
have a `.` method-call form (`s.length()`) — see [Methods](#methods)
below. See [`syntax.md`](syntax.md) for the language itself.

Errors thrown by these functions are runtime errors like any other — they
stop the script and print `runtime error: <message> at <line:col>` to
stderr, `<line:col>` being the actual call site.

## Output

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

## Lists

### `listOf(vararg values)`

Builds a `List` from any number of arguments (including zero).

```kotlin
val xs = listOf(1, 2, 3)
for (x in xs) { println(x) }
```

Lists are also produced by an inclusive `Int` range, e.g. `1..5` — see
[`syntax.md`](syntax.md#for). `+` concatenates two lists.

## Strings

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

No other type has methods yet, and `.field` access (no call) always
fails at runtime — there's no object system yet to have real fields (see
`docs/syntax.md`'s "Not supported"). `receiver.name` is otherwise
ambiguous with a package-qualified reference (`util.math.square`, see
`syntax.md`'s "Packages"); this is resolved by trying `receiver` as a
local value first and only falling back to a package lookup if it isn't
one — so a local variable always wins over a same-named package.

## Math

### `abs(x: Int | Double): Int | Double`

Absolute value; the result type matches the argument's type.

```kotlin
abs(-5)      // 5
abs(-3.5)    // 3.5
```

### `sqrt(x: Int | Double): Double`

```kotlin
sqrt(16)     // 4.0 (printed as "4", see below)
```

### `max(a, b)` / `min(a, b)`

Both arguments must be `Int` or `Double`. Returns `Int` only if both
arguments are `Int`; otherwise returns `Double`.

```kotlin
max(3, 7)       // 7
min(3.5, 2)     // 2.0 (printed as "2")
```

## Assertions

`kotlin.test`-shaped, but plain global functions — no `import` needed (see
[`syntax.md`](syntax.md#imports); `import kotlin.test.*` itself doesn't
resolve, since ksl has no such package). Each aborts the script with a
runtime error on failure, exactly like any other stdlib error.

### `assertEquals(expected, actual, message: String? = null)`

```kotlin
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
