-- 0097 — re-pin connector instances to a newer platform template version (demo_retail@3 → @4).
-- Forward-only (pre-prod).
--
-- Migrations run BEFORE the connector seeder (crate::state::build: migrate, then crate::connectors::seed::run), so a
-- plain `UPDATE connector_instance SET template_version = '4'` here would hit the 0093 trigger
-- `connector_instance_template_exists` (demo_retail@4 is not inserted yet) on any database that already has a
-- demo_retail@3 instance. The re-pin therefore runs in the seeder, in the transaction that inserts the new version,
-- through this function: `agent_platform` has no grant on the tenant table `connector_instance` and its RLS policy is
-- per tenant, so the cross-tenant UPDATE is a SECURITY DEFINER function (the owner is not subject to RLS), callable
-- by `agent_platform` only.
--
-- It moves every PLATFORM-scope instance of `p_key` pinned at `p_from` to `p_to`, which must be an active platform
-- version of the same key (a draft or retired target is refused, like a tenant re-pin). Tenant-scope templates are
-- never touched. Returns the number of instances re-pinned. References only 0091 / 0093 objects.

CREATE OR REPLACE FUNCTION agent_connector_repin_platform(p_key text, p_from text, p_to text)
    RETURNS bigint
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
DECLARE
    n bigint;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM connector_template t WHERE t.key = p_key AND t.version = p_to AND t.status = 'active'
    ) THEN
        RAISE EXCEPTION 'connector_template_not_found'
            USING DETAIL = format('no active platform connector template %s@%s', p_key, p_to);
    END IF;
    UPDATE connector_instance
       SET template_version = p_to, updated_at = now()
     WHERE template_scope = 'platform' AND template_key = p_key AND template_version = p_from;
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END $$;
REVOKE ALL ON FUNCTION agent_connector_repin_platform(text, text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_connector_repin_platform(text, text, text) FROM agent_app;
GRANT EXECUTE ON FUNCTION agent_connector_repin_platform(text, text, text) TO agent_platform;
