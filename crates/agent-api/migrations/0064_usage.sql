-- 0064 — usage metering (B4 T3; plan Data model `0064`, port of care-bus-agents@96c608c `0011_metering.sql`).
-- Forward-only (pre-prod), drift-tolerant.
--
-- Order (§0 R2): production already ran ≤0122 and applies this AFTER 0122; a fresh database applies it in number
-- order. So it references only objects ≤0064: `tenants`, `jobs`, `member_entitlement` (0037),
-- `app_current_tenant()`, `app_current_job()` and the `agent_app` role. `agent_runtime`'s grants are 0065.
--
-- `usage_event` is the usage ledger: one row per metered call (LLM, gate, knowledge) of a customer turn, a staff test
-- turn or a platform run. `job_id` is NULL only for a run outside any job (a B8 generator run); the RESTRICTIVE
-- `job_scope` policy then admits the row only when no job is in scope (`job_id = app_current_job()` is not true for
-- a NULL job). `use` is the metering label (§0 R8: `assistant` is a label here, not an LLM use) and is free text in
-- a fixed shape so a new label needs no migration. `project` is the tenant id (a projectless app), derived.
--
-- `metering_outbox` is the transactional outbox to billing (`billing:costs`), written in the ledger row's
-- transaction only with METERING_EMIT_ENABLED=true (K52: false); billing dedups on `event_key`.
--
-- `usage_daily` buckets by UTC day whatever the session TimeZone (security_invoker: the caller's RLS applies).
--
-- `agent_licence_attribution(actor)` returns the licence ids a usage row carries, never a token: a member's active
-- entitlement for a staff actor, else the tenant's own licence (subject NULL). SECURITY DEFINER (the migration
-- owner bypasses RLS on its own tables), so it filters by `app_current_tenant()` itself and refuses an unscoped call.

CREATE TABLE IF NOT EXISTS usage_event (
    id              bigserial PRIMARY KEY,
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid DEFAULT app_current_job(),
    -- billing dedups on it; the outbox references it (so it is not dropped and re-added below)
    event_key       uuid NOT NULL CONSTRAINT usage_event_event_key_key UNIQUE,
    occurred_at     timestamptz NOT NULL DEFAULT now(),
    source          text NOT NULL,
    use             text NOT NULL,
    activity        text,
    conversation_id text,
    case_id         text,
    actor           text NOT NULL,
    project         text GENERATED ALWAYS AS (tenant_id::text) STORED,
    provider_id     text NOT NULL,
    model           text NOT NULL DEFAULT '',
    funding         text NOT NULL,
    tokens_in       bigint NOT NULL DEFAULT 0,
    tokens_out      bigint NOT NULL DEFAULT 0,
    calls           integer NOT NULL DEFAULT 1,
    latency_ms      bigint NOT NULL DEFAULT 0,
    license_id      text,
    subject_id      text
);
ALTER TABLE usage_event
    DROP CONSTRAINT IF EXISTS usage_event_job,
    ADD CONSTRAINT usage_event_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS usage_event_source_check,
    ADD CONSTRAINT usage_event_source_check CHECK (source IN (
        'customer_turn', 'staff_test', 'generator', 'discovery', 'validation', 'gate')),
    DROP CONSTRAINT IF EXISTS usage_event_use_check,
    ADD CONSTRAINT usage_event_use_check CHECK (use ~ '^[a-z][a-z0-9_]{0,39}$'),
    DROP CONSTRAINT IF EXISTS usage_event_funding_check,
    ADD CONSTRAINT usage_event_funding_check CHECK (funding IN ('local_gpu', 'platform_external', 'tenant_byo')),
    DROP CONSTRAINT IF EXISTS usage_event_actor_check,
    ADD CONSTRAINT usage_event_actor_check CHECK (btrim(actor) <> '' AND char_length(actor) <= 200),
    DROP CONSTRAINT IF EXISTS usage_event_provider_check,
    ADD CONSTRAINT usage_event_provider_check CHECK (btrim(provider_id) <> '' AND char_length(provider_id) <= 200),
    DROP CONSTRAINT IF EXISTS usage_event_model_check,
    ADD CONSTRAINT usage_event_model_check CHECK (char_length(model) <= 200),
    DROP CONSTRAINT IF EXISTS usage_event_counts_check,
    ADD CONSTRAINT usage_event_counts_check CHECK (
        tokens_in >= 0 AND tokens_out >= 0 AND calls >= 0 AND latency_ms >= 0);
CREATE INDEX IF NOT EXISTS usage_event_tenant_time ON usage_event (tenant_id, occurred_at);
CREATE INDEX IF NOT EXISTS usage_event_job_time ON usage_event (tenant_id, job_id, occurred_at);
ALTER TABLE usage_event ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON usage_event;
CREATE POLICY tenant_isolation ON usage_event FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
-- a job-less row only outside a job scope: `job_id = app_current_job()` is NULL (not true) for it
DROP POLICY IF EXISTS job_scope ON usage_event;
CREATE POLICY job_scope ON usage_event AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
-- the ledger is append-only
REVOKE ALL ON usage_event FROM agent_app;
GRANT SELECT, INSERT ON usage_event TO agent_app;
GRANT USAGE ON SEQUENCE usage_event_id_seq TO agent_app;

