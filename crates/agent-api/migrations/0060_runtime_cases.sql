-- 0060 — runtime cases (B4 T2 shapes, pulled forward by B12 §1). Forward-only (pre-prod).
--
-- Deltas to B4 (B2 is not merged): `release_id` / `draft_rev` are nullable with no FK and no prod/test CHECK (B4
-- adds both NOT VALID in a migration ≥0068); the channel CHECK carries the widened set. References only `tenants`,
-- `jobs`, `app_current_tenant()` and `app_current_job()` (≤0033): production applies this before B2/B3's 0044–0057.
--
-- RLS: `tenant_isolation` plus a RESTRICTIVE `job_scope` (a transaction scoped to a job sees only that job).

CREATE TABLE agent_case (
    tenant_id            uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id               uuid NOT NULL DEFAULT app_current_job(),
    case_id              text NOT NULL CHECK (case_id ~ '^CS-[1-9][0-9]*$'),
    conversation_id      text NOT NULL CHECK (btrim(conversation_id) <> '' AND char_length(conversation_id) <= 200),
    channel              text NOT NULL,
    env                  text NOT NULL CHECK (env IN ('prod', 'test')),
    release_id           text,
    draft_rev            bigint CHECK (draft_rev IS NULL OR draft_rev >= 0),
    qualified_release_id text,
    qualified_draft_rev  bigint,
    activity             text,
    request_type         text,
    context_space        text,
    title                text NOT NULL DEFAULT '' CHECK (char_length(title) <= 200),
    status               text NOT NULL DEFAULT 'open' CHECK (status IN (
                             'open', 'identity_required', 'confirmation_required', 'handover', 'resolved',
                             'not_eligible', 'failed', 'cancelled_offboard')),
    outcome              text,
    escalation_reason    text,
    identity_level       smallint NOT NULL DEFAULT 0 CHECK (identity_level BETWEEN 0 AND 3),
    customer_id          text,
    customer_name        text,
    initiator_sub        text,
    procedure_name       text,
    procedure_version    integer,
    alert                text,
    op_calls             integer NOT NULL DEFAULT 0 CHECK (op_calls >= 0),
    started_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    resolved_at          timestamptz,
    duration_ms          bigint CHECK (duration_ms IS NULL OR duration_ms >= 0),
    PRIMARY KEY (tenant_id, job_id, case_id),
    CONSTRAINT agent_case_channel_check CHECK (channel IN (
        'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api')),
    CONSTRAINT agent_case_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE INDEX agent_case_recent ON agent_case (tenant_id, job_id, updated_at DESC);
CREATE INDEX agent_case_open ON agent_case (tenant_id, job_id, updated_at DESC)
    WHERE status IN ('open', 'identity_required', 'confirmation_required', 'handover');
CREATE INDEX agent_case_conversation ON agent_case (tenant_id, job_id, conversation_id);
ALTER TABLE agent_case ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON agent_case FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON agent_case AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON agent_case TO agent_app;

-- The per-job case counter behind `CS-<n>`.
CREATE TABLE agent_case_seq (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id    uuid NOT NULL DEFAULT app_current_job(),
    next      bigint NOT NULL CHECK (next >= 1),
    PRIMARY KEY (tenant_id, job_id),
    CONSTRAINT agent_case_seq_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
ALTER TABLE agent_case_seq ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON agent_case_seq FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON agent_case_seq AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON agent_case_seq TO agent_app;

-- The next case id of the scoped job (`app.current_tenant` + `app.current_job`): `CS-1`, `CS-2`, … per job. The
-- row lock of the upsert serialises concurrent callers; an unscoped call fails NOT NULL (no job).
CREATE FUNCTION agent_next_case_id() RETURNS text
    LANGUAGE sql VOLATILE
    SET search_path = public, pg_temp
    AS $$
        INSERT INTO agent_case_seq AS s (tenant_id, job_id, next)
             VALUES (app_current_tenant(), app_current_job(), 2)
        ON CONFLICT (tenant_id, job_id) DO UPDATE SET next = s.next + 1
        RETURNING 'CS-' || (s.next - 1)
    $$;
REVOKE ALL ON FUNCTION agent_next_case_id() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_next_case_id() TO agent_app;

-- The case timeline (runtime `TimelineEvent` minus kind/at in `body`). Append-only: no UPDATE or DELETE grant.
CREATE TABLE case_event (
    seq       bigserial PRIMARY KEY,
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id    uuid NOT NULL DEFAULT app_current_job(),
    case_id   text NOT NULL,
    at        timestamptz NOT NULL DEFAULT now(),
    kind      text NOT NULL CHECK (kind IN (
                  'message', 'gate', 'qualified', 'knowledge', 'operation', 'state', 'reply', 'alert')),
    body      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(body) = 'object'),
    by_sub    text,
    CONSTRAINT case_event_case FOREIGN KEY (tenant_id, job_id, case_id)
        REFERENCES agent_case (tenant_id, job_id, case_id)
);
CREATE INDEX case_event_case_seq ON case_event (tenant_id, job_id, case_id, seq);
ALTER TABLE case_event ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON case_event FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON case_event AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT ON case_event TO agent_app;
GRANT USAGE ON SEQUENCE case_event_seq_seq TO agent_app;
