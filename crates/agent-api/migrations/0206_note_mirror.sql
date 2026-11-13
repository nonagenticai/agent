-- 0206 — SW-11: a staff note's private comment on the helpdesk ticket, as a durable outbox
-- (crate::conversations::note_mirror). Forward-only (pre-prod), drift-tolerant (IF NOT EXISTS, CREATE OR REPLACE).
--
-- Order: it references only 0061's `conversation_message` (0201 added the `note` role) and the `agent_app` role, so it
-- runs after 0201 in every order.
--
-- A note written while its conversation's handover item is open, on a job with a helpdesk worker binding, is queued in
-- the note's own transaction: `mirror_state = 'pending'`, `mirror_item` = that item, `mirror_due_at` = now. The note's
-- own request tries it once after commit; ONE sweeper loop per replica drains the rest across tenants through
-- `agent_claim_note_mirror(limit, lease_secs)` (SECURITY DEFINER, as 0134's `agent_claim_unanswered`): due rows are
-- picked `FOR UPDATE SKIP LOCKED`, leased (`mirror_due_at` pushed out), stamped with a fresh `mirror_claim` token and
-- counted (`mirror_attempts`) in one statement, committed before the ticket is called. Every outcome is written back
-- only while the token is still the row's: two sweepers never post one note twice. `mirror_unknown` marks an attempt
-- whose outcome is unknown (a timeout after the request was sent): the next attempt looks the comment up first.
-- States: pending → sent | skipped (no ticket / no binding: nothing to mirror to) | failed (refused, or given up).
-- Returns ids and the claim only — never note text.

ALTER TABLE conversation_message
    ADD COLUMN IF NOT EXISTS mirror_state text,
    ADD COLUMN IF NOT EXISTS mirror_item text,
    ADD COLUMN IF NOT EXISTS mirror_due_at timestamptz,
    ADD COLUMN IF NOT EXISTS mirror_claim uuid,
    ADD COLUMN IF NOT EXISTS mirror_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS mirror_unknown boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS mirror_error text,
    ADD COLUMN IF NOT EXISTS mirror_ticket text,
    ADD COLUMN IF NOT EXISTS mirrored_at timestamptz;

ALTER TABLE conversation_message DROP CONSTRAINT IF EXISTS conversation_message_mirror_check;
ALTER TABLE conversation_message ADD CONSTRAINT conversation_message_mirror_check CHECK (
    mirror_state IS NULL
    OR (role = 'note' AND mirror_item IS NOT NULL
        AND mirror_state IN ('pending', 'sent', 'skipped', 'failed')
        AND (mirror_state = 'pending') = (mirror_due_at IS NOT NULL)
        AND (mirror_state = 'sent') = (mirrored_at IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS conversation_message_mirror_due
    ON conversation_message (mirror_due_at) WHERE mirror_state = 'pending';

CREATE OR REPLACE FUNCTION agent_claim_note_mirror(p_limit integer, p_lease_secs integer)
    RETURNS TABLE (tenant_id uuid, job_id uuid, conversation_id text, message_id uuid, claim uuid, attempts integer)
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
#variable_conflict use_column
BEGIN
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 1000 THEN
        RAISE EXCEPTION 'agent_claim_note_mirror: limit must be 1..1000' USING ERRCODE = '22023';
    END IF;
    IF p_lease_secs IS NULL OR p_lease_secs < 1 OR p_lease_secs > 3600 THEN
        RAISE EXCEPTION 'agent_claim_note_mirror: lease must be 1..3600 s' USING ERRCODE = '22023';
    END IF;
    RETURN QUERY
        WITH due AS (
            SELECT m.message_id AS id
              FROM conversation_message m
             WHERE m.mirror_state = 'pending' AND m.mirror_due_at <= now()
             ORDER BY m.mirror_due_at, m.message_id
             LIMIT p_limit
               FOR UPDATE SKIP LOCKED
        )
        UPDATE conversation_message c
           SET mirror_due_at = now() + make_interval(secs => p_lease_secs),
               mirror_claim = gen_random_uuid(),
               mirror_attempts = c.mirror_attempts + 1
          FROM due
         WHERE c.message_id = due.id
        RETURNING c.tenant_id, c.job_id, c.conversation_id, c.message_id, c.mirror_claim, c.mirror_attempts;
END
$$;
REVOKE ALL ON FUNCTION agent_claim_note_mirror(integer, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_claim_note_mirror(integer, integer) TO agent_app;