CREATE TABLE IF NOT EXISTS metering_outbox (
    id              bigserial PRIMARY KEY,
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid DEFAULT app_current_job(),
    event_key       uuid NOT NULL,
    fields          jsonb NOT NULL,
    status          text NOT NULL DEFAULT 'pending',
    attempts        integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    last_error      text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    sent_at         timestamptz
);
ALTER TABLE metering_outbox
    DROP CONSTRAINT IF EXISTS metering_outbox_event_key_key,
    ADD CONSTRAINT metering_outbox_event_key_key UNIQUE (event_key),
    DROP CONSTRAINT IF EXISTS metering_outbox_usage,
    ADD CONSTRAINT metering_outbox_usage FOREIGN KEY (event_key) REFERENCES usage_event (event_key),
    DROP CONSTRAINT IF EXISTS metering_outbox_job,
    ADD CONSTRAINT metering_outbox_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS metering_outbox_fields_check,
    ADD CONSTRAINT metering_outbox_fields_check CHECK (jsonb_typeof(fields) = 'object'),
    DROP CONSTRAINT IF EXISTS metering_outbox_status_check,
    ADD CONSTRAINT metering_outbox_status_check CHECK (status IN ('pending', 'sent')),
    DROP CONSTRAINT IF EXISTS metering_outbox_attempts_check,
    ADD CONSTRAINT metering_outbox_attempts_check CHECK (attempts >= 0),
    DROP CONSTRAINT IF EXISTS metering_outbox_last_error_check,
    ADD CONSTRAINT metering_outbox_last_error_check CHECK (last_error IS NULL OR char_length(last_error) <= 500),
    DROP CONSTRAINT IF EXISTS metering_outbox_sent_check,
    ADD CONSTRAINT metering_outbox_sent_check CHECK ((status = 'sent') = (sent_at IS NOT NULL));
CREATE INDEX IF NOT EXISTS metering_outbox_due ON metering_outbox (next_attempt_at) WHERE status = 'pending';
ALTER TABLE metering_outbox ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON metering_outbox;
CREATE POLICY tenant_isolation ON metering_outbox FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON metering_outbox;
CREATE POLICY job_scope ON metering_outbox AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON metering_outbox FROM agent_app;
GRANT SELECT, INSERT ON metering_outbox TO agent_app;
GRANT UPDATE (status, attempts, next_attempt_at, last_error, sent_at) ON metering_outbox TO agent_app;
GRANT USAGE ON SEQUENCE metering_outbox_id_seq TO agent_app;

-- Usage per UTC day. The caller's RLS applies (security_invoker).
CREATE OR REPLACE VIEW usage_daily WITH (security_invoker = true) AS
    SELECT tenant_id,
           job_id,
           (occurred_at AT TIME ZONE 'UTC')::date AS day,
           source,
           use,
           funding,
           provider_id,
           model,
           actor,
           count(*)::bigint        AS events,
           sum(calls)::bigint      AS calls,
           sum(tokens_in)::bigint  AS tokens_in,
           sum(tokens_out)::bigint AS tokens_out,
           sum(latency_ms)::bigint AS latency_ms
      FROM usage_event
     GROUP BY tenant_id, job_id, (occurred_at AT TIME ZONE 'UTC')::date, source, use, funding, provider_id, model,
              actor;
REVOKE ALL ON usage_daily FROM agent_app;
GRANT SELECT ON usage_daily TO agent_app;

-- The licence ids for a usage row of `p_actor` in the scoped tenant: the member's active entitlement when `p_actor`
-- is a member's sub, else the tenant licence. Never a token. One row, possibly NULLs (licence not minted yet).
CREATE OR REPLACE FUNCTION agent_licence_attribution(p_actor text)
    RETURNS TABLE (license_id text, subject_id text)
    LANGUAGE plpgsql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    t uuid := app_current_tenant();
BEGIN
    IF t IS NULL THEN
        RAISE EXCEPTION 'agent_licence_attribution needs app.current_tenant' USING ERRCODE = '42501';
    END IF;
    RETURN QUERY
        SELECT e.license_id, e.license_subject_id
          FROM member_entitlement e
         WHERE e.tenant_id = t AND e.user_sub = p_actor AND e.status = 'active';
    IF NOT FOUND THEN
        RETURN QUERY SELECT tn.licence_id, NULL::text FROM tenants tn WHERE tn.id = t;
    END IF;
END
$$;
REVOKE ALL ON FUNCTION agent_licence_attribution(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_licence_attribution(text) TO agent_app;
