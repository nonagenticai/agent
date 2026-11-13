-- 0207 — SW-7: a per-tenant platform flag in front of round-robin auto-assignment (owner 2026-10-27). Forward-only.
--
-- `tenants.support_auto_assign` is OFF by default: with it off nothing is auto-assigned, whatever
-- `support_routing.mode` says (`crate::support::routing::route_item` re-checks it under the tenant row lock, the
-- sweeper's read below filters on it, and the NOTIFY path ends in `route_item`). Only a platform super_admin turns it
-- on (`PATCH /api/tenants/{id}`); a tenant's own roles never write it. `agent_app` holds a table-wide UPDATE grant on
-- `tenants` (0001), so the database cannot tell that handler from other app code: the handler in
-- `routes::tenants` / `tenants::lifecycle::set_support_auto_assign` is the only writer, by construction.
-- Existing tenants default to OFF (auto-assignment stops on deploy until a super_admin enables it).

ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS support_auto_assign boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS support_auto_assign_set_by text
        CHECK (support_auto_assign_set_by IS NULL OR btrim(support_auto_assign_set_by) <> ''),
    ADD COLUMN IF NOT EXISTS support_auto_assign_set_at timestamptz;

-- the sweeper's read (0204), now limited to tenants with the flag on
CREATE OR REPLACE FUNCTION agent_support_routing_due(p_limit integer)
    RETURNS TABLE (tenant_id uuid, job_id uuid, conversation_id text, item_id text, kind text, assignee text)
    LANGUAGE plpgsql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
#variable_conflict use_column
BEGIN
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 1000 THEN
        RAISE EXCEPTION 'agent_support_routing_due: limit must be 1..1000' USING ERRCODE = '22023';
    END IF;
    RETURN QUERY
    WITH online AS (
        SELECT u.tenant_id, u.sub
          FROM users u
         WHERE u.disabled_at IS NULL
           AND u.roles && ARRAY['support_agent', 'support_manager']
           AND EXISTS (SELECT 1 FROM member_session s
                        WHERE s.tenant_id = u.tenant_id AND s.sub = u.sub
                          AND s.support_seen_at > now() - interval '10 minutes')
    ),
    due AS (
        SELECT h.tenant_id, h.job_id, h.conversation_id, h.id AS item_id, 'queued'::text AS kind,
               NULL::text AS assignee, h.queued_at AS due_at
          FROM agent_handover_items h
          JOIN conversation_control cc
            ON cc.tenant_id = h.tenant_id AND cc.job_id = h.job_id AND cc.conversation_id = h.conversation_id
          JOIN tenants t ON t.id = h.tenant_id AND t.status = 'active' AND t.support_auto_assign
          LEFT JOIN support_routing r ON r.tenant_id = h.tenant_id
         WHERE h.state = 'queued' AND cc.mode = 'queue'
           AND COALESCE(r.mode, 'round_robin') = 'round_robin'
           AND EXISTS (SELECT 1 FROM online o WHERE o.tenant_id = h.tenant_id)
           AND NOT EXISTS (SELECT 1 FROM connector_bindings b WHERE b.tenant_id = h.tenant_id
                            AND b.job_id = h.job_id AND b.side = 'worker' AND b.enabled)
        UNION ALL
        SELECT h.tenant_id, h.job_id, h.conversation_id, h.id, 'stale'::text, cc.by_sub, h.auto_assigned_at
          FROM agent_handover_items h
          JOIN conversation_control cc
            ON cc.tenant_id = h.tenant_id AND cc.job_id = h.job_id AND cc.conversation_id = h.conversation_id
          JOIN tenants t ON t.id = h.tenant_id AND t.status = 'active' AND t.support_auto_assign
          LEFT JOIN support_routing r ON r.tenant_id = h.tenant_id
         WHERE h.state = 'assigned' AND h.auto_assigned_at IS NOT NULL
           AND cc.mode = 'staff' AND cc.by_sub IS NOT NULL
           AND COALESCE(r.mode, 'round_robin') = 'round_robin'
           AND NOT EXISTS (SELECT 1 FROM connector_bindings b WHERE b.tenant_id = h.tenant_id
                            AND b.job_id = h.job_id AND b.side = 'worker' AND b.enabled)
           AND h.auto_assigned_at < now() - make_interval(secs => COALESCE(r.reassign_after_secs, 300))
           AND NOT EXISTS (SELECT 1 FROM online o WHERE o.tenant_id = h.tenant_id AND o.sub = cc.by_sub)
           AND EXISTS (SELECT 1 FROM online o WHERE o.tenant_id = h.tenant_id AND o.sub <> cc.by_sub)
           AND NOT EXISTS (SELECT 1 FROM conversation_message m
                            WHERE m.tenant_id = h.tenant_id AND m.job_id = h.job_id
                              AND m.conversation_id = h.conversation_id
                              AND m.role IN ('staff', 'note') AND m.author_sub = cc.by_sub
                              AND m.at_ms > (extract(epoch FROM h.auto_assigned_at) * 1000)::bigint)
    )
    SELECT d.tenant_id, d.job_id, d.conversation_id, d.item_id, d.kind, d.assignee
      FROM due d
     ORDER BY d.due_at, d.item_id
     LIMIT p_limit;
END
$$;
REVOKE ALL ON FUNCTION agent_support_routing_due(integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_support_routing_due(integer) TO agent_app;
