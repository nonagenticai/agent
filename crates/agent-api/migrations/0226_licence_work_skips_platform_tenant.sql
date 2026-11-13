-- 0226 — the platform-owned tenant is not a licences customer. Forward-only.
--
-- The licence reconciler's `tenant` work (0036/0038, D77 heal) registers every active tenant without a licence_id
-- with licences. The platform tenant (AGENT_PLATFORM_TENANT_ID, `platform_owned`, ensured at boot by
-- crate::platform::bootstrap) is served by the platform's own service licence: the runtime gate already admits it
-- without a verdict (crate::licensing::gate::decide) and its model is the platform key's lane, so registering it as a
-- licences tenant would only fail or bill the platform to itself. Both the work list and the prune skip it; with no
-- licence_id it is never in the app-licence work (0216) either.

CREATE OR REPLACE FUNCTION agent_licence_work(p_limit bigint)
    RETURNS TABLE (kind text, tenant_id uuid, item text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        (SELECT 'mint'::text, e.tenant_id, e.user_sub
           FROM member_entitlement e JOIN tenants t ON t.id = e.tenant_id
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'mint' AND b.tenant_id = e.tenant_id AND b.item = e.user_sub
          WHERE e.status = 'pending' AND t.status = 'active'
            AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
          ORDER BY b.next_attempt_at NULLS FIRST, e.updated_at LIMIT p_limit)
        UNION ALL
        (SELECT 'revoke'::text, e.tenant_id, e.user_sub
           FROM member_entitlement e
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'revoke' AND b.tenant_id = e.tenant_id AND b.item = e.user_sub
          WHERE e.status = 'revoking' AND e.license_id IS NOT NULL
            AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
          ORDER BY b.next_attempt_at NULLS FIRST, e.updated_at LIMIT p_limit)
        UNION ALL
        (SELECT 'parked'::text, t.id, p.id
           FROM tenants t CROSS JOIN LATERAL unnest(t.licence_revoke_pending) AS p (id)
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'parked' AND b.tenant_id = t.id AND b.item = p.id
          WHERE b.next_attempt_at IS NULL OR b.next_attempt_at <= now()
          ORDER BY b.next_attempt_at NULLS FIRST, t.id, p.id LIMIT p_limit)
        UNION ALL
        (SELECT 'tenant'::text, t.id, NULL::text
           FROM tenants t
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'tenant' AND b.tenant_id = t.id AND b.item = ''
          WHERE t.status = 'active' AND t.licence_id IS NULL AND NOT t.platform_owned
            AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
          ORDER BY b.next_attempt_at NULLS FIRST, t.created_at LIMIT p_limit)
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
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND t.status = 'active'
                        AND (SELECT count(*) FROM tenant_app_licence a WHERE a.tenant_id = t.id AND a.provisioned_at IS NOT NULL) < 3)
                 ELSE false
             END
            RETURNING 1
        )
        SELECT count(*) FROM gone
    $$;
