-- 0039 — fine-grained tenant roles (D88) and narrow-only view-as (D79), B1 T21 (plan §R.2). Forward-only (pre-prod):
-- `users.tenant_role` (tenant_admin | tenant_user, D55) becomes the role SET `users.roles`; the functions that read or
-- wrote `tenant_role` are recreated BEFORE the column is dropped (ALTER does not revalidate function bodies).
--
-- Scope classes (tests/common/classes.rs): member_view_as = T (RLS on tenant_id), super_admin_view_as = D
-- (SECURITY DEFINER-only, like super_admin_active).

-- The vocabulary (plan §R.1, in its order). Adding a role is a migration.
CREATE FUNCTION agent_role_keys() RETURNS text[]
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT ARRAY['tenant_admin', 'job_designer', 'release_manager', 'knowledge_manager',
                       'integrations_manager', 'support_manager', 'support_agent', 'operator']::text[] $$;

-- A CHECK may not hold a subquery, so "no role twice" is an IMMUTABLE function.
CREATE FUNCTION agent_roles_distinct(p text[]) RETURNS boolean
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT cardinality(p) = (SELECT count(DISTINCT r) FROM unnest(p) AS r) $$;

ALTER TABLE users ADD COLUMN roles text[] NOT NULL DEFAULT '{}';
-- D55 admins kept everything; tenant_user arrives as an observer.
UPDATE users SET roles = CASE tenant_role WHEN 'tenant_admin' THEN agent_role_keys() ELSE '{}'::text[] END;
ALTER TABLE users ADD CONSTRAINT users_roles_vocab CHECK (roles <@ agent_role_keys());
ALTER TABLE users ADD CONSTRAINT users_roles_distinct CHECK (agent_roles_distinct(roles));

-- A member's narrow-only view (T): at most 8 hours; the API checks it narrows the held set.
CREATE TABLE member_view_as (
    sub        text PRIMARY KEY REFERENCES users (sub) ON DELETE CASCADE,
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    roles      text[] NOT NULL CHECK (roles <@ agent_role_keys() AND agent_roles_distinct(roles)),
    started_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
               CHECK (expires_at > started_at AND expires_at <= started_at + interval '8 hours')
);
ALTER TABLE member_view_as ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON member_view_as FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON member_view_as TO agent_app;

-- The home tenant of a non-disabled member, its role set and a live view-as (an expired row is ignored, never an
-- error). The return type changes, so DROP + CREATE (and re-GRANT).
DROP FUNCTION agent_auth_user(text);
CREATE FUNCTION agent_auth_user(p_sub text)
    RETURNS TABLE (tenant_id uuid, roles text[], tenant_status text, view_as text[], view_as_expires_at timestamptz)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT u.tenant_id, u.roles, t.status, v.roles, v.expires_at
          FROM users u
          JOIN tenants t ON t.id = u.tenant_id
          LEFT JOIN member_view_as v ON v.sub = u.sub AND v.tenant_id = u.tenant_id AND v.expires_at > now()
         WHERE u.sub = p_sub AND u.disabled_at IS NULL
    $$;
REVOKE ALL ON FUNCTION agent_auth_user(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_auth_user(text) TO agent_app;

-- 0036's rollup: an admin is a member holding tenant_admin.
CREATE OR REPLACE FUNCTION agent_platform_tenant_rollup()
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
                 WHERE u.tenant_id = t.id AND u.disabled_at IS NULL AND 'tenant_admin' = ANY (u.roles)),
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

-- 0036's re-home: the member arrives in the target tenant as an observer (no roles).
CREATE OR REPLACE FUNCTION agent_user_rehome(p_sub text, p_to uuid) RETURNS uuid
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
    DELETE FROM member_view_as WHERE sub = p_sub;
    UPDATE users
       SET tenant_id = p_to, roles = '{}', disabled_at = NULL, invited_at = NULL,
           updated_at = now(), updated_by = 'rehome'
     WHERE sub = p_sub;
    RETURN prev;
END $$;

ALTER TABLE users DROP COLUMN tenant_role;

-- A super_admin's view per tenant they act in (D): reached only through the functions below. Keyed (sub, tenant) so
-- a view set in one tenant never applies in another.
CREATE TABLE super_admin_view_as (
    sub        text NOT NULL CHECK (btrim(sub) <> ''),
    tenant_id  uuid NOT NULL REFERENCES tenants (id),
    roles      text[] NOT NULL CHECK (roles <@ agent_role_keys() AND agent_roles_distinct(roles)),
    started_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
               CHECK (expires_at > started_at AND expires_at <= started_at + interval '8 hours'),
    PRIMARY KEY (sub, tenant_id)
);
ALTER TABLE super_admin_view_as ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON super_admin_view_as FROM PUBLIC;

-- The live view of `p_sub` in `p_tenant` (no row when none or expired).
CREATE FUNCTION agent_sa_view_as(p_sub text, p_tenant uuid)
    RETURNS TABLE (roles text[], expires_at timestamptz)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT v.roles, v.expires_at FROM super_admin_view_as v
         WHERE v.sub = p_sub AND v.tenant_id = p_tenant AND v.expires_at > now()
    $$;
REVOKE ALL ON FUNCTION agent_sa_view_as(text, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_sa_view_as(text, uuid) TO agent_app;

-- Start (or replace) the view; returns its expiry.
CREATE FUNCTION agent_set_sa_view_as(p_sub text, p_tenant uuid, p_roles text[], p_ttl int) RETURNS timestamptz
    LANGUAGE sql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        INSERT INTO super_admin_view_as AS v (sub, tenant_id, roles, started_at, expires_at)
             VALUES (p_sub, p_tenant, p_roles, now(), now() + make_interval(secs => p_ttl))
        ON CONFLICT (sub, tenant_id) DO UPDATE
            SET roles = EXCLUDED.roles, started_at = EXCLUDED.started_at, expires_at = EXCLUDED.expires_at
        RETURNING v.expires_at
    $$;
REVOKE ALL ON FUNCTION agent_set_sa_view_as(text, uuid, text[], int) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_set_sa_view_as(text, uuid, text[], int) TO agent_app;

-- End the view; true when a LIVE view was ended (an expired row is removed silently).
CREATE FUNCTION agent_clear_sa_view_as(p_sub text, p_tenant uuid) RETURNS boolean
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    live boolean;
BEGIN
    DELETE FROM super_admin_view_as v WHERE v.sub = p_sub AND v.tenant_id = p_tenant
    RETURNING v.expires_at > now() INTO live;
    RETURN coalesce(live, false);
END $$;
REVOKE ALL ON FUNCTION agent_clear_sa_view_as(text, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_clear_sa_view_as(text, uuid) TO agent_app;

-- The hourly sweep also drops view-as rows that expired more than a day ago (both kinds).
CREATE OR REPLACE FUNCTION agent_member_session_sweep(p_days int) RETURNS bigint
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    n bigint;
BEGIN
    DELETE FROM member_session WHERE last_seen_at < now() - make_interval(days => p_days);
    GET DIAGNOSTICS n = ROW_COUNT;
    DELETE FROM member_view_as WHERE expires_at < now() - interval '1 day';
    DELETE FROM super_admin_view_as WHERE expires_at < now() - interval '1 day';
    RETURN n;
END $$;
