#!/usr/bin/env bash
# B8 T17 LOCAL acceptance (rows A1-A15).
#
#   scripts/local/b8-acceptance.sh            prepare, wait for agent-api, seed, start the web, run the specs
#   scripts/local/b8-acceptance.sh prepare    only: check ports + deps, create the database, start the fakes,
#                                             write the API env file, print the Cargo command for the operator
#   scripts/local/b8-acceptance.sh test       only: seed, start the web, run the specs (API already up)
#   scripts/local/b8-acceptance.sh list       list the specs (no browser, no API needed)
#   scripts/local/b8-acceptance.sh down       stop the fakes and the web started by this script
#
# What it starts (all loopback, internal = external port; never 8000):
#   fake IdP (JWKS + PKCE sign-in)   scripts/local/b8-fake-idp.mjs   B8_IDP_PORT  (default 9352)
#   fake LLM (OpenAI + Laya)         scripts/local/b8-fake-llm.mjs   B8_LLM_PORT  (default 9353)
#   fake Strata (rag + ingest, A6)   scripts/local/b8-fake-strata.mjs B8_STRATA_PORT (default 9354)
#   agent-api                        YOU start it (below)            B8_API_PORT  (default 9350, metrics 9351)
#   web (vite dev, live mode)        npx vite dev                    B8_WEB_PORT  (default 3000)
# Dependencies: the managed dev Postgres 18 on :5433 (compose.local.yml `postgres-dev`). The run uses its own
# database `agent_b8_acceptance` on it, dropped and recreated by `prepare` (B8_KEEP_DB=1 keeps it). Dev data in
# `agent_development` is never touched. Redis is not used (B8 does not need the runtime bus).
#
# agent-api is NOT started by this script. The Rust build belongs to the integration session's single build queue
# (docs/runbooks/local-rust-development.md) and the machine-wide Cargo hook, so `prepare` prints the exact command
# and the operator runs it once in another terminal, from the checkout that holds the B8 commit being validated.
# `test` / the default mode then wait (B8_API_WAIT seconds, default 1800: a cold build) for GET /health.
#
# Browser: the Playwright run only starts when B8_BROWSER_LEASE=held, i.e. when you hold the machine-wide browser
# lease (browser-testing skill). Without it the script stops before the browser and says so.
#
# Env: B8_LOCAL_DIR (logs, env file, IdP key; default <main checkout>/target/local/b8-acceptance),
#      B8_PLAYWRIGHT_ARGS (extra args, e.g. "-g A7"), B8_KEEP_DB=1.
set -euo pipefail

MODE="${1:-all}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MAIN="$(cd "$(git -C "$ROOT" rev-parse --git-common-dir)/.." && pwd)"
LOCAL="${B8_LOCAL_DIR:-$MAIN/target/local/b8-acceptance}"
DEPS_ENV="$MAIN/target/local/dependencies.env"

IDP_PORT="${B8_IDP_PORT:-9352}"
LLM_PORT="${B8_LLM_PORT:-9353}"
STRATA_PORT="${B8_STRATA_PORT:-9354}"
API_PORT="${B8_API_PORT:-9350}"
METRICS_PORT="${B8_METRICS_PORT:-9351}"
WEB_PORT="${B8_WEB_PORT:-3000}"
DB_NAME="agent_b8_acceptance"
DEV_PG_PORT=5433

# Fixed ids: the API config names them before the rows exist (scripts/local/b8-seed.sql inserts them).
PLATFORM_TENANT="0195f48b-d000-7000-8000-0000000b8001"
DEMO_TENANT="0195f48b-d000-7000-8000-0000000b8002"
OTHER_TENANT="0195f48b-d000-7000-8000-0000000b8003"
GEN_PROVIDER="0195f48b-d000-7000-8000-0000000b8010"

say() { printf '[b8] %s\n' "$*"; }
die() { printf '[b8] ERROR: %s\n' "$*" >&2; exit 1; }

