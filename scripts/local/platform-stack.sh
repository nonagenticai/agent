#!/usr/bin/env bash
# Interactive PLATFORM STACK on this Mac: the real binaries, the fake IdP/LLM/Strata, the web dev server, so a human can
# click through the platform end to end. Never runs Cargo: BIN_DIR is a frozen dir from the integration owner.
#
#   scripts/local/platform-stack.sh up    --bin <dir> [--real-llm]   boot ONLY the platform (migrations, NO seed) + fakes
#   scripts/local/platform-stack.sh onboard                          the tenant-admin journey, steps 1-5 (see below), leaves
#                                                                    the stack ready for a human to chat (step 6)
#   scripts/local/platform-stack.sh smoke [--bin <dir>]              the whole journey headless (platform-stack-smoke.mjs)
#   scripts/local/platform-stack.sh status | logs <svc> [-f] | down | reset
#
# The platform pre-builds NO tenant, job, procedure or demo data. The journey is the owner's: a customer signs up
# (POST /api/signup-approvals/intake, the website/chat intake door), the platform admin (sa) approves it in the signups
# queue (POST /api/signup-approvals/{id}/approve), which creates the tenant, its tenant_admin identity and seat; that admin (ta) configures a job from platform templates, uploads own
# knowledge and a procedure, connects an integration (demo-backends is only the TARGET), sets channels and staff, tests,
# publishes; then a customer chats, asks for a person and a staff agent (ag) replies.
#
# <dir> must hold executables: agent-api agent-edge orchestrator runner interaction (demo-backends optional: fd7543d6 has none, the
# frozen $MAIN/target/local/platform-stack/bin-1650/demo-backends is used as the tenant's external system).
# Ports (internal = external; never 8000; all checked free by `up`; disjoint from kc-acceptance 8097-8099/9098-9099/
# 9352-9356, b8-acceptance 9350-9354 and web 3000, so they can run side by side):
#   web 3100 | agent-api 8198 (metrics 9198) | agent-edge 8199 (metrics 9199) | demo-backends 8197
#   fake IdP 9452 | fake LLM 9453 | fake Strata 9454 -> b8 strata 9456 | fake licences/KC-admin 9455 | fake artifacts 9457
# Dependencies (shared docker stack, target/local/dependencies.env sourced, never printed): postgres-dev :5433, redis-dev
# :6380. OWN databases: agent_platform_stack (api+daemons) and agent_platform_demo (demo-backends); Redis prefix ps:.
# `down` keeps the data; `reset` drops ONLY those two databases and deletes the ps:* keys (PORT_BASE / PS_DB_SUFFIX /
# PS_LOCAL_DIR: a second instance with shifted ports, its own databases + Redis prefix, its own state; see below).
# State: target/local/platform-stack/ (*.pid, *.log, keys.env 0600, *.env files that hold references only,
# fake-licences.json, harness-tenant). `up` writes no tenant to env; the ONE exception is the LOCAL-HARNESS-ONLY step after signup.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MAIN="$(cd "$(git -C "$ROOT" rev-parse --git-common-dir)/.." && pwd)"
LOCAL="${PS_LOCAL_DIR:-$MAIN/target/local/platform-stack}"
DEPS="$MAIN/target/local/dependencies.env"
PG=5433; REDIS=6380
# A second instance (e.g. the web e2e run) beside this default one: PORT_BASE shifts every port below, PS_DB_SUFFIX
# names its own databases and Redis prefix, PS_LOCAL_DIR its own state dir. All three default to the instance above.
PORT_BASE="${PORT_BASE:-0}"; PS_DB_SUFFIX="${PS_DB_SUFFIX:-}"
[[ "$PORT_BASE" =~ ^[0-9]{1,4}$ ]] || { echo "[ps] ERROR: PORT_BASE must be 0-9999" >&2; exit 1; }
[[ "$PS_DB_SUFFIX" =~ ^[a-z0-9_]{0,20}$ ]] || { echo "[ps] ERROR: PS_DB_SUFFIX must match [a-z0-9_]{0,20}" >&2; exit 1; }
WEB=$((3100 + PORT_BASE)); API_P=$((8198 + PORT_BASE)); API_M=$((9198 + PORT_BASE)); EDGE_P=$((8199 + PORT_BASE))
EDGE_M=$((9199 + PORT_BASE)); DEMO=$((8197 + PORT_BASE)); B4DEMO=$((8187 + PORT_BASE))
IDP=$((9452 + PORT_BASE)); LLM=$((9453 + PORT_BASE)); STRATA=$((9454 + PORT_BASE)); ADMIN=$((9455 + PORT_BASE))
B8STRATA=$((9456 + PORT_BASE)); ARTIFACTS=$((9457 + PORT_BASE))
for p in "$WEB" "$API_P" "$API_M" "$EDGE_P" "$EDGE_M" "$DEMO" "$IDP" "$LLM" "$STRATA" "$ADMIN" "$B8STRATA" "$ARTIFACTS" "$B4DEMO"; do
  [ "$p" -ne 8000 ] && [ "$p" -le 65535 ] || { echo "[ps] ERROR: PORT_BASE $PORT_BASE puts a service on port $p (8000 is never used; max 65535)" >&2; exit 1; }
