# Vendored Kotlin box tests

This directory (and its sibling `../kotlin-box-pending/`) vendors a
hand-picked subset of [JetBrains/kotlin][kotlin]'s own compiler conformance
suite — `compiler/testData/codegen/box/` — for ksl's own conformance-test
harness. See `docs/roadmap.md`'s **Phase 0.5** for the full design
rationale; this file only covers the mechanics of using/updating it.

[kotlin]: https://github.com/JetBrains/kotlin

## Layout

- `kotlin-box/<category>/<name>.ksl` — vendored files hand-verified to run
  through ksl and print `"OK"`, exercised by
  `crates/ksl-core/tests/kotlin_box_tests.rs`'s `vendored_box_tests_pass`
  (a normal, hard-failing `cargo test`).
- `kotlin-box-pending/<category>/<name>.ksl` — vendored files not yet
  passing, kept for visibility and future promotion. Exercised by the same
  test file's `pending_box_tests_parse_status`, which only checks each file
  *parses* and never fails the build — run
  `cargo test -p ksl-core --test kotlin_box_tests -- --nocapture` to see
  the `PARSES`/`BLOCKED` summary.

Every file in both directories is byte-for-byte identical to upstream at
the pinned commit below — **never hand-edit a vendored file's content**.
The one thing the fetch script *does* change is the extension: upstream's
own `.kt` becomes `.ksl` on disk here (`scripts/fetch-kotlin-box-tests.sh`'s
`fetch` function strips `.kt` and appends `.ksl` on the destination path
only — the fetch URL still uses real upstream's `.kt` path). This is purely
so editors with a Kotlin plugin (VS Code included) don't pick these files
up as real Kotlin project source and flag every ksl-unsupported construct
as an error — `.ksl` routes them to ksl's own tooling instead.

The only *content* adaptation applied is at the *harness* level, in memory,
documented in `kotlin_box_tests.rs`'s own doc comment: `println(box())` is
appended, and stdout must be exactly `"OK\n"`. A vendored file's own
`import kotlin.test.*`/`import kotlin.test.<name>` line, where it has one,
needs no special handling — `kotlin.test` is a real, resolvable package to
ksl (see `docs/syntax.md`'s "Imports"), so it runs as genuinely unmodified
Kotlin.

## Pinned upstream commit

`8fc606186b9d2be2bce6b390f7bf12f7cdb41adc` (`JetBrains/kotlin`, `master`,
fetched 2026-09-04) — recorded in `scripts/fetch-kotlin-box-tests.sh`'s
`SHA` variable, the single source of truth. Bump it there and re-run the
script to pick up newer upstream test content; the script always
re-downloads every listed file, so this stays a clean diff against
upstream.

## License

`compiler/testData/codegen/box/**` is part of the JetBrains/kotlin
repository, licensed Apache License 2.0 (see that repo's
`license/LICENSE.txt`). Vendored here under the same license; no
modifications to file content (see above — only the on-disk extension
changes, `.kt` to `.ksl`).

## Adding a file

1. Pick a `.kt` file under upstream's `compiler/testData/codegen/box/`
   that either already runs unmodified through ksl (after the harness's
   `println(box())` trailer above) or is worth tracking as a known gap.
2. Add its path (relative to `compiler/testData/codegen/box/`, still with
   its real upstream `.kt` extension — the fetch script renames it to
   `.ksl` on the way in) to `ACTIVE_FILES` or `PENDING_FILES` in
   `scripts/fetch-kotlin-box-tests.sh`.
3. Re-run `scripts/fetch-kotlin-box-tests.sh`.
4. `cargo test -p ksl-core --test kotlin_box_tests` — new `ACTIVE_FILES`
   entries must pass; new `PENDING_FILES` entries don't need to.

## Promoting a pending file

Once a language/stdlib gap that blocked a `kotlin-box-pending/` file is
fixed: move its path from `PENDING_FILES` to `ACTIVE_FILES` in
`scripts/fetch-kotlin-box-tests.sh`, move the file itself from
`kotlin-box-pending/` to `kotlin-box/` (`git mv`), re-run the fetch script
(a no-op on content, just confirms the path), and confirm
`vendored_box_tests_pass` passes. Do this as part of whichever phase's own
"Docs/tests" work fixed the gap, not as a separate cleanup pass — see
`docs/roadmap.md`'s Phase 0.5.

## Currently pending, and why

| File | Blocked on |
|---|---|
| `controlStructures/doWhile.ksl` | `do { } while (...)` — no such loop construct in ksl yet (not on any roadmap phase as of this writing) |
| `controlStructures/factorialTest.ksl` | same (`do`/`while`) |
| `controlStructures/kt2147.ksl` | classes (`docs/roadmap.md` Phase 6) |
| `basics/local_variable.ksl` | `throw` (`docs/roadmap.md` Phase 5) |
