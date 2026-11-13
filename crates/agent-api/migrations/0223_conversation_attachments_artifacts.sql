-- 0223 — conversation attachments move their bytes to artifact_service (Staff Workspace SW-9).
-- Forward-only (pre-prod), drift-tolerant like 0183/0200. Builds on 0205 (`conversation_attachment`, which stored the
-- bytes in a `bytea` column) and does not edit it: 0205 is already folded into the base and stays byte-identical.
--
-- Owner direction 2026-10-27: the bytes live in the platform artifact_service (it scans every upload with ClamAV
-- through storage_service), referenced by `artifact_id`; `conversation_attachment` holds the METADATA only. The
-- `crate::attachments::store::AttachmentStore` trait is unchanged for its callers.
--
-- 1. Existing rows are removed first: their bytes cannot be moved to an artifact (pre-prod: SW-9 is not live, so
--    there are none that matter), and `artifact_id` is NOT NULL.
-- 2. `bytes` goes (with the size check that read it, re-added without it); `artifact_id` is unique per tenant.
-- 3. EVERY deletion of a row (conversation cascade, staff DELETE after the soft-delete hour, the 1 h sweep) queues
--    the remote bytes for deletion: an AFTER DELETE trigger inserts `conversation_attachment_tombstone` (tenant,
--    artifact id). The tombstone worker (crate::attachments::sweep) deletes at artifact_service (404 = done) and
--    removes the tombstone; a failure keeps it with attempts + last_error_at. Definer-only (the worker is
--    cross-tenant): reached through the `agent_attachment_tombstone_*` functions below.
-- 4. Upload intents close the crash window between the upload and the metadata row (see below).

DELETE FROM conversation_attachment;

ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_size_check;
ALTER TABLE conversation_attachment DROP COLUMN IF EXISTS bytes;
ALTER TABLE conversation_attachment ADD COLUMN IF NOT EXISTS artifact_id uuid NOT NULL;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_size_check
    CHECK (byte_size BETWEEN 1 AND 10485760);
ALTER TABLE conversation_attachment DROP CONSTRAINT IF EXISTS conversation_attachment_artifact;
ALTER TABLE conversation_attachment ADD CONSTRAINT conversation_attachment_artifact UNIQUE (tenant_id, artifact_id);

-- The sweep (SW-9 lifecycle): rows nobody will ever read again. An upload that was never sent (still unlinked after one
-- hour), a soft-deleted row (staff removed an unsent upload; kept one hour for a double click) and, for good measure,
-- nothing else — a LINKED attachment is part of the transcript and goes only with its conversation (ON DELETE CASCADE).
-- Cross-tenant, so SECURITY DEFINER with owner rights (no FORCE ROW LEVEL SECURITY), like agent_channel_counters_sweep
-- (0124). Returns the number of rows deleted.
DROP FUNCTION IF EXISTS agent_conversation_attachment_sweep();
CREATE FUNCTION agent_conversation_attachment_sweep()
    RETURNS integer
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        n integer;
    BEGIN
        DELETE FROM conversation_attachment
         WHERE (message_id IS NULL AND created_at < now() - interval '1 hour')
            OR (deleted_at IS NOT NULL AND deleted_at < now() - interval '1 hour');
        GET DIAGNOSTICS n = ROW_COUNT;
        RETURN n;
    END
    $$;
REVOKE ALL ON FUNCTION agent_conversation_attachment_sweep() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_conversation_attachment_sweep() TO agent_app;

-- The remote-deletion outbox. One row per artifact_service artifact whose attachment row is gone. No foreign key
-- (the outbox outlives the rows it was made from, and a tenant row never blocks it). Definer-only like the other
-- cross-tenant outboxes: RLS on, no grant to agent_app; the tombstone worker reaches it through the functions below.
CREATE TABLE IF NOT EXISTS conversation_attachment_tombstone (
    tenant_id     uuid        NOT NULL,
    artifact_id   uuid        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    attempts      integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error_at timestamptz,
    PRIMARY KEY (tenant_id, artifact_id)
);
-- the worker's read: oldest first
CREATE INDEX IF NOT EXISTS conversation_attachment_tombstone_due
    ON conversation_attachment_tombstone (created_at);

ALTER TABLE conversation_attachment_tombstone ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON conversation_attachment_tombstone;
CREATE POLICY tenant_isolation ON conversation_attachment_tombstone FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON conversation_attachment_tombstone FROM PUBLIC;

-- AFTER DELETE on the attachment row: queue its bytes. SECURITY DEFINER so it works for every deleter (agent_app in a
-- tenant transaction, the sweep function, the owner in a maintenance script) whatever its table grants.
DROP TRIGGER IF EXISTS conversation_attachment_tombstone ON conversation_attachment;
DROP FUNCTION IF EXISTS conversation_attachment_tombstone_trg();
CREATE FUNCTION conversation_attachment_tombstone_trg()
    RETURNS trigger
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    BEGIN
        INSERT INTO conversation_attachment_tombstone (tenant_id, artifact_id)
        VALUES (OLD.tenant_id, OLD.artifact_id)
        ON CONFLICT DO NOTHING;
        RETURN NULL;
    END
    $$;
