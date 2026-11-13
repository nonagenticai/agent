-- 0200 — one owner record per conversation (Staff Workspace T1, SW-1 / SW-2). Forward-only (pre-prod). References
-- only 0060 `agent_case`, 0061 `conversation` / `conversation_control`, 0063 `agent_handover_items` /
-- `agent_handover_events` and the 0065 `agent_runtime` role, so it applies after the reserved B8 range 0191–0199.
--
-- Two owner records existed: `agent_handover_items.assignee` (queue, claim, the item reply gate) and
-- `conversation_control.mode/by_sub` (console takeover), and no path wrote both. From here on:
--
-- 1. `conversation_control` is THE owner (`crate::conversations::ownership`): `mode` is `agent | queue | staff`
--    (`queue` = an open handover item nobody has taken) and `by_sub` is set exactly when `mode = 'staff'`. Every
--    conversation has a row (backfilled here; written when a conversation is created), so a command can lock it.
-- 2. Backfill, in this order: an `agent` row for every conversation without one; an open item with an assignee makes
--    its conversation `staff` by that assignee (the newest such item when several); an open item without one makes
--    its still agent-owned conversation `queue`. A console takeover (`staff`) without an assigned item keeps its
--    owner. Prod at authoring: 5 open items (2 assigned), control rows 1 agent + 1 staff.
-- 3. Items carry `closed_by` (the actor of the event that closed the item, else its last assignee) and
--    `resolution` (set by the later Resolve command).
-- 4. `agent_handover_items.assignee` and `.channel` are dropped. View `agent_handover_items_owned`
--    (security_invoker: the caller's grants and RLS on the base tables apply) derives them: `assignee` = the owner's
--    sub while the item is open and the conversation is staff-owned, else the item's `closed_by` once closed
--    (never `agent`); `channel` = the conversation's (the case's when the conversation row is missing).
--    `agent_handover_events.assignee` stays as history. No index named the dropped columns; the owner lookup by sub
--    ("mine") gets `conversation_control_staff_by`.
-- 5. Grants: SELECT on the view to `agent_app` and `agent_runtime` (both read `agent_handover_items`).
--    `agent_runtime` queues items (`handover::create_with` from the orchestrator / runner / drain), which now sets the
--    owner to `queue` in the same transaction, so it may write the owner columns of `conversation_control` and
--    `closed_by` of an item (`handover::move_to`).

