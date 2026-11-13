-- 0177 — the validation grid per release on approve (F-06) and question hygiene (F-50) (B8 T12a; plan
-- 2026-10-20-b8-journeys-generator-discovery, § Data model `0107`, renumbered by § Revision 2026-10-22 R16).
--
-- - `validation_grid_run`: one grading of a release cut, queued by a person's approve (`trigger = 'approve'`, B8-9:
--   the B2 `ReleaseApproved` hook after the approve COMMIT) or by `runValidationCut` (`manual`). At most one active
--   (`queued`/`running`) run per release. Boot resumes active rows; nothing creates a row on a schedule.
-- - `validation_question.deleted_at` (soft delete: stored results of earlier cuts keep their question),
--   `source` (`manual` | `csv`) and `import_batch` (one id per CSV import).
--
-- Drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT). Forward-only. Needs 0172.

CREATE TABLE IF NOT EXISTS validation_grid_run (
    id          uuid NOT NULL DEFAULT gen_random_uuid() PRIMARY KEY,
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    release_id  text NOT NULL,
    trigger     text NOT NULL,
    status      text NOT NULL DEFAULT 'queued',
    done        integer NOT NULL DEFAULT 0,
    total       integer NOT NULL DEFAULT 0,
    queued_at   timestamptz NOT NULL DEFAULT now(),
    started_at  timestamptz,
    finished_at timestamptz,
    -- the person whose approve (or run) caused it: usage rows are attributed to them
    by          text NOT NULL,
    error_code  text
);
ALTER TABLE validation_grid_run
    DROP CONSTRAINT IF EXISTS validation_grid_run_release,
    ADD CONSTRAINT validation_grid_run_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS validation_grid_run_trigger_check,
    ADD CONSTRAINT validation_grid_run_trigger_check CHECK (trigger IN ('approve', 'manual')),
    DROP CONSTRAINT IF EXISTS validation_grid_run_status_check,
    ADD CONSTRAINT validation_grid_run_status_check
        CHECK (status IN ('queued', 'running', 'done', 'failed', 'skipped_no_questions')),
    DROP CONSTRAINT IF EXISTS validation_grid_run_progress_check,
    ADD CONSTRAINT validation_grid_run_progress_check CHECK (done >= 0 AND total >= 0 AND done <= total),
    DROP CONSTRAINT IF EXISTS validation_grid_run_finished_check,
    ADD CONSTRAINT validation_grid_run_finished_check
        CHECK ((status IN ('queued', 'running')) = (finished_at IS NULL)),
    DROP CONSTRAINT IF EXISTS validation_grid_run_error_check,
    ADD CONSTRAINT validation_grid_run_error_check
        CHECK ((status = 'failed') = (error_code IS NOT NULL)
               AND (error_code IS NULL OR error_code ~ '^[a-z][a-z0-9_]{1,60}$')),
    DROP CONSTRAINT IF EXISTS validation_grid_run_by_check,
    ADD CONSTRAINT validation_grid_run_by_check CHECK (btrim(by) <> '');
DROP INDEX IF EXISTS validation_grid_run_one_active;
CREATE UNIQUE INDEX validation_grid_run_one_active ON validation_grid_run (tenant_id, job_id, release_id)
    WHERE status IN ('queued', 'running');
CREATE INDEX IF NOT EXISTS validation_grid_run_recent
    ON validation_grid_run (tenant_id, job_id, release_id, queued_at DESC);
ALTER TABLE validation_grid_run ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON validation_grid_run;
CREATE POLICY tenant_isolation ON validation_grid_run FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON validation_grid_run;
CREATE POLICY job_scope ON validation_grid_run AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON validation_grid_run FROM PUBLIC;
REVOKE ALL ON validation_grid_run FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON validation_grid_run TO agent_app;

-- Boot resume (B8-9): the active runs of every tenant. SECURITY DEFINER because the boot task has no tenant scope;
-- it returns ids and scopes only, and every run then continues under its own `begin_scoped`. Reads, never writes.
CREATE OR REPLACE FUNCTION validation_grid_active()
    RETURNS TABLE (id uuid, tenant_id uuid, job_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT g.id, g.tenant_id, g.job_id FROM validation_grid_run g
         WHERE g.status IN ('queued', 'running') ORDER BY g.queued_at, g.id
    $$;
REVOKE ALL ON FUNCTION validation_grid_active() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION validation_grid_active() TO agent_app;

ALTER TABLE validation_question
    ADD COLUMN IF NOT EXISTS deleted_at timestamptz,
    ADD COLUMN IF NOT EXISTS source text NOT NULL DEFAULT 'manual',
    ADD COLUMN IF NOT EXISTS import_batch text;
ALTER TABLE validation_question
    DROP CONSTRAINT IF EXISTS validation_question_source_check,
    ADD CONSTRAINT validation_question_source_check
        CHECK (source IN ('manual', 'csv') AND ((source = 'csv') = (import_batch IS NOT NULL))),
    DROP CONSTRAINT IF EXISTS validation_question_batch_check,
    ADD CONSTRAINT validation_question_batch_check
        CHECK (import_batch IS NULL OR import_batch ~ '^imp-[0-9a-f]{8,32}$');
CREATE INDEX IF NOT EXISTS validation_question_live ON validation_question (tenant_id, job_id)
    WHERE deleted_at IS NULL;