done
RPFX="ps${PS_DB_SUFFIX}:"
PLATFORM_TENANT=5b1a7c10-0000-4000-8000-0000000000a1   # D70 platform-owned tenant id (config only; its tenants row is a bootstrap item)
DB=agent_platform_stack$PS_DB_SUFFIX; DEMO_DB=agent_platform_demo$PS_DB_SUFFIX; B4_DB=agent_platform_b4demo$PS_DB_SUFFIX
API="http://127.0.0.1:$API_P"; EDGE="http://localhost:$EDGE_P"; WEBURL="http://127.0.0.1:$WEB"
REAL_LLM_URL="${PS_REAL_LLM_URL:-}"; REAL_LLM_MODEL="${PS_REAL_LLM_MODEL:-local-llm}"
# Port of an LLM base URL (loopback fake or PS_REAL_LLM_URL), for the harness private-target entry.
llm_port() { local hp="${1#*://}"; hp="${hp%%/*}"; case "$hp" in *:*) echo "${hp##*:}" ;; *) case "$1" in https:*) echo 443 ;; *) echo 80 ;; esac ;; esac; }
SVCS=(idp admin llm b8strata strata artifacts demo relay6 b4demo b4relay6 api orchestrator runner interaction edge web)

say() { printf '[ps] %s\n' "$*"; }
die() { printf '[ps] ERROR: %s\n' "$*" >&2; exit 1; }
port_free() { ! lsof -iTCP:"$1" -sTCP:LISTEN -nP >/dev/null 2>&1; }
alive() { [ -f "$LOCAL/$1.pid" ] && kill -0 "$(cat "$LOCAL/$1.pid")" 2>/dev/null; }
wait_http() { local i=0; until [ "$(curl -s -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || true)" != 000 ]; do
  i=$((i+2)); [ $i -ge "$2" ] && die "$3 did not answer $1 in ${2}s (log: $LOCAL/$3.log)"; sleep 2; done; }
load_deps() { [ -f "$DEPS" ] || die "missing $DEPS (managed dependency stack not provisioned)"; set -a; . "$DEPS"; set +a
  : "${AGENT_DEV_POSTGRES_PASSWORD:?missing in dependencies.env}" "${AGENT_DEV_REDIS_PASSWORD:?missing in dependencies.env}" "${AGENT_DEMO_POSTGRES_PASSWORD:?missing in dependencies.env}"
  ADMIN_URL="postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:$PG/postgres"
  OWNER_URL="postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:$PG/$DB"; }
mint() { curl -fsS "http://127.0.0.1:$IDP/__mint?persona=${1:-sa}" | node -p 'JSON.parse(require("fs").readFileSync(0,"utf8")).access_token'; }
binary() { [ -x "$BIN_DIR/$1" ] || die "$BIN_DIR/$1 is not executable"; echo "$BIN_DIR/$1"; }

# launch <name> <envfile|-> <cmd...>: background, own log, pid recorded; the subshell execs so $! IS the service
launch() {
  local name="$1" envf="$2"; shift 2
  ( set -a; . "$DEPS"; . "$LOCAL/keys.env"; [ "$envf" = - ] || . "$LOCAL/$envf"; set +a
    exec nohup "$@" ) >>"$LOCAL/$name.log" 2>&1 &
  echo $! > "$LOCAL/$name.pid"
}

stop_all() {
  [ -d "$LOCAL" ] || return 0
  local n pid
  for n in "${SVCS[@]}"; do
    [ -f "$LOCAL/$n.pid" ] || continue
    pid="$(cat "$LOCAL/$n.pid")"
    if kill -0 "$pid" 2>/dev/null; then pkill -P "$pid" 2>/dev/null || true; kill "$pid" 2>/dev/null && say "stopped $n ($pid)"; fi
    rm -f "$LOCAL/$n.pid"
  done
}

gen_keys() {
  mkdir -p "$LOCAL"
  if [ -f "$LOCAL/keys.env" ]; then   # older state dir: add the fake bundle-token secret once
    grep -q '^PS_BUNDLE_SECRET=' "$LOCAL/keys.env" || ( umask 077; echo "PS_BUNDLE_SECRET=$(openssl rand -hex 32)" >> "$LOCAL/keys.env" )
    return
  fi
  ( umask 077; {
    echo "LICENSE_TOKEN_ENC_KEY=$(openssl rand -base64 32)"
    echo "TENANT_DATA_KEYS='{\"loc\":\"$(openssl rand -base64 32)\"}'"
    echo "TENANT_DATA_KEY_CURRENT=loc"
    echo "PROVIDER_SECRET_KEYS='{\"loc\":\"$(openssl rand -base64 32)\"}'"
    echo "PROVIDER_SECRET_KEY_CURRENT=loc"
    echo "EDGE_INTERNAL_KEY=$(openssl rand -hex 24)"
    echo "DEMO_BACKENDS_TOKEN=$(openssl rand -hex 24)"
    echo "PS_BUNDLE_SECRET=$(openssl rand -hex 32)"
  } > "$LOCAL/keys.env"; )
}

