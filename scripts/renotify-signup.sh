#!/usr/bin/env bash
# scripts/renotify-signup.sh — re-post the Telegram new-signup notice (with Approve/Reject buttons) for ONE
# pending early-access signup. Port of commonpractice scripts/renotify-signup.ts (#478).
#
# Why: the notice is posted once, at intake. If a decision tap fails and the message loses its buttons, or the
# message is deleted, the only way left to decide is a super_admin sign-in on /platform/signups. This posts a
# fresh, tappable notice. It changes NOTHING in the database (a pending row stays pending; the decision door is
# idempotent, so two live notices cannot decide one signup twice).
#
#   scripts/renotify-signup.sh <signupId>            # dry run: prints what it would post
#   scripts/renotify-signup.sh <signupId> --apply    # posts it
#
# Runs `agent-api renotify-signup` inside the running agent-api pod (it needs the pod's configuration: database,
# Telegram token). Requires kubectl access to the cluster (KUBECTL, default `kubectl`; NAMESPACE default `agent`;
# DEPLOYMENT default `agent-api`). Exit: 0 ok · 1 usage / not found / not pending / Telegram off or refused.
set -euo pipefail

id="${1:-}"
if [[ -z "$id" ]]; then
    echo "usage: $0 <signupId> [--apply]" >&2
    exit 1
fi
shift
extra=()
for a in "$@"; do
    case "$a" in
        --apply) extra+=(--apply) ;;
        *) echo "unknown argument: $a" >&2; exit 1 ;;
    esac
done

KUBECTL="${KUBECTL:-kubectl}"
NAMESPACE="${NAMESPACE:-agent}"
DEPLOYMENT="${DEPLOYMENT:-agent-api}"

exec $KUBECTL -n "$NAMESPACE" exec "deploy/$DEPLOYMENT" -- agent-api renotify-signup "$id" "${extra[@]+"${extra[@]}"}"
