-- 0142 — B5 procedure versions + history (plan 2026-10-20-b5-job-design, Data model `0142`, T2). Forward-only
-- (pre-prod), drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, as 0047). 0141 is not created (R5: its tables
-- are R2-dropped; context spaces are B2 `job_overlay_row` rows).
--
-- Tenant versions only (R1): base versions are the compiled `procedure` rows (`name@N`), never copied here.
-- `version` is an integer ≥ 1 (R6, as `agent_case.procedure_version`). `runs_7d` / `success_rate` / `test_runs` are
-- derived from `agent_case`, not stored.
--
-- RLS: `tenant_isolation` plus a RESTRICTIVE `job_scope` (as 0060 `agent_case`). Grants are in 0145.

CREATE TABLE IF NOT EXISTS procedure_version (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    job_id        uuid NOT NULL DEFAULT app_current_job(),
    name          text NOT NULL,
    version       integer NOT NULL,
    status        text NOT NULL DEFAULT 'DRAFT',
    context_space text NOT NULL,
    description   text NOT NULL,
    reviewers     text[] NOT NULL DEFAULT '{}',
    steps         jsonb NOT NULL,
    released_in   text,
    created_by    text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, name, version)
);
ALTER TABLE procedure_version
    DROP CONSTRAINT IF EXISTS procedure_version_job,
    ADD CONSTRAINT procedure_version_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS procedure_version_name_check,
    ADD CONSTRAINT procedure_version_name_check CHECK (name ~ '^[a-z][a-z0-9_]{0,63}$'),
    DROP CONSTRAINT IF EXISTS procedure_version_version_check,
    ADD CONSTRAINT procedure_version_version_check CHECK (version >= 1),
    DROP CONSTRAINT IF EXISTS procedure_version_status_check,
    ADD CONSTRAINT procedure_version_status_check
        CHECK (status IN ('DRAFT', 'CANARY', 'PUBLISHED', 'ARCHIVED', 'DISCARDED')),
    DROP CONSTRAINT IF EXISTS procedure_version_steps_check,
    ADD CONSTRAINT procedure_version_steps_check CHECK (jsonb_typeof(steps) = 'array'),
    DROP CONSTRAINT IF EXISTS procedure_version_created_by_check,
    ADD CONSTRAINT procedure_version_created_by_check CHECK (btrim(created_by) <> '');
-- At most one PUBLISHED tenant version per name and job.
CREATE UNIQUE INDEX IF NOT EXISTS procedure_version_one_published
    ON procedure_version (tenant_id, job_id, name) WHERE status = 'PUBLISHED';
ALTER TABLE procedure_version ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON procedure_version;
CREATE POLICY tenant_isolation ON procedure_version FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON procedure_version;
CREATE POLICY job_scope ON procedure_version AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- Append-only change history (agent_app: SELECT, INSERT only; 0145). Not a snapshot place (D62). No FK to
-- `procedure_version`: a refused transition of a BASE version (409 `base_version`) is recorded too.
CREATE TABLE IF NOT EXISTS procedure_history (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    name        text NOT NULL,
    version     integer NOT NULL,
    at          timestamptz NOT NULL DEFAULT clock_timestamp(),
    actor       text NOT NULL,
    action      text NOT NULL,
    from_status text,
    to_status   text,
    ok          boolean NOT NULL,
    reason      text
);
ALTER TABLE procedure_history
    DROP CONSTRAINT IF EXISTS procedure_history_job,
    ADD CONSTRAINT procedure_history_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS procedure_history_version_check,
    ADD CONSTRAINT procedure_history_version_check CHECK (version >= 1),
    DROP CONSTRAINT IF EXISTS procedure_history_action_check,
    ADD CONSTRAINT procedure_history_action_check CHECK (action IN ('create', 'approve', 'transition')),
    DROP CONSTRAINT IF EXISTS procedure_history_actor_check,
    ADD CONSTRAINT procedure_history_actor_check CHECK (btrim(actor) <> '');
CREATE INDEX IF NOT EXISTS procedure_history_by_version
    ON procedure_history (tenant_id, job_id, name, version, at DESC, id DESC);
ALTER TABLE procedure_history ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON procedure_history;
CREATE POLICY tenant_isolation ON procedure_history FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON procedure_history;
CREATE POLICY job_scope ON procedure_history AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
