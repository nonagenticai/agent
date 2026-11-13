#!/usr/bin/env bash
# Run the workspace's test binaries N at a time instead of one after another (`cargo test` runs them serially:
# 121 binaries, 27 min of summed test time on CI 2026-10-26, b4/b3/b5 alone 9 min). Each binary already owns
# its Postgres (one testcontainer per binary, tests/common/mod.rs), so binaries share nothing but the CPU.
#
#   scripts/test-parallel.sh [extra cargo args]     # e.g. --locked
#   TEST_JOBS=4 (default) binaries at once; RUST_TEST_THREADS is left to the caller.
#
# Each binary's log is printed whole when it finishes (never interleaved), with its `test result:` line, so the
# CI archetype's "at least one test passed" grep still applies. Every binary runs even after a failure; the
# script exits non-zero and lists the failed binaries at the end. Doc tests run last, as `cargo test` would.
set -euo pipefail

# Local runs go through cargo-guard (the hook denies a bare compiling cargo); CI has no wrapper
# and falls back to cargo.
GUARD="${CARGO_GUARD:-}"
if [ -z "$GUARD" ]; then
  if [ -x "$HOME/.claude/scripts/cargo-guard.py" ]; then
    GUARD="$HOME/.claude/scripts/cargo-guard.py"
  else
    GUARD=cargo
  fi
fi

# On one persistent server (TEST_DATABASE_URL, the local workflow) binaries share its cluster-global ROLES and race
# on them during migrations ("role agent_platform does not exist"): run them one at a time there unless told otherwise.
# CI gives every binary its own testcontainer, so it runs TEST_JOBS (default 4) at once.
if [ -n "${TEST_DATABASE_URL:-}" ]; then jobs="${TEST_JOBS:-1}"; else jobs="${TEST_JOBS:-4}"; fi
logs="$(mktemp -d)"
trap 'rm -rf "$logs"' EXIT

# Build every test target once; one line per binary: <package dir> <target name> <executable>.
# The directory binaries (tests/<dir>/main.rs) are the long ones: they start first.
"$GUARD" test --workspace --no-run --message-format=json "$@" |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null)
        | .target.name as $n
        | (["b4","b3","b5","b6b","kh","b8","pk"] | index($n)) as $i
        | "\($i // 9) \(.manifest_path | sub("/[^/]*$"; "")) \($n) \(.executable)"' |
    sort -n | cut -d' ' -f2- >"$logs/binaries"

count="$(wc -l <"$logs/binaries" | tr -d ' ')"
# Zero binaries is a broken listing, never a pass.
[[ "$count" -gt 0 ]] || { echo "no test binaries found" >&2; exit 1; }
echo "running $count test binaries, $jobs at a time"

run_one() {
    local dir="$1" name="$2" exe="$3" log
    log="$(mktemp "$LOGS/$name.XXXX")"
    # cargo runs a test binary from its package root; tests read fixtures relative to it.
    if (cd "$dir" && "$exe") >"$log" 2>&1; then
        printf '     Running %s\n%s\n' "$name" "$(cat "$log")"
    else
        printf '     Running %s (FAILED)\n%s\n' "$name" "$(cat "$log")"
        echo "$name" >>"$LOGS/failed"
    fi
}
export -f run_one
export LOGS="$logs"

# run_one records a failure instead of returning non-zero, so xargs keeps going and every binary runs.
xargs -P "$jobs" -L1 bash -c 'run_one "$0" "$1" "$2"' <"$logs/binaries"

"$GUARD" test --workspace --doc "$@" || echo "doc-tests" >>"$logs/failed"

if [[ -s "$logs/failed" ]]; then
    echo "FAILED test binaries:" >&2
    sed 's/^/  /' "$logs/failed" >&2
    exit 1
fi
