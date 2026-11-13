-- 0153 — Telegram webhook health (B7 T15b, owner decision 2026-10-22). Forward-only, drift-tolerant like 0120: the
-- table is created IF NOT EXISTS and every constraint / policy is dropped and re-added. References only 0001
-- (`tenants`, `app_current_tenant()`, `app_current_job()`, `agent_app`) and 0120 (`connector_bindings`).
--
-- One row per channel binding, OBSERVED by agent-edge (which is DB-less): the edge counts what it sees per binding
-- (the last update it received, consecutive send failures, the last send error code) and posts a snapshot every 60 s
-- to `POST /api/internal/channel/webhook-status`; agent-api checks the binding (tenant / job / kind) and upserts here
-- in the binding's job scope (crate::conversations::internal, `store::begin_job`, role `agent_app`). The Bot API is
-- never asked about the webhook (the crate-wide forbidden-name test), so `url` and `pending_update_count` stay NULL
-- unless something that actually knows them writes them; they are never faked. No row = the binding view's
-- `webhook: null` ("unknown").
--
-- status: `ok`; `degraded` after 3 consecutive send failures; `down` when the last update is older than the stale
-- window (crate::channels::webhook_health::status_of).

CREATE TABLE IF NOT EXISTS channel_webhook_status (
    binding_id           uuid PRIMARY KEY,
    tenant_id            uuid NOT NULL DEFAULT app_current_tenant(),
    job_id               uuid NOT NULL DEFAULT app_current_job(),
    url                  text,
    status               text NOT NULL,
    last_update_at       timestamptz,
    pending_update_count integer,
    last_error           text,
    last_error_at        timestamptz,
    observed_at          timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_binding;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_binding
    FOREIGN KEY (binding_id) REFERENCES connector_bindings (id) ON DELETE CASCADE;
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_tenant;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_tenant
    FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_status_check;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_status_check
    CHECK (status IN ('ok', 'degraded', 'down'));
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_pending_check;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_pending_check
    CHECK (pending_update_count IS NULL OR pending_update_count >= 0);
-- An error is a short code (never message text, never a token) and comes with its time.
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_error_check;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_error_check CHECK (
    (last_error IS NULL) = (last_error_at IS NULL)
    AND (last_error IS NULL OR (char_length(last_error) BETWEEN 1 AND 120 AND last_error ~ '^[A-Za-z0-9_.:-]+$')));
ALTER TABLE channel_webhook_status DROP CONSTRAINT IF EXISTS channel_webhook_status_url_check;
ALTER TABLE channel_webhook_status ADD CONSTRAINT channel_webhook_status_url_check
    CHECK (url IS NULL OR char_length(url) <= 2048);
CREATE INDEX IF NOT EXISTS channel_webhook_status_job_idx ON channel_webhook_status (tenant_id, job_id);

-- Tenant + job scope exactly as `connector_bindings` (0120).
ALTER TABLE channel_webhook_status ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON channel_webhook_status;
CREATE POLICY tenant_isolation ON channel_webhook_status FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON channel_webhook_status;
CREATE POLICY job_scope ON channel_webhook_status AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- agent-api writes (the internal door) and reads (the binding view) as agent_app. No DELETE: a row goes with its
-- binding (ON DELETE CASCADE).
REVOKE ALL ON channel_webhook_status FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON channel_webhook_status TO agent_app;
