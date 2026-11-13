-- 0204 — round-robin auto-assignment of handovers (Staff Workspace, support routing). Forward-only (pre-prod).
-- References only 0001 `tenants`, 0032 `member_session`, 0063 `agent_handover_items`, 0061 `conversation_control` /
-- `conversation_message` and the 0200 owner model, so it applies after the reserved B8 range 0191–0199 and after
-- 0200–0203.
--
-- 1. `support_routing` (one row per tenant, RLS tenant): `mode` off | round_robin and `reassign_after_secs`. A tenant
--    without a row has the defaults (round_robin, 300 s): the row is written by the first PUT and, as the lock the
--    assigners serialise on, by the first assignment (`crate::support::routing`).
-- 2. `support_routing_member`: when a member was last handed an item (NULL first = least recently assigned).
-- 3. `agent_handover_items.auto_assigned_at`: set when routing assigned the item, cleared by every human Take /
--    Assign. Only an item that still carries it is ever reassigned by the sweeper (a human choice never is).
-- 4. `member_session.surface` gains `support` (the staff workspace) and `support_seen_at` records the last time the
--    session was seen THERE: a session has one row, so `surface` flips to whatever surface wrote last, but
--    `support_seen_at` only moves on a support write. Online = `support_seen_at` within 10 minutes.
-- 5. `agent_support_routing_due(limit)` (SECURITY DEFINER, read-only): the items the sweeper should act on, across
--    tenants (RLS would confine a plain read to one tenant). Ids only. The assignment itself runs per tenant, under
--    RLS, with the tenant's routing row locked.
--
-- Online = a member session seen on `support` within the last 10 minutes. A job with an enabled worker-side staff
-- binding (helpdesk, Slack, Teams, email) is never auto-assigned: the external tool routes there. Eligible = an enabled member holding
-- support_agent or support_manager.

-- 1. the setting
CREATE TABLE support_routing (
    tenant_id          uuid PRIMARY KEY DEFAULT app_current_tenant() REFERENCES tenants (id),
    mode               text NOT NULL DEFAULT 'round_robin' CHECK (mode IN ('off', 'round_robin')),
    reassign_after_secs integer NOT NULL DEFAULT 300 CHECK (reassign_after_secs BETWEEN 60 AND 86400),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text CHECK (updated_by IS NULL OR btrim(updated_by) <> '')
);
ALTER TABLE support_routing ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON support_routing FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON support_routing FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE ON support_routing TO agent_app;

-- 2. who was handed an item last
CREATE TABLE support_routing_member (
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    sub              text NOT NULL CHECK (btrim(sub) <> ''),
    last_assigned_at timestamptz,
    PRIMARY KEY (tenant_id, sub)
);
ALTER TABLE support_routing_member ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON support_routing_member FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON support_routing_member FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE ON support_routing_member TO agent_app;

-- 3. how the item was assigned
ALTER TABLE agent_handover_items ADD COLUMN IF NOT EXISTS auto_assigned_at timestamptz;

-- 4. the support surface
ALTER TABLE member_session DROP CONSTRAINT IF EXISTS member_session_surface_check;
ALTER TABLE member_session ADD CONSTRAINT member_session_surface_check
    CHECK (surface IN ('console', 'chat', 'admin', 'support'));
ALTER TABLE member_session ADD COLUMN IF NOT EXISTS support_seen_at timestamptz;

-- 5. the sweeper's read
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
          JOIN tenants t ON t.id = h.tenant_id AND t.status = 'active'
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
          JOIN tenants t ON t.id = h.tenant_id AND t.status = 'active'
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
