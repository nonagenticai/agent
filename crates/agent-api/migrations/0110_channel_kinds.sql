-- 0110 — the canonical channel set, re-asserted (B13 §2). Forward-only (pre-prod), drift-tolerant.
--
-- B12 created these CHECKs inline (0060, 0061, 0063, 0067) under these names; a database that drifted (an older
-- set, a hand edit) gets the canonical set back. Production applies this after 0122; a fresh database in number order
-- (before 0120–0122). References only 0060, 0061, 0063 and 0067.

ALTER TABLE agent_case DROP CONSTRAINT IF EXISTS agent_case_channel_check;
ALTER TABLE agent_case ADD CONSTRAINT agent_case_channel_check CHECK (channel IN (
    'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api'));

ALTER TABLE conversation DROP CONSTRAINT IF EXISTS conversation_channel_check;
ALTER TABLE conversation ADD CONSTRAINT conversation_channel_check CHECK (channel IN (
    'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api'));

ALTER TABLE agent_handover_items DROP CONSTRAINT IF EXISTS agent_handover_items_channel_check;
ALTER TABLE agent_handover_items ADD CONSTRAINT agent_handover_items_channel_check CHECK (channel IN (
    'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api'));

ALTER TABLE case_csat DROP CONSTRAINT IF EXISTS case_csat_channel_check;
ALTER TABLE case_csat ADD CONSTRAINT case_csat_channel_check CHECK (channel IN (
    'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api'));
