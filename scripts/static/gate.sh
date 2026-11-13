#!/usr/bin/env bash
# Pre-submit static gate: run it in your worktree BEFORE handing a SHA to the build queue.
# Nothing here compiles or touches target/: rustfmt, ast-grep rules, and every literal sqlx SQL string
# PREPAREd against the migrated schema (scripts/static/sqlcheck.py). It catches what would otherwise cost a
# queue round-trip; it does not replace the queue's clippy/test run.
#
#   scripts/static/gate.sh            # all checks
#   scripts/static/gate.sh --no-sql   # without the database check (reported as SKIPPED, not passed)
#
# Needs: ast-grep, uv, and the scratch DB in AGENT_SQLCHECK_URL / ~/.config/agent-sqlcheck/env
# (docs/runbooks/local-rust-development.md § Static gate).
set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 2

sql=1
for a in "$@"; do
  case "$a" in
    --no-sql) sql=0 ;;
    *) echo "usage: $0 [--no-sql]" >&2; exit 2 ;;
  esac
done

# CI's toolchain (rust.yml / runbook) when rustup has it; rustfmt output can differ between releases.
toolchain="$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/bin"
[[ -x "$toolchain/cargo" ]] && PATH="$toolchain:$PATH"

failed=()
step() {
  local name=$1; shift
  echo "── $name"
  if "$@"; then echo "   ok"; else failed+=("$name"); fi
}

step "rustfmt ($(rustfmt --version | cut -d' ' -f2))" cargo fmt --all -- --check
step "ast-grep rules" ast-grep scan -c scripts/static/ast-grep/sgconfig.yml crates
if (( sql )); then
  step "sqlcheck" uv run -q scripts/static/sqlcheck.py
else
  echo "── sqlcheck SKIPPED (--no-sql)"
fi

if (( ${#failed[@]} )); then
  echo "static gate FAILED: ${failed[*]}" >&2
  exit 1
fi
echo "static gate passed$( (( sql )) || echo ' (sqlcheck skipped)') at $(git rev-parse --short HEAD)$(git diff --quiet HEAD || echo ' + uncommitted changes')"
