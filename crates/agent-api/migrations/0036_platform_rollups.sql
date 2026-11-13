-- 0036 — the platform reads across tenants (B1 T10; P4 T7 badge, P3 T14 rehome). Every function is SECURITY DEFINER
-- (owned by the migration owner, which bypasses RLS on its own tables), `search_path = public, pg_temp`, EXECUTE for
-- `agent_platform` only: a super_admin route reaches them inside `begin_platform`, never as plain `agent_app`.
-- Forward-only.
--
-- The rollup reads `tenants.licence_id`, which 0037 adds (0037 is numbered last of B1's range, D77). A SQL function
-- body is checked against the catalog when it is created, so the check is deferred to the first call for this
-- migration's transaction only (what pg_dump does); every column exists once the migration set has applied.
SET LOCAL check_function_bodies = off;

-- The registry: one row per tenant with its member/job counts and its provisioning state. `prov_*` count the outbox
-- rows by state (the worst one is the registry badge); `provision` is the latest row per target
-- (`[{target, status, last_error_code}]`).
CREATE FUNCTION agent_platform_tenant_rollup()
    RETURNS TABLE (
        id uuid, name text, status text, residency text, brand text, description text, industry text,
        created_at timestamptz, suspended_at timestamptz, archived_at timestamptz, licence_id text,
        jobs bigint, users_active bigint, admins bigint, disabled bigint, invited bigint,
        prov_open bigint, prov_failed bigint, prov_dead bigint, provision jsonb
    )
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT t.id, t.name, t.status, t.residency, t.brand, t.description, t.industry,
               t.created_at, t.suspended_at, t.archived_at, t.licence_id,
               (SELECT count(*) FROM jobs j WHERE j.tenant_id = t.id),
               (SELECT count(*) FROM users u
                 WHERE u.tenant_id = t.id AND u.disabled_at IS NULL AND u.invited_at IS NULL),
               (SELECT count(*) FROM users u
                 WHERE u.tenant_id = t.id AND u.disabled_at IS NULL AND u.tenant_role = 'tenant_admin'),
               (SELECT count(*) FROM users u WHERE u.tenant_id = t.id AND u.disabled_at IS NOT NULL),
               (SELECT count(*) FROM users u
                 WHERE u.tenant_id = t.id AND u.disabled_at IS NULL AND u.invited_at IS NOT NULL),
               (SELECT count(*) FROM provision_outbox o
                 WHERE o.tenant_id = t.id AND o.status IN ('pending', 'in_flight')),
               (SELECT count(*) FROM provision_outbox o WHERE o.tenant_id = t.id AND o.status = 'failed'),
               (SELECT count(*) FROM provision_outbox o WHERE o.tenant_id = t.id AND o.status = 'dead'),
               COALESCE((
                   SELECT jsonb_agg(jsonb_build_object('target', l.target, 'status', l.status::text,
                                                       'last_error_code', l.last_error_code) ORDER BY l.target)
                     FROM (SELECT DISTINCT ON (o.target) o.target, o.status, o.last_error_code
                             FROM provision_outbox o
                            WHERE o.tenant_id = t.id
                            ORDER BY o.target, o.created_at DESC, o.id DESC) l
               ), '[]'::jsonb)
          FROM tenants t
         ORDER BY t.name, t.id
    $$;
REVOKE ALL ON FUNCTION agent_platform_tenant_rollup() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_platform_tenant_rollup() TO agent_platform;

-- The tenant of an outbox row (platform retry/cancel by row id); NULL when there is no such row.
CREATE FUNCTION agent_provision_row_tenant(p_id uuid) RETURNS uuid
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$ SELECT o.tenant_id FROM provision_outbox o WHERE o.id = p_id $$;
REVOKE ALL ON FUNCTION agent_provision_row_tenant(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_provision_row_tenant(uuid) TO agent_platform;

-- Move a DISABLED member to another ACTIVE tenant, enabled as tenant_user; returns the tenant it left. The message
-- of each refusal is the API's code (crate::error::KNOWN_DB_CODES): user_not_found, same_tenant, user_active,
-- tenant_not_found, tenant_not_active.
CREATE FUNCTION agent_user_rehome(p_sub text, p_to uuid) RETURNS uuid
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    prev uuid;
    dis  timestamptz;
    st   text;
BEGIN
    SELECT u.tenant_id, u.disabled_at INTO prev, dis FROM users u WHERE u.sub = p_sub FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'user_not_found';
    END IF;
    IF prev = p_to THEN
        RAISE EXCEPTION 'same_tenant';
    END IF;
    IF dis IS NULL THEN
        RAISE EXCEPTION 'user_active';
    END IF;
    SELECT t.status INTO st FROM tenants t WHERE t.id = p_to FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'tenant_not_found';
    END IF;
    IF st <> 'active' THEN
        RAISE EXCEPTION 'tenant_not_active';
    END IF;
    UPDATE users
       SET tenant_id = p_to, tenant_role = 'tenant_user', disabled_at = NULL, invited_at = NULL,
           updated_at = now(), updated_by = 'rehome'
     WHERE sub = p_sub;
    RETURN prev;
END $$;
REVOKE ALL ON FUNCTION agent_user_rehome(text, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_user_rehome(text, uuid) TO agent_platform;

-- The licence reconciler's work list (D77 L7, crate::signup::provisioning::reconcile), at most p_limit per kind:
-- `mint` (a pending member entitlement of an active tenant; item = the member's sub), `revoke` (a revoking one; item =
-- sub), `parked` (an id in tenants.licence_revoke_pending; item = the licence id), `tenant` (an active tenant without a
-- tenant licence; item NULL). Read as agent_platform; the work is done under each tenant's own scope.
CREATE FUNCTION agent_licence_work(p_limit bigint)
    RETURNS TABLE (kind text, tenant_id uuid, item text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        (SELECT 'mint'::text, e.tenant_id, e.user_sub
           FROM member_entitlement e JOIN tenants t ON t.id = e.tenant_id
          WHERE e.status = 'pending' AND t.status = 'active'
          ORDER BY e.updated_at LIMIT p_limit)
        UNION ALL
        (SELECT 'revoke'::text, e.tenant_id, e.user_sub
           FROM member_entitlement e
          WHERE e.status = 'revoking' AND e.license_id IS NOT NULL
          ORDER BY e.updated_at LIMIT p_limit)
        UNION ALL
        (SELECT 'parked'::text, t.id, p.id
           FROM tenants t CROSS JOIN LATERAL unnest(t.licence_revoke_pending) AS p (id)
          LIMIT p_limit)
        UNION ALL
        (SELECT 'tenant'::text, t.id, NULL::text
           FROM tenants t
          WHERE t.status = 'active' AND t.licence_id IS NULL
          ORDER BY t.created_at LIMIT p_limit)
    $$;
REVOKE ALL ON FUNCTION agent_licence_work(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_licence_work(bigint) TO agent_platform;
