-- 0183 — the known customer of a web chat (B4 known-customer plan §1–§3; owner ruling 2026-10-26: customers are KYC'd
-- and known before the conversation starts, the agent never asks for an identifier). Forward-only (pre-prod),
-- drift-tolerant like 0124/0182: columns IF NOT EXISTS, constraints dropped and re-added, functions dropped and
-- re-created. References only 0061 (`conversation`), 0063/0179 (`agent_handover_items`), 0120 (`connector_bindings`)
-- and 0124 (`web_chat_session`, `agent_channel_counters_sweep`), so it applies in any order after 0179.
--
-- 1. `web_chat_session.customer_ref / customer_email / identified_at`: the verified `sub` (and optional email) of the
--    tenant site's signed customer assertion, set once when the session is created and never changed. The session row
--    is the authority for identity; the edge's wire body is never trusted. NULL = anonymous visitor.
-- 2. `conversation.identity_source / customer_email`: how `conversation.customer_id` became known — `staff_pick` (the
--    staff playground) or `assertion` (a verified web chat customer) — and the KYC'd email. NULL = not identified.
--    The turn drain reads these (identity level 2) and the runtime reads them as `agent_runtime` (0065 grants SELECT
--    on the whole table, so no new runtime grant is needed; `agent_app` has table-level INSERT/UPDATE).
-- 3. `webchat_assertion_jti` (Definer class): the single-use `jti` of an assertion per binding, reached only through
--    `agent_webchat_jti_claim`, swept by the existing 10-minute `agent_channel_counters_sweep`.
-- 4. The handover reason `step_up_required` (an op needing identity level 3 hands over; it never asks for a factor).

-- 1. the session's known customer ---------------------------------------------------------------------------------
ALTER TABLE web_chat_session ADD COLUMN IF NOT EXISTS customer_ref text;
ALTER TABLE web_chat_session ADD COLUMN IF NOT EXISTS customer_email text;
ALTER TABLE web_chat_session ADD COLUMN IF NOT EXISTS identified_at timestamptz;

ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_customer_ref_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_customer_ref_check
    CHECK (customer_ref IS NULL OR (btrim(customer_ref) <> '' AND char_length(customer_ref) <= 200));
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_customer_email_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_customer_email_check
    CHECK (customer_email IS NULL OR (btrim(customer_email) <> '' AND char_length(customer_email) <= 320));
-- a customer ref and its identification time exist together; an email never exists without a customer
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_identified_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_identified_check
    CHECK ((customer_ref IS NULL) = (identified_at IS NULL) AND (customer_email IS NULL OR customer_ref IS NOT NULL));