init_db() {  # idempotent; creates only the two platform-stack databases and the shared service roles when missing
  command -v psql >/dev/null || die "psql not found"
  psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q <<SQL
\set demopw \`printf %s "\$AGENT_DEMO_POSTGRES_PASSWORD"\`
SELECT 'CREATE ROLE agent_app LOGIN PASSWORD ''agent_app'' NOSUPERUSER NOBYPASSRLS' WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='agent_app') \gexec
SELECT 'CREATE ROLE agent_runtime LOGIN PASSWORD ''agent_runtime'' NOSUPERUSER NOBYPASSRLS' WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='agent_runtime') \gexec
SELECT 'CREATE ROLE agent_platform NOLOGIN NOSUPERUSER NOBYPASSRLS' WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='agent_platform') \gexec
SELECT format('CREATE ROLE agent_demo LOGIN PASSWORD %L NOSUPERUSER NOBYPASSRLS', :'demopw') WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='agent_demo') \gexec
GRANT agent_platform TO agent_app WITH INHERIT FALSE, SET TRUE;
SELECT 'CREATE DATABASE $DB' WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname='$DB') \gexec
SELECT 'CREATE DATABASE $DEMO_DB OWNER agent_demo' WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname='$DEMO_DB') \gexec
GRANT CONNECT ON DATABASE $DB TO agent_app, agent_runtime;
REVOKE CONNECT ON DATABASE $DB FROM PUBLIC;
GRANT CONNECT ON DATABASE $DEMO_DB TO agent_demo;
REVOKE CONNECT ON DATABASE $DEMO_DB FROM PUBLIC;
SQL
  say "databases ok: $DB, $DEMO_DB (locked)"
}

# No tenant lives in env at `up`. The fake strata needs a non-empty token list to boot (this placeholder matches no tenant).
FAKE_STRATA_TOKENS="$PLATFORM_TENANT:ps-strata-platform"

# ===== LOCAL-HARNESS ONLY ===================================================================================================
# fd7543d6: the tenant's LLM and strata access are per-tenant BUNDLE tokens minted by signup provisioning (BUNDLE_MINTER_*
# -> the fake licences minter in kc-fake-idp.mjs), so no strata token pair and no LLM env is written per tenant any more.
# What stays local-only: the integration target (demo-backends, the tenant's own external REST system) is on 127.0.0.1,
# which netguard refuses for every tenant unless the operator lists that tenant in CONNECTOR_PRIVATE_TARGETS (in prod an
# external system is public https). So AFTER signup the harness writes the NEW tenant uuid into that one var ONCE and
# restarts the services that read it (harness_tenant). netguard and the code are unchanged.
harness_tenant_id() { [ -f "$LOCAL/harness-tenant" ] && cat "$LOCAL/harness-tenant" || true; }
harness_env_append() {  # harness_env_append <envfile> api|daemon
  local t; t="$(harness_tenant_id)"; [ -n "$t" ] || return 0
  # An external MCP system under test (the conveyor) is loopback too: opt in with PS_CONVEYOR_PORT, or take the
  # port from PS_CONVEYOR_URL, so netguard's exact host/port match admits it for this tenant only.
  local cvport="${PS_CONVEYOR_PORT:-}"
  if [ -n "$cvport" ]; then
    case "$cvport" in *[!0-9]*) die "PS_CONVEYOR_PORT is not a port: $cvport" ;; esac
  fi
  if [ -z "$cvport" ] && [ -n "${PS_CONVEYOR_URL:-}" ]; then
    cvport="${PS_CONVEYOR_URL#*://}"; cvport="${cvport%%/*}"; cvport="${cvport##*:}"
    case "$cvport" in ''|*[!0-9]*) die "PS_CONVEYOR_URL does not carry a port" ;; esac
  fi
  local conveyor=""
  if [ -n "$cvport" ]; then
    conveyor=",{\"host\":\"127.0.0.1\",\"port\":$cvport,\"scheme\":\"http\",\"tenants\":[\"$t\"]}"
  fi
  {
    echo "# LOCAL-HARNESS ONLY: tenant $t written after signup; not product behaviour"
    echo "CONNECTOR_PRIVATE_TARGETS='[{\"host\":\"localhost\",\"port\":$DEMO,\"scheme\":\"http\",\"tenants\":[\"$t\"]},{\"host\":\"localhost\",\"port\":$B4DEMO,\"scheme\":\"http\",\"tenants\":[\"$t\"]},{\"host\":\"127.0.0.1\",\"port\":$LLM_PORT_USED,\"scheme\":\"http\",\"tenants\":[\"$t\"]}${conveyor}]'"
  } >> "$1"
}
harness_tenant() {  # harness-tenant <uuid>: ONCE per tenant, from the smoke right after signup
  local id="${1:-}" n pid bin T="$ROOT/scripts/local"
  printf '%s' "$id" | grep -Eq '^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$' || die "usage: $0 harness-tenant <tenant-uuid>"
  alive api || die "stack is not running (up first)"
  [ "$(harness_tenant_id)" = "$id" ] && grep -q "\"port\":$B4DEMO" "$LOCAL/api.env" 2>/dev/null && { say "harness env already holds tenant $id"; return 0; }
  load_deps
  printf '%s\n' "$id" > "$LOCAL/harness-tenant"
  LLM_URL="$(sed -n 1p "$LOCAL/llm.info")"; LLM_MODEL="$(sed -n 2p "$LOCAL/llm.info")"
  LLM_PORT_USED="$(llm_port "$LLM_URL")"
  write_envs
  bin="$(cat "$LOCAL/bin.dir")"; BIN_DIR="$bin"
  for n in api orchestrator runner interaction; do   # b8strata keeps running: its in-memory stores must survive. edge, web, fakes and demo-backends do not read these vars
    if alive "$n"; then pid="$(cat "$LOCAL/$n.pid")"; pkill -P "$pid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true; fi
    rm -f "$LOCAL/$n.pid"
  done
  sleep 1
  launch api api.env "$(binary agent-api)"
  for n in orchestrator runner interaction; do launch "$n" daemon.env "$(binary "$n")"; done
  wait_http "$API/ready" "${PS_API_WAIT:-300}" api
  sleep 3; for n in orchestrator runner interaction; do alive "$n" || die "$n exited after the harness restart (log: $LOCAL/$n.log)"; done
  say "LOCAL-HARNESS ONLY: tenant $id written to api/daemon env (CONNECTOR_PRIVATE_TARGETS only); restarted api orchestrator runner interaction"
}

write_envs() {  # references only; secrets come from dependencies.env + keys.env, sourced by launch()
  cat > "$LOCAL/api.env" <<ENV
DATABASE_URL=postgres://agent_app:agent_app@127.0.0.1:$PG/$DB
MIGRATE_DATABASE_URL=postgres://agent_dev_owner:\${AGENT_DEV_POSTGRES_PASSWORD}@127.0.0.1:$PG/$DB
REDIS_URL=redis://:\${AGENT_DEV_REDIS_PASSWORD}@127.0.0.1:$REDIS/0
AGENT_PREFIX=$RPFX
AGENT_API_BIND=127.0.0.1:$API_P
METRICS_BIND=127.0.0.1:$API_M
LOG_FORMAT=pretty
ARTIFACTS_URL=http://127.0.0.1:$ARTIFACTS
KEYCLOAK_ISSUER=http://127.0.0.1:$IDP/realms/portal
KEYCLOAK_JWKS_URL=http://127.0.0.1:$IDP/realms/portal/protocol/openid-connect/certs
KEYCLOAK_AUDIENCE=agent
AGENT_PUBLIC_URL=$WEBURL
KEYCLOAK_ADMIN_TOKEN_URL=http://127.0.0.1:$ADMIN/token
KEYCLOAK_ADMIN_CLIENT_ID=agent-admin
KEYCLOAK_ADMIN_CLIENT_SECRET=local-inert
KEYCLOAK_ADMIN_API_URL=http://127.0.0.1:$ADMIN/admin/realms/portal
AGENT_KC_CLIENT_UUID=kc-inert-client
AGENT_PLATFORM_TENANT_ID=$PLATFORM_TENANT
LICENSES_ADMIN_TOKEN_URL=http://127.0.0.1:$ADMIN/token
LICENSES_ADMIN_CLIENT_ID=agent-licenses-admin
LICENSES_ADMIN_CLIENT_SECRET=local-inert
LICENSES_ADMIN_URL=http://127.0.0.1:$ADMIN
LICENSE_INTROSPECT_URL=http://127.0.0.1:$ADMIN/license/introspect
EDGE_DELIVER_URL=http://127.0.0.1:$EDGE_P
WEBCHAT_PUBLIC_BASE=$EDGE
DEMO_BACKENDS_URL=http://127.0.0.1:$DEMO
STRATA_RAG_URL=http://127.0.0.1:$STRATA
STRATA_INGEST_URL=http://127.0.0.1:$STRATA
# the API refuses to boot with STRATA_RAG_URL and no token pair; the platform tenant (operator-owned) holds the only static
# one. A signed-up tenant has none: its strata calls carry a bundle token minted by the fake licences (fd7543d6)
STRATA_RAG_TOKENS=$PLATFORM_TENANT:ps-strata-platform
STRATA_INGEST_TOKENS=$PLATFORM_TENANT:ps-strata-platform
EARLY_ACCESS_SIGNUP_ENABLED=true
BUNDLE_MINTER_LICENSES_URL=http://127.0.0.1:$ADMIN
BUNDLE_MINTER_TOKEN_URL=http://127.0.0.1:$ADMIN/token
BUNDLE_MINTER_CLIENT_SECRET=local-inert
BUNDLE_MINTER_CLIENT_ID=agent-bundle-minter
LICENSE_BUNDLE_KEY=agent
BUNDLE_TOKEN_TTL_MINUTES=15
INTERNAL_LLM_URL=$LLM_URL
INTERNAL_LLM_MODEL=$LLM_MODEL
ENV
  harness_env_append "$LOCAL/api.env" api
  cat > "$LOCAL/daemon.env" <<ENV
DATABASE_URL=postgres://agent_runtime:agent_runtime@127.0.0.1:$PG/$DB
REDIS_URL=redis://:\${AGENT_DEV_REDIS_PASSWORD}@127.0.0.1:$REDIS/0
AGENT_PREFIX=$RPFX
LOG_FORMAT=pretty
INTERNAL_LLM_URL=$LLM_URL
INTERNAL_LLM_MODEL=$LLM_MODEL
STRATA_RAG_URL=http://127.0.0.1:$STRATA
STRATA_INGEST_URL=http://127.0.0.1:$STRATA
STRATA_RAG_TOKENS=$PLATFORM_TENANT:ps-strata-platform
STRATA_INGEST_TOKENS=$PLATFORM_TENANT:ps-strata-platform
BUNDLE_MINTER_LICENSES_URL=http://127.0.0.1:$ADMIN
BUNDLE_MINTER_TOKEN_URL=http://127.0.0.1:$ADMIN/token
BUNDLE_MINTER_CLIENT_SECRET=local-inert
BUNDLE_MINTER_CLIENT_ID=agent-bundle-minter
LICENSE_BUNDLE_KEY=agent
BUNDLE_TOKEN_TTL_MINUTES=15
ENV
  harness_env_append "$LOCAL/daemon.env" daemon
  cat > "$LOCAL/edge.env" <<ENV
EDGE_BIND=127.0.0.1:$EDGE_P
EDGE_METRICS_BIND=127.0.0.1:$EDGE_M
LOG_FORMAT=pretty
EDGE_CORE_URL=http://127.0.0.1:$API_P
EDGE_ANSWER_TIMEOUT_S=120
WEBCHAT_PUBLIC_URL=$EDGE
ENV
  cat > "$LOCAL/demo.env" <<ENV
DATABASE_URL=postgres://agent_demo:\${AGENT_DEMO_POSTGRES_PASSWORD}@127.0.0.1:$PG/$DEMO_DB
DEMO_BACKENDS_BIND=127.0.0.1:$DEMO
LOG_FORMAT=pretty
ENV
}

parse_up_args() {
  BIN_DIR=""; REAL=0
  while [ $# -gt 0 ]; do case "$1" in
    --bin) BIN_DIR="${2:-}"; shift 2 ;;
    --real-llm) REAL=1; shift ;;
    *) die "unknown argument $1" ;; esac; done
}

up() {
  parse_up_args "$@"
  [ -n "$BIN_DIR" ] || die "up needs --bin <dir> with agent-api agent-edge orchestrator runner interaction demo-backends"
  BIN_DIR="$(cd "$BIN_DIR" && pwd)"
  command -v node >/dev/null && [ "$(node -p 'process.versions.node.split(".")[0]')" -ge 24 ] || die "node >= 24 needed"
  command -v npm >/dev/null || die "npm not found"
  local webdir="${PS_WEB_DIR:-$ROOT/web}"
  [ -x "$webdir/node_modules/.bin/vite" ] || die "$webdir/node_modules/.bin/vite missing: install the web dependencies THERE (cd $webdir && npm ci); this script never installs into another worktree"
  local n; for n in agent-api agent-edge orchestrator runner interaction; do binary "$n" >/dev/null; done
  # fd7543d6 has no demo-backends crate. It stays the tenant's EXTERNAL REST system (from the platform: a customer endpoint),
  # taken from the frozen bin-1650 build when --bin does not hold it.
  DEMO_BIN="$BIN_DIR/demo-backends"
  if [ ! -x "$DEMO_BIN" ]; then
    DEMO_BIN="$MAIN/target/local/platform-stack/bin-1650/demo-backends"
    [ -x "$DEMO_BIN" ] || die "no demo-backends in $BIN_DIR and no fallback at $DEMO_BIN"
    say "demo-backends not in --bin: using the fallback $DEMO_BIN (the tenant's external system, not part of the platform)"
  fi
  load_deps
  docker ps --format '{{.Names}} {{.Status}}' | grep -q '^agent-local-postgres-dev-1 .*healthy' || die "postgres-dev is not healthy (docker compose -f compose.local.yml up -d postgres-dev redis-dev)"
  docker ps --format '{{.Names}} {{.Status}}' | grep -q '^agent-local-redis-dev-1 .*healthy' || die "redis-dev is not healthy"
  gen_keys
  stop_all   # restart semantics: env files are rewritten, every service must pick them up
  local ports=($WEB $API_P $API_M $EDGE_P $EDGE_M $DEMO $IDP $ADMIN $STRATA $B8STRATA $ARTIFACTS) p
  if [ "$REAL" = 1 ]; then
    # The pgx vLLM is reachable only behind inference-router's licence gate (pgx-vllm-guard.nft rejects
    # SSH forwards); never tunnel around it. Real-model acceptance runs on the deployed licensed route.
    [ -n "$REAL_LLM_URL" ] || die "--real-llm needs PS_REAL_LLM_URL (an OpenAI-compatible /v1 you are entitled to; no pgx tunnel)"
    LLM_URL="${REAL_LLM_URL%/}"; LLM_MODEL="$REAL_LLM_MODEL"; LLM_PORT_USED="$(llm_port "$LLM_URL")"
    curl -fsS -m 10 "$LLM_URL/models" 2>/dev/null | grep -q "\"$LLM_MODEL\"" \
      || die "real LLM $LLM_URL does not answer / serve $LLM_MODEL"
  else
    LLM_URL="http://127.0.0.1:$LLM/v1"; LLM_MODEL="local-llm"; LLM_PORT_USED=$LLM; ports+=($LLM)
  fi
  for p in "${ports[@]}"; do
    [ "$p" = 8000 ] && die "port 8000 is never used"
    port_free "$p" || die "port $p is busy (lsof -iTCP:$p -sTCP:LISTEN -nP)"
  done
  rm -f "$LOCAL/harness-tenant"
  init_db; write_envs
  printf '%s' "$BIN_DIR" > "$LOCAL/bin.dir"
  printf '%s\n%s\n' "$LLM_URL" "$LLM_MODEL" > "$LOCAL/llm.info"
  local T="$ROOT/scripts/local"
  launch idp - env B8_IDP_PORT=$IDP B8_IDP_KEY_FILE="$LOCAL/idp-key.pem" node "$T/b8-fake-idp.mjs"
  launch admin - env KC_IDP_ADMIN_PORT=$ADMIN KC_FAKE_STATE="$LOCAL/fake-licences.json" node "$T/kc-fake-idp.mjs"
  [ "$REAL" = 1 ] && say "WARN: --real-llm: the real model does not enforce fake bundle tokens, so signup provisioning fails the inference-router negative control (app_not_enforcing); use the fake LLM for the smoke"
  [ "$REAL" = 1 ] || launch llm - env KC_LLM_PORT=$LLM node "$T/kc-fake-llm.mjs"
  launch b8strata - env B8_STRATA_PORT=$B8STRATA B8_STRATA_TOKENS="$FAKE_STRATA_TOKENS" node "$T/b8-fake-strata.mjs"
  launch strata - env KC_STRATA_PORT=$STRATA KC_STRATA_UPSTREAM=http://127.0.0.1:$B8STRATA node "$T/kc-fake-strata.mjs"
  # the platform artifact_service (conversation attachments, SW-9), in memory
  launch artifacts - env PS_ARTIFACTS_PORT=$ARTIFACTS node "$T/ps-fake-artifacts.mjs"
  for p in $IDP $ADMIN $STRATA $B8STRATA $ARTIFACTS; do wait_http "http://127.0.0.1:$p/health" 20 "fake:$p"; done
  [ "$REAL" = 1 ] || wait_http "http://127.0.0.1:$LLM/health" 20 "llm"
  say "fakes up; LLM $LLM_URL"
  launch demo demo.env "$DEMO_BIN"; wait_http "http://127.0.0.1:$DEMO/health" 60 demo
  # connectors call localhost:$DEMO and localhost resolves to ::1 first; demo-backends binds 127.0.0.1 only
  launch relay6 - env KC_RELAY_PORT=$DEMO node "$T/kc-loopback6.mjs"
  launch api api.env "$(binary agent-api)"; wait_http "$API/health" "${PS_API_WAIT:-300}" api   # migrates on boot (no seed)
  for n in orchestrator runner interaction; do launch "$n" daemon.env "$(binary "$n")"; done
  launch edge edge.env "$(binary agent-edge)"; wait_http "http://127.0.0.1:$EDGE_P/health" 60 edge
  sleep 3; for n in orchestrator runner interaction; do alive "$n" || die "$n exited at start (log: $LOCAL/$n.log)"; done
  ( cd "${PS_WEB_DIR:-$ROOT/web}"   # PS_WEB_DIR: serve another checkout's web (e.g. a lane worktree)
    set -a; . "$LOCAL/keys.env"; set +a
    AGENT_API_PROXY="$API" FIXTURE_AUTH="" PUBLIC_AGENT_DATA=live \
      PUBLIC_KEYCLOAK_URL="http://127.0.0.1:$IDP" PUBLIC_KEYCLOAK_REALM=portal PUBLIC_KEYCLOAK_CLIENT_ID=agent \
      exec nohup ./node_modules/.bin/vite dev --host 127.0.0.1 --port "$WEB" --strictPort ) >>"$LOCAL/web.log" 2>&1 &
  echo $! > "$LOCAL/web.pid"
  wait_http "$WEBURL/" 120 web
  click_through
}

click_through() {
  cat <<MSG

==================== PLATFORM STACK READY (platform only, no tenant yet) ====================
 Web (admin + staff console)  $WEBURL     API $API     edge $EDGE     LLM $LLM_URL ($LLM_MODEL)
 logs: $LOCAL/<svc>.log     stop: $0 down     the whole journey headless: $0 smoke

 Logins (fake IdP, no passwords): cookie b8_persona on host 127.0.0.1 (DevTools console, then reload):
     document.cookie='b8_persona=sa; path=/'      sa = platform admin (default)
 A tenant's own users are real identities made by the product: after step 1/4 below, '$0 onboard' prints
 their cookies (b8_persona + b8_sub + b8_email) for the tenant admin and the staff agent.

 The journey (steps 1-5 are what '$0 onboard' does for you over the API; do them by hand in the web UI if you prefer):
  0. Platform admin (sa), empty database: author layers/job template ($WEBURL/admin/platform/layers, templates) and platform
     knowledge ($WEBURL/admin/platform/knowledge: store, document, pack). Connector templates: $WEBURL/platform/connector-templates (Create template, add mappings and fixtures, Publish).
  1. A customer signs up, the platform admin approves (the real onboarding; no env edit, no restart): the website/chat
     intake posts POST $API/api/signup-approvals/intake with the intake service token (curl -s -XPOST
     "http://127.0.0.1:$IDP/__mint?persona=intake" gives it; body {"email","name","company","source":"web"}). As sa open
     $WEBURL/platform/signups (the queue), Approve: that creates the tenant, its tenant_admin identity and seat (about a
     minute in prod). Sign in as that admin: its sub/email come from the fake Keycloak admin (:$ADMIN/__state).
     LOCAL-HARNESS ONLY: right after approval run  $0 harness-tenant <tenant-uuid>  (once; '$0 onboard' does it for you). It writes
     the tenant into the api/daemon env (CONNECTOR_PRIVATE_TARGETS for the loopback demo-backends) and restarts api + daemons.
     In prod the LLM, integration endpoints and strata tokens are not loopback: this is not product behaviour.
  2. Tenant admin: $WEBURL/admin/jobs -> "Create job from template"; bind the platform-offered knowledge
     ($WEBURL/admin/jobs/<job>/knowledge-kinds), upload your own documents ($WEBURL/admin/knowledge) and author a
     procedure ($WEBURL/admin/jobs/<job>/procedures).
  3. Integrations ($WEBURL/admin/connections): add a system from the demo_retail template (a test stub and a prod one
     pointed at http://localhost:$DEMO/api, token = DEMO_BACKENDS_TOKEN in $LOCAL/keys.env), run the contract tests, route
     the job's operations ($WEBURL/admin/jobs/<job>/tools).
  4. Channels: $WEBURL/admin/jobs/<job>/channels (web chat binding + identity secret); staff team ($WEBURL/admin/company or
     the job page: invite an agent, add them to the job staff).
  5. Test (staff test chat on the job page, Test env), make readiness clear ($WEBURL/admin/jobs/<job>/readiness),
     publish ($WEBURL/admin/jobs/<job>/release: cut, apply).
  6. Customer: the web chat page printed by onboard ($EDGE/webchat/demo?key=...): pick a customer, ask where the parcel is,
     then say "I would like to speak to a person". As the staff agent open $WEBURL/support, Take it, reply; the reply
     shows in the customer's chat window.
==============================================================================================
MSG
}

status() {
  [ -d "$LOCAL" ] || { say "no state dir $LOCAL (never started)"; return 0; }
  local n pid st
  for n in "${SVCS[@]}"; do [ -f "$LOCAL/$n.pid" ] || continue
    pid="$(cat "$LOCAL/$n.pid")"; st=dead; kill -0 "$pid" 2>/dev/null && st=up
    printf '%-13s pid %-7s %s\n' "$n" "$pid" "$st"; done
  local u; for u in "api $API/health" "edge http://127.0.0.1:$EDGE_P/health" "demo http://127.0.0.1:$DEMO/health" "idp http://127.0.0.1:$IDP/health" "web $WEBURL/"; do
    printf '%-13s http %s\n' "${u%% *}" "$(curl -s -o /dev/null -m 3 -w '%{http_code}' "${u#* }" 2>/dev/null || true)"; done
}

journey() {  # journey <until-step> [script]: the owner's tenant-admin journey over the API (or another scenario on the same stack)
  [ -f "$LOCAL/llm.info" ] || die "stack was never started (up first)"
  load_deps
  . "$LOCAL/keys.env"
  local dp="${PS_B4_DEMO:-$DEMO}"   # b4 exports PS_B4_DEMO: ps-lib's demoAdmin/demoCustomers (PS_DEMO_DIRECT) then hit b4's own demo-backends
  PS_API="$API" PS_EDGE="http://127.0.0.1:$EDGE_P" PS_IDP="http://127.0.0.1:$IDP" PS_KCADMIN="http://127.0.0.1:$ADMIN" \
    PS_DEMO_URL="http://localhost:$dp/api" PS_DEMO_DIRECT="http://127.0.0.1:$dp/api" PS_DEMO_TOKEN="$DEMO_BACKENDS_TOKEN" PS_OPDEFS="${PS_OPDEFS:-$(dirname "${PS_WEB_DIR:-$ROOT/web}")/crates/agent-api/contracts/opdefs.json}" PS_SELF="$0" PS_UNTIL="$1" \
    PS_STATE="$LOCAL/journey.json" PS_LLM_URL="$(sed -n 1p "$LOCAL/llm.info")" PS_LLM_MODEL="$(sed -n 2p "$LOCAL/llm.info")" \
    PS_REPORT="$LOCAL/${3:-journey}-report.json" node "$ROOT/scripts/local/${2:-platform-stack-smoke.mjs}"
}

b4_demo_up() {  # b4's OWN demo-backends (+ its ::1 relay): own port, own locked DB, own pid/log. Idempotent: healthy -> reused.
  load_deps; . "$LOCAL/keys.env"
  [ "$B4DEMO" = 8000 ] && die "port 8000 is never used"
  if alive b4demo && alive b4relay6 && [ "$(curl -s -o /dev/null -m 3 -w '%{http_code}' "http://127.0.0.1:$B4DEMO/health" 2>/dev/null || true)" = 200 ]; then
    say "b4 demo-backends already healthy on :$B4DEMO"; return 0
  fi
  local n pid
  for n in b4relay6 b4demo; do   # half-up: stop what is ours, then start clean
    if alive "$n"; then pid="$(cat "$LOCAL/$n.pid")"; pkill -P "$pid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true; fi
    rm -f "$LOCAL/$n.pid"
  done
  sleep 1
  port_free "$B4DEMO" || die "b4 port $B4DEMO is taken by something that is not ours (lsof -iTCP:$B4DEMO -sTCP:LISTEN -nP)"
  ! docker ps --format '{{.Ports}}' | grep -Eq "(:|^)$B4DEMO->" || die "b4 port $B4DEMO is published by a docker container (docker ps --format '{{.Names}} {{.Ports}}')"
  local bin="" ; [ -f "$LOCAL/bin.dir" ] && bin="$(cat "$LOCAL/bin.dir")"
  local dbin="$bin/demo-backends"
  if [ ! -x "$dbin" ]; then   # same fallback as `up`
    dbin="$MAIN/target/local/platform-stack/bin-1650/demo-backends"
    [ -x "$dbin" ] || die "no demo-backends in '$bin' and no fallback at $dbin"
  fi
  command -v psql >/dev/null || die "psql not found"
  psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q <<SQL
SELECT 'CREATE DATABASE $B4_DB OWNER agent_demo' WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname='$B4_DB') \gexec
GRANT CONNECT ON DATABASE $B4_DB TO agent_demo;
REVOKE CONNECT ON DATABASE $B4_DB FROM PUBLIC;
SQL
  say "database ok: $B4_DB (locked, owner agent_demo, dev :$PG)"
  cat > "$LOCAL/b4demo.env" <<ENV
DATABASE_URL=postgres://agent_demo:\${AGENT_DEMO_POSTGRES_PASSWORD}@127.0.0.1:$PG/$B4_DB
DEMO_BACKENDS_BIND=127.0.0.1:$B4DEMO
LOG_FORMAT=pretty
ENV
  launch b4demo b4demo.env "$dbin"; wait_http "http://127.0.0.1:$B4DEMO/health" 60 b4demo
  launch b4relay6 - env KC_RELAY_PORT=$B4DEMO node "$ROOT/scripts/local/kc-loopback6.mjs"
  sleep 1; alive b4relay6 || die "b4 relay exited (log: $LOCAL/b4relay6.log)"
  say "b4 demo-backends up on :$B4DEMO (log $LOCAL/b4demo.log)"
}

b4_netguard_check() {  # the API and daemons read CONNECTOR_PRIVATE_TARGETS at boot only: b4 never restarts them
  local n pid miss=""
  for n in api orchestrator runner interaction; do
    alive "$n" || die "$n is not running (up first)"
    pid="$(cat "$LOCAL/$n.pid")"
    ps eww -p "$pid" 2>/dev/null | grep -q "\"port\":$B4DEMO" || miss="$miss $n"
  done
  [ -z "$miss" ] || die "running${miss} lack localhost:$B4DEMO in CONNECTOR_PRIVATE_TARGETS (read at boot only): run harness-tenant / restart api to pick up B4 port: $0 harness-tenant $(harness_tenant_id)"
}

b4() {  # b4: B4 runtime acceptance (agent-4e) on the tenant + job the smoke built, against b4's OWN demo-backends
  [ -f "$LOCAL/journey.json" ] || die "no journey state: run smoke first"
  [ -f "$ROOT/scripts/local/platform-stack-b4.mjs" ] || die "scripts/local/platform-stack-b4.mjs is missing (agent-4e branch feat/platform-stack-b4 not folded)"
  alive api || die "stack is not running (up first)"
  [ -n "$(harness_tenant_id)" ] || die "no harness tenant: run $0 harness-tenant <tenant-uuid> first"
  b4_netguard_check
  b4_demo_up
  export PS_B4_DEMO="$B4DEMO" PS_B4_DEMO_URL="http://localhost:$B4DEMO/api" PS_B4_DEMO_TOKEN="$DEMO_BACKENDS_TOKEN"
  journey 6 platform-stack-b4.mjs b4
}

smoke() {
  local bin=""; while [ $# -gt 0 ]; do case "$1" in --bin) bin="${2:-}"; shift 2 ;; *) die "unknown argument $1" ;; esac; done
  if ! { alive api && alive edge && alive web; }; then
    [ -n "$bin" ] || die "stack is not running: pass --bin <dir> (smoke starts it) or run 'up' first"
    up --bin "$bin" >/dev/null
  fi
  journey 6
}

onboard() {
  alive api || die "stack is not running (up first)"
  journey 5
}

reset_all() {
  stop_all; load_deps
  psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -q -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" -c "DROP DATABASE IF EXISTS $DEMO_DB WITH (FORCE)" -c "DROP DATABASE IF EXISTS $B4_DB WITH (FORCE)"
  docker exec agent-local-redis-dev-1 sh -c 'redis-cli -p '"$REDIS"' -a "$0" --no-auth-warning --scan --pattern "'"$RPFX"'*" | xargs -r redis-cli -p '"$REDIS"' -a "$0" --no-auth-warning DEL >/dev/null' "$AGENT_DEV_REDIS_PASSWORD" \
    || say "WARN: $RPFX* redis keys not cleared"
  rm -f "$LOCAL/fake-licences.json" "$LOCAL/journey.json" "$LOCAL/harness-tenant"
  say "reset ok: $DB, $DEMO_DB and $B4_DB dropped, $RPFX* keys deleted"
}

logs() {
  local svc="${1:-}"; shift || true
  [ -n "$svc" ] || die "usage: $0 logs <$(IFS='|'; echo "${SVCS[*]}")> [-f]"
  [ -f "$LOCAL/$svc.log" ] || die "no log $LOCAL/$svc.log"
  if [ "${1:-}" = "-f" ]; then tail -n 100 -f "$LOCAL/$svc.log"; else tail -n 200 "$LOCAL/$svc.log"; fi
}

CMD="${1:-}"; [ $# -gt 0 ] && shift
case "$CMD" in
  up) up "$@" ;;
  smoke) smoke "$@" ;;
  b4) b4 ;;
  onboard) onboard ;;
  harness-tenant) harness_tenant "$@" ;;
  status) status ;;
  logs) logs "$@" ;;
  down) stop_all ;;
  reset) reset_all ;;
  *) die "usage: $0 {up --bin <dir> [--real-llm] | onboard | smoke [--bin <dir>] | b4 | status | logs <svc> [-f] | down | reset}" ;;
esac
