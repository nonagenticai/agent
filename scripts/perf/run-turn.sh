#!/usr/bin/env bash
# Stage 0 turn-latency baseline on the REAL platform stack (dev Postgres 5433, dev Redis 6380).
# The model is the deterministic fake LLM, so a change in these numbers is the app's overhead.
#
#   scripts/perf/run-turn.sh                    # clean tenant, concurrency 1 4 16, 10 turns each
#   scripts/perf/run-turn.sh --keep             # reuse the running stack and its tenant
#   scripts/perf/run-turn.sh --turns 20 --concurrencies "1 8"
#
# It captures, for the same window: p50/p95 per concurrency, and every Postgres statement
# (count + total ms) the platform issued. The dev database's log_min_duration_statement is
# raised for the window and reset on exit.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
DEPS="$ROOT/target/local/dependencies.env"
LOCAL="${PS_LOCAL_DIR:-$ROOT/target/local/platform-stack}"
OUT="$ROOT/target/local/perf"
BIN=""; KEEP=0; TURNS="${PERF_TURNS:-10}"; CONCS="${PERF_CONCURRENCIES:-1 4 16}"

usage() { echo "usage: $0 [--bin <dir>] [--keep] [--turns N] [--concurrencies \"1 4 16\"]" >&2; exit 2; }
while [ $# -gt 0 ]; do case "$1" in
  --bin) BIN="${2:-}"; shift 2 ;;
  --keep) KEEP=1; shift ;;
  --turns) TURNS="${2:-}"; shift 2 ;;
  --concurrencies) CONCS="${2:-}"; shift 2 ;;
  *) usage ;;
esac; done

[ -f "$DEPS" ] || { echo "missing $DEPS (managed dependency stack not provisioned)" >&2; exit 1; }
set -a; . "$DEPS"; set +a
: "${AGENT_DEV_POSTGRES_PASSWORD:?missing in dependencies.env}"
ADMIN_URL="postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:5433/postgres"
SHA="$(git rev-parse --short HEAD)"
[ -n "$BIN" ] || BIN="$ROOT/target/local/perf-bin-$SHA"
[ -d "$BIN" ] || { echo "no binary dir $BIN; build one with: ~/.claude/scripts/cargo-guard.py build --locked -p agent-api --bins" >&2; exit 1; }
for b in agent-api agent-edge orchestrator runner interaction; do
  [ -x "$BIN/$b" ] || { echo "$BIN/$b is not executable" >&2; exit 1; }
done
[ "$(cat "$BIN/SOURCE_SHA" 2>/dev/null || true)" = "$(git rev-parse HEAD)" ] \
  || echo "[perf] WARN: $BIN is not pinned to HEAD $(git rev-parse --short HEAD)" >&2

command -v docker >/dev/null && docker info >/dev/null 2>&1 || { echo "docker is not running" >&2; exit 1; }
command -v node >/dev/null || { echo "node not found" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl not found" >&2; exit 1; }
mkdir -p "$OUT"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"

echo "[perf] dependencies (dev Postgres 5433, dev Redis 6380)"
docker compose -f compose.local.yml up -d postgres-dev redis-dev >/dev/null
for _ in $(seq 1 30); do
  [ "$(docker inspect --format '{{.State.Health.Status}}' agent-local-postgres-dev-1 2>/dev/null || true)" = healthy ] \
    && [ "$(docker inspect --format '{{.State.Health.Status}}' agent-local-redis-dev-1 2>/dev/null || true)" = healthy ] \
    && break
  sleep 1
done
[ "$(docker inspect --format '{{.State.Health.Status}}' agent-local-postgres-dev-1 2>/dev/null || true)" = healthy ] \
  || { echo "postgres-dev is not healthy" >&2; exit 1; }
[ "$(docker inspect --format '{{.State.Health.Status}}' agent-local-redis-dev-1 2>/dev/null || true)" = healthy ] \
  || { echo "redis-dev is not healthy" >&2; exit 1; }

if [ "$KEEP" = 1 ] && [ -f "$LOCAL/journey.json" ]; then
  # Same tenant, new binaries: `up` restarts every service and removes the harness opt-in,
  # so restore it for the signup-created tenant without re-running the journey.
  echo "[perf] restarting the stack with $BIN (tenant preserved)"
  scripts/local/platform-stack.sh up --bin "$BIN"
  TENANT_ID="$(node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")).tenant)' "$LOCAL/journey.json")"
  scripts/local/platform-stack.sh harness-tenant "$TENANT_ID"
