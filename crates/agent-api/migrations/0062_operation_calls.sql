-- 0062 — operation calls (B4 T2, plan Data model `0062`; column names as B7's `connectors::runtime` consumes them).
-- Forward-only (pre-prod), drift-tolerant.
--
-- Order (§0 R2): production already ran ≤0122 and applies this AFTER 0122, a fresh database applies it in number
-- order (before B7's 0094 and B3's 0122). So it references only objects ≤0062: `tenants`, `jobs`, `agent_case`
-- (0060), `app_current_tenant()`, `app_current_job()` and the `agent_app` role. `agent_runtime`'s grants are 0065.
--
-- RLS: `tenant_isolation` plus a RESTRICTIVE `job_scope` (as 0060).

-- One call of an operation (`object.verb{variant}`) by the runner, a manual handover (T12) or the assistant loop.
-- A duplicate `idempotency_key` in the job gets the stored outcome (12 §3.7); reads carry no key (NULL).
-- A case carries `release_id` (env prod; NULL on a never-released job, §0 Q-B4-1) or `draft_rev` (env test).
CREATE TABLE IF NOT EXISTS operation_call (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    call_id         text NOT NULL CHECK (call_id ~ '^opc_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    case_id         text,
    operation       text NOT NULL CHECK (btrim(operation) <> '' AND char_length(operation) <= 200),
    route           text NOT NULL,
    instance_id     text,
    template        text,
    env             text NOT NULL CHECK (env IN ('prod', 'test')),
    state           text NOT NULL DEFAULT 'pending',
    outcome_code    text CHECK (outcome_code IS NULL OR (btrim(outcome_code) <> '' AND char_length(outcome_code) <= 100)),
    started_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz,
    latency_ms      bigint CHECK (latency_ms IS NULL OR latency_ms >= 0),
    initiator       text NOT NULL CHECK (btrim(initiator) <> '' AND char_length(initiator) <= 200),
    attempt         integer NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    idempotency_key text CHECK (idempotency_key IS NULL OR (btrim(idempotency_key) <> '' AND char_length(idempotency_key) <= 200)),
    confirm_ref     text,
    detail          text CHECK (detail IS NULL OR char_length(detail) <= 2000),
    release_id      text,
    draft_rev       bigint CHECK (draft_rev IS NULL OR draft_rev >= 0),
    step            jsonb CHECK (step IS NULL OR jsonb_typeof(step) = 'object'),
    object_change   jsonb CHECK (object_change IS NULL OR jsonb_typeof(object_change) = 'object'),
    PRIMARY KEY (tenant_id, job_id, call_id),
    CONSTRAINT operation_call_route_check CHECK (route IN (
        'instance', 'builtin', 'fallback', 'deep_link', 'not_offered')),
    CONSTRAINT operation_call_state_check CHECK (state IN (
        'pending', 'ok', 'error', 'unknown_outcome', 'denied')),
    -- a call is finished exactly when it left `pending`
    CONSTRAINT operation_call_finished_check CHECK ((state = 'pending') = (finished_at IS NULL)),
    -- §0 Q-B4-1: test carries draft_rev and no release; prod carries no draft_rev (release_id NULL = baseline)
    CONSTRAINT operation_call_env_design_check CHECK (
        (env = 'test' AND draft_rev IS NOT NULL AND release_id IS NULL)
        OR (env = 'prod' AND draft_rev IS NULL)),
    CONSTRAINT operation_call_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CONSTRAINT operation_call_case FOREIGN KEY (tenant_id, job_id, case_id)
        REFERENCES agent_case (tenant_id, job_id, case_id),
    CONSTRAINT operation_call_idempotency UNIQUE (tenant_id, job_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS operation_call_recent ON operation_call (tenant_id, job_id, started_at DESC);
CREATE INDEX IF NOT EXISTS operation_call_op ON operation_call (tenant_id, job_id, operation, started_at);
CREATE INDEX IF NOT EXISTS operation_call_case_idx ON operation_call (tenant_id, job_id, case_id, started_at)
    WHERE case_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS operation_call_unknown ON operation_call (tenant_id, job_id, started_at DESC)
    WHERE state = 'unknown_outcome';
CREATE INDEX IF NOT EXISTS operation_call_instance ON operation_call (tenant_id, instance_id, started_at)
    WHERE instance_id IS NOT NULL;
ALTER TABLE operation_call ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON operation_call;
CREATE POLICY tenant_isolation ON operation_call FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON operation_call;
CREATE POLICY job_scope ON operation_call AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
-- agent_app writes the manual-handover row (T12); the runner (agent_runtime, 0065) updates the outcome columns.
REVOKE ALL ON operation_call FROM agent_app;
GRANT SELECT, INSERT ON operation_call TO agent_app;

-- An operator's verdict on an `unknown_outcome` call (T13 `checkUnknownOutcome`). One per call: a PK conflict
-- answers `already_checked`. Append-only: no UPDATE or DELETE grant.
CREATE TABLE IF NOT EXISTS operation_call_check (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id    uuid NOT NULL DEFAULT app_current_job(),
    call_id   text NOT NULL,
    verdict   text NOT NULL,
    note      text CHECK (note IS NULL OR char_length(note) <= 2000),
    by_sub    text NOT NULL CHECK (btrim(by_sub) <> ''),
    at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id, call_id),
    CONSTRAINT operation_call_check_verdict_check CHECK (verdict IN ('landed', 'not_landed')),
    CONSTRAINT operation_call_check_call FOREIGN KEY (tenant_id, job_id, call_id)
        REFERENCES operation_call (tenant_id, job_id, call_id)
);
ALTER TABLE operation_call_check ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON operation_call_check;
CREATE POLICY tenant_isolation ON operation_call_check FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON operation_call_check;
CREATE POLICY job_scope ON operation_call_check AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON operation_call_check FROM agent_app;
GRANT SELECT, INSERT ON operation_call_check TO agent_app;