-- `identity_rotated`: the identity-secret rotation with `revoke_sessions=true` blocks the binding's identified sessions
-- for good (like `rotated` of 0124's widget-key rotation).
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_blocked_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_blocked_check CHECK (
    (blocked_at IS NULL) = (blocked_reason IS NULL)
    AND (blocked_reason IS NULL
         OR blocked_reason IN ('rotated', 'rate_limited', 'repeat_text', 'identity_rotated')));

-- 2. the conversation's identity source ---------------------------------------------------------------------------
ALTER TABLE conversation ADD COLUMN IF NOT EXISTS identity_source text;
ALTER TABLE conversation ADD COLUMN IF NOT EXISTS customer_email text;

ALTER TABLE conversation DROP CONSTRAINT IF EXISTS conversation_identity_source_check;
ALTER TABLE conversation ADD CONSTRAINT conversation_identity_source_check
    CHECK (identity_source IS NULL OR identity_source IN ('staff_pick', 'assertion'));
ALTER TABLE conversation DROP CONSTRAINT IF EXISTS conversation_customer_email_check;
ALTER TABLE conversation ADD CONSTRAINT conversation_customer_email_check
    CHECK (customer_email IS NULL OR (btrim(customer_email) <> '' AND char_length(customer_email) <= 320));

-- 3. single-use assertion ids -------------------------------------------------------------------------------------
-- Definer class like channel_rate_counter (0124): RLS on, no grant to any app role, rows reached only through
-- `agent_webchat_jti_claim`. `expires_at` is the assertion's `exp`; the sweep deletes a row 5 minutes after it.
CREATE TABLE IF NOT EXISTS webchat_assertion_jti (
    binding_id uuid        NOT NULL,
    jti        text        NOT NULL,
    expires_at timestamptz NOT NULL,
    claimed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (binding_id, jti)
);
ALTER TABLE webchat_assertion_jti DROP CONSTRAINT IF EXISTS webchat_assertion_jti_binding;
ALTER TABLE webchat_assertion_jti ADD CONSTRAINT webchat_assertion_jti_binding
    FOREIGN KEY (binding_id) REFERENCES connector_bindings (id) ON DELETE CASCADE;
ALTER TABLE webchat_assertion_jti DROP CONSTRAINT IF EXISTS webchat_assertion_jti_jti_check;
ALTER TABLE webchat_assertion_jti ADD CONSTRAINT webchat_assertion_jti_jti_check
    CHECK (char_length(jti) BETWEEN 16 AND 64);
CREATE INDEX IF NOT EXISTS webchat_assertion_jti_expires_idx ON webchat_assertion_jti (expires_at);
ALTER TABLE webchat_assertion_jti ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON webchat_assertion_jti FROM PUBLIC;
REVOKE ALL ON webchat_assertion_jti FROM agent_app;
REVOKE ALL ON webchat_assertion_jti FROM agent_platform;
REVOKE ALL ON webchat_assertion_jti FROM agent_runtime;

-- Claim `p_jti` of binding `p_binding` (assertion expiry `p_exp`): true when it is new, false when it was claimed
-- before (a replay). The binding must belong to app_current_tenant() — a tenant can neither burn nor probe another
-- tenant's ids (an unknown or foreign binding raises, like an unscoped call). The stored expiry is clamped to
-- [now, now + 1 h] so a hostile `exp` can neither make the row sweepable before the assertion has lapsed nor keep it
-- for ever. `ON CONFLICT DO NOTHING`: two racing claims of one jti, exactly one wins.
DROP FUNCTION IF EXISTS agent_webchat_jti_claim(uuid, text, timestamptz);
CREATE FUNCTION agent_webchat_jti_claim(p_binding uuid, p_jti text, p_exp timestamptz)
    RETURNS boolean
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    t uuid := app_current_tenant();
BEGIN
    IF t IS NULL THEN
        RAISE EXCEPTION 'agent_webchat_jti_claim needs app.current_tenant' USING ERRCODE = '42501';
    END IF;
    IF p_binding IS NULL OR p_jti IS NULL OR p_exp IS NULL OR char_length(p_jti) NOT BETWEEN 16 AND 64 THEN
        RAISE EXCEPTION 'agent_webchat_jti_claim: bad arguments' USING ERRCODE = '22023';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM connector_bindings b WHERE b.id = p_binding AND b.tenant_id = t) THEN
        RAISE EXCEPTION 'agent_webchat_jti_claim: unknown binding' USING ERRCODE = '42501';
    END IF;
    INSERT INTO webchat_assertion_jti (binding_id, jti, expires_at)
    VALUES (p_binding, p_jti, least(greatest(p_exp, now()), now() + interval '1 hour'))
    ON CONFLICT (binding_id, jti) DO NOTHING;
    RETURN FOUND;
END
$$;
REVOKE ALL ON FUNCTION agent_webchat_jti_claim(uuid, text, timestamptz) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_webchat_jti_claim(uuid, text, timestamptz) TO agent_app;

-- The 10-minute tick of 0124, extended: assertion ids 5 minutes past their expiry go too. Still returns the number of
-- counter rows deleted; owner rights (no FORCE ROW LEVEL SECURITY), so the sweep spans every tenant.
DROP FUNCTION IF EXISTS agent_channel_counters_sweep();
CREATE FUNCTION agent_channel_counters_sweep()
    RETURNS integer
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        n integer;
    BEGIN
        DELETE FROM channel_rate_counter WHERE window_start < now() - interval '2 hours';
        GET DIAGNOSTICS n = ROW_COUNT;
        DELETE FROM channel_abuse_event WHERE at < now() - interval '7 days';
        DELETE FROM web_chat_session WHERE expires_at < now() - interval '1 day';
        DELETE FROM webchat_assertion_jti WHERE expires_at < now() - interval '5 minutes';
        RETURN n;
    END
    $$;
REVOKE ALL ON FUNCTION agent_channel_counters_sweep() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_channel_counters_sweep() TO agent_app;

-- 4. the handover reason ------------------------------------------------------------------------------------------
ALTER TABLE agent_handover_items DROP CONSTRAINT IF EXISTS agent_handover_items_reason_check;
ALTER TABLE agent_handover_items ADD CONSTRAINT agent_handover_items_reason_check CHECK (reason IN (
    'identity_failed', 'gate_blocked', 'customer_asked', 'tool_failed', 'unknown_outcome',
    'out_of_scope', 'staff_decision', 'gate_unavailable', 'not_offered', 'success_criteria_unmet',
    'step_up_required'));

-- 6. a known customer's web chat session marks its party verified from the inbound path (agent_app), like the
--    runtime does after a verification (0182 granted only agent_runtime).
GRANT EXECUTE ON FUNCTION party_mark_verified(uuid, text) TO agent_app;
