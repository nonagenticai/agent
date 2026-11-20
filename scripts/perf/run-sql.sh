#!/usr/bin/env bash
# Stage 0 SQL baseline on a dedicated Postgres 18 (port 55432, internal = external).
# It measures the app's real read models with EXPLAIN (ANALYZE, BUFFERS) and
# pg_stat_statements — the closest available analogue to a database engine's
# cache/DRAM counters. It does not touch the dev or test stack.
#
#   scripts/perf/run-sql.sh          # start, migrate, seed, measure
#   scripts/perf/run-sql.sh down     # stop this project's container (keeps the volume)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
DEPS="$ROOT/target/local/dependencies.env"
OUT="$ROOT/target/local/perf"
TENANT="11111111-1111-4111-8111-111111111111"
PORT=55432
URL=""

usage() { echo "usage: $0 [down]" >&2; exit 2; }
[ $# -le 1 ] || usage
[ -f "$DEPS" ] || { echo "missing $DEPS (managed dependency stack not provisioned)" >&2; exit 1; }
set -a; . "$DEPS"; set +a
: "${AGENT_DEV_POSTGRES_PASSWORD:?missing in dependencies.env}"
URL="postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:${PORT}/agent_perf"

if [ "${1:-}" = "down" ]; then
  docker compose -f scripts/perf/docker-compose.perf.yml down
  exit 0
fi
[ $# -eq 0 ] || usage

command -v docker >/dev/null || { echo "docker not found" >&2; exit 1; }
command -v uv >/dev/null || { echo "uv not found (needed by scripts/static/sqlcheck.py)" >&2; exit 1; }
command -v psql >/dev/null || { echo "psql not found" >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "docker is not running" >&2; exit 1; }

RUNNING="$(docker ps --filter name=agent-perf-postgres-perf-1 --format '{{.Names}}')"
if [ -z "$RUNNING" ] && lsof -nP -iTCP:$PORT -sTCP:LISTEN >/dev/null 2>&1; then
  echo "port $PORT is busy and is not this benchmark container:" >&2
  lsof -nP -iTCP:$PORT -sTCP:LISTEN >&2
  exit 1
fi

mkdir -p "$OUT"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"

echo "[perf] starting postgres $PORT"
docker compose -f scripts/perf/docker-compose.perf.yml up -d
for _ in $(seq 1 60); do
  [ "$(docker inspect --format '{{.State.Health.Status}}' agent-perf-postgres-perf-1 2>/dev/null || true)" = healthy ] && break
  sleep 1
done
[ "$(docker inspect --format '{{.State.Health.Status}}' agent-perf-postgres-perf-1 2>/dev/null || true)" = healthy ] \
  || { echo "perf postgres did not become healthy" >&2; docker logs --tail 40 agent-perf-postgres-perf-1 >&2; exit 1; }

echo "[perf] extension + schema"
PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$URL" -v ON_ERROR_STOP=1 -q <<'SQL'
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
-- The platform-stack's init_db creates these before migrating (0031 requires agent_platform
-- and its grant; 0001/0065 create the other two only when the owner has CREATEROLE).
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_app') THEN
    EXECUTE 'CREATE ROLE agent_app NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_runtime') THEN
    EXECUTE 'CREATE ROLE agent_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_platform') THEN
    EXECUTE 'CREATE ROLE agent_platform NOLOGIN NOSUPERUSER NOBYPASSRLS';
  END IF;
END $$;
GRANT agent_platform TO agent_app WITH INHERIT FALSE, SET TRUE;
SQL
# The whole source tree, not one file: sqlcheck runs one ast-grep pattern at a time and
# ast-grep exits 1 when a pattern has no match, so a narrow path aborts before migrating.
AGENT_SQLCHECK_URL="$URL" uv run scripts/static/sqlcheck.py crates/agent-api/src

echo "[perf] seeding 1M usage events, 10k conversations, 200k messages, 100k calls"
PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$URL" -v ON_ERROR_STOP=1 -q -v tenant="$TENANT" \
  -f scripts/perf/seed.sql
PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$URL" -v ON_ERROR_STOP=1 -q -c "ANALYZE;"

echo "[perf] resetting pg_stat_statements"
PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$URL" -v ON_ERROR_STOP=1 -q \
  -c "SELECT pg_stat_statements_reset();" >/dev/null

BASELINE="$OUT/sql-baseline-$STAMP.txt"
{
  echo "# SQL baseline $STAMP (tenant $TENANT)"
  echo
  echo "## EXPLAIN (ANALYZE, BUFFERS), report queries as agent_app with RLS"
  PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$URL" -v ON_ERROR_STOP=1 -v tenant="$TENANT" \
    -f scripts/perf/slices.sql
  echo
  echo "## pg_stat_statements (top by total_exec_time)"
  PGPASSWORD="$AGENT_DEV_POSTGRES_PASSWORD" psql "$URL" -v ON_ERROR_STOP=1 -x -c \
    "SELECT calls, rows, round(total_exec_time::numeric, 1) AS total_ms,
            round(mean_exec_time::numeric, 2) AS mean_ms,
            shared_blks_hit, shared_blks_read, temp_blks_written,
            left(regexp_replace(query, '\s+', ' ', 'g'), 220) AS query
       FROM pg_stat_statements
      WHERE query NOT LIKE '%pg_stat_statements%'
      ORDER BY total_exec_time DESC LIMIT 15;"
} | tee "$BASELINE"

echo "[perf] baseline written to $BASELINE"
echo "[perf] container left running; stop it with: scripts/perf/run-sql.sh down"