port_free() { ! lsof -iTCP:"$1" -sTCP:LISTEN -nP >/dev/null 2>&1; }
wait_http() { # url seconds label
  local url="$1" secs="$2" label="$3" i=0
  while ! curl -fsS -o /dev/null "$url" 2>/dev/null; do
    i=$((i + 2))
    [ "$i" -ge "$secs" ] && die "$label did not answer $url within ${secs}s (logs: $LOCAL)"
    sleep 2
  done
}
pid_alive() { [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null; }
stop_pid() {
  if pid_alive "$1"; then kill "$(cat "$1")" 2>/dev/null || true; fi
  rm -f "$1"
}

load_deps() {
  [ -f "$DEPS_ENV" ] || die "missing $DEPS_ENV (the managed dependency stack's env file)"
  set -a
  # shellcheck disable=SC1090
  . "$DEPS_ENV"
  set +a
  OWNER_URL="postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:${DEV_PG_PORT}/${DB_NAME}"
  ADMIN_URL="postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:${DEV_PG_PORT}/postgres"
}

ensure_roles() {
  psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q <<'SQL'
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_app') THEN
    CREATE ROLE agent_app LOGIN PASSWORD 'agent_app' NOSUPERUSER NOBYPASSRLS;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_platform') THEN
    CREATE ROLE agent_platform NOLOGIN NOSUPERUSER NOBYPASSRLS;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_runtime') THEN
    CREATE ROLE agent_runtime LOGIN PASSWORD 'agent_runtime' NOSUPERUSER NOBYPASSRLS;
  END IF;
EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
END $$;
GRANT agent_platform TO agent_app WITH INHERIT FALSE, SET TRUE;
SQL
  # The roles are server-wide on the dev server; an existing one is left alone, so it must accept the known password.
  PGPASSWORD=agent_app psql "postgres://agent_app@127.0.0.1:${DEV_PG_PORT}/postgres" -qtAc 'select 1' >/dev/null 2>&1 \
    || die "role agent_app exists on :${DEV_PG_PORT} with another password; align the role or use a dedicated server"
}

prepare() {
  say "host load: $(uptime | sed 's/.*load averages*: //')  (build and browser runs queue behind other sessions)"
  mkdir -p "$LOCAL"
  load_deps
  command -v psql >/dev/null || die "psql not found"
  docker ps --format '{{.Names}} {{.Status}}' | grep -q '^agent-local-postgres-dev-1 .*healthy' \
    || die "agent-local-postgres-dev-1 is not running/healthy (docker compose -f compose.local.yml up -d postgres-dev)"
  for p in "$IDP_PORT" "$LLM_PORT" "$STRATA_PORT"; do
    port_free "$p" || pid_alive "$LOCAL/idp.pid" || pid_alive "$LOCAL/llm.pid" || pid_alive "$LOCAL/strata.pid" || die "port $p is busy (lsof -iTCP:$p)"
  done
  for p in "$API_PORT" "$METRICS_PORT"; do
    port_free "$p" || die "port $p is busy (lsof -iTCP:$p); pick another with B8_API_PORT/B8_METRICS_PORT"
  done

  ensure_roles
  if [ "${B8_KEEP_DB:-0}" != "1" ]; then
    say "recreating database $DB_NAME on :$DEV_PG_PORT"
    psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS $DB_NAME WITH (FORCE)" -c "CREATE DATABASE $DB_NAME"
  fi

  # Keys are generated once per local dir: stable across runs, never printed, never committed.
  if [ ! -f "$LOCAL/keys.env" ]; then
    (
      umask 077
      {
        echo "LICENSE_TOKEN_ENC_KEY=$(openssl rand -base64 32)"
        echo "TENANT_DATA_KEYS='{\"b8\":\"$(openssl rand -base64 32)\"}'"
        echo "PROVIDER_SECRET_KEYS='{\"b8\":\"$(openssl rand -base64 32)\"}'"
      } >"$LOCAL/keys.env"
    )
  fi
  # LICENSE_TOKEN_ENC_KEY, TENANT_DATA_KEYS, PROVIDER_SECRET_KEYS
  set -a
  # shellcheck disable=SC1091
  . "$LOCAL/keys.env"
  set +a

  # one static token per tenant (rag and ingest share the value; the fake pins each token to its tenant)
  STRATA_RAG_TOKENS="$PLATFORM_TENANT:b8-strata-platform,$DEMO_TENANT:b8-strata-demo,$OTHER_TENANT:b8-strata-other"
  stop_pid "$LOCAL/idp.pid"
  stop_pid "$LOCAL/llm.pid"
  stop_pid "$LOCAL/strata.pid"
  B8_IDP_PORT="$IDP_PORT" B8_IDP_KEY_FILE="$LOCAL/idp-key.pem" nohup node "$ROOT/scripts/local/b8-fake-idp.mjs" >"$LOCAL/idp.log" 2>&1 &
  echo $! >"$LOCAL/idp.pid"
  B8_LLM_PORT="$LLM_PORT" nohup node "$ROOT/scripts/local/b8-fake-llm.mjs" >"$LOCAL/llm.log" 2>&1 &
  echo $! >"$LOCAL/llm.pid"
  B8_STRATA_PORT="$STRATA_PORT" B8_STRATA_TOKENS="$STRATA_RAG_TOKENS" nohup node "$ROOT/scripts/local/b8-fake-strata.mjs" >"$LOCAL/strata.log" 2>&1 &
  echo $! >"$LOCAL/strata.pid"
  wait_http "http://127.0.0.1:$IDP_PORT/health" 20 "fake IdP"
  wait_http "http://127.0.0.1:$LLM_PORT/health" 20 "fake LLM"
  wait_http "http://127.0.0.1:$STRATA_PORT/health" 20 "fake Strata"
  say "fake IdP :$IDP_PORT, fake LLM :$LLM_PORT and fake Strata :$STRATA_PORT up"

  local targets
  targets="[{\"host\":\"127.0.0.1\",\"port\":$LLM_PORT,\"scheme\":\"http\",\"tenants\":[\"$PLATFORM_TENANT\",\"$DEMO_TENANT\",\"$OTHER_TENANT\"]}]"
  (
    umask 077
    cat >"$LOCAL/api.env" <<ENV
DATABASE_URL=postgres://agent_app:agent_app@127.0.0.1:${DEV_PG_PORT}/${DB_NAME}
MIGRATE_DATABASE_URL=${OWNER_URL}
AGENT_API_BIND=127.0.0.1:${API_PORT}
METRICS_BIND=127.0.0.1:${METRICS_PORT}
LOG_FORMAT=pretty
KEYCLOAK_ISSUER=http://127.0.0.1:${IDP_PORT}/realms/portal
KEYCLOAK_JWKS_URL=http://127.0.0.1:${IDP_PORT}/realms/portal/protocol/openid-connect/certs
KEYCLOAK_AUDIENCE=agent
AGENT_PUBLIC_URL=http://127.0.0.1:${WEB_PORT}
LICENSE_TOKEN_ENC_KEY=${LICENSE_TOKEN_ENC_KEY}
TENANT_DATA_KEYS='${TENANT_DATA_KEYS}'
TENANT_DATA_KEY_CURRENT=b8
PROVIDER_SECRET_KEYS='${PROVIDER_SECRET_KEYS}'
PROVIDER_SECRET_KEY_CURRENT=b8
KEYCLOAK_ADMIN_TOKEN_URL=http://127.0.0.1:${IDP_PORT}/token
KEYCLOAK_ADMIN_CLIENT_ID=agent-admin
KEYCLOAK_ADMIN_CLIENT_SECRET=b8-inert
KEYCLOAK_ADMIN_API_URL=http://127.0.0.1:${IDP_PORT}/admin/realms/portal
AGENT_KC_CLIENT_UUID=b8-inert
LICENSES_ADMIN_TOKEN_URL=http://127.0.0.1:${IDP_PORT}/token
LICENSES_ADMIN_CLIENT_ID=agent-licenses-admin
LICENSES_ADMIN_CLIENT_SECRET=b8-inert
LICENSES_ADMIN_URL=http://127.0.0.1:${IDP_PORT}
AGENT_PLATFORM_TENANT_ID=${PLATFORM_TENANT}
GENERATOR_PROVIDER_ID=${GEN_PROVIDER}
GENERATOR_MODEL=fake-gen
GENERATOR_DRAFT_TIMEOUT_SECS=60
STRATA_RAG_URL=http://127.0.0.1:${STRATA_PORT}
STRATA_INGEST_URL=http://127.0.0.1:${STRATA_PORT}
STRATA_RAG_TOKENS=${STRATA_RAG_TOKENS}
STRATA_INGEST_TOKENS=${STRATA_RAG_TOKENS}
# the platform tenant's gates (Laya) and so evidence classification: the fake LLM answers /v1/systemone
INTERNAL_LAYA_URL=http://127.0.0.1:${LLM_PORT}
INTERNAL_LAYA_KEY=b8-fake-laya-key
CONNECTOR_PRIVATE_TARGETS='${targets}'
ENV
  )
  say "wrote $LOCAL/api.env (holds generated keys; not for git)"
  cat <<MSG

Start agent-api in another terminal (this script never runs Cargo), then run '$0 test':

    ~/.claude/scripts/cargo-guard.py build -p agent-api --locked
    set -a; . '$LOCAL/api.env'; set +a
    exec '$MAIN/target/claude/debug/agent-api'

MSG
}

seed() {
  say "seeding tenants, users, licence verdicts and the generator provider"
  psql "$OWNER_URL" -v ON_ERROR_STOP=1 -q \
    -v platform="$PLATFORM_TENANT" -v demo="$DEMO_TENANT" -v other="$OTHER_TENANT" \
    -v provider="$GEN_PROVIDER" -v llm_url="http://127.0.0.1:$LLM_PORT" \
    -f "$ROOT/scripts/local/b8-seed.sql"
}

start_web() {
  [ -d "$ROOT/web/node_modules" ] || die "web/node_modules is missing (cd web && npm ci)"
  stop_pid "$LOCAL/web.pid"
  port_free "$WEB_PORT" || die "port $WEB_PORT is busy (lsof -iTCP:$WEB_PORT); pick another with B8_WEB_PORT"
  # Live mode = real sign-in against the fake IdP; /api is proxied to agent-api (vite.config.ts).
  (
    cd "$ROOT/web"
    AGENT_API_PROXY="http://127.0.0.1:$API_PORT" PUBLIC_AGENT_DATA=live \
      PUBLIC_KEYCLOAK_URL="http://127.0.0.1:$IDP_PORT" PUBLIC_KEYCLOAK_REALM=portal PUBLIC_KEYCLOAK_CLIENT_ID=agent \
      nohup npx vite dev --host 127.0.0.1 --port "$WEB_PORT" --strictPort >"$LOCAL/web.log" 2>&1 &
    echo $! >"$LOCAL/web.pid"
  )
  wait_http "http://127.0.0.1:$WEB_PORT/" 120 "web dev server"
  say "web up on :$WEB_PORT (proxying /api to :$API_PORT)"
}

run_specs() {
  if [ "${B8_BROWSER_LEASE:-}" != "held" ]; then
    say "STOP before the browser: acquire the machine-wide browser lease (browser-testing skill), then re-run with B8_BROWSER_LEASE=held"
    return 3
  fi
  say "running Playwright project b8-acceptance"
  local extra=()
  # shellcheck disable=SC2206
  [ -n "${B8_PLAYWRIGHT_ARGS:-}" ] && extra=(${B8_PLAYWRIGHT_ARGS})
  (
    cd "$ROOT/web"
    B8_WEB_URL="http://127.0.0.1:$WEB_PORT" B8_IDP_URL="http://127.0.0.1:$IDP_PORT" B8_LLM_URL="http://127.0.0.1:$LLM_PORT" \
      B8_OWNER_URL="$OWNER_URL" B8_DEMO_TENANT="$DEMO_TENANT" B8_OTHER_TENANT="$OTHER_TENANT" B8_PLATFORM_TENANT="$PLATFORM_TENANT" \
      npx playwright test -c playwright.b8.config.ts ${extra[@]+"${extra[@]}"}
  )
}

case "$MODE" in
  prepare) prepare ;;
  list) (cd "$ROOT/web" && npx playwright test -c playwright.b8.config.ts --list) ;;
  down)
    stop_pid "$LOCAL/web.pid"
    stop_pid "$LOCAL/idp.pid"
    stop_pid "$LOCAL/llm.pid"
    stop_pid "$LOCAL/strata.pid"
    say "stopped web + fakes (agent-api is yours to stop)"
    ;;
  test | all)
    load_deps
    [ "$MODE" = all ] && prepare
    if ! curl -fsS -o /dev/null "http://127.0.0.1:$API_PORT/health" 2>/dev/null; then
      say "waiting for agent-api /health on :$API_PORT (start it with the command above)"
      wait_http "http://127.0.0.1:$API_PORT/health" "${B8_API_WAIT:-1800}" "agent-api"
    fi
    seed
    start_web
    run_specs
    ;;
  *) die "usage: $0 [all|prepare|test|list|down]" ;;
esac