REVOKE ALL ON FUNCTION conversation_attachment_tombstone_trg() FROM PUBLIC;
CREATE TRIGGER conversation_attachment_tombstone
    AFTER DELETE ON conversation_attachment
    FOR EACH ROW EXECUTE FUNCTION conversation_attachment_tombstone_trg();

-- Queue an artifact the store uploaded and could not keep (the metadata insert failed, or the request failed after
-- the upload) when deleting it right away failed too. Runs outside the failed transaction, so it is explicit about
-- the tenant.
DROP FUNCTION IF EXISTS agent_attachment_tombstone_queue(uuid, uuid);
CREATE FUNCTION agent_attachment_tombstone_queue(p_tenant uuid, p_artifact uuid)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        INSERT INTO conversation_attachment_tombstone (tenant_id, artifact_id)
        VALUES (p_tenant, p_artifact)
        ON CONFLICT DO NOTHING
    $$;
REVOKE ALL ON FUNCTION agent_attachment_tombstone_queue(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_tombstone_queue(uuid, uuid) TO agent_app;

-- The tombstones to work now, oldest first: never tried, or last failed longer ago than the back-off
-- (30 s * 2^attempts, at most one hour).
DROP FUNCTION IF EXISTS agent_attachment_tombstones_due(integer);
CREATE FUNCTION agent_attachment_tombstones_due(p_limit integer)
    RETURNS TABLE (tenant_id uuid, artifact_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT t.tenant_id, t.artifact_id
          FROM conversation_attachment_tombstone t
         WHERE t.last_error_at IS NULL
            OR t.last_error_at < now() - LEAST(interval '1 hour', interval '30 seconds' * power(2::float8, LEAST(t.attempts, 7)::float8))
         ORDER BY t.created_at
         LIMIT GREATEST(p_limit, 0)
    $$;
REVOKE ALL ON FUNCTION agent_attachment_tombstones_due(integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_tombstones_due(integer) TO agent_app;

-- The remote delete succeeded (or the artifact was already gone): drop the tombstone.
DROP FUNCTION IF EXISTS agent_attachment_tombstone_done(uuid, uuid);
CREATE FUNCTION agent_attachment_tombstone_done(p_tenant uuid, p_artifact uuid)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        DELETE FROM conversation_attachment_tombstone WHERE tenant_id = p_tenant AND artifact_id = p_artifact
    $$;
REVOKE ALL ON FUNCTION agent_attachment_tombstone_done(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_tombstone_done(uuid, uuid) TO agent_app;

-- The remote delete failed: keep the tombstone, count the attempt.
DROP FUNCTION IF EXISTS agent_attachment_tombstone_failed(uuid, uuid);
CREATE FUNCTION agent_attachment_tombstone_failed(p_tenant uuid, p_artifact uuid)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        UPDATE conversation_attachment_tombstone
           SET attempts = attempts + 1, last_error_at = now()
         WHERE tenant_id = p_tenant AND artifact_id = p_artifact
    $$;
REVOKE ALL ON FUNCTION agent_attachment_tombstone_failed(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_tombstone_failed(uuid, uuid) TO agent_app;

-- How many tombstones wait (the gauge `agent_attachment_tombstones_pending`).
DROP FUNCTION IF EXISTS agent_attachment_tombstones_pending();
CREATE FUNCTION agent_attachment_tombstones_pending()
    RETURNS bigint
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT count(*) FROM conversation_attachment_tombstone
    $$;
REVOKE ALL ON FUNCTION agent_attachment_tombstones_pending() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_tombstones_pending() TO agent_app;

-- Upload intents (SW-9: no residual orphan path between the upload and the metadata row).
--
-- `PgAttachmentStore::put` uploads the bytes to artifact_service and THEN inserts the `conversation_attachment` row. A
-- process that dies between the two left an artifact nobody references (it carries the tag `att:<attachment id>`).
-- Now `put` first COMMITS an intent (own transaction, outside the request's), uploads, and deletes the intent in the very
-- transaction that inserts the row — so a committed row never has an intent and a surviving old intent means "an upload
-- may have left bytes". The sweep (crate::attachments::sweep::process_intents) resolves intents older than 15 minutes:
-- it CLAIMS the intent, finds the artifacts by that tag at artifact_service, deletes them (none = done) and drops the
-- intent. A claimed intent can no longer be closed by a late `put` (`agent_attachment_intent_close` answers false and
-- the put refuses and abandons its upload), so the sweep can never delete bytes a row is about to reference.
--
-- Definer-only like the tombstone table: RLS on, no grant to agent_app, reached through the functions below.
CREATE TABLE IF NOT EXISTS conversation_attachment_upload_intent (
    tenant_id     uuid        NOT NULL,
    attachment_id uuid        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    claimed_at    timestamptz,
    attempts      integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error_at timestamptz,
    PRIMARY KEY (tenant_id, attachment_id)
);
CREATE INDEX IF NOT EXISTS conversation_attachment_upload_intent_due
    ON conversation_attachment_upload_intent (created_at);

ALTER TABLE conversation_attachment_upload_intent ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON conversation_attachment_upload_intent;
CREATE POLICY tenant_isolation ON conversation_attachment_upload_intent FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON conversation_attachment_upload_intent FROM PUBLIC;

-- Record the intent (its own committed transaction: the caller runs this on the pool, before the upload).
DROP FUNCTION IF EXISTS agent_attachment_intent_open(uuid, uuid);
CREATE FUNCTION agent_attachment_intent_open(p_tenant uuid, p_attachment uuid)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        INSERT INTO conversation_attachment_upload_intent (tenant_id, attachment_id)
        VALUES (p_tenant, p_attachment)
        ON CONFLICT DO NOTHING
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intent_open(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intent_open(uuid, uuid) TO agent_app;

-- The upload was kept: delete the intent in the transaction that inserts the row. false = the intent is gone or the
-- sweep claimed it, so the upload may be deleted any moment and the caller must not keep a row naming it. (A DELETE of a
-- row the sweep is claiming waits for the claim, then sees `claimed_at` set: no race either way.)
DROP FUNCTION IF EXISTS agent_attachment_intent_close(uuid, uuid);
CREATE FUNCTION agent_attachment_intent_close(p_tenant uuid, p_attachment uuid)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        WITH d AS (
            DELETE FROM conversation_attachment_upload_intent
             WHERE tenant_id = p_tenant AND attachment_id = p_attachment AND claimed_at IS NULL
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM d)
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intent_close(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intent_close(uuid, uuid) TO agent_app;

-- The upload definitely stored nothing (refused by the scanner) or its artifact is already deleted or tombstoned:
-- drop the intent whatever its state.
DROP FUNCTION IF EXISTS agent_attachment_intent_discard(uuid, uuid);
CREATE FUNCTION agent_attachment_intent_discard(p_tenant uuid, p_attachment uuid)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        DELETE FROM conversation_attachment_upload_intent
         WHERE tenant_id = p_tenant AND attachment_id = p_attachment
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intent_discard(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intent_discard(uuid, uuid) TO agent_app;

-- The intents to resolve now, oldest first: older than 15 minutes and never tried, or last failed longer ago than the
-- back-off (30 s * 2^attempts, at most one hour).
DROP FUNCTION IF EXISTS agent_attachment_intents_due(integer);
CREATE FUNCTION agent_attachment_intents_due(p_limit integer)
    RETURNS TABLE (tenant_id uuid, attachment_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT i.tenant_id, i.attachment_id
          FROM conversation_attachment_upload_intent i
         WHERE i.created_at < now() - interval '15 minutes'
           AND (i.last_error_at IS NULL
                OR i.last_error_at < now() - LEAST(interval '1 hour', interval '30 seconds' * power(2::float8, LEAST(i.attempts, 7)::float8)))
         ORDER BY i.created_at
         LIMIT GREATEST(p_limit, 0)
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intents_due(integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intents_due(integer) TO agent_app;

-- Claim an intent before touching its artifact. false = it is gone (a put closed it first): nothing to do. Idempotent
-- (a retry keeps the first claim time).
DROP FUNCTION IF EXISTS agent_attachment_intent_claim(uuid, uuid);
CREATE FUNCTION agent_attachment_intent_claim(p_tenant uuid, p_attachment uuid)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        WITH c AS (
            UPDATE conversation_attachment_upload_intent
               SET claimed_at = COALESCE(claimed_at, now())
             WHERE tenant_id = p_tenant AND attachment_id = p_attachment
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM c)
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intent_claim(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intent_claim(uuid, uuid) TO agent_app;

-- The claimed intent's artifacts are deleted (or there were none): drop it.
DROP FUNCTION IF EXISTS agent_attachment_intent_done(uuid, uuid);
CREATE FUNCTION agent_attachment_intent_done(p_tenant uuid, p_attachment uuid)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        DELETE FROM conversation_attachment_upload_intent
         WHERE tenant_id = p_tenant AND attachment_id = p_attachment
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intent_done(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intent_done(uuid, uuid) TO agent_app;

-- Resolving failed: keep the intent, count the attempt.
DROP FUNCTION IF EXISTS agent_attachment_intent_failed(uuid, uuid);
CREATE FUNCTION agent_attachment_intent_failed(p_tenant uuid, p_attachment uuid)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        UPDATE conversation_attachment_upload_intent
           SET attempts = attempts + 1, last_error_at = now()
         WHERE tenant_id = p_tenant AND attachment_id = p_attachment
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intent_failed(uuid, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intent_failed(uuid, uuid) TO agent_app;

-- How many intents wait (the gauge `agent_attachment_intents_pending`).
DROP FUNCTION IF EXISTS agent_attachment_intents_pending();
CREATE FUNCTION agent_attachment_intents_pending()
    RETURNS bigint
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT count(*) FROM conversation_attachment_upload_intent
    $$;
REVOKE ALL ON FUNCTION agent_attachment_intents_pending() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_attachment_intents_pending() TO agent_app;
