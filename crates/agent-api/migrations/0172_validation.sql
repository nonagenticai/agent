-- 0172 — the validation grid (J17) and draft test runs (B8 T12; plan 2026-10-20-b8-journeys-generator-discovery,
-- § Data model, renumbered by § Revision 2026-10-22 R16).
--
-- D69 live data, never a design write: questions, per-release results and verdicts, the last run of a cut.
-- - `validation_question`: what every later cut is graded on (job scope).
-- - `validation_result`: one row per (release, question) once a cut ran; a human verdict overwrites the judge's
--   (`judged_by` = the person's sub). The question text is read from the question row (0177 soft-deletes them, so a
--   stored result keeps its question).
-- - `validation_cut_run`: when a cut last ran and who ran it.
-- - `draft_test_run`: a D54 TEST-step run of the scenarios against the draft on env test (T13 writes it).
--
-- Every table: RLS `tenant_isolation` + restrictive `job_scope`; agent_app SELECT/INSERT/UPDATE/DELETE (no
-- TRUNCATE). Drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT). Forward-only. References
-- only B1/B2 tables (`jobs`, `job_release`).

CREATE TABLE IF NOT EXISTS validation_question (
    id         uuid NOT NULL DEFAULT gen_random_uuid() PRIMARY KEY,
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    question   text NOT NULL,
    expected   text NOT NULL,
    -- the passage the answer must cite (`policy.returns §2`)
    reference  text NOT NULL,
    -- the qualified activity (`after_sales:return_item`), '' = any
    activity   text NOT NULL DEFAULT '',
    created_at timestamptz NOT NULL DEFAULT now(),
    created_by text NOT NULL
);
ALTER TABLE validation_question
    DROP CONSTRAINT IF EXISTS validation_question_job,
    ADD CONSTRAINT validation_question_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS validation_question_text_check,
    ADD CONSTRAINT validation_question_text_check
        CHECK (btrim(question) <> '' AND char_length(question) <= 2000
               AND btrim(expected) <> '' AND char_length(expected) <= 4000
               AND btrim(reference) <> '' AND char_length(reference) <= 500),
    DROP CONSTRAINT IF EXISTS validation_question_activity_check,
    ADD CONSTRAINT validation_question_activity_check
        CHECK (activity = '' OR activity ~ '^[a-z][a-z0-9_]{0,40}:[a-z][a-z0-9_]{0,60}$'),
    DROP CONSTRAINT IF EXISTS validation_question_created_by_check,
    ADD CONSTRAINT validation_question_created_by_check CHECK (btrim(created_by) <> '');
CREATE INDEX IF NOT EXISTS validation_question_job ON validation_question (tenant_id, job_id, created_at, id);

CREATE TABLE IF NOT EXISTS validation_result (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    release_id  text NOT NULL,
    question_id uuid NOT NULL,
    response    text,
    cited       text[] NOT NULL DEFAULT '{}',
    verdict     text NOT NULL DEFAULT 'pending',
    reason      text,
    -- `judge` (LLM-as-judge) or the sub of the person who set the verdict; NULL = not judged yet
    judged_by   text,
    judged_at   timestamptz,
    PRIMARY KEY (tenant_id, job_id, release_id, question_id)
);
ALTER TABLE validation_result
    DROP CONSTRAINT IF EXISTS validation_result_release,
    ADD CONSTRAINT validation_result_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS validation_result_question,
    ADD CONSTRAINT validation_result_question FOREIGN KEY (question_id) REFERENCES validation_question (id),
    DROP CONSTRAINT IF EXISTS validation_result_response_check,
    ADD CONSTRAINT validation_result_response_check CHECK (response IS NULL OR char_length(response) <= 8000),
    DROP CONSTRAINT IF EXISTS validation_result_verdict_check,
    ADD CONSTRAINT validation_result_verdict_check CHECK (verdict IN ('pass', 'rejected', 'pending')),
    DROP CONSTRAINT IF EXISTS validation_result_reason_check,
    ADD CONSTRAINT validation_result_reason_check
        CHECK ((verdict = 'rejected') = (reason IS NOT NULL)
               AND (reason IS NULL OR reason IN ('missing_citation', 'wrong_citation', 'incorrect'))),
    DROP CONSTRAINT IF EXISTS validation_result_judged_check,
    ADD CONSTRAINT validation_result_judged_check CHECK ((judged_by IS NULL) = (judged_at IS NULL));

CREATE TABLE IF NOT EXISTS validation_cut_run (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    release_id text NOT NULL,
    ran_at     timestamptz NOT NULL DEFAULT now(),
    ran_by     text NOT NULL,
    PRIMARY KEY (tenant_id, job_id, release_id)
);
ALTER TABLE validation_cut_run
    DROP CONSTRAINT IF EXISTS validation_cut_run_release,
    ADD CONSTRAINT validation_cut_run_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS validation_cut_run_by_check,
    ADD CONSTRAINT validation_cut_run_by_check CHECK (btrim(ran_by) <> '');

CREATE TABLE IF NOT EXISTS draft_test_run (
    id           text NOT NULL,
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    env          text NOT NULL DEFAULT 'test',
    draft_rev    bigint NOT NULL,
    draft_fp     text NOT NULL,
    change_keys  text[] NOT NULL DEFAULT '{}',
    base_release text,
    at           timestamptz NOT NULL DEFAULT now(),
    by           text NOT NULL,
    results      jsonb NOT NULL,
    summary      jsonb NOT NULL,
    PRIMARY KEY (tenant_id, job_id, id)
);
ALTER TABLE draft_test_run
    DROP CONSTRAINT IF EXISTS draft_test_run_job,
    ADD CONSTRAINT draft_test_run_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS draft_test_run_id_check,
    ADD CONSTRAINT draft_test_run_id_check CHECK (id ~ '^tr-[1-9][0-9]{0,8}$'),
    DROP CONSTRAINT IF EXISTS draft_test_run_env_check,
    ADD CONSTRAINT draft_test_run_env_check CHECK (env = 'test'),
    DROP CONSTRAINT IF EXISTS draft_test_run_rev_check,
    ADD CONSTRAINT draft_test_run_rev_check CHECK (draft_rev >= 0),
    DROP CONSTRAINT IF EXISTS draft_test_run_json_check,
    ADD CONSTRAINT draft_test_run_json_check
        CHECK (jsonb_typeof(results) = 'array' AND jsonb_typeof(summary) = 'object'),
    DROP CONSTRAINT IF EXISTS draft_test_run_by_check,
    ADD CONSTRAINT draft_test_run_by_check CHECK (btrim(by) <> '');
CREATE INDEX IF NOT EXISTS draft_test_run_recent ON draft_test_run (tenant_id, job_id, at DESC);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['validation_question', 'validation_result', 'validation_cut_run',
                             'draft_test_run'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('DROP POLICY IF EXISTS job_scope ON %I', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
    END LOOP;
END $$;

REVOKE ALL ON validation_question, validation_result, validation_cut_run, draft_test_run FROM PUBLIC;
REVOKE ALL ON validation_question, validation_result, validation_cut_run, draft_test_run FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON validation_question, validation_result, validation_cut_run, draft_test_run TO agent_app;
