# Standard library reference

All built-ins are plain global functions — there is no `.` method-call
syntax, so e.g. `length(s)` rather than `s.length()`. See
[`syntax.md`](syntax.md) for the language itself.

Errors thrown by these functions are runtime errors like any other (they
stop the script and print `runtime error: <message> at <line:col>` to
stderr), though the reported location currently always points at `0:0`
for stdlib errors rather than the actual call site — a known gap, not
something to rely on.

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
| Function/lambda | `<function name>` / `<function lambda>` |
