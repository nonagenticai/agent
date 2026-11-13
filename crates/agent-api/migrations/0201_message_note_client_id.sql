-- 0201 — typed staff messages (Staff Workspace T3). Forward-only (pre-prod). References only 0061
-- `conversation_message`, so it applies after the reserved B8 range 0191–0199 and after 0200 in either order.
--
-- 1. `role` gains `note`: a staff note on the conversation. It is stored and shown to staff only; it is never delivered
--    to the customer's channel, never mirrored to a worker channel (SW-11) and never part of the model's history.
-- 2. `client_msg_id`: the sender's id of one Send (`POST /api/jobs/{job}/conversations/{id}/messages`). A second Send
--    with the same id in the same conversation is the same message (idempotent retry); unique per conversation.
-- 3. `delivery_error`: the edge's code when a staff reply did not reach the customer's channel (`delivered_at` stays
--    NULL); cleared when a retry delivers it.
--
-- Grants: none. 0061 grants `agent_app` SELECT, INSERT, UPDATE and 0065 grants `agent_runtime` SELECT, INSERT on the
-- whole table, which covers the new columns.

-- 1. the role CHECK (0061's column CHECK is `conversation_message_role_check`)
ALTER TABLE conversation_message DROP CONSTRAINT IF EXISTS conversation_message_role_check;
ALTER TABLE conversation_message ADD CONSTRAINT conversation_message_role_check
    CHECK (role IN ('customer', 'agent', 'staff', 'system', 'note'));

-- 2. the client message id
ALTER TABLE conversation_message ADD COLUMN IF NOT EXISTS client_msg_id uuid;
CREATE UNIQUE INDEX IF NOT EXISTS conversation_message_client
    ON conversation_message (tenant_id, job_id, conversation_id, client_msg_id) WHERE client_msg_id IS NOT NULL;

-- 3. the delivery error
ALTER TABLE conversation_message ADD COLUMN IF NOT EXISTS delivery_error text;
ALTER TABLE conversation_message DROP CONSTRAINT IF EXISTS conversation_message_delivery_error_check;
ALTER TABLE conversation_message ADD CONSTRAINT conversation_message_delivery_error_check
    CHECK (delivery_error IS NULL OR (btrim(delivery_error) <> '' AND char_length(delivery_error) <= 64));
