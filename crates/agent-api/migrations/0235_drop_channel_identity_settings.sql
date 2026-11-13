-- 0235 — A7 (caller-identity plan): the tenant `channel_identity_settings` policy is gone (route, page, table). The
-- only reader besides its own routes was `party_mark_verified` (0182: "merge_on_verify must hold, no row = true"), so
-- the function is redefined without the read — always true, exactly the behaviour of a tenant with no row — before the
-- table is dropped. Forward-only (pre-prod): no compatibility shim for tenants that stored `merge_on_verify = false`.
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

DROP TABLE IF EXISTS channel_identity_settings;
