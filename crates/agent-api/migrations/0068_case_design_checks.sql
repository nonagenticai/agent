-- 0068 — the design a case runs on (B4 T2b; plan §0 R2, Q-B4-1). Forward-only (pre-prod), drift-tolerant.
--
-- B12 shipped `agent_case` (0060), `agent_handover_items` (0063) and `case_csat` (0067) with nullable `release_id` /
-- `draft_rev` and neither a foreign key nor an env CHECK (B2 was not merged). This adds both, `NOT VALID` (rows B12
-- wrote before its writers stamped a design are left alone; every new or updated row is checked):
--   - `(tenant_id, job_id, release_id) → job_release` (0045). MATCH SIMPLE: a NULL `release_id` is not checked.
--   - the env CHECK (Q-B4-1 default): a `test` row carries `draft_rev` and no release; a `prod` row carries no
--     `draft_rev` and a release, or NULL when the job was never released (it runs its baseline,
--     `CaseDesign::Live { release_id: None }`).
-- `test_case.release_id` stays as 0061 made it.
--
-- And drops `conversation_message.grounding` (0061): `answer_grounding` (0122) is the only grounding store (§0 R3);
-- nothing reads or writes the column.
--
-- References only 0045, 0060, 0061, 0063 and 0067: it applies in number order on a fresh database and after 0122 in
-- production.

ALTER TABLE agent_case
    DROP CONSTRAINT IF EXISTS agent_case_release,
    ADD CONSTRAINT agent_case_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id) NOT VALID,
    DROP CONSTRAINT IF EXISTS agent_case_env_design_check,
    ADD CONSTRAINT agent_case_env_design_check CHECK (
        (env = 'test' AND draft_rev IS NOT NULL AND release_id IS NULL)
        OR (env = 'prod' AND draft_rev IS NULL)) NOT VALID;

ALTER TABLE agent_handover_items
    DROP CONSTRAINT IF EXISTS agent_handover_items_release,
    ADD CONSTRAINT agent_handover_items_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id) NOT VALID,
    DROP CONSTRAINT IF EXISTS agent_handover_items_env_design_check,
    ADD CONSTRAINT agent_handover_items_env_design_check CHECK (
        (env = 'test' AND draft_rev IS NOT NULL AND release_id IS NULL)
        OR (env = 'prod' AND draft_rev IS NULL)) NOT VALID;

ALTER TABLE case_csat
    DROP CONSTRAINT IF EXISTS case_csat_release,
    ADD CONSTRAINT case_csat_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id) NOT VALID,
    DROP CONSTRAINT IF EXISTS case_csat_env_design_check,
    ADD CONSTRAINT case_csat_env_design_check CHECK (
        (env = 'test' AND draft_rev IS NOT NULL AND release_id IS NULL)
        OR (env = 'prod' AND draft_rev IS NULL)) NOT VALID;

ALTER TABLE conversation_message DROP COLUMN IF EXISTS grounding;
