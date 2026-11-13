-- 0143 — B5 job gates (plan 2026-10-20-b5-job-design, Data model `0143`, T3). Forward-only (pre-prod), drift-tolerant
-- (IF NOT EXISTS, DROP … IF EXISTS + ADD, CREATE OR REPLACE FUNCTION, as 0047).
--
-- Tenant rows only (R1): the platform gate library is B6a's `gate_question` layer rows (`catalogue.gates` at the
-- design's pins), never a table here. Generated options (GT/GS/GA/GO) are B2 `gate_option` overlay rows (R2); this
-- file refines LIBRARY options only. The verdict log never stores customer or reply text.
--
-- RLS: `tenant_isolation` plus a RESTRICTIVE `job_scope` (as 0060 `agent_case`). Grants are in 0145.

-- A tenant question of one job (`addGateQuestion`): noul only; `safety` is always false (no column).
CREATE TABLE IF NOT EXISTS job_gate_question (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    key             text NOT NULL,
    point           text NOT NULL,
    type            text NOT NULL DEFAULT 'noul',
    question        text NOT NULL,
    options         jsonb NOT NULL,
    threshold       text,
    threshold_value numeric,
    fail            text NOT NULL,
    effect          text,
    rule            jsonb NOT NULL,
    action          text NOT NULL,
    tools           text[],
    updated_by      text NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, key)
);
ALTER TABLE job_gate_question
    DROP CONSTRAINT IF EXISTS job_gate_question_job,
    ADD CONSTRAINT job_gate_question_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_gate_question_key_check,
    ADD CONSTRAINT job_gate_question_key_check CHECK (key ~ '^[A-Z][A-Z0-9]{1,7}$'),
    DROP CONSTRAINT IF EXISTS job_gate_question_point_check,
    ADD CONSTRAINT job_gate_question_point_check CHECK (point IN ('P1', 'P2', 'R1')),
    DROP CONSTRAINT IF EXISTS job_gate_question_type_check,
    ADD CONSTRAINT job_gate_question_type_check CHECK (type = 'noul'),
    DROP CONSTRAINT IF EXISTS job_gate_question_question_check,
    ADD CONSTRAINT job_gate_question_question_check CHECK (char_length(question) BETWEEN 8 AND 2000),
    DROP CONSTRAINT IF EXISTS job_gate_question_fail_check,
    ADD CONSTRAINT job_gate_question_fail_check CHECK (fail IN ('open', 'closed')),
    DROP CONSTRAINT IF EXISTS job_gate_question_rule_check,
    ADD CONSTRAINT job_gate_question_rule_check CHECK (jsonb_typeof(rule) = 'object'),
    DROP CONSTRAINT IF EXISTS job_gate_question_action_check,
    ADD CONSTRAINT job_gate_question_action_check CHECK (action IN ('handover', 'block', 'annotate')),
    DROP CONSTRAINT IF EXISTS job_gate_question_tools_check,
    ADD CONSTRAINT job_gate_question_tools_check CHECK (point = 'P2' OR tools IS NULL),
    DROP CONSTRAINT IF EXISTS job_gate_question_updated_by_check,
    ADD CONSTRAINT job_gate_question_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_gate_question ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_gate_question;
CREATE POLICY tenant_isolation ON job_gate_question FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_gate_question;
CREATE POLICY job_scope ON job_gate_question AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- A tenant refinement of one LIBRARY (non-generated) option. Ledger key `gate:<gate>:<option>`. Deleted when every
-- field is back to the base. `base_hash` = row_hash of the base option.
CREATE TABLE IF NOT EXISTS job_gate_option_refinement (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    gate        text NOT NULL,
    option      text NOT NULL,
    label       text,
    description text,
    hidden      boolean NOT NULL DEFAULT false,
    base_hash   text NOT NULL,
    updated_by  text NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, gate, option)
);
ALTER TABLE job_gate_option_refinement
    DROP CONSTRAINT IF EXISTS job_gate_option_refinement_job,
    ADD CONSTRAINT job_gate_option_refinement_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_gate_option_refinement_base_hash_check,
    ADD CONSTRAINT job_gate_option_refinement_base_hash_check CHECK (btrim(base_hash) <> ''),
    DROP CONSTRAINT IF EXISTS job_gate_option_refinement_updated_by_check,
    ADD CONSTRAINT job_gate_option_refinement_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_gate_option_refinement ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_gate_option_refinement;
CREATE POLICY tenant_isolation ON job_gate_option_refinement FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_gate_option_refinement;
CREATE POLICY job_scope ON job_gate_option_refinement AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- A tenant refinement of one question's threshold OR rule (a later edit clears the other). Ledger key
-- `gate:<gate>:threshold`. `base_hash` NULL for a tenant question.
CREATE TABLE IF NOT EXISTS job_gate_rule_refinement (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    gate            text NOT NULL,
    threshold_value numeric,
    rule            jsonb,
    base_hash       text,
    updated_by      text NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, gate)
);
ALTER TABLE job_gate_rule_refinement
    DROP CONSTRAINT IF EXISTS job_gate_rule_refinement_job,
    ADD CONSTRAINT job_gate_rule_refinement_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_gate_rule_refinement_one_check,
    ADD CONSTRAINT job_gate_rule_refinement_one_check CHECK (num_nonnulls(threshold_value, rule) <= 1),
    DROP CONSTRAINT IF EXISTS job_gate_rule_refinement_rule_check,
    ADD CONSTRAINT job_gate_rule_refinement_rule_check CHECK (rule IS NULL OR jsonb_typeof(rule) = 'object'),
    DROP CONSTRAINT IF EXISTS job_gate_rule_refinement_updated_by_check,
    ADD CONSTRAINT job_gate_rule_refinement_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_gate_rule_refinement ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_gate_rule_refinement;
