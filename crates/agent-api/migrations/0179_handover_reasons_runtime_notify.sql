-- 0179 — the orchestrator's handover reasons and worker notify (B4 T6a, F-20; live HO-15/22/23–26). Forward-only
-- (pre-prod), drift-tolerant (DROP … IF EXISTS + ADD; REVOKE + GRANT of exactly these columns). References only
-- 0063, 0065, 0120 and 0121, so it applies whichever open branch merges first.
--
-- 1. Two J21 reasons (owner ruling with the B4 owner, 2026-10-24): `not_offered` — a required or routed operation
--    the job does not offer — and `success_criteria_unmet` — every step answered ok but the procedure's success
--    criteria do not hold. Both were recorded `tool_failed`, which stays the reason of an operation that did not
--    work. The CHECK is re-added as the full union: 0063's eight plus these two.
-- 2. `agent_runtime` grants for `conversations::handover::notify_workers`, which the B4 daemons (orchestrator,
--    interaction) run on their own pool after queueing an item: read the job's verified worker bindings
--    (`connector_bindings`, never the sealed credential or the verify code) and store the thread each worker channel
--    opens (`store::save_worker_ref`, an upsert on `channel_conversation_refs`). Without them every notify from a
--    daemon failed `42501` and was only logged. `tenant_id` / `job_id` for the `tenant_isolation` / `job_scope`
--    policies (FOR ALL, no TO clause: they apply to agent_runtime as to agent_app).

ALTER TABLE agent_handover_items DROP CONSTRAINT IF EXISTS agent_handover_items_reason_check;
ALTER TABLE agent_handover_items ADD CONSTRAINT agent_handover_items_reason_check CHECK (reason IN (
    'identity_failed', 'gate_blocked', 'customer_asked', 'tool_failed', 'unknown_outcome',
    'out_of_scope', 'staff_decision', 'gate_unavailable', 'not_offered', 'success_criteria_unmet'));

REVOKE ALL ON connector_bindings FROM agent_runtime;
GRANT SELECT (id, tenant_id, job_id, kind, side, settings, enabled, verified_at, created_at)
    ON connector_bindings TO agent_runtime;

REVOKE ALL ON channel_conversation_refs FROM agent_runtime;
GRANT SELECT ON channel_conversation_refs TO agent_runtime;
GRANT INSERT (tenant_id, job_id, conversation_id, side, item_id, channel, external_id, route, updated_at)
    ON channel_conversation_refs TO agent_runtime;
GRANT UPDATE (item_id, conversation_id, route, updated_at) ON channel_conversation_refs TO agent_runtime;
