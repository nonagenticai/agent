-- 0121 — provider conversation references (B12 §2, B11 phase B). Forward-only (pre-prod), drift-tolerant like 0120.
--
-- Maps a provider conversation / thread key to an agent conversation, per side: `customer` = where the customer
-- writes (reply there), `worker` = the staff thread a handover item was posted to (`item_id`; a reply there is a
-- staff reply). `route` is the edge's `ConversationRef.route` (service URL, channel, thread ts, …). References
-- ≤0033 objects, 0061 and 0063.

CREATE TABLE IF NOT EXISTS channel_conversation_refs (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    conversation_id text NOT NULL,
    side            text NOT NULL,
    item_id         text,
    channel         text NOT NULL,
    external_id     text NOT NULL,
    route           jsonb NOT NULL DEFAULT '{}',
    updated_at      timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_tenant;
ALTER TABLE channel_conversation_refs
    ADD CONSTRAINT channel_conversation_refs_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_conversation;
ALTER TABLE channel_conversation_refs ADD CONSTRAINT channel_conversation_refs_conversation
    FOREIGN KEY (tenant_id, job_id, conversation_id) REFERENCES conversation (tenant_id, job_id, conversation_id);
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_item;
ALTER TABLE channel_conversation_refs ADD CONSTRAINT channel_conversation_refs_item
    FOREIGN KEY (tenant_id, job_id, item_id) REFERENCES agent_handover_items (tenant_id, job_id, id);
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_side_check;
ALTER TABLE channel_conversation_refs
    ADD CONSTRAINT channel_conversation_refs_side_check CHECK (side IN ('customer', 'worker'));
-- Only a worker thread names a handover item.
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_item_check;
ALTER TABLE channel_conversation_refs
    ADD CONSTRAINT channel_conversation_refs_item_check CHECK (side = 'worker' OR item_id IS NULL);
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_channel_check;
ALTER TABLE channel_conversation_refs ADD CONSTRAINT channel_conversation_refs_channel_check CHECK (channel IN (
    'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api'));
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_external_id_check;
ALTER TABLE channel_conversation_refs ADD CONSTRAINT channel_conversation_refs_external_id_check
    CHECK (btrim(external_id) <> '' AND char_length(external_id) <= 512);
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_route_check;
ALTER TABLE channel_conversation_refs
    ADD CONSTRAINT channel_conversation_refs_route_check CHECK (jsonb_typeof(route) = 'object');
ALTER TABLE channel_conversation_refs DROP CONSTRAINT IF EXISTS channel_conversation_refs_key;
ALTER TABLE channel_conversation_refs
    ADD CONSTRAINT channel_conversation_refs_key UNIQUE (tenant_id, job_id, channel, side, external_id);
CREATE INDEX IF NOT EXISTS channel_conversation_refs_conversation_idx
    ON channel_conversation_refs (tenant_id, job_id, conversation_id, side);
CREATE INDEX IF NOT EXISTS channel_conversation_refs_item_idx
    ON channel_conversation_refs (tenant_id, job_id, item_id) WHERE item_id IS NOT NULL;

ALTER TABLE channel_conversation_refs ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON channel_conversation_refs;
CREATE POLICY tenant_isolation ON channel_conversation_refs FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON channel_conversation_refs;
CREATE POLICY job_scope ON channel_conversation_refs AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON channel_conversation_refs FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON channel_conversation_refs TO agent_app;
