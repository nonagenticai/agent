#!/usr/bin/env bash
# Image smoke for agent-api (rust.yml build_smoke; B4 T19). One image, five entrypoints:
#   1. every binary is present, executable and fully linked;
#   2. each B4 runtime daemon (orchestrator, runner, interaction) and agent-api itself EXITS non-zero, within the
#      window, naming the variable, when a required variable is missing or malformed — never hangs, never echoes a
#      URL (the smoke passes a password marker and fails if it shows up in the output);
#   3. with an unreachable Postgres / Redis the daemons fail by name instead of hanging at boot.
# Usage: scripts/image-smoke.sh <image ref>   (the archetype exports $IMAGE_REF)
set -euo pipefail

IMAGE="${1:-${IMAGE_REF:?IMAGE_REF or an argument is required}}"
WINDOW="${SMOKE_WINDOW_SECONDS:-90}"
PW_MARKER="smokeSecretPw"
# Unreachable, syntactically valid endpoints (container loopback port 1 refuses at once).
PG_DOWN="postgres://agent_runtime:${PW_MARKER}@127.0.0.1:1/agent"
REDIS_DOWN="redis://:${PW_MARKER}@127.0.0.1:1"
BINARIES=(agent-api agent-edge orchestrator runner interaction)

for attempt in 1 2 3; do
  if docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE" >/dev/null; then break; fi
  [ "$attempt" -lt 3 ] || { echo "::error::cannot pull $IMAGE"; exit 1; }
  sleep $((attempt * 20))
done

echo "== 1. binaries present + linked"
docker run --rm --entrypoint sh "$IMAGE" -c '
  rc=0
  for b in '"${BINARIES[*]}"'; do
    p=/usr/local/bin/$b
    if [ ! -x "$p" ]; then echo "MISSING $p"; rc=1; continue; fi
    if ldd "$p" | grep "not found"; then echo "UNRESOLVED LIBS in $p"; rc=1; continue; fi
    echo "ok $p"
  done
  exit $rc'

fail=0
# expect <binary> <substring the output must contain> [KEY=VALUE ...]
expect() {
  local bin="$1" want="$2"; shift 2
  local args=() kv
  for kv in "$@"; do args+=(-e "$kv"); done
  local cid code out
  cid="$(docker run -d --entrypoint "/usr/local/bin/$bin" ${args[@]+"${args[@]}"} "$IMAGE")"
  if ! code="$(timeout "$WINDOW" docker wait "$cid")"; then
    docker rm -f "$cid" >/dev/null 2>&1 || true
    echo "::error::$bin [$*] still running after ${WINDOW}s (hang; wanted a fast failure naming '$want')"
    fail=1
    return
  fi
  out="$(docker logs "$cid" 2>&1 || true)"
  docker rm -f "$cid" >/dev/null 2>&1 || true
  if [ "$code" = "0" ]; then
    echo "::error::$bin [$*] exited 0; wanted a failure naming '$want'"; echo "$out"; fail=1; return
  fi
  if [ "$code" = "126" ] || [ "$code" = "127" ]; then
    echo "::error::$bin could not be executed (exit $code)"; echo "$out"; fail=1; return
  fi
  if ! grep -qF -- "$want" <<<"$out"; then
    echo "::error::$bin [$*] exit $code without '$want' in its output:"; echo "$out"; fail=1; return
  fi
  if grep -qF -- "$PW_MARKER" <<<"$out"; then
    echo "::error::$bin [$*] printed a credential from its URL"; fail=1; return
  fi
  echo "ok $bin [$*] -> exit $code: $(grep -F -- "$want" <<<"$out" | head -1 | cut -c1-200)"
}

echo "== 2. missing / malformed env fails fast, by name"
expect orchestrator "REDIS_URL is required"
expect orchestrator "DATABASE_URL is required" "REDIS_URL=$REDIS_DOWN"
expect orchestrator "CASE_DEADLINE_MS must be a positive integer" \
  "REDIS_URL=$REDIS_DOWN" "DATABASE_URL=$PG_DOWN" "CASE_DEADLINE_MS=soon"
expect orchestrator "AGENT_PREFIX may not contain whitespace" "REDIS_URL=$REDIS_DOWN" "AGENT_PREFIX=a b"
expect runner "REDIS_URL is required"
expect runner "MIGRATE_DATABASE_URL must not be set" "REDIS_URL=$REDIS_DOWN" "MIGRATE_DATABASE_URL=$PG_DOWN"
expect runner "DATABASE_URL is required" "REDIS_URL=$REDIS_DOWN"
expect interaction "DATABASE_URL is required"
expect interaction "REDIS_URL is required" "DATABASE_URL=$PG_DOWN"
expect interaction "LOG_FORMAT must be json or pretty" "LOG_FORMAT=xml"
expect agent-api "is not set"

echo "== 3. unreachable dependencies fail by name, no hang"
expect orchestrator "REDIS_URL" "REDIS_URL=$REDIS_DOWN" "DATABASE_URL=$PG_DOWN"
expect interaction "DATABASE_URL" "DATABASE_URL=$PG_DOWN" "REDIS_URL=$REDIS_DOWN"
# B4 F1: the runner gets past its config with NO identity / Keycloak-admin / licences-admin variable set
expect runner "connect DATABASE_URL" "DATABASE_URL=$PG_DOWN" "REDIS_URL=$REDIS_DOWN"

if [ "$fail" -ne 0 ]; then
  echo "::error::image smoke FAILED"
  exit 1
fi
echo "image smoke OK: ${BINARIES[*]}"
