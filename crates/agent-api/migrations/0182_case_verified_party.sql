-- 0182 — the verified party of a case and its failed verifications (B4 customer verification). Forward-only
-- (pre-prod), drift-tolerant (IF NOT EXISTS; REVOKE-free column GRANTs). References only 0060 and 0065, so it applies
-- in any order after 0181.
--
-- `verified_party`: the customer (`order.read.customer_id`) a `credential.verify` match bound to the case. Set once:
--   a later match for another customer never re-binds it. Every owned operation of a customer case (order.read,
--   payment.*, party.read) is limited to this party's records; NULL on a customer channel = no owned record at all.
-- `verified_email`: the email of the order the match was made on (`order.read.customer_email`), bound with the party in
--   the same statement. The runtime sets a connector case.create's `customer_email` from it when the case party has
--   no email identity, so the ticket is raised for the verified customer, never for a value a model names.
-- `verify_failures`: the case's verifications that did not match (three hand the case over `identity_failed`).
-- The runner (agent_runtime) writes all three.

ALTER TABLE agent_case ADD COLUMN IF NOT EXISTS verified_party text
    CHECK (verified_party IS NULL OR (btrim(verified_party) <> '' AND char_length(verified_party) <= 200));
ALTER TABLE agent_case ADD COLUMN IF NOT EXISTS verified_email text
    CHECK (verified_email IS NULL OR (btrim(verified_email) <> '' AND char_length(verified_email) <= 320));
ALTER TABLE agent_case ADD COLUMN IF NOT EXISTS verify_failures smallint NOT NULL DEFAULT 0
    CHECK (verify_failures >= 0);

GRANT UPDATE (verified_party, verified_email, verify_failures) ON agent_case TO agent_runtime;

-- `party_mark_verified`: a verify match gives the case's ANONYMOUS party its `external_customer_ref`, only when empty
-- (the web chat "Verified" badge's party-side signal). Never a merge (staff `link`, B9 T3), never `kind`. The runtime
-- role has no access to `party` or `channel_identity_settings`, so this one narrow SECURITY DEFINER function (the
-- 0064 convention) does it: the tenant is app_current_tenant() (no tenant argument), `merge_on_verify` must hold (no
-- settings row = true), a merged party is followed to the end of its chain, the ref is non-blank and <= 200 chars.
-- Answers whether the ref was set.
CREATE OR REPLACE FUNCTION party_mark_verified(p_party uuid, p_ref text)
    RETURNS boolean
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    t   uuid := app_current_tenant();
    cur uuid := p_party;
    nxt uuid;
    n   int;
BEGIN
    IF t IS NULL THEN
        RAISE EXCEPTION 'party_mark_verified needs app.current_tenant' USING ERRCODE = '42501';
    END IF;
    IF p_party IS NULL OR p_ref IS NULL OR btrim(p_ref) = '' OR char_length(btrim(p_ref)) > 200 THEN
        RETURN false;
    END IF;
    IF NOT COALESCE((SELECT s.merge_on_verify FROM channel_identity_settings s WHERE s.tenant_id = t), true) THEN
        RETURN false;
    END IF;
    FOR n IN 1..8 LOOP
        SELECT p.merged_into INTO nxt FROM party p WHERE p.tenant_id = t AND p.id = cur;
        EXIT WHEN nxt IS NULL;
        cur := nxt;
    END LOOP;
    UPDATE party SET external_customer_ref = btrim(p_ref), updated_at = now()
     WHERE tenant_id = t AND party.id = cur AND kind = 'anonymous' AND merged_into IS NULL
       AND (external_customer_ref IS NULL OR btrim(external_customer_ref) = '');
    RETURN FOUND;
END
$$;
REVOKE ALL ON FUNCTION party_mark_verified(uuid, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION party_mark_verified(uuid, text) TO agent_runtime;