CREATE POLICY tenant_isolation ON job_gate_rule_refinement FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_gate_rule_refinement;
CREATE POLICY job_scope ON job_gate_rule_refinement AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- One row per runtime gate-point call (the playground writes none). No customer or reply text.
CREATE TABLE IF NOT EXISTS gate_evaluation (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    at              timestamptz NOT NULL DEFAULT now(),
    point           text NOT NULL,
    env             text NOT NULL,
    release_id      text,
    draft_rev       bigint,
    conversation_id text NOT NULL,
    case_id         text,
    outcome         text NOT NULL,
    decision        text NOT NULL,
    deciding        text[] NOT NULL DEFAULT '{}',
    model           text,
    latency_ms      integer,
    error           text
);
ALTER TABLE gate_evaluation
    DROP CONSTRAINT IF EXISTS gate_evaluation_job,
    ADD CONSTRAINT gate_evaluation_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS gate_evaluation_point_check,
    ADD CONSTRAINT gate_evaluation_point_check CHECK (point IN ('P1', 'P2', 'R1')),
    DROP CONSTRAINT IF EXISTS gate_evaluation_env_check,
    ADD CONSTRAINT gate_evaluation_env_check CHECK (env IN ('prod', 'test')),
    DROP CONSTRAINT IF EXISTS gate_evaluation_draft_rev_check,
    ADD CONSTRAINT gate_evaluation_draft_rev_check CHECK (draft_rev IS NULL OR draft_rev >= 0),
    DROP CONSTRAINT IF EXISTS gate_evaluation_conversation_check,
    ADD CONSTRAINT gate_evaluation_conversation_check
        CHECK (btrim(conversation_id) <> '' AND char_length(conversation_id) <= 200),
    DROP CONSTRAINT IF EXISTS gate_evaluation_outcome_check,
    ADD CONSTRAINT gate_evaluation_outcome_check
        CHECK (outcome IN ('pass', 'fired', 'failed_open', 'failed_closed', 'skipped')),
    DROP CONSTRAINT IF EXISTS gate_evaluation_decision_check,
    ADD CONSTRAINT gate_evaluation_decision_check CHECK (decision IN ('allow', 'block', 'handover')),
    DROP CONSTRAINT IF EXISTS gate_evaluation_latency_check,
    ADD CONSTRAINT gate_evaluation_latency_check CHECK (latency_ms IS NULL OR latency_ms >= 0);
CREATE INDEX IF NOT EXISTS gate_evaluation_recent ON gate_evaluation (tenant_id, job_id, at DESC, id DESC);
ALTER TABLE gate_evaluation ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON gate_evaluation;
CREATE POLICY tenant_isolation ON gate_evaluation FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON gate_evaluation;
CREATE POLICY job_scope ON gate_evaluation AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- One row per asked question of an evaluation: the answer LABEL, never the text. `unsure` = below `min_confidence`.
CREATE TABLE IF NOT EXISTS gate_answer (
    evaluation_id bigint NOT NULL,
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    job_id        uuid NOT NULL DEFAULT app_current_job(),
    at            timestamptz NOT NULL DEFAULT now(),
    gate          text NOT NULL,
    answer        text,
    value         text,
    measure       double precision,
    confidence    double precision,
    fired         boolean NOT NULL DEFAULT false,
    unsure        boolean NOT NULL DEFAULT false,
    action        text,
    PRIMARY KEY (evaluation_id, gate)
);
ALTER TABLE gate_answer
    DROP CONSTRAINT IF EXISTS gate_answer_evaluation,
    ADD CONSTRAINT gate_answer_evaluation FOREIGN KEY (evaluation_id) REFERENCES gate_evaluation (id)
        ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS gate_answer_job,
    ADD CONSTRAINT gate_answer_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS gate_answer_action_check,
    ADD CONSTRAINT gate_answer_action_check CHECK (action IS NULL OR action IN ('handover', 'block', 'annotate'));
CREATE INDEX IF NOT EXISTS gate_answer_recent ON gate_answer (tenant_id, job_id, gate, at DESC);
ALTER TABLE gate_answer ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON gate_answer;
CREATE POLICY tenant_isolation ON gate_answer FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON gate_answer;
CREATE POLICY job_scope ON gate_answer AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- Retention: delete the caller's gate log older than 90 days (SECURITY INVOKER: RLS scopes it to the caller's tenant
-- and, when set, job; `gate_answer` rows go with their evaluation by the FK cascade). DB-only, never a Laya call.
-- Returns the number of evaluations deleted. EXECUTE: agent_app only (0145).
CREATE OR REPLACE FUNCTION job_design_purge_gate_log() RETURNS bigint
    LANGUAGE sql VOLATILE SECURITY INVOKER
    SET search_path = public, pg_temp
    AS $$
        WITH gone AS (
            DELETE FROM gate_evaluation WHERE at < now() - interval '90 days' RETURNING 1
        )
        SELECT count(*) FROM gone
    $$;
