-- 0250 — Conveyor direct interface (owner 2026-11-08): the conveyor is an external MCP server that authenticates the
-- agent with a platform-minted licence bearer (`AuthKind::LicenceBearer`, minted per call by the executor), so a
-- tenant template may declare that kind and stores no credential for it. The same template declares which of its
-- operations run as durable MCP tasks (`task_tools`, op keys), replacing the global `MCP_TASK_TOOLS` for an external
-- system; `mcp_tasks` records which origin started a task. Forward-only (pre-prod).
--
-- 1. `licence_bearer`: the 0092 column check gains the kind. Nothing is stored for it in `connector_credential`:
--    the token lives minutes, so a stored one would go stale (crate::connectors::wire::AuthKind).
ALTER TABLE tenant_connector_template
    DROP CONSTRAINT tenant_connector_template_auth_kind_check;
ALTER TABLE tenant_connector_template
    ADD CONSTRAINT tenant_connector_template_auth_kind_check CHECK (auth_kind IN
        ('none', 'api_key_header', 'basic', 'bearer_static', 'oauth2_client_credentials', 'licence_bearer', 'mtls'));

-- 2. The operations of this template that run as durable MCP tasks: `object.verb` op keys, checked by
--    crate::connectors::tenant_templates::check_task_tools (at most 16, unique, each one of the template's `ops`).
ALTER TABLE tenant_connector_template
    ADD COLUMN task_tools text[] NOT NULL DEFAULT '{}';

-- 3. Which server started a task: the platform MCP server (`platform`, `origin_instance` NULL) or a tenant's
--    connector instance (`connector`, its id). A platform task has no instance; a connector task always has one.
ALTER TABLE mcp_tasks
    ADD COLUMN origin text NOT NULL DEFAULT 'platform' CHECK (origin IN ('platform', 'connector')),
    ADD COLUMN origin_instance text,
    ADD CONSTRAINT mcp_tasks_origin_instance CHECK ((origin = 'platform') = (origin_instance IS NULL));

-- 4. The relay's due set reads the conveyor's activity too (crate::runtime::task_relay): a conveyor task carries
--    `{project_id, run_id, activity}` on `_meta["ai.nonagentic/conveyor"]` instead of fr's channel (0249). The
--    activity path is fr's when present, else the conveyor's; everything else is 0249's function unchanged. The
--    activity seq is still read defensively (a missing/foreign `last_detail` is an empty list; an entry without a
--    numeric `seq` cannot be ordered and does not count).
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
                                                                               m.last_detail #> '{_meta,ai.nonagentic/conveyor,activity}')) = 'array'
                                                    THEN COALESCE(m.last_detail #> '{_meta,ai.nonagentic/fr,activity}',
                                                                  m.last_detail #> '{_meta,ai.nonagentic/conveyor,activity}')
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

-- 5. A task id is unique on its own server (SEP-2663), never across servers or tenants. A connector task is stored
--    under a namespaced key (tenant:instance:wire id) and the server's own id is kept beside it; a platform task's
--    key stays its wire id. The key widens because the namespace prefix is added.
ALTER TABLE mcp_tasks ADD COLUMN server_task_id text;
UPDATE mcp_tasks SET server_task_id = task_id;
ALTER TABLE mcp_tasks ALTER COLUMN server_task_id SET NOT NULL;
ALTER TABLE mcp_tasks DROP CONSTRAINT mcp_tasks_task_id_check;
ALTER TABLE mcp_tasks ADD CONSTRAINT mcp_tasks_task_id_check CHECK (length(task_id) BETWEEN 1 AND 512);
