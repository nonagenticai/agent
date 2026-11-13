-- 0043 — job template tags resolve to layer tags, the projection grant, and the cross-tenant pin read (B6a T4; plan
-- B6 §2 "0043", §4). Forward-only.

-- B1's `job_template_tag` (0033) names a layer tag: (layer, version) → layer_tag (family, version).
--
-- NOT VALID, and deliberately not validated here: migrations run before the boot seeder fills `layer_tag`, and 0033
-- already inserted four seed rows, so VALIDATE would fail on every fresh database and on prod. NOT VALID still checks
-- every new INSERT and every UPDATE of (layer, version); the existing rows are proven by a test after seeding
-- (ledger Ruling 5). A later migration may VALIDATE once the seeder has run everywhere.
ALTER TABLE job_template_tag
    ADD CONSTRAINT job_template_tag_layer_tag_fk FOREIGN KEY (layer, version)
        REFERENCES layer_tag (family, version) NOT VALID;

-- B6a T8 upserts the projection (agent_api::platform::templates) as agent_platform; 0033 granted SELECT only.
GRANT INSERT ON job_template_tag TO agent_platform;
GRANT UPDATE (key, label, industry, pins, status) ON job_template_tag TO agent_platform;

-- Every tenant job's template pins, one row per pin (P6 pins / tenant lag). `jobs` is RLS-scoped (0033
-- `tenant_isolation`) and agent_platform has no grant on it, and B1 has no cross-tenant jobs reader, so this is a
-- SECURITY DEFINER function owned by the migration owner (which bypasses RLS on its own tables), like 0036.
-- EXECUTE for agent_platform only.
CREATE FUNCTION agent_template_pins_all()
    RETURNS TABLE (
        tenant_id uuid, tenant_name text, tenant_status text, job text, job_label text, layer_key text,
        version integer
    )
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT t.id, t.name, t.status, j.key, j.label, p.pin ->> 'layer', (p.pin ->> 'version')::integer
          FROM jobs j
          JOIN tenants t ON t.id = j.tenant_id
         CROSS JOIN LATERAL jsonb_array_elements(j.pins) AS p (pin)
         ORDER BY t.name, t.id, j.key, p.pin ->> 'layer'
    $$;
REVOKE ALL ON FUNCTION agent_template_pins_all() FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_template_pins_all() FROM agent_app;
GRANT EXECUTE ON FUNCTION agent_template_pins_all() TO agent_platform;
