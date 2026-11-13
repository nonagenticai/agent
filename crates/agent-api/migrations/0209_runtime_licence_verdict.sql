-- 0209 (B5 runner step 2): the runner's licence gate reads the tenant's stored licence verdict (spec 12 §3.3, K35).
-- `agent_runtime` holds no grant on licence tables (0065; `runtime_role_cannot_read_signup_or_licence_tables`), so the
-- verdict reaches it through one SECURITY DEFINER function bound to `app_current_tenant()`: it returns the status, the
-- reason, the D70 mark and the age in seconds of the CALLER'S tenant's row only (no licence id, plan, kid or token),
-- and no row when the tenant has none. Forward-only. Needs 0090, 0065.

CREATE OR REPLACE FUNCTION agent_runtime_licence_verdict()
    RETURNS TABLE (status text, reason text, platform_owned boolean, age_seconds bigint)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT v.status, v.reason, v.platform_owned, EXTRACT(EPOCH FROM now() - v.checked_at)::bigint
          FROM tenant_licence_verdict v
         WHERE v.tenant_id = app_current_tenant()
    $$;

REVOKE ALL ON FUNCTION agent_runtime_licence_verdict() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_runtime_licence_verdict() TO agent_runtime;
