-- 0130 — Telegram and Helpdesk channel kinds (B7 remaining, R6/R9). Forward-only, drift-tolerant like 0120/0121:
-- each CHECK is dropped and re-added. Numbered after 0120/0121 because it rewrites their constraints (a fresh DB
-- applies in number order, so a lower number would be overwritten by 0120). References only 0120/0121 objects.
--
-- `telegram`: customer side, the owner's test bot (R7, D75). `helpdesk`: worker side, Frappe Helpdesk tickets (R9);
-- its worker thread refs (`channel_conversation_refs.channel = 'helpdesk'`, external_id = ticket name).

ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_kind_check;
ALTER TABLE connector_bindings
    ADD CONSTRAINT connector_bindings_kind_check
    CHECK (kind IN ('teams', 'slack', 'whatsapp', 'email', 'web_chat', 'telegram', 'helpdesk'));

ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_channel_check;
ALTER TABLE channel_conversation_refs ADD CONSTRAINT channel_conversation_refs_channel_check CHECK (channel IN (
    'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api', 'helpdesk'));
