-- 0090 — tenant licence verdicts and the reseal-run log (B7 T2; plan B7 "Data model" 0090 part 2, addendum
-- 2026-10-21 R3/R4). Forward-only (pre-prod). The v2 keyring CHECKs the plan put in 0090 part 1 moved to WS-A's 0096.
--
-- 1. `tenant_licence_verdict` (Tenant class): what licences last said about each tenant's OWN licence
--    (crate::licensing::verdict). One row per tenant, rewritten by the refresher every 60 s and on a nudge. `status`
--    `active|inactive|revoked|expired|none` (`none` = the tenant holds no licence id yet); `reason` says why when not
--    active (`license_unavailable`, `license_unreadable`, `tenant_suspended`, licences' own reason, ...). Never a token.
-- 2. `licence_reseal_run` (Platform class): one row per reseal run (written by WS-C's reseal route as agent_platform).
-- 3. Definer functions, EXECUTE for agent_platform only (a super_admin route or the refresher reaches them inside
--    `begin_platform`): `agent_licence_verdicts()` (every row, for the platform overview and the edge's core seam),
--    `agent_licence_verdict_tenants()` (the refresher's work list: every non-archived tenant) and
--    `agent_licence_pending()` (R4: the age of each active tenant's oldest pending licence).
--
-- The functions read `tenants.platform_owned` (0050), which a migration-order test may apply after this one: bodies
-- are checked at the first call, not here (what 0036 does).
SET LOCAL check_function_bodies = off;

CREATE TABLE tenant_licence_verdict (
    tenant_id      uuid        PRIMARY KEY DEFAULT app_current_tenant() REFERENCES tenants (id),
    status         text        NOT NULL CHECK (status IN ('active', 'inactive', 'revoked', 'expired', 'none')),
    reason         text        CHECK (reason IS NULL OR char_length(reason) BETWEEN 1 AND 80),
    licence_id     text        CHECK (licence_id IS NULL OR licence_id <> ''),
    plan           text        CHECK (plan IS NULL OR char_length(plan) BETWEEN 1 AND 80),
    -- The keyring kid the stored token is sealed under (`v2:<kid>:…`); NULL for a `v1:` token or no token.
    kid            text        CHECK (kid IS NULL OR kid ~ '^[A-Za-z0-9_-]{1,32}$'),
    platform_owned boolean     NOT NULL DEFAULT false,
    checked_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT tenant_licence_verdict_active_has_licence CHECK (status <> 'active' OR licence_id IS NOT NULL)
);
ALTER TABLE tenant_licence_verdict ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_licence_verdict FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON tenant_licence_verdict TO agent_app;

CREATE TABLE licence_reseal_run (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    at         timestamptz NOT NULL DEFAULT now(),
    actor      text        NOT NULL CHECK (btrim(actor) <> ''),
    kid        text        NOT NULL CHECK (kid ~ '^[A-Za-z0-9_-]{1,32}$'),
    resealed   integer     NOT NULL CHECK (resealed >= 0),
    remaining  integer     NOT NULL CHECK (remaining >= 0),
    unreadable integer     NOT NULL CHECK (unreadable >= 0),
    tenants    uuid[]      NOT NULL DEFAULT '{}'
);
CREATE INDEX licence_reseal_run_at ON licence_reseal_run (at DESC);
REVOKE ALL ON licence_reseal_run FROM PUBLIC;
REVOKE ALL ON licence_reseal_run FROM agent_app;
GRANT SELECT, INSERT ON licence_reseal_run TO agent_platform;

-- Every verdict row, across tenants.
CREATE FUNCTION agent_licence_verdicts()
    RETURNS TABLE (tenant_id uuid, status text, reason text, licence_id text, plan text, kid text,
                   platform_owned boolean, checked_at timestamptz)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT v.tenant_id, v.status, v.reason, v.licence_id, v.plan, v.kid, v.platform_owned, v.checked_at
          FROM tenant_licence_verdict v
         ORDER BY v.tenant_id
    $$;
REVOKE ALL ON FUNCTION agent_licence_verdicts() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_licence_verdicts() TO agent_platform;

-- The refresher's work list: every tenant that is not archived (an archived tenant's verdict is left as it was).
CREATE FUNCTION agent_licence_verdict_tenants()
    RETURNS TABLE (tenant_id uuid, status text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT t.id, t.status FROM tenants t WHERE t.status <> 'archived' ORDER BY t.id
    $$;
REVOKE ALL ON FUNCTION agent_licence_verdict_tenants() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_licence_verdict_tenants() TO agent_platform;

-- R4: per ACTIVE tenant, the age in seconds of its oldest pending licence (0 when none): the tenant's own licence
-- while `licence_id IS NULL`, and every `pending` member entitlement. A failing mint bumps `updated_at`, so the age
-- starts at the earlier of `updated_at` and the reconciler's `first_failed_at` (0038) for that item.
CREATE FUNCTION agent_licence_pending()
    RETURNS TABLE (tenant_id uuid, pending_seconds double precision)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT t.id,
               GREATEST(0, COALESCE(EXTRACT(EPOCH FROM now() - LEAST(
                   (SELECT LEAST(t.updated_at, b.first_failed_at)
                      FROM (SELECT 1) one
                      LEFT JOIN licence_work_backoff b
                             ON b.kind = 'tenant' AND b.tenant_id = t.id AND b.item = ''
                     WHERE t.licence_id IS NULL),
                   (SELECT min(LEAST(e.updated_at, b.first_failed_at))
                      FROM member_entitlement e
                      LEFT JOIN licence_work_backoff b
                             ON b.kind = 'mint' AND b.tenant_id = e.tenant_id AND b.item = e.user_sub
                     WHERE e.tenant_id = t.id AND e.status = 'pending')
               ))::double precision, 0))
          FROM tenants t
         WHERE t.status = 'active'
         ORDER BY t.id
    $$;
REVOKE ALL ON FUNCTION agent_licence_pending() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_licence_pending() TO agent_platform;
