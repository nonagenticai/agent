-- 0031 — the `agent_platform` role split (B1 T3; spec 01 §5.2, P3 T3/T17).
--
-- Platform tables (no tenant column, no RLS) are written only as `agent_platform`. The runtime role
-- `agent_app` reaches it with `SET LOCAL ROLE agent_platform` inside a transaction (crate::platform_db) and
-- inherits NOTHING from it, so a plain `agent_app` statement can never touch a platform table, and a tenant
-- GUC set earlier in the transaction survives the switch.
--
-- This migration VERIFIES the role and the membership; it never creates them. Production creates them in the
-- DBA step (worker4 primary, `provision-service-db.sh PLATFORM_ROLE=agent_platform`); the test harness
-- creates them once per server. Missing or wrong → the migration raises and the API does not boot.

DO $$
DECLARE
    r pg_roles%ROWTYPE;
    n_ok int;
    n_bad int;
BEGIN
    SELECT * INTO r FROM pg_roles WHERE rolname = 'agent_platform';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'role agent_platform does not exist: create it (NOLOGIN NOSUPERUSER NOBYPASSRLS) and GRANT agent_platform TO agent_app WITH INHERIT FALSE, SET TRUE before migrating';
    END IF;
    IF r.rolcanlogin OR r.rolsuper OR r.rolbypassrls THEN
        RAISE EXCEPTION 'role agent_platform must be NOLOGIN, NOSUPERUSER and NOBYPASSRLS';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_class WHERE relowner = r.oid)
       OR EXISTS (SELECT 1 FROM pg_proc WHERE proowner = r.oid)
       OR EXISTS (SELECT 1 FROM pg_namespace WHERE nspowner = r.oid)
       OR EXISTS (SELECT 1 FROM pg_type WHERE typowner = r.oid) THEN
        RAISE EXCEPTION 'role agent_platform must own nothing in this database';
    END IF;
    SELECT count(*) FILTER (WHERE NOT m.inherit_option AND m.set_option),
           count(*) FILTER (WHERE m.inherit_option OR NOT m.set_option)
      INTO n_ok, n_bad
      FROM pg_auth_members m
     WHERE m.roleid = r.oid
       AND m.member = (SELECT oid FROM pg_roles WHERE rolname = 'agent_app');
    IF n_ok = 0 OR n_bad > 0 THEN
        RAISE EXCEPTION 'agent_app must be a member of agent_platform WITH INHERIT FALSE, SET TRUE (found % ok, % wrong grants)', n_ok, n_bad;
    END IF;
END $$;

GRANT USAGE ON SCHEMA public TO agent_platform;

-- The signup queue and its provisioning journal are platform data: agent_platform only.
REVOKE ALL ON signup_approvals, signup_provisioning_steps FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON signup_approvals, signup_provisioning_steps TO agent_platform;

-- The cross-tenant picker is a platform read.
REVOKE EXECUTE ON FUNCTION agent_list_tenants() FROM agent_app;
GRANT EXECUTE ON FUNCTION agent_list_tenants() TO agent_platform;
