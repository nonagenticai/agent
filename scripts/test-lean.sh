#!/usr/bin/env bash
# Run the workspace's tests one target at a time and delete each test executable once it has run, so the disk only
# ever holds the shared dependencies plus ONE test binary. CI's runner work volume is 20Gi; agent-api's ~110 former
# integration binaries (~150-200 MB each) built together got the pod evicted (PR #109, 2026-10-29). They are one
# target now (crates/agent-api/tests/all.rs); this keeps the disk bound regardless. Same scope as `cargo test --workspace --locked`: every package's lib/bin unit tests, every
# integration test target, then the doc tests. Every target runs even after a failure; the exit status is non-zero
# when any target failed or no target was found.
#
#   scripts/test-lean.sh [extra cargo args]       # e.g. --locked
#   LEAN_ONLY=<regex>                             # local check: only targets whose "<package> <kind> <name>" matches
set -euo pipefail

# Local runs go through cargo-guard (the hook denies a bare compiling cargo); CI has no wrapper
# and falls back to cargo. Only the wrapper understands --print-target; plain cargo reports its
# target dir through metadata instead.
GUARD="${CARGO_GUARD:-}"
if [ -z "$GUARD" ]; then
  if [ -x "$HOME/.claude/scripts/cargo-guard.py" ]; then
    GUARD="$HOME/.claude/scripts/cargo-guard.py"
  else
    GUARD=cargo
  fi
fi
meta="$(cargo metadata --no-deps --format-version 1 "$@")"
target_dir="$("$GUARD" --print-target 2>/dev/null)" || target_dir="$(jq -r .target_directory <<<"$meta")"
deps="$target_dir/debug/deps"

# "<package> <selector...>" per line: lib and bins first, then the integration tests, by name
mapfile -t units < <(jq -r '.packages[] | .name as $p
    | ([.targets[] | select(.kind | index("lib"))] | if length > 0 then "\($p) --lib" else empty end),
      (.targets[] | select(.kind | index("bin")) | "\($p) --bin \(.name)")' <<<"$meta")
mapfile -t tests < <(jq -r '.packages[] | .name as $p | .targets[] | select(.kind | index("test"))
    | "\($p) --test \(.name)"' <<<"$meta" | sort)
all=("${units[@]}" "${tests[@]}")
if [ -n "${LEAN_ONLY:-}" ]; then
  mapfile -t all < <(printf '%s\n' "${all[@]}" | grep -E "$LEAN_ONLY" || true)
fi
[ "${#all[@]}" -gt 0 ] || { echo "test-lean: no test targets found" >&2; exit 1; }

failed=()
stamp="$(mktemp)"
trap 'rm -f "$stamp"' EXIT
for t in "${all[@]}"; do
  read -r pkg sel <<<"$t"
  touch "$stamp"
  echo "=== test-lean: $pkg $sel"
  # shellcheck disable=SC2086 # $sel is "--test name" / "--lib" / "--bin name"
  if ! "$GUARD" test -p "$pkg" $sel "$@"; then failed+=("$pkg $sel"); fi
  # the test executables just linked (no extension in deps/); rlibs, rmeta, .d and proc-macro libraries stay
  find "$deps" -maxdepth 1 -type f -newer "$stamp" ! -name '*.*' -delete 2>/dev/null || true
done
if [ -z "${LEAN_ONLY:-}" ]; then
  echo "=== test-lean: doc tests"
  "$GUARD" test --workspace --doc "$@" || failed+=("doc tests")
fi

if [ "${#failed[@]}" -gt 0 ]; then
  echo "test-lean: FAILED targets:" >&2
  printf '  %s\n' "${failed[@]}" >&2
  exit 1
fi
echo "test-lean: all ${#all[@]} targets passed"
