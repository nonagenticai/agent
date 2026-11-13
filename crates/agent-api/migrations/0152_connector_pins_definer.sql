-- 0152 — the cross-tenant connector pin reads (B7 T7/T9). Forward-only.

-- P16 coverage / P17 usage read every tenant's platform-scope connector instances and every tenant template's base.
-- `connector_instance` (0093) and `tenant_connector_template` (0092) are RLS-scoped (`tenant_isolation`) and
-- agent_platform has no grant on them, so — like `agent_template_pins_all()` (0043) — these are SECURITY DEFINER
-- functions owned by the migration owner (which bypasses RLS on its own tables; no FORCE ROW LEVEL SECURITY).
-- EXECUTE for agent_platform only. They replace the tenant-by-tenant `set_tenant` walk in
-- crate::connectors::coverage::tenant_rows; the ORDER BY is that walk's order (agent_list_tenants() then per tenant).

-- One row per platform-scope connector instance of any tenant.
CREATE FUNCTION agent_connector_pins()
    RETURNS TABLE (
        tenant_id uuid, tenant_name text, instance_id text, env text, template_key text, template_version text,
        label text, status text
    )
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT t.id, t.name, ci.id, ci.env, ci.template_key, ci.template_version, ci.label, ci.status
          FROM connector_instance ci
          JOIN tenants t ON t.id = ci.tenant_id
         WHERE ci.template_scope = 'platform'
         ORDER BY t.name, t.id, ci.id
    $$;
REVOKE ALL ON FUNCTION agent_connector_pins() FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_connector_pins() FROM agent_app;
GRANT EXECUTE ON FUNCTION agent_connector_pins() TO agent_platform;

-- One row per tenant connector template of any tenant, with the platform template it is built on.
CREATE FUNCTION agent_tenant_connector_bases()
    RETURNS TABLE (tenant_id uuid, tenant_name text, key text, version text, status text, base text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT t.id, t.name, tct.key, tct.version, tct.status, tct.base
          FROM tenant_connector_template tct
          JOIN tenants t ON t.id = tct.tenant_id
         ORDER BY t.name, t.id, tct.key, tct.version
    $$;
REVOKE ALL ON FUNCTION agent_tenant_connector_bases() FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_tenant_connector_bases() FROM agent_app;
GRANT EXECUTE ON FUNCTION agent_tenant_connector_bases() TO agent_platform;
