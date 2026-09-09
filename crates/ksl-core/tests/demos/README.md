# Demo scripts

Real, working `.ksl` programs that exercise the terminal/game stdlib added
in `docs/roadmap.md`'s **Phase 6.6** — `List.get/set/add/size/removeAt`,
`kotlin.random`, `ksl.term`'s raw-mode input, and the `\u{XXXX}` string
escape. Unlike `../examples/` (deterministic, golden-`stdout`-diffed by
`run_examples.rs`), these are **interactive and real-time** — there is no
"expected output" to diff against, so they aren't run by ordinary
`cargo test` at all. See `demos.rs` (this crate's `tests/` directory) for
the one thing that *is* automated: a parse+static-type-check smoke test,
which needs no terminal and no timing, but also proves nothing about
whether the game actually plays correctly.

## Files

- `tui.ksl` — a tiny screen/cursor-control library (`clearScreen`,
  `moveCursor`, `hideCursor`, `showCursor`), built entirely out of ANSI
  escape *strings* via the `\u{XXXX}` escape — no native support at all,
  proof that `ksl.term` only needed to cover what a script genuinely can't
  do itself. Meant to be `@file:Import`ed, not run directly.
- `snake.ksl` — real terminal snake. `@file:Import("tui.ksl")`, WASD to
  steer, `q` to quit.

## Running it

```sh
cargo run -p ksl-cli -- crates/ksl-core/tests/demos/snake.ksl
```

Must be run from a real terminal (not piped/redirected) — `snake.ksl`
checks `isTerminal()` itself and prints a clear message instead of
erroring if it isn't one.

## What to verify manually

- The board renders and updates every tick, no leftover garbage between
  frames (`clearScreen`/`moveCursor` working).
- `w`/`a`/`s`/`d` steer the snake in real time, no need to press Enter
  (`enableRawMode`/`readByte` working) — and the snake keeps moving even
  when no key is pressed (non-blocking read).
- Eating `*` grows the snake and respawns the food elsewhere
  (`kotlin.random`); running into a wall or into itself ends the game.
- **Terminal restoration, both ways**:
  - Quit normally (`q`) — the cursor reappears, the screen clears, and
    typing at the resulting shell prompt behaves normally again (echoes
    characters, doesn't need raw bytes interpreted by hand).
  - Interrupt with **Ctrl+C** mid-game instead — the process must still
    exit, and the terminal must still come back to normal (this is the
    specific case `ksl-cli`'s own `SIGINT` handler exists for — see
    `stdlib::terminal`'s module doc comment in `ksl-core`'s source). If a
    Ctrl+C ever leaves the shell echoing garbage/not accepting input
    normally afterward, that's a real regression here — `reset` recovers
    the terminal by hand if it ever happens.