else
  [ "$KEEP" = 1 ] || { echo "[perf] resetting the stack's own databases and keys"; scripts/local/platform-stack.sh reset; }
  echo "[perf] up (migrations, no seed) + smoke (creates the tenant and publishes web chat)"
  scripts/local/platform-stack.sh up --bin "$BIN"
  scripts/local/platform-stack.sh smoke --bin "$BIN"
fi
[ -f "$LOCAL/journey.json" ] || { echo "no $LOCAL/journey.json after smoke" >&2; exit 1; }

# Benchmark-only: the shipped web-chat limits (30 messages per IP per 10 min) throttle a
# concurrency run, so raise them on THIS local binding. The product's defaults are unchanged.
BINDING_ID="$(node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")).binding || "")' "$LOCAL/journey.json")"
printf '%s' "$BINDING_ID" | grep -Eq '^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$' \
  || { echo "no binding id in $LOCAL/journey.json" >&2; exit 1; }
PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql \
  "postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:5433/agent_platform_stack" \
  -v ON_ERROR_STOP=1 -q -c \
  "UPDATE connector_bindings SET settings = jsonb_set(settings, '{limits}', '{\"sessions_per_ip_hour\":100000,\"msgs_per_ip_10m\":100000,\"msgs_per_session_hour\":100000,\"msgs_per_minute\":100000}'::jsonb) WHERE id = '$BINDING_ID';"

LOGGING_ON=0
revert_logging() {
  if [ "$LOGGING_ON" = 1 ]; then
    PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$ADMIN_URL" -q -v ON_ERROR_STOP=1 \
      -c "ALTER SYSTEM RESET log_min_duration_statement;" -c "SELECT pg_reload_conf();" >/dev/null 2>&1 || true
    LOGGING_ON=0
  fi
}
trap 'revert_logging' EXIT INT TERM

START="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "[perf] capturing every Postgres statement from $START (reverted on exit)"
PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$ADMIN_URL" -q -v ON_ERROR_STOP=1 \
  -c "ALTER SYSTEM SET log_min_duration_statement = 0;" -c "SELECT pg_reload_conf();" >/dev/null
LOGGING_ON=1

BASELINE="$OUT/turn-baseline-$STAMP.txt"
: > "$BASELINE"
# Distinct client IPs per run and per level, so a rerun does not inherit a spent flood budget.
IP_OFFSET=$((RANDOM % 180))
LEVEL=0
for c in $CONCS; do
  echo "[perf] concurrency $c, $TURNS turns per worker"
  PS_STATE="$LOCAL/journey.json" PS_EDGE="http://localhost:8199" \
  PERF_CONCURRENCY="$c" PERF_TURNS="$TURNS" PERF_IP_OFFSET=$((IP_OFFSET + LEVEL * 20)) \
  PERF_OUT="$OUT/turn-$STAMP-c$c.json" \
    node scripts/perf/turn-bench.mjs | tee -a "$BASELINE"
  LEVEL=$((LEVEL + 1))
done

revert_logging
LOG="$OUT/turn-$STAMP-pg.log"
docker logs --since "$START" agent-local-postgres-dev-1 >"$LOG" 2>&1 || true
TOTAL="$(grep -cE 'duration: [0-9.]+ ms  (statement|execute)' "$LOG" || true)"
TOTAL_MS="$(grep -oE 'duration: [0-9.]+ ms  (statement|execute)' "$LOG" | awk '{s+=$2} END {printf "%.1f", s+0}')"
{
  echo
  echo "## Postgres statements in the window"
  echo "statements: $TOTAL   total_exec_ms: $TOTAL_MS"
  echo "top statement prefixes by count:"
  grep -E 'duration: [0-9.]+ ms  (statement|execute)' "$LOG" \
    | sed -E 's/.*(statement|execute[^:]*): //' \
    | cut -c1-120 | sort | uniq -c | sort -rn | head -25
} | tee -a "$BASELINE"

echo "[perf] baseline written to $BASELINE"
echo "[perf] raw Postgres log: $LOG"
