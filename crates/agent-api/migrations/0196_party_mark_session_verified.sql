-- 0196 — the web chat's door to `party_mark_verified` takes no caller-supplied ref (B4 known-customer hardening).
-- Forward-only (pre-prod), drift-tolerant (CREATE OR REPLACE; REVOKE/GRANT are idempotent). References only 0112
-- (`party`, `conversation.party_id`), 0124/0183 (`web_chat_session`, `conversation.identity_source`) and 0182
-- (`party_mark_verified`), so it applies in any order after 0183.
--
-- 0183 granted `party_mark_verified(uuid, text)` to `agent_app` for the known-customer bind: with it the app role could
-- set ANY ref on ANY empty anonymous party of its tenant. `party_mark_session_verified(conversation)` is the narrow
-- replacement: the tenant is app_current_tenant(); the ref is the `customer_ref` of the conversation's identified web
-- chat session (`customer_ref` and `identified_at` set — the verified assertion; exactly one such session, else
-- nothing); the party is that conversation's `party_id`, and only when the conversation is bound to the same customer
-- by an assertion (`customer_id = ref`, `identity_source = 'assertion'`). Anything else answers false. The marking
-- itself stays in `party_mark_verified` (anonymous, empty ref, `merge_on_verify`, merged chain), called from here as
-- the definer. `agent_app` loses its direct grant; `agent_runtime` keeps its 0182 one (the runner's `credential.verify`).
CREATE OR REPLACE FUNCTION party_mark_session_verified(p_conversation text)
    RETURNS boolean
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    t     uuid := app_current_tenant();
    ref   text;
    party uuid;
    n     int;
BEGIN
    IF t IS NULL THEN
        RAISE EXCEPTION 'party_mark_session_verified needs app.current_tenant' USING ERRCODE = '42501';
    END IF;
    IF p_conversation IS NULL THEN
        RETURN false;
    END IF;
    SELECT count(*), min(s.customer_ref) INTO n, ref
      FROM web_chat_session s
     WHERE s.tenant_id = t AND s.conversation_id = p_conversation
       AND s.customer_ref IS NOT NULL AND s.identified_at IS NOT NULL;
    IF n <> 1 OR ref IS NULL THEN
        RETURN false;
    END IF;
    SELECT count(*), min(c.party_id::text)::uuid INTO n, party
      FROM conversation c
     WHERE c.tenant_id = t AND c.conversation_id = p_conversation
       AND c.customer_id = ref AND c.identity_source = 'assertion' AND c.party_id IS NOT NULL;
    IF n <> 1 OR party IS NULL THEN
        RETURN false;
    END IF;
    RETURN party_mark_verified(party, ref);
END
$$;
REVOKE ALL ON FUNCTION party_mark_session_verified(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION party_mark_session_verified(text) TO agent_app;

REVOKE EXECUTE ON FUNCTION party_mark_verified(uuid, text) FROM agent_app;