-- 1. the owner CHECKs (0061's column CHECK is `conversation_control_mode_check`)
UPDATE conversation_control SET by_sub = NULL WHERE mode <> 'staff' AND by_sub IS NOT NULL;
UPDATE conversation_control SET mode = 'agent' WHERE mode = 'staff' AND by_sub IS NULL;
ALTER TABLE conversation_control DROP CONSTRAINT IF EXISTS conversation_control_mode_check;
ALTER TABLE conversation_control ADD CONSTRAINT conversation_control_mode_check
    CHECK (mode IN ('agent', 'queue', 'staff'));
ALTER TABLE conversation_control DROP CONSTRAINT IF EXISTS conversation_control_staff_by_check;
ALTER TABLE conversation_control ADD CONSTRAINT conversation_control_staff_by_check
    CHECK ((mode = 'staff') = (by_sub IS NOT NULL));
CREATE INDEX IF NOT EXISTS conversation_control_staff_by
    ON conversation_control (tenant_id, job_id, by_sub) WHERE mode = 'staff';

-- 2. backfill the owner of every conversation
INSERT INTO conversation_control (tenant_id, job_id, conversation_id, mode, by_sub, since)
SELECT v.tenant_id, v.job_id, v.conversation_id, 'agent', NULL, v.started_at
  FROM conversation v
 WHERE NOT EXISTS (SELECT 1 FROM conversation_control cc
                    WHERE cc.tenant_id = v.tenant_id AND cc.job_id = v.job_id
                      AND cc.conversation_id = v.conversation_id);

UPDATE conversation_control cc SET mode = 'staff', by_sub = a.assignee, since = now()
  FROM (SELECT DISTINCT ON (h.tenant_id, h.job_id, h.conversation_id)
               h.tenant_id, h.job_id, h.conversation_id, h.assignee
          FROM agent_handover_items h
         WHERE h.state NOT IN ('resolved', 'returned_to_agent') AND h.assignee IS NOT NULL
         ORDER BY h.tenant_id, h.job_id, h.conversation_id, h.queued_at DESC, h.id DESC) a
 WHERE cc.tenant_id = a.tenant_id AND cc.job_id = a.job_id AND cc.conversation_id = a.conversation_id;

UPDATE conversation_control cc SET mode = 'queue', by_sub = NULL, since = now()
 WHERE cc.mode = 'agent'
   AND EXISTS (SELECT 1 FROM agent_handover_items h
                WHERE h.tenant_id = cc.tenant_id AND h.job_id = cc.job_id
                  AND h.conversation_id = cc.conversation_id
                  AND h.state NOT IN ('resolved', 'returned_to_agent'));

-- 3. who closed an item, and how it was resolved
ALTER TABLE agent_handover_items ADD COLUMN IF NOT EXISTS closed_by text;
ALTER TABLE agent_handover_items ADD COLUMN IF NOT EXISTS resolution text;
ALTER TABLE agent_handover_items DROP CONSTRAINT IF EXISTS agent_handover_items_resolution_check;
ALTER TABLE agent_handover_items ADD CONSTRAINT agent_handover_items_resolution_check
    CHECK (resolution IN ('resolved', 'not_resolvable', 'duplicate'));

UPDATE agent_handover_items h
   SET closed_by = COALESCE(
           (SELECT e.actor FROM agent_handover_events e
             WHERE e.tenant_id = h.tenant_id AND e.job_id = h.job_id AND e.item_id = h.id
               AND e.to_state IN ('resolved', 'returned_to_agent')
             ORDER BY e.seq DESC LIMIT 1),
           h.assignee)
 WHERE h.state IN ('resolved', 'returned_to_agent') AND h.closed_by IS NULL;

-- 4. the derived assignee and channel
DROP VIEW IF EXISTS agent_handover_items_owned;
ALTER TABLE agent_handover_items DROP COLUMN IF EXISTS assignee;
ALTER TABLE agent_handover_items DROP COLUMN IF EXISTS channel;

CREATE VIEW agent_handover_items_owned WITH (security_invoker = true) AS
SELECT h.*,
       COALESCE(v.channel, a.channel) AS channel,
       CASE
           WHEN h.state IN ('resolved', 'returned_to_agent') THEN NULLIF(h.closed_by, 'agent')
           WHEN cc.mode = 'staff' THEN cc.by_sub
       END AS assignee
  FROM agent_handover_items h
  JOIN agent_case a
    ON a.tenant_id = h.tenant_id AND a.job_id = h.job_id AND a.case_id = h.case_id
  LEFT JOIN conversation v
    ON v.tenant_id = h.tenant_id AND v.job_id = h.job_id AND v.conversation_id = h.conversation_id
  LEFT JOIN conversation_control cc
    ON cc.tenant_id = h.tenant_id AND cc.job_id = h.job_id AND cc.conversation_id = h.conversation_id;

-- 5. grants
REVOKE ALL ON agent_handover_items_owned FROM PUBLIC;
GRANT SELECT ON agent_handover_items_owned TO agent_app;
GRANT SELECT ON agent_handover_items_owned TO agent_runtime;
GRANT INSERT (tenant_id, job_id, conversation_id, mode, by_sub, since) ON conversation_control TO agent_runtime;
GRANT UPDATE (mode, by_sub, since) ON conversation_control TO agent_runtime;
GRANT UPDATE (closed_by) ON agent_handover_items TO agent_runtime;
