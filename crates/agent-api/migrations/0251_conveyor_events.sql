-- 0251 — The conveyor's relay feed is `events`, not `activity` (2026-11-09): the conveyor emits
-- `_meta["ai.nonagentic/conveyor"] = {project_id, run_id, status, events: [{seq, type, payload, created_at}]}`
-- (crate::runtime::task_relay), so 0250's due function — reading `.../conveyor/activity`, an array that never
-- exists — never makes a working conveyor task due. This replaces the function with 0250's body unchanged except
-- for the conveyor path inside the COALESCE: `{_meta,ai.nonagentic/conveyor,events}` in both the jsonb_typeof CASE
-- and its THEN. The 1..200 limit guard, the integral-seq regex guard, SECURITY DEFINER, search_path, REVOKEs and
-- GRANT are identical. 0250's function is untouched: migrations are forward-only.
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
                                               CASE WHEN jsonb_typeof(COALESCE(m.last_detail #> '{_meta,ai.nonagentic/fr,activity}',
                                                                               m.last_detail #> '{_meta,ai.nonagentic/conveyor,events}')) = 'array'
                                                    THEN COALESCE(m.last_detail #> '{_meta,ai.nonagentic/fr,activity}',
                                                                  m.last_detail #> '{_meta,ai.nonagentic/conveyor,events}')
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
