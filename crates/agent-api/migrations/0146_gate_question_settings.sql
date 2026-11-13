-- 0146 — B5 per-job gate question settings (plan 2026-10-20-b5-job-design, Data model `0146`, T8a; F-16, F-17).
-- Forward-only (pre-prod), drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT, as 0047).
--
-- Order: every object named here is ≤0146 (`jobs` 0003, `job_gate_question` 0143, roles 0001 / 0065).
--
-- `job_gate_question_setting`: one job's setting of one question (library or tenant) — on/off (F-17) and the text the
-- customer reads when the question hands over or blocks (F-16). NULL = the base (on; the question's own
-- `customer_reply`, else the care-bus default). Ledger kind `gate_setting`, key `gate-setting:<gate>`. The row is
-- deleted when both fields are back to the base. A safety question cannot be turned off (409 `safety_gate`, checked
-- at write: the library says which questions are safety questions, the table cannot).
CREATE TABLE IF NOT EXISTS job_gate_question_setting (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    job_id         uuid NOT NULL DEFAULT app_current_job(),
    gate           text NOT NULL,
    enabled        boolean,
    customer_reply text,
    updated_by     text NOT NULL,
    updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, gate)
);
ALTER TABLE job_gate_question_setting
    DROP CONSTRAINT IF EXISTS job_gate_question_setting_job,
    ADD CONSTRAINT job_gate_question_setting_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_gate_question_setting_gate_check,
    ADD CONSTRAINT job_gate_question_setting_gate_check CHECK (char_length(gate) BETWEEN 1 AND 64),
    DROP CONSTRAINT IF EXISTS job_gate_question_setting_reply_check,
    ADD CONSTRAINT job_gate_question_setting_reply_check
        CHECK (customer_reply IS NULL OR char_length(customer_reply) BETWEEN 1 AND 500),
    DROP CONSTRAINT IF EXISTS job_gate_question_setting_some_check,
    ADD CONSTRAINT job_gate_question_setting_some_check CHECK (num_nonnulls(enabled, customer_reply) >= 1),
    DROP CONSTRAINT IF EXISTS job_gate_question_setting_updated_by_check,
    ADD CONSTRAINT job_gate_question_setting_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_gate_question_setting ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_gate_question_setting;
CREATE POLICY tenant_isolation ON job_gate_question_setting FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_gate_question_setting;
CREATE POLICY job_scope ON job_gate_question_setting AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- A tenant question carries its own customer text (NULL = the care-bus default).
ALTER TABLE job_gate_question ADD COLUMN IF NOT EXISTS customer_reply text;
ALTER TABLE job_gate_question
    DROP CONSTRAINT IF EXISTS job_gate_question_customer_reply_check,
    ADD CONSTRAINT job_gate_question_customer_reply_check
        CHECK (customer_reply IS NULL OR char_length(customer_reply) BETWEEN 1 AND 500);

-- Grants: nothing for PUBLIC; agent_app (routes) reads and writes; agent_runtime (R7: `RuntimeDesign::read` of the
-- Test env reads the working rows, T12) reads only.
REVOKE ALL ON job_gate_question_setting FROM PUBLIC;
REVOKE ALL ON job_gate_question_setting FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_gate_question_setting TO agent_app;
REVOKE ALL ON job_gate_question_setting FROM agent_runtime;
GRANT SELECT ON job_gate_question_setting TO agent_runtime;
