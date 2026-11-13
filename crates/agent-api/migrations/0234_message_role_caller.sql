-- 0234 — a machine caller's message (A4, caller-identity plan): an API key / connector caller of a live job writes
-- role `caller`, distinct from `customer` (an end user) and from the platform's own `system` notices, so a platform
-- notice can never stand in for the user's confirmation of a write (runner step 4). Forward-only.
ALTER TABLE conversation_message DROP CONSTRAINT IF EXISTS conversation_message_role_check;
ALTER TABLE conversation_message ADD CONSTRAINT conversation_message_role_check
    CHECK (role IN ('customer', 'caller', 'agent', 'staff', 'system', 'note'));
