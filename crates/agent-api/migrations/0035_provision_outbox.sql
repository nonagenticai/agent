-- 0035 — the provisioning outbox (B1 T9; spec 06 §4.5 "K45", P4 T3/T4a). Rows are what agent still has to tell a
-- downstream (licences, strata_rag, strata_ingest) for a tenant; a fenced drainer per replica claims due rows
-- (provision_claim), sends OUTSIDE any transaction, and finishes them with the lease token it claimed with
-- (provision_finish): a finish whose lease was lost changes nothing. Senders register per op (crate::provision); a row
-- whose op has no sender is never claimed and stays visible as pending. Forward-only.

CREATE TYPE provision_status AS ENUM
    ('pending', 'in_flight', 'succeeded', 'failed', 'dead', 'superseded', 'cancelled');

CREATE TABLE provision_outbox (
    id               uuid PRIMARY KEY DEFAULT uuidv7(),  -- sent as Idempotency-Key
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    target           text NOT NULL CHECK (target IN ('licences', 'strata_rag', 'strata_ingest')),
    op               text NOT NULL CHECK (op IN ('licence_ensure', 'licence_revoke', 'connections_put', 'store_ensure')),
    dedupe_key       text NOT NULL CHECK (btrim(dedupe_key) <> ''),
    payload          jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(payload) = 'object'),
    status           provision_status NOT NULL DEFAULT 'pending',
    attempts         integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts     integer NOT NULL DEFAULT 12 CHECK (max_attempts > 0),
    next_attempt_at  timestamptz NOT NULL DEFAULT now(),
    lease_until      timestamptz,
    lease_token      uuid,
    last_http_status integer,
    last_error_code  text,
    last_error       text CHECK (last_error IS NULL OR char_length(last_error) <= 2000),
    result           jsonb,
    created_by       text NOT NULL CHECK (btrim(created_by) <> ''),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    completed_at     timestamptz
);
CREATE INDEX provision_outbox_due ON provision_outbox (next_attempt_at) WHERE status = 'pending';
CREATE INDEX provision_outbox_lease ON provision_outbox (lease_until) WHERE status = 'in_flight';
CREATE INDEX provision_outbox_fifo ON provision_outbox (tenant_id, target, created_at, id);
CREATE UNIQUE INDEX provision_outbox_one_pending ON provision_outbox (tenant_id, dedupe_key) WHERE status = 'pending';
ALTER TABLE provision_outbox ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON provision_outbox FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON provision_outbox TO agent_app;

-- What agent last knew per (tenant, target). Licence ids are TEXT (D77).
CREATE TABLE provision_state (
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    target           text NOT NULL CHECK (target IN ('licences', 'strata_rag', 'strata_ingest')),
    licence_id       text,
    licence_state    text CHECK (licence_state IS NULL OR licence_state IN ('ensured', 'revoked')),
    slot_versions    jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(slot_versions) = 'object'),
    manifest_version integer,
    manifest         jsonb,
    last_success_at  timestamptz,
    drift            jsonb,
    updated_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, target)
);
ALTER TABLE provision_state ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON provision_state FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON provision_state TO agent_app;

-- Claim (06 §4.5): due pending rows, or in_flight rows whose lease expired, of the ops that have a sender; per
-- (tenant, target) FIFO (no older pending/in_flight row of that pair); SKIP LOCKED; in_flight with a new lease and
-- lease token, attempts + 1. Cross-tenant, so definer.
CREATE FUNCTION provision_claim(p_batch int, p_lease_s int, p_ops text[]) RETURNS SETOF provision_outbox
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    WITH due AS (
        SELECT o.id
          FROM provision_outbox o
         WHERE o.op = ANY (p_ops)
           AND ((o.status = 'pending' AND o.next_attempt_at <= now())
                OR (o.status = 'in_flight' AND o.lease_until < now()))
           AND NOT EXISTS (
                SELECT 1 FROM provision_outbox p
                 WHERE p.tenant_id = o.tenant_id AND p.target = o.target
                   AND p.status IN ('pending', 'in_flight')
                   AND (p.created_at, p.id) < (o.created_at, o.id))
         ORDER BY o.next_attempt_at, o.created_at, o.id
         LIMIT p_batch
         FOR UPDATE OF o SKIP LOCKED
    )
    UPDATE provision_outbox u
       SET status = 'in_flight',
           lease_until = now() + make_interval(secs => p_lease_s),
           lease_token = gen_random_uuid(),
           attempts = u.attempts + 1,
           updated_at = now()
      FROM due
     WHERE u.id = due.id
    RETURNING u.*
$$;
REVOKE ALL ON FUNCTION provision_claim(int, int, text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION provision_claim(int, int, text[]) TO agent_app;

-- Finish (06 §4.5), called inside begin_scoped(row.tenant_id): only the holder of the current lease token can
-- finish a row; false = the lease was lost and nothing changed. A retry that is not counted gives the claim's
-- attempt back.
CREATE FUNCTION provision_finish(
    p_id uuid, p_lease_token uuid, p_status text, p_http_status int, p_error_code text, p_error text,
    p_result jsonb, p_next_attempt_at timestamptz, p_count_attempt boolean
) RETURNS boolean
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    n int;
BEGIN
    UPDATE provision_outbox
       SET status = p_status::provision_status,
           next_attempt_at = COALESCE(p_next_attempt_at, next_attempt_at),
           attempts = attempts - CASE WHEN p_count_attempt THEN 0 ELSE 1 END,
           last_http_status = p_http_status,
           last_error_code = p_error_code,
           last_error = left(p_error, 2000),
           result = COALESCE(p_result, result),
           lease_until = NULL,
           lease_token = NULL,
           updated_at = now(),
           completed_at = CASE WHEN p_status IN ('succeeded', 'failed', 'dead', 'cancelled', 'superseded')
                               THEN now() END
     WHERE id = p_id AND lease_token = p_lease_token AND status = 'in_flight'
       AND tenant_id = app_current_tenant();
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n = 1;
END $$;
REVOKE ALL ON FUNCTION provision_finish(uuid, uuid, text, int, text, text, jsonb, timestamptz, boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION provision_finish(uuid, uuid, text, int, text, text, jsonb, timestamptz, boolean) TO agent_app;

-- The metrics tick's one aggregate (no tenant ids): per (target, op) pending and dead counts and the oldest pending
-- age in seconds. Read as agent_platform.
CREATE FUNCTION agent_provision_stats()
    RETURNS TABLE (target text, op text, pending bigint, dead bigint, oldest_pending_s double precision)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT o.target, o.op,
               count(*) FILTER (WHERE o.status = 'pending'),
               count(*) FILTER (WHERE o.status = 'dead'),
               COALESCE(extract(epoch FROM now() - min(o.created_at) FILTER (WHERE o.status = 'pending')), 0)::float8
          FROM provision_outbox o
         GROUP BY o.target, o.op
    $$;
REVOKE ALL ON FUNCTION agent_provision_stats() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_provision_stats() TO agent_platform;
