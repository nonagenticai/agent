-- 0096 — licence keyring v2 CHECKs, the revoked-licence tombstone and the re-issue history (B7 T1, T3c, R2).
-- Forward-only (pre-prod). References only `tenants` (0001), `audit_event` (0034), `member_entitlement` (0037),
-- `app_current_tenant()` (0001) and the `agent_app` / `agent_platform` roles.
--
-- 1. Licence tokens (B7 T1, crate::crypto::licence): new tokens are `v2:<kid>:` (the LicenceToken keyring); the boot
--    reseal (crate::crypto::licence::reseal_v1) rewrites every `v1:` one. 0037's CHECKs admit `v1:` only, so they are
--    replaced by CHECKs that admit both: during a rolling deploy an older replica still seals `v1:`, and a `v1:` row
--    the reseal could not open must still accept its other updates (a member disable, a re-issue clear). A later
--    migration narrows them to `v2:` once production's `/ready` shows no `v1:` token left.
-- 2. `licence_revocation` (B7 T3c): the tombstone of every licence id agent revoked, one row per (tenant, licence),
--    written in the transaction that records the revoke (crate::licensing::store::record_revocation). It replaces the
--    `audit_event` read of crate::identity::licence_revoked (unindexed on action / licence id, and subject to audit
--    retention). Tenant data under RLS; append-only for `agent_app` (SELECT, INSERT; no UPDATE / DELETE).
--    Backfilled from the audited `license.revoke` successes.
-- 3. `licence_reissue` (R2): one row per re-issue / re-mint of a holder's licence (crate::licensing::reissue), read for
--    "last re-issue" on `/platform/licensing` and the tenant licence card. A platform table: `agent_platform` SELECT,
--    INSERT; nothing for `agent_app`. No FK to `tenants`: an archived tenant keeps its history. Never a token.

-- 1. v2 CHECKs (0037 named its column CHECKs by the default `<table>_<column>_check`).
ALTER TABLE tenants
    DROP CONSTRAINT tenants_licence_token_encrypted_check,
    ADD CONSTRAINT tenants_licence_token_sealed CHECK (
        licence_token_encrypted IS NULL
        OR licence_token_encrypted LIKE 'v1:%'
        OR licence_token_encrypted ~ '^v2:[A-Za-z0-9_-]{1,32}:[A-Za-z0-9_-]+$');
ALTER TABLE member_entitlement
    DROP CONSTRAINT member_entitlement_token_encrypted_check,
    ADD CONSTRAINT member_entitlement_token_sealed CHECK (
        token_encrypted IS NULL
        OR token_encrypted LIKE 'v1:%'
        OR token_encrypted ~ '^v2:[A-Za-z0-9_-]{1,32}:[A-Za-z0-9_-]+$');

-- 2. The tombstone.
CREATE TABLE licence_revocation (
    tenant_id  uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    license_id text        NOT NULL CHECK (license_id <> ''),
    revoked_at timestamptz NOT NULL DEFAULT now(),
    actor      text        NOT NULL CHECK (btrim(actor) <> ''),
    via        text        NOT NULL CHECK (via IN ('member', 'tenant', 'drain', 'reissue', 'lost_race')),
    -- The `license.revoke` audit row of the same transaction (none for a lost race's give-back). No FK: the
    -- tombstone outlives audit retention.
    audit_id   bigint,
    PRIMARY KEY (tenant_id, license_id)
);
CREATE INDEX licence_revocation_license ON licence_revocation (license_id);
ALTER TABLE licence_revocation ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON licence_revocation FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON licence_revocation FROM agent_app;
GRANT SELECT, INSERT ON licence_revocation TO agent_app;

-- Backfill: every audited, confirmed revoke (the earliest per licence id). `via` from the row: a re-issue says so in
-- its details, a tenant-resource row is the tenant's own licence (offboard or drain), the rest are member revokes.
INSERT INTO licence_revocation (tenant_id, license_id, revoked_at, actor, via, audit_id)
SELECT DISTINCT ON (tenant_id, details ->> 'license_id')
       tenant_id,
       details ->> 'license_id',
       at,
       actor_id,
       CASE
           WHEN details ->> 'via' = 'reissue' THEN 'reissue'
           WHEN resource_type = 'tenant' THEN 'tenant'
           ELSE 'member'
       END,
       id
  FROM audit_event
 WHERE action = 'license.revoke' AND status = 'success'
   AND coalesce(details ->> 'license_id', '') <> ''
 ORDER BY tenant_id, details ->> 'license_id', id
    ON CONFLICT (tenant_id, license_id) DO NOTHING;

-- 3. The re-issue history.
CREATE TABLE licence_reissue (
    id             uuid        PRIMARY KEY DEFAULT uuidv7(),
    tenant_id      uuid        NOT NULL,
    at             timestamptz NOT NULL DEFAULT now(),
    actor          text        NOT NULL CHECK (btrim(actor) <> ''),
    holder         text        NOT NULL CHECK (holder IN ('tenant', 'member')),
    member_sub     text        CHECK (member_sub IS NULL OR btrim(member_sub) <> ''),
    kind           text        NOT NULL CHECK (kind IN ('reissue', 'remint')),
    -- The verdict that called for it (T3a/T3b), `leaked` for a re-mint, `requested` for an operator re-issue on any
    -- verdict (the F-31 route as built: no verdict machinery gates it).
    reason         text        NOT NULL
                   CHECK (reason IN ('expired', 'revoked', 'inactive', 'unreadable', 'leaked', 'requested')),
    old_licence_id text        CHECK (old_licence_id IS NULL OR old_licence_id <> ''),
    -- NULL while the provisioning tick still has to mint it (`status = pending`).
    new_licence_id text        CHECK (new_licence_id IS NULL OR new_licence_id <> ''),
    -- The keyring kid the new token is sealed under (NULL when nothing new is stored yet).
    kid            text        CHECK (kid IS NULL OR kid ~ '^[A-Za-z0-9_-]{1,32}$'),
    old_revoke     text        NOT NULL CHECK (old_revoke IN ('revoked', 'pending', 'none')),
    status         text        NOT NULL CHECK (status IN ('reissued', 'pending')),
    CONSTRAINT licence_reissue_member_sub CHECK ((holder = 'member') = (member_sub IS NOT NULL)),
    CONSTRAINT licence_reissue_new_when_reissued CHECK (status <> 'reissued' OR new_licence_id IS NOT NULL)
);
CREATE INDEX licence_reissue_tenant_at ON licence_reissue (tenant_id, at DESC);
REVOKE ALL ON licence_reissue FROM agent_app;
GRANT SELECT, INSERT ON licence_reissue TO agent_platform;
