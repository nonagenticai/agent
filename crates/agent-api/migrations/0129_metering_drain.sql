-- 0129 — the metering outbox claim (B4 T11; crate::usage::drainer). Forward-only (pre-prod), drift-tolerant.
--
-- Order (§0 R2): production already ran ≤0128 and 0130 and applies this after them; a fresh database applies it in
-- number order. It references only 0064's `metering_outbox` and the `agent_app` role.
--
-- `metering_outbox` has RLS (tenant_isolation + job_scope), but ONE drainer serves every tenant: it claims due rows
-- across tenants through `agent_claim_metering(limit)` (SECURITY DEFINER; the migration owner bypasses RLS on its
-- own table). The claim is a lease, not a held lock: the rows are picked `FOR UPDATE SKIP LOCKED` and their
-- `next_attempt_at` is pushed 120 s out in the same statement, so the drainer commits before it sends to the billing
-- stream (no transaction is held across Redis). A second drainer, concurrent or later, skips the locked rows and
-- then finds them not due; a drainer that crashes after the claim leaves them due again after the lease (at least
-- once; billing dedups on `event_key`). The drainer settles each row (sent / backed off) under the row's own tenant
-- GUC, so that write stays under RLS. Returns ids, the tenant, the outbox fields and attempts — never a token (the
-- fields carry licence ids only).

CREATE OR REPLACE FUNCTION agent_claim_metering(p_limit integer)
    RETURNS TABLE (id bigint, tenant_id uuid, fields jsonb, attempts integer)
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
#variable_conflict use_column
BEGIN
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 1000 THEN
        RAISE EXCEPTION 'agent_claim_metering: limit must be 1..1000' USING ERRCODE = '22023';
    END IF;
    RETURN QUERY
        WITH due AS (
            SELECT o.id
              FROM metering_outbox o
             WHERE o.status = 'pending' AND o.next_attempt_at <= now()
             ORDER BY o.next_attempt_at, o.id
             LIMIT p_limit
               FOR UPDATE SKIP LOCKED
        )
        UPDATE metering_outbox m
           SET next_attempt_at = now() + interval '120 seconds'
          FROM due
         WHERE m.id = due.id
        RETURNING m.id, m.tenant_id, m.fields, m.attempts;
END
$$;
REVOKE ALL ON FUNCTION agent_claim_metering(integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_claim_metering(integer) TO agent_app;
