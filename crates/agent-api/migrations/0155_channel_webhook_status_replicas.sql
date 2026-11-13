-- 0155 — Telegram webhook health across edge replicas, plus what Telegram itself says about the webhook (B7 T15b
-- follow-up to 0153; owner rulings 2026-10-22 / 2026-10-23). Forward-only, drift-tolerant like 0153 (ADD COLUMN IF
-- NOT EXISTS, constraints dropped and re-added). Touches only `channel_webhook_status`; its RLS, grants and job scope
-- are 0153's and unchanged (agent_app keeps SELECT, INSERT, UPDATE).
--
-- Two sources feed a row (crate::channels::webhook_health):
-- - what agent-edge OBSERVES per replica: updates arriving, sends succeeding or failing;
-- - what Telegram SAYS, read by the edge every pass for every Telegram binding it serves (traffic or not) through the
--   read-only webhook-info call. Reading the registration claims nothing, so C1 (the crate never claims the bot's
--   updates) holds; the claiming methods stay forbidden crate-wide. An older edge, or a failed call, reports without
--   it, and then `down` is not derivable.
--
-- 1. `degraded` does not flap between replicas. Each replica counts failures in its own memory; the edge also reports
--    its newest successful send, merged here by newest (`last_send_ok_at`), and agent-api keeps `degraded_at`
--    (webhook_health::degraded_since): set to the newest failure of a run of 3 that is newer than every known
--    successful send, never cleared by a report with fewer failures.
-- 2. `pending_update_count` (0153) is now written from the webhook info (a report without it keeps the stored count).
--    Telegram's delivery error is reduced to a code on the edge (`webhook_http_502`, `webhook_timeout`, …) and
--    competes with the send error for `last_error` / `last_error_at` (newer wins).
-- 3. `url` (0153) stays NULL on purpose: the registered URL carries the binding id in its path (and may carry more);
--    the edge keeps it in memory and reports only whether it is set and whether it is this binding's route.
-- 4. `webhook_down_reason` says why a binding is `down`: `url_unset`, `url_mismatch` or `webhook_error`; set exactly
--    when the status is `down`.
-- 5. `webhook_info_at` is when a report last carried webhook info. A report without it (an older edge, a replica
--    whose call failed) keeps the stored reason and pending count while that is younger than 5 min (5 reporting
--    passes), so replicas do not flap a `down` binding; after that it falls back to the observed rule.
--
-- status (webhook_health::status_of, precedence down > degraded > quiet > ok):
-- - `down`: from this report's webhook info (or the stored one while fresh, see 5): no URL registered (`url_unset`); a URL that is not the binding's
--   route (`url_mismatch`); a delivery error at most 15 min old and newer than the last update (`webhook_error`);
-- - `degraded`: degraded_at is set and no last_send_ok_at is newer, or more than 100 updates are pending;
-- - `quiet` (informational: nobody wrote to the bot): the last update is older than 24 h;
-- - else `ok`.
-- A `down` stored by 0153's code meant "no update for 24 h" and has no reason; it is rewritten to `quiet` (what that
-- means now) so the down CHECK below can be added. Prod held only an `ok` row when this was written, but 0153's code
-- keeps writing that `down` until this deploys, so the rewrite is not left to chance. The next report recomputes it.

ALTER TABLE channel_webhook_status ADD COLUMN IF NOT EXISTS last_send_ok_at timestamptz;
ALTER TABLE channel_webhook_status ADD COLUMN IF NOT EXISTS degraded_at timestamptz;
ALTER TABLE channel_webhook_status ADD COLUMN IF NOT EXISTS webhook_down_reason text;
ALTER TABLE channel_webhook_status ADD COLUMN IF NOT EXISTS webhook_info_at timestamptz;

ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_status_check;
UPDATE channel_webhook_status SET status = 'quiet' WHERE status = 'down' AND webhook_down_reason IS NULL;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_status_check
    CHECK (status IN ('ok', 'degraded', 'quiet', 'down'));
-- `degraded` is derived from degraded_at or the pending count, so they cannot disagree. A `degraded` stored by 0153's
-- code has no degraded_at yet: it gets its last failure's time, so it stays degraded until a send succeeds after it.
UPDATE channel_webhook_status SET degraded_at = COALESCE(last_error_at, observed_at)
    WHERE status = 'degraded' AND degraded_at IS NULL;
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_degraded_check;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_degraded_check
    CHECK (status <> 'degraded' OR degraded_at IS NOT NULL OR COALESCE(pending_update_count, 0) > 100);
-- `down` always says why, and only `down` has a reason.
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_down_check;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_down_check
    CHECK (status <> 'down' OR webhook_down_reason IS NOT NULL);
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_down_reason_check;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_down_reason_check
    CHECK (webhook_down_reason IS NULL
           OR (status = 'down' AND webhook_down_reason IN ('url_unset', 'url_mismatch', 'webhook_error')));
