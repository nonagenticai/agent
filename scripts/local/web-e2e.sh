#!/usr/bin/env bash
# The web e2e run (web/tests/e2e) on the REAL local platform stack: its own platform-stack instance (PORT_BASE 200:
# web 3300, api 8398, edge 8399, fake IdP 9652 …; databases *_e2e; Redis prefix ps_e2e:; state target/local/platform-
# stack-e2e), so it never touches the default instance others use. Never runs Cargo: BIN_DIR is a frozen bin dir.
#
#   scripts/local/web-e2e.sh --bin <dir> [--reuse] [playwright args…]
#
# 1. boots the instance when its web does not answer (platform-stack.sh up),
# 2. runs the platform-stack smoke, which builds the journey through the product's own routes (a new tenant each run)
#    and writes journey.json; --reuse keeps the last journey when it exists,
# 3. runs Playwright under the machine-wide browser lease (~/.claude/scripts/browser-lease.py) with PS_E2E_* set.
# Stop the instance with: PORT_BASE=200 PS_DB_SUFFIX=_e2e PS_LOCAL_DIR=<state> scripts/local/platform-stack.sh down
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MAIN="$(cd "$(git -C "$ROOT" rev-parse --git-common-dir)/.." && pwd)"
export PORT_BASE="${PORT_BASE:-200}" PS_DB_SUFFIX="${PS_DB_SUFFIX:-_e2e}"
export PS_LOCAL_DIR="${PS_LOCAL_DIR:-$MAIN/target/local/platform-stack-e2e}" PS_WEB_DIR="${PS_WEB_DIR:-$ROOT/web}"
BIN="${BIN_DIR:-}"; REUSE=0; ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BIN="$2"; shift 2 ;;
    --reuse) REUSE=1; shift ;;
    *) ARGS+=("$1"); shift ;;
  esac
done
PS="$ROOT/scripts/local/platform-stack.sh"
WEB="http://127.0.0.1:$((3100 + PORT_BASE))"
say() { printf '[web-e2e] %s\n' "$*"; }
[ -e "$PS_WEB_DIR/node_modules" ] || { say "ERROR: $PS_WEB_DIR/node_modules is missing (npm ci there)"; exit 1; }
if [ "$(curl -s -o /dev/null -w '%{http_code}' "$WEB/" || true)" = 000 ]; then
  [ -n "$BIN" ] || { say "ERROR: the instance is not up; pass --bin <frozen bin dir>"; exit 1; }
  say "booting the e2e platform-stack instance (PORT_BASE $PORT_BASE)"
  "$PS" up --bin "$BIN"
fi
if [ "$REUSE" = 0 ] || [ ! -f "$PS_LOCAL_DIR/journey.json" ]; then
  say "building the journey through the product routes (platform-stack smoke)"
  "$PS" smoke
fi
export PS_E2E_WEB="$WEB" PS_E2E_IDP="http://127.0.0.1:$((9452 + PORT_BASE))" PS_E2E_EDGE="http://localhost:$((8199 + PORT_BASE))"
export PS_E2E_JOURNEY="$PS_LOCAL_DIR/journey.json" PS_E2E_KCADMIN="http://127.0.0.1:$((9455 + PORT_BASE))" PS_E2E_LLM="http://127.0.0.1:$((9453 + PORT_BASE))/v1" PS_E2E_STRATA="http://127.0.0.1:$((9456 + PORT_BASE))" PS_E2E_INGEST="http://127.0.0.1:$((9454 + PORT_BASE))"
export PS_E2E_ARTIFACTS="http://127.0.0.1:$((9457 + PORT_BASE))"
# the stack database owner's URL (read-only checks by the acceptance spec); never echoed
if [ -f "$MAIN/target/local/dependencies.env" ]; then
  set -a; . "$MAIN/target/local/dependencies.env"; set +a
  export PS_E2E_OWNER_URL="postgres://agent_dev_owner:${AGENT_DEV_POSTGRES_PASSWORD:?missing in dependencies.env}@127.0.0.1:5433/agent_platform_stack${PS_DB_SUFFIX}"
fi
cd "$PS_WEB_DIR"
exec /usr/bin/python3 "$HOME/.claude/scripts/browser-lease.py" run --owner "web-e2e-$$" --wait 900 -- \
  npx playwright test "${ARGS[@]+"${ARGS[@]}"}"
