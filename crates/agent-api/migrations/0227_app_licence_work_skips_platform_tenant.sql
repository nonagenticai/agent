-- 0227 — the platform-owned tenant holds no tenant app licences either (0216, 0226). Forward-only.
--
-- The app-licence backfill (agent_app_licence_work, 0216) picks every active tenant with a licence_id and fewer than
-- three provisioned apps. The platform tenant reaches its inference-router and strata on the platform's own service
-- licence (crate::providers::llm::internal_resolved), so it is never a backfill candidate, whatever its licence_id;
-- the prune drops its stale `app_licence` backoff row.

CREATE OR REPLACE FUNCTION agent_app_licence_work(p_limit bigint) RETURNS TABLE (tenant_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT t.id
          FROM tenants t
          LEFT JOIN licence_work_backoff b
                 ON b.kind = 'app_licence' AND b.tenant_id = t.id AND b.item = ''
         WHERE t.status = 'active' AND t.licence_id IS NOT NULL AND NOT t.platform_owned
           AND (SELECT count(*) FROM tenant_app_licence a WHERE a.tenant_id = t.id AND a.provisioned_at IS NOT NULL) < 3
           AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
         ORDER BY b.next_attempt_at NULLS FIRST, t.created_at
         LIMIT p_limit
    $$;

CREATE OR REPLACE FUNCTION agent_licence_backoff_prune() RETURNS bigint
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        WITH gone AS (
            DELETE FROM licence_work_backoff b
             WHERE NOT CASE b.kind
                 WHEN 'mint' THEN EXISTS (
                     SELECT 1 FROM member_entitlement e JOIN tenants t ON t.id = e.tenant_id
                      WHERE e.tenant_id = b.tenant_id AND e.user_sub = b.item
                        AND e.status = 'pending' AND t.status = 'active')
                 WHEN 'revoke' THEN EXISTS (
                     SELECT 1 FROM member_entitlement e
                      WHERE e.tenant_id = b.tenant_id AND e.user_sub = b.item
                        AND e.status = 'revoking' AND e.license_id IS NOT NULL)
                 WHEN 'parked' THEN EXISTS (
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND b.item = ANY (t.licence_revoke_pending))
                 WHEN 'tenant' THEN EXISTS (
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND t.status = 'active' AND t.licence_id IS NULL
                        AND NOT t.platform_owned)
                 WHEN 'app_licence' THEN EXISTS (
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND t.status = 'active' AND NOT t.platform_owned
                        AND (SELECT count(*) FROM tenant_app_licence a WHERE a.tenant_id = t.id AND a.provisioned_at IS NOT NULL) < 3)
                 ELSE false
             END
            RETURNING 1
        )
        SELECT count(*) FROM gone
    $$;
