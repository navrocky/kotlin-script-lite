#!/usr/bin/env bash
# Vendors a curated subset of JetBrains/kotlin's own compiler box-test
# corpus (compiler/testData/codegen/box/) into this repo, at one pinned
# upstream commit, for ksl's own conformance-test harness. See
# docs/roadmap.md's "Phase 0.5" and
# crates/ksl-core/tests/kotlin-box/README.md for the full design.
#
# Usage: scripts/fetch-kotlin-box-tests.sh
#
# Files are copied byte-for-byte (never hand-edited) so a re-run against
# the same SHA is a no-op and a re-run after bumping SHA stays a clean
# diff against upstream. Vendored files are saved with a `.ksl` extension,
# not upstream's own `.kt` — same content, but `.kt` makes editors with a
# Kotlin plugin (e.g. VS Code) treat them as real Kotlin project files and
# flag every ksl-unsupported construct as an error; `.ksl` routes them to
# ksl's own tooling instead, which is what actually runs them. To add a
# file: add its path (relative to compiler/testData/codegen/box/, still
# `.kt` — that's the real upstream extension the fetch URL needs) to
# ACTIVE_FILES (if it already passes through ksl unmodified) or
# PENDING_FILES (vendored for visibility, not yet expected to pass) below,
# then re-run this script. To pick up newer upstream test content: bump
# SHA, re-run.

set -euo pipefail

SHA="8fc606186b9d2be2bce6b390f7bf12f7cdb41adc"
BASE_URL="https://raw.githubusercontent.com/JetBrains/kotlin/$SHA/compiler/testData/codegen/box"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ACTIVE_DIR="$ROOT/crates/ksl-core/tests/kotlin-box"
PENDING_DIR="$ROOT/crates/ksl-core/tests/kotlin-box-pending"

# --- kotlin-box: hand-verified to run through ksl today and print "OK"
# once the harness appends `println(box())` — see kotlin_box_tests.rs.
# Every other line is real, unmodified upstream Kotlin.
ACTIVE_FILES=(
  "controlStructures/bottles.kt"
  "controlStructures/continueInWhile.kt"
  "controlStructures/factorialTest.kt"
  "controlStructures/ifConst1.kt"
  "controlStructures/kt1899.kt"
  "controlStructures/kt870.kt"
  "basics/concatenation.kt"
  "elvis/nullNullOk.kt"
)

# --- kotlin-box-pending: vendored for visibility and future promotion,
# each blocked on one concrete, already-tracked gap:
#   doWhile.kt        -- bare builtin property access (`z.length`, real
#                         Kotlin's own `String.length`) -- deliberately
#                         out of scope until classes/properties exist
#                         (roadmap Phase 6); `do { } while (...)` itself
#                         (Phase 0.8) is no longer what blocks this one.
#   kt2147.kt         -- classes (roadmap Phase 6)
#   local_variable.kt -- `throw` (roadmap Phase 5)
PENDING_FILES=(
  "controlStructures/doWhile.kt"
  "controlStructures/kt2147.kt"
  "basics/local_variable.kt"
  "elvis/nullNullOk.kt"
)

fetch() {
  local dest_root="$1" rel_path="$2"
  # Upstream's own extension (.kt) is what the fetch URL needs; the
  # vendored copy on disk gets .ksl instead — see the top comment.
  local dest="$dest_root/${rel_path%.kt}.ksl"
  mkdir -p "$(dirname "$dest")"
  curl -fsSL "$BASE_URL/$rel_path" -o "$dest"
  echo "fetched $rel_path -> ${dest#"$ROOT"/}"
}

for f in "${ACTIVE_FILES[@]}"; do fetch "$ACTIVE_DIR" "$f"; done
for f in "${PENDING_FILES[@]}"; do fetch "$PENDING_DIR" "$f"; done

echo "Done: ${#ACTIVE_FILES[@]} active + ${#PENDING_FILES[@]} pending file(s) fetched at $SHA."
