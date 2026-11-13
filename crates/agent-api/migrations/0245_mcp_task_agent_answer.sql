-- 0245 — the agent answers an MCP task's question (owner 2026-10-31): every fr task message is handled by the
-- platform's agents unless there is an absolute need for user input; the customer is asked only for what the agent
-- leaves out. Forward-only.
--
-- One model attempt per QUESTION per ten minutes (fr asks several questions on one task, so a task-level marker
-- would deny the agent its turn on the second question): the relay records when it tried and which question it
-- tried (`agent_attempted_question`), both under the tenant/job-scoped transaction. The attempt is a claim (another
-- relay replica leaves the question alone while it runs); `agent_done_question` records that the agent has finished
-- with a question (answered it, or left fields for the customer), and only then may the customer be asked.
ALTER TABLE mcp_tasks
    ADD COLUMN IF NOT EXISTS agent_attempted_at       timestamptz,
    ADD COLUMN IF NOT EXISTS agent_attempted_question text,
    ADD COLUMN IF NOT EXISTS agent_done_question      text;
