-- 0252 — the DRIVERS usage report probes agent_case by conversation after the event time.
--
-- The 0060 index (tenant_id, job_id, conversation_id) cannot serve
-- `started_at >= u.occurred_at ORDER BY started_at LIMIT 1` without a filter + sort per probe.
-- Extending it with started_at lets each conversation-arm probe be one index range scan. The
-- four-column index serves every prefix query of the three-column one, so the old index is dropped
-- rather than kept as write amplification.
-- Build the replacement first: CREATE INDEX takes SHARE (reads continue) while DROP INDEX takes
-- ACCESS EXCLUSIVE, so dropping last shortens the read-blocking window to the commit itself.
CREATE INDEX IF NOT EXISTS agent_case_conversation_started
    ON agent_case (tenant_id, job_id, conversation_id, started_at);
DROP INDEX IF EXISTS agent_case_conversation;
