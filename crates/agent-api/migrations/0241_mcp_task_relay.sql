-- B2 (caller identity Part B): agent-api relays an MCP task's questions and result to the conversation whose case
-- started it (crate::runtime::task_relay). Its sweep finds that work across tenants through this definer only; it
-- returns ids, never a question, a result or an answer, and the relay reads the row under the tenant's own scope.
CREATE FUNCTION agent_mcp_task_relay_due(p_limit integer)
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
           AND (m.status = 'input_required' OR (m.terminal_at IS NOT NULL AND m.delivered_at IS NULL))
         ORDER BY m.updated_at
         LIMIT p_limit;
    END
    $$;
REVOKE ALL ON FUNCTION agent_mcp_task_relay_due(integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_mcp_task_relay_due(integer) FROM agent_runtime;
GRANT EXECUTE ON FUNCTION agent_mcp_task_relay_due(integer) TO agent_app;

CREATE INDEX mcp_tasks_relay ON mcp_tasks (updated_at)
    WHERE case_id IS NOT NULL AND (status = 'input_required' OR (terminal_at IS NOT NULL AND delivered_at IS NULL));
