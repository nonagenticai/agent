-- agent-api base schema: tenants, users, row-level security and the runtime role.
--
-- The tenancy subset of care-bus-agents@45ee128 (crates/carebus-api/migrations 0006_tenancy, 0007_rls,
-- 0013_list_tenants), renamed for the merged app (D53: role `agent_app`, functions `agent_*`), on a fresh
-- database. NO seed rows: care-bus's bootstrap tenant and job (0006) and its demo data (0002_seed) are not
-- carried; tests create what they need. care-bus's runtime tables (jobs, procedures, connectors, ...) are
-- not part of this base; they arrive with the plans that use them (P1a ledger, amended 2026-10-19).
--
-- Migrations run as the database owner, which bypasses RLS on its own tables (no FORCE). The API serves
-- through `agent_app` (NOSUPERUSER, NOBYPASSRLS), which sees exactly the rows of the tenant in the
-- transaction-local GUC `app.current_tenant`. A session-level GUC is never used: pgbouncer may run this
-- database in session mode, so it would leak to the next client of that server connection.

CREATE TABLE tenants (
    id         uuid PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    name       text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 120),
    status     text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by text NOT NULL
);

-- PK on `sub` alone: a Keycloak subject belongs to exactly one tenant.
CREATE TABLE users (
    sub          text PRIMARY KEY CHECK (btrim(sub) <> ''),
    tenant_id    uuid NOT NULL REFERENCES tenants (id),
    tenant_role  text NOT NULL CHECK (tenant_role IN ('tenant_admin', 'tenant_user')),
    display_name text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text NOT NULL
);
CREATE INDEX users_tenant ON users (tenant_id);

CREATE FUNCTION app_current_tenant() RETURNS uuid
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT nullif(current_setting('app.current_tenant', true), '')::uuid $$;

ALTER TABLE users ALTER COLUMN tenant_id SET DEFAULT app_current_tenant();

-- The runtime role. Production creates it LOGIN with a password before this runs (gitops
-- provision-service-db.sh APP_ROLE=agent_app); a fresh test server gets it NOLOGIN here and the test harness
-- grants LOGIN. A migration owner without CREATEROLE skips creation, and the GRANTs below then fail loudly
-- instead of serving without a role. Test binaries migrate many databases of one server concurrently, so a
-- lost CREATE ROLE race is not an error.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_app') THEN
        CREATE ROLE agent_app NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;
    END IF;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
    WHEN insufficient_privilege THEN
        RAISE NOTICE 'agent_app not created: the migration owner lacks CREATEROLE';
END $$;

GRANT USAGE ON SCHEMA public TO agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON tenants, users TO agent_app;
REVOKE ALL ON _sqlx_migrations FROM agent_app;

ALTER TABLE users ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON users FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());

ALTER TABLE tenants ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_self ON tenants FOR ALL
    USING (id = app_current_tenant()) WITH CHECK (id = app_current_tenant());

-- The only reads before a tenant is known. Owned by the migration owner, which bypasses RLS on its tables.
-- pg_temp is listed last so a caller's temporary table cannot shadow `users` or `tenants`.
CREATE FUNCTION agent_auth_user(p_sub text)
    RETURNS TABLE (tenant_id uuid, tenant_role text, tenant_status text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT u.tenant_id, u.tenant_role, t.status
          FROM users u JOIN tenants t ON t.id = u.tenant_id
         WHERE u.sub = p_sub
    $$;

-- The super_admin tenant picker: id, name and status of every tenant, nothing else.
CREATE FUNCTION agent_list_tenants()
    RETURNS TABLE (id uuid, name text, status text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT t.id, t.name, t.status FROM tenants t ORDER BY t.name, t.id
    $$;

REVOKE ALL ON FUNCTION agent_auth_user(text) FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_list_tenants() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_auth_user(text) TO agent_app;
GRANT EXECUTE ON FUNCTION agent_list_tenants() TO agent_app;
