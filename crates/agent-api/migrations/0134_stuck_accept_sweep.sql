-- 0134 — the stuck-accept sweeper (B7 R6 follow-up; crate::conversations::answer_sweeper). Forward-only (pre-prod),
-- drift-tolerant (IF NOT EXISTS, CREATE OR REPLACE).
--
-- Order: it references only 0061's `conversation_message` and the `agent_app` role, so it runs after 0061 in every
-- order (fresh: number order; production: main's ≤0131 already applied).
--
-- An `accept_only` customer message the agent would answer is armed: `answer_due_at` = accepted + 60 s. The edge's
-- answer call clears it (or takes it for a 300 s lease, `answer_claim` NULL). When it is still armed and due, ONE
-- sweeper loop per replica claims it across tenants through `agent_claim_unanswered(limit, lease_secs)` (SECURITY
-- DEFINER; the migration owner bypasses RLS on its own table): the rows are picked `FOR UPDATE SKIP LOCKED`, leased
-- (`answer_due_at` pushed out), stamped with a fresh `answer_claim` token and counted (`answer_attempts`) in the same
-- statement, so the sweeper commits before it answers. It then re-checks the token under the conversation's inbound
-- lock (under the row's own tenant GUC, so that write stays under RLS): two sweepers, or a sweeper and the edge,
-- never answer one message twice. Returns ids and the claim only — never message text.

ALTER TABLE conversation_message
    ADD COLUMN IF NOT EXISTS answer_due_at timestamptz,
    ADD COLUMN IF NOT EXISTS answer_claim uuid,
    ADD COLUMN IF NOT EXISTS answer_attempts integer NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS conversation_message_answer_due
    ON conversation_message (answer_due_at) WHERE answer_due_at IS NOT NULL;

CREATE OR REPLACE FUNCTION agent_claim_unanswered(p_limit integer, p_lease_secs integer)
    RETURNS TABLE (tenant_id uuid, job_id uuid, conversation_id text, message_id uuid, external_id text,
                   claim uuid, attempts integer)
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
#variable_conflict use_column
BEGIN
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 1000 THEN
        RAISE EXCEPTION 'agent_claim_unanswered: limit must be 1..1000' USING ERRCODE = '22023';
    END IF;
    IF p_lease_secs IS NULL OR p_lease_secs < 1 OR p_lease_secs > 3600 THEN
        RAISE EXCEPTION 'agent_claim_unanswered: lease must be 1..3600 s' USING ERRCODE = '22023';
    END IF;
    RETURN QUERY
        WITH due AS (
            SELECT m.message_id AS id
              FROM conversation_message m
             WHERE m.answer_due_at IS NOT NULL AND m.answer_due_at <= now()
             ORDER BY m.answer_due_at, m.message_id
             LIMIT p_limit
               FOR UPDATE SKIP LOCKED
        )
        UPDATE conversation_message c
           SET answer_due_at = now() + make_interval(secs => p_lease_secs),
               answer_claim = gen_random_uuid(),
               answer_attempts = c.answer_attempts + 1
          FROM due
         WHERE c.message_id = due.id
        RETURNING c.tenant_id, c.job_id, c.conversation_id, c.message_id, c.external_id, c.answer_claim,
                  c.answer_attempts;
END
$$;
REVOKE ALL ON FUNCTION agent_claim_unanswered(integer, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_claim_unanswered(integer, integer) TO agent_app;
