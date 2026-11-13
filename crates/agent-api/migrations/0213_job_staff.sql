-- 0213 — D125 (tenant job design): the staff side is configured per job. Forward-only (pre-prod).
--
-- 1. `job_staff`: the job's staff team. No rows for a job = every conversation worker of the tenant (today's
--    behaviour). A support_agent works a job's conversations only when the team is empty or they are on it; a
--    support_manager works every job (crate::support::job_staff).
-- 2. `job_routing`: the job's auto-assign setting: `inherit` (the tenant's `support_routing`), `off`, `round_robin`,
--    with an optional own reassign window.
-- 3. `agent_support_routing_due` (0207) honours both: a job's mode `off` excludes it, `inherit` uses the tenant mode,
--    the reassign window is coalesce(job, tenant), and only online members of the job's team count as online for it.

CREATE TABLE job_staff (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id    uuid NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    sub       text NOT NULL CHECK (btrim(sub) <> ''),
    added_by  text NOT NULL CHECK (btrim(added_by) <> ''),
    added_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id, sub)
);
CREATE INDEX job_staff_sub_idx ON job_staff (tenant_id, sub);
ALTER TABLE job_staff ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON job_staff FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_staff FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_staff TO agent_app;

CREATE TABLE job_routing (
    tenant_id           uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id              uuid NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    mode                text NOT NULL DEFAULT 'inherit' CHECK (mode IN ('inherit', 'off', 'round_robin')),
    reassign_after_secs integer CHECK (reassign_after_secs IS NULL OR reassign_after_secs BETWEEN 60 AND 86400),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    updated_by          text CHECK (updated_by IS NULL OR btrim(updated_by) <> ''),
    PRIMARY KEY (tenant_id, job_id)
);
ALTER TABLE job_routing ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON job_routing FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_routing FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_routing TO agent_app;

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
          LEFT JOIN job_routing jr ON jr.tenant_id = h.tenant_id AND jr.job_id = h.job_id
         WHERE h.state = 'queued' AND cc.mode = 'queue'
           AND (CASE WHEN COALESCE(jr.mode, 'inherit') = 'inherit' THEN COALESCE(r.mode, 'round_robin')
                     ELSE jr.mode END) = 'round_robin'
           AND EXISTS (SELECT 1 FROM online o
                        WHERE o.tenant_id = h.tenant_id
                          AND (NOT EXISTS (SELECT 1 FROM job_staff js
                                            WHERE js.tenant_id = h.tenant_id AND js.job_id = h.job_id)
                               OR EXISTS (SELECT 1 FROM job_staff js
                                           WHERE js.tenant_id = h.tenant_id AND js.job_id = h.job_id
                                             AND js.sub = o.sub)))
           AND NOT EXISTS (SELECT 1 FROM connector_bindings b WHERE b.tenant_id = h.tenant_id
                            AND b.job_id = h.job_id AND b.side = 'worker' AND b.enabled)
        UNION ALL
        SELECT h.tenant_id, h.job_id, h.conversation_id, h.id, 'stale'::text, cc.by_sub, h.auto_assigned_at
          FROM agent_handover_items h
          JOIN conversation_control cc
            ON cc.tenant_id = h.tenant_id AND cc.job_id = h.job_id AND cc.conversation_id = h.conversation_id
          JOIN tenants t ON t.id = h.tenant_id AND t.status = 'active' AND t.support_auto_assign
          LEFT JOIN support_routing r ON r.tenant_id = h.tenant_id
          LEFT JOIN job_routing jr ON jr.tenant_id = h.tenant_id AND jr.job_id = h.job_id
         WHERE h.state = 'assigned' AND h.auto_assigned_at IS NOT NULL
           AND cc.mode = 'staff' AND cc.by_sub IS NOT NULL
           AND (CASE WHEN COALESCE(jr.mode, 'inherit') = 'inherit' THEN COALESCE(r.mode, 'round_robin')
                     ELSE jr.mode END) = 'round_robin'
           AND NOT EXISTS (SELECT 1 FROM connector_bindings b WHERE b.tenant_id = h.tenant_id
                            AND b.job_id = h.job_id AND b.side = 'worker' AND b.enabled)
           AND h.auto_assigned_at < now() - make_interval(
                   secs => COALESCE(jr.reassign_after_secs, r.reassign_after_secs, 300))
           AND NOT EXISTS (SELECT 1 FROM online o WHERE o.tenant_id = h.tenant_id AND o.sub = cc.by_sub)
           AND EXISTS (SELECT 1 FROM online o
                        WHERE o.tenant_id = h.tenant_id AND o.sub <> cc.by_sub
                          AND (NOT EXISTS (SELECT 1 FROM job_staff js
                                            WHERE js.tenant_id = h.tenant_id AND js.job_id = h.job_id)
                               OR EXISTS (SELECT 1 FROM job_staff js
                                           WHERE js.tenant_id = h.tenant_id AND js.job_id = h.job_id
                                             AND js.sub = o.sub)))
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
