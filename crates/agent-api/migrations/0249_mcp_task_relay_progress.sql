-- 0249 — MCP task relay progress (owner 2026-11-06): the MCP server's `tasks/get` result carries fr's project id, its
-- activity log and its signals on `_meta["ai.nonagentic/fr"]` (rmcp flattens the task object and drops unknown
-- TOP-LEVEL fields, so `_meta` is the only channel). crate::runtime::task_relay relays them while the task is live.
-- Forward-only (pre-prod).
--
-- 1. How far the conversation has been brought: the highest activity `seq` relayed and the status seen at that
--    point. `last_relayed_status` starts NULL, so a task's first sight is due (that is also when the project id
--    first appears). The status values mirror `mcp_tasks.status` (0240).
ALTER TABLE mcp_tasks
    ADD COLUMN last_relayed_seq    bigint NOT NULL DEFAULT 0 CHECK (last_relayed_seq >= 0),
    ADD COLUMN last_relayed_status text CHECK (last_relayed_status IS NULL
        OR last_relayed_status IN ('working', 'input_required', 'completed', 'cancelled', 'failed', 'lost'));

-- 2. The sweep's due set gains a `working` task with something new to relay: the status changed since the last
--    sweep, or its activity advanced past `last_relayed_seq`. The current rows keep their behaviour: a task waiting
--    for input, and a terminal task whose outcome the conversation has not been given. The activity seq is read from
--    the fr meta channel, defensively (a missing/foreign `last_detail` is an empty list; an entry without a numeric
--    `seq` cannot be ordered and does not count).
CREATE OR REPLACE FUNCTION agent_mcp_task_relay_due(p_limit integer)
    RETURNS TABLE (tenant_id uuid, job_id uuid, task_id text)
    LANGUAGE plpgsql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    BEGIN
        IF p_limit IS NULL OR p_limit < 1 OR p_limit > 200 THEN
            RAISE EXCEPTION 'agent_mcp_task_relay_due: a limit of 1..200 is required'
                USING ERRCODE = 'invalid_parameter_value';
        END IF;
        RETURN QUERY
        SELECT m.tenant_id, m.job_id, m.task_id FROM mcp_tasks m
         WHERE m.case_id IS NOT NULL
           AND (m.status = 'input_required'
                OR (m.terminal_at IS NOT NULL AND m.delivered_at IS NULL)
                OR (m.status = 'working'
                    AND (m.last_relayed_status IS DISTINCT FROM m.status
                         OR COALESCE((SELECT max((e.value ->> 'seq')::bigint)
                                        FROM jsonb_array_elements(
                                               CASE WHEN jsonb_typeof(m.last_detail #> '{_meta,ai.nonagentic/fr,activity}') = 'array'
                                                    THEN m.last_detail #> '{_meta,ai.nonagentic/fr,activity}'
                                                    ELSE '[]'::jsonb END) AS e(value)
                                       -- Integral only, by the TEXT: jsonb_typeof says 'number' for 1.5 and
                                       -- ('1.5')::bigint raises 22P02, which would abort this query — and with it
                                       -- every sweep's relaying — on one malformed entry in tenant-supplied JSON.
                                       WHERE (e.value ->> 'seq') ~ '^[0-9]{1,18}$'), 0)
                            > m.last_relayed_seq)))
         ORDER BY m.updated_at
         LIMIT p_limit;
    END
    $$;
REVOKE ALL ON FUNCTION agent_mcp_task_relay_due(integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_mcp_task_relay_due(integer) FROM agent_runtime;
GRANT EXECUTE ON FUNCTION agent_mcp_task_relay_due(integer) TO agent_app;

-- The 0241 partial index covered only the input_required / terminal rows; the widened due set needs every case row.
DROP INDEX IF EXISTS mcp_tasks_relay;
CREATE INDEX mcp_tasks_relay ON mcp_tasks (updated_at) WHERE case_id IS NOT NULL;
