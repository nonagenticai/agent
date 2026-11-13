-- 0150 — the handover notice delivered ack (B7 R6 follow-up; crate::conversations::internal `notice-delivered`).
-- Forward-only (pre-prod), drift-tolerant (IF NOT EXISTS). References only 0063's `agent_handover_items`, whose
-- table-level UPDATE grant to `agent_app` covers the new column.
--
-- The edge stamps it once the customer got the item's handover notice; a resent accept of the queuing message then
-- gets `duplicate`, never the notice a second time (`handover::queued_by`). NULL = not (yet) known delivered.

ALTER TABLE agent_handover_items ADD COLUMN IF NOT EXISTS notice_delivered_at timestamptz;
