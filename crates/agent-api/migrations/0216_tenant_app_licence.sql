-- 0216 — per-tenant platform-app licences, MINT-ON-DEMAND (mirrors nonagenticai_mcp_server's bundle-token minter).
-- Forward-only (pre-prod).
--
-- Every platform upstream a tenant's traffic reaches (inference-router, strata-rag, strata-ingest) is called with a
-- short-lived BUNDLE TOKEN minted for that TENANT on demand (licences `POST /internal/tenants/{id}/bundle-token`,
-- bundle `agent`), so the call is attributable and budgetable per tenant. NO token is stored anywhere: there is no
-- secret column. This table is only the provisioning STATUS of one (tenant, app): the licence and token ids (non-secret
-- identifiers licences returned), when the provisioning smoke last passed as the tenant, and the last failure.
-- `provisioned_at` is set once the app has been minted and smoke-tested as the tenant; a row without it is a failed
-- attempt (`last_error` says why). The failure is also visible in the signup journal (`signup_provisioning_steps`,
-- step `app_licences`) and, for the backfill, in `licence_work_backoff` (kind `app_licence`).
--
-- Grants: agent_app (the API process that provisions, in the signup step and the reconciler tick) SELECT, INSERT,
-- UPDATE. agent_runtime reads nothing here (the daemons mint on demand and keep no status). No DELETE. PUBLIC has
-- nothing.
CREATE TABLE tenant_app_licence (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    app_key        text NOT NULL CHECK (app_key IN ('inference-router', 'strata-rag', 'strata-ingest')),
    licence_id     text CHECK (licence_id IS NULL OR btrim(licence_id) <> ''),
    token_id       text CHECK (token_id IS NULL OR btrim(token_id) <> ''),
    provisioned_at timestamptz,
    last_smoke_at  timestamptz,
    last_error     text,
    PRIMARY KEY (tenant_id, app_key)
);
ALTER TABLE tenant_app_licence ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_app_licence FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON tenant_app_licence FROM PUBLIC;
REVOKE ALL ON tenant_app_licence FROM agent_app, agent_runtime;
GRANT SELECT, INSERT, UPDATE ON tenant_app_licence TO agent_app;

-- The backfill candidates: ACTIVE tenants that hold their own licence (the subject the app licences hang off) and
-- have fewer than three provisioned apps, whose backoff (kind `app_licence`, item '') is not pending. At most p_limit TENANTS,
-- never-failed first, then the longest overdue. Platform-only (owner rights: one cross-tenant read).
CREATE FUNCTION agent_app_licence_work(p_limit bigint) RETURNS TABLE (tenant_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT t.id
          FROM tenants t
          LEFT JOIN licence_work_backoff b
                 ON b.kind = 'app_licence' AND b.tenant_id = t.id AND b.item = ''
         WHERE t.status = 'active' AND t.licence_id IS NOT NULL
           AND (SELECT count(*) FROM tenant_app_licence a WHERE a.tenant_id = t.id AND a.provisioned_at IS NOT NULL) < 3
           AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
         ORDER BY b.next_attempt_at NULLS FIRST, t.created_at
         LIMIT p_limit
    $$;
REVOKE ALL ON FUNCTION agent_app_licence_work(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_app_licence_work(bigint) TO agent_platform;

-- The reconciler's backoff table (0038) takes the new kind, and its prune (0038) knows when that work is gone.
ALTER TABLE licence_work_backoff DROP CONSTRAINT licence_work_backoff_kind_check;
ALTER TABLE licence_work_backoff ADD CONSTRAINT licence_work_backoff_kind_check
    CHECK (kind IN ('mint', 'revoke', 'parked', 'tenant', 'app_licence'));

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
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND t.status = 'active' AND t.licence_id IS NULL)
                 WHEN 'app_licence' THEN EXISTS (
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND t.status = 'active'
                        AND (SELECT count(*) FROM tenant_app_licence a WHERE a.tenant_id = t.id AND a.provisioned_at IS NOT NULL) < 3)
                 ELSE false
             END
            RETURNING 1
        )
        SELECT count(*) FROM gone
    $$;
