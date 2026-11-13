-- 0205 — conversation attachments (Staff Workspace SW-9; design 2026-10-27, integration-owner conditions 1–5).
-- Forward-only (pre-prod), drift-tolerant like 0183/0200: IF NOT EXISTS, constraints and policies dropped and
-- re-added. References only 0001 `tenants`, 0061 `conversation` / `conversation_message` and the 0065 `agent_runtime`
-- role, so it applies after 0065 and 0204 in any order (it grants `agent_runtime`, so 0065's REVOKE must run first).
--
-- A photo or PDF a customer sends (web chat, Telegram) or a staff member attaches to a reply. The bytes live in a
-- `bytea` column behind the `crate::attachments::store::AttachmentStore` trait (a later move to object storage
-- changes one impl, not the callers). Bytes are only ever what `crate::attachments::validate::accept` returned:
-- magic-byte checked, images re-encoded without metadata, PDFs free of active content.
--
-- 1. `message_id` is NULL until the attachment is linked to the message it was sent with (the stored message and the
--    link are written in one transaction); an unlinked upload is swept after one hour. Deleting a message unlinks it.
-- 2. The FK to `conversation` is ON DELETE CASCADE: retention (and demo reset) that deletes a conversation takes
--    its attachments with it.
-- 3. `deleted_at` is a soft delete (staff removes an unsent upload); a sweep removes the rows for good. Quota counts
--    live rows only.
-- 4. Grants: `agent_app` SELECT, INSERT, UPDATE (message_id, deleted_at), DELETE; `agent_runtime` SELECT (it reads
--    that a customer sent a file, never the bytes' content — v1 has no vision).

CREATE TABLE IF NOT EXISTS conversation_attachment (
    id              uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid        NOT NULL DEFAULT app_current_job(),
    conversation_id text        NOT NULL,
    message_id      uuid,
    direction       text        NOT NULL,
    media_type      text        NOT NULL,
    byte_size       integer     NOT NULL,
    sha256          bytea       NOT NULL,
    display_name    text        NOT NULL,
    bytes           bytea       NOT NULL,
    width           integer,
    height          integer,
    created_by      text        NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz
);

-- already-compressed payloads: no TOAST compression, out-of-line so a list of rows never drags the bytes along
ALTER TABLE conversation_attachment ALTER COLUMN bytes SET STORAGE EXTERNAL;

ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_conversation;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_conversation
    FOREIGN KEY (tenant_id, job_id, conversation_id)
    REFERENCES conversation (tenant_id, job_id, conversation_id) ON DELETE CASCADE;
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_message;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_message
    FOREIGN KEY (message_id) REFERENCES conversation_message (message_id) ON DELETE SET NULL;

ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_direction_check;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_direction_check
    CHECK (direction IN ('staff', 'customer'));
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_media_type_check;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_media_type_check
    CHECK (media_type IN ('image/jpeg', 'image/png', 'image/webp', 'application/pdf'));
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_size_check;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_size_check
    CHECK (byte_size BETWEEN 1 AND 10485760 AND octet_length(bytes) = byte_size);
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_sha256_check;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_sha256_check
    CHECK (octet_length(sha256) = 32);
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_display_name_check;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_display_name_check
    CHECK (btrim(display_name) <> '' AND char_length(display_name) <= 80);
-- an image has its (re-encoded) dimensions, a PDF none
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_dims_check;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_dims_check CHECK (
    CASE WHEN media_type = 'application/pdf'
         THEN width IS NULL AND height IS NULL
         ELSE width BETWEEN 1 AND 4096 AND height BETWEEN 1 AND 4096 END);
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_created_by_check;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_created_by_check
    CHECK (btrim(created_by) <> '' AND char_length(created_by) <= 200
           AND (direction <> 'customer' OR created_by = 'customer'));

-- the transcript read, the FK's lookup and the per-tenant quota sum (leading tenant_id)
CREATE INDEX IF NOT EXISTS conversation_attachment_conversation
    ON conversation_attachment (tenant_id, job_id, conversation_id, created_at);
CREATE INDEX IF NOT EXISTS conversation_attachment_message
    ON conversation_attachment (message_id) WHERE message_id IS NOT NULL;
-- the sweep of unlinked uploads and of soft-deleted rows
CREATE INDEX IF NOT EXISTS conversation_attachment_unlinked
    ON conversation_attachment (created_at) WHERE message_id IS NULL;

ALTER TABLE conversation_attachment ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON conversation_attachment;
CREATE POLICY tenant_isolation ON conversation_attachment FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON conversation_attachment;
CREATE POLICY job_scope ON conversation_attachment AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

REVOKE ALL ON conversation_attachment FROM PUBLIC;
GRANT SELECT, INSERT, DELETE ON conversation_attachment TO agent_app;
GRANT UPDATE (message_id, deleted_at) ON conversation_attachment TO agent_app;
GRANT SELECT ON conversation_attachment TO agent_runtime;
