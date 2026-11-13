-- 0244 — MCP task escalation (owner 2026-10-31): the customer answers a task's questions by default; staff take over
-- in the Tasks inbox when fr refuses an answer the agent cannot give, when nobody answers in time, when the task
-- fails, or when the conversation is handed over. Forward-only.

-- An answer fr refused is kept (audit) and no longer counts as the question's answer, so the question opens again
-- for staff: one row per attempt, at most one that still counts per question. `outcome`: NULL = it stands,
-- `refused` = fr refused it (the question is open again), `dropped` = the question closed elsewhere in fr.
ALTER TABLE mcp_task_answers
    ADD COLUMN attempt      smallint NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    ADD COLUMN outcome      text CHECK (outcome IN ('refused', 'dropped')),
    ADD COLUMN refusal_code text CHECK (refusal_code IS NULL OR length(refusal_code) BETWEEN 1 AND 64),
    ADD COLUMN refused_at   timestamptz,
    -- The conversation messages a customer's answer came from (the job answered for them; fr records only the job).
    ADD COLUMN source_messages text[] NOT NULL DEFAULT '{}',
    ADD CONSTRAINT mcp_task_answers_refusal CHECK ((outcome IS NULL) = (refused_at IS NULL));
ALTER TABLE mcp_task_answers DROP CONSTRAINT mcp_task_answers_pkey;
ALTER TABLE mcp_task_answers ADD PRIMARY KEY (task_id, input_key, attempt);
CREATE UNIQUE INDEX mcp_task_answers_standing ON mcp_task_answers (task_id, input_key)
    WHERE outcome IS DISTINCT FROM 'refused';

-- Escalation: when and why staff own the task now; the customer is told once (`escalation_notified_at`).
-- `input_since`: when the task last started waiting for input (the no-answer timeout counts from it).
ALTER TABLE mcp_tasks
    ADD COLUMN escalated_at           timestamptz,
    ADD COLUMN escalation_reason      text CHECK (escalation_reason IS NULL OR length(escalation_reason) BETWEEN 1 AND 80),
    ADD COLUMN escalation_notified_at timestamptz,
    ADD COLUMN input_since            timestamptz,
    ADD CONSTRAINT mcp_tasks_escalation CHECK ((escalated_at IS NULL) = (escalation_reason IS NULL));
UPDATE mcp_tasks SET input_since = updated_at WHERE status = 'input_required';
