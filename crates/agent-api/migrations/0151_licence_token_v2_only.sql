-- 0151 — licence tokens are `v2:` only (B7 T1 follow-up; crate::crypto::licence). Forward-only (pre-prod).
-- References only 0096's CHECKs on `tenants.licence_token_encrypted` and `member_entitlement.token_encrypted`.
--
-- 0096 admitted both the legacy `v1:` format and `v2:<kid>:` while the boot reseal (the retired `reseal_v1`) rewrote
-- every `v1:` token. Production finished it (resealed 4, remaining 0, verified 2026-10-21) and the code no longer opens
-- `v1:`, so the CHECKs narrow to `v2:` under the same names. A `v1:` row still stored would be unreadable from now on:
-- the guard fails the migration (and so the deploy) naming the count, never a value, instead of losing the licence.

-- 1. The guard (the migration owner owns both tables: row-level security does not apply).
DO $$
DECLARE
    n_tenant bigint;
    n_member bigint;
BEGIN
    SELECT count(*) INTO n_tenant FROM tenants WHERE licence_token_encrypted LIKE 'v1:%';
    SELECT count(*) INTO n_member FROM member_entitlement WHERE token_encrypted LIKE 'v1:%';
    IF n_tenant + n_member > 0 THEN
        RAISE EXCEPTION 'licence_v1_tokens_remain: % tenant and % member licence tokens are still legacy v1: (reseal or re-issue them on a build before 0151, then deploy again)',
            n_tenant, n_member;
    END IF;
END
$$;

-- 2. v2-only CHECKs (0096's names).
ALTER TABLE tenants
    DROP CONSTRAINT tenants_licence_token_sealed,
    ADD CONSTRAINT tenants_licence_token_sealed CHECK (
        licence_token_encrypted IS NULL
        OR licence_token_encrypted ~ '^v2:[A-Za-z0-9_-]{1,32}:[A-Za-z0-9_-]+$');
ALTER TABLE member_entitlement
    DROP CONSTRAINT member_entitlement_token_sealed,
    ADD CONSTRAINT member_entitlement_token_sealed CHECK (
        token_encrypted IS NULL
        OR token_encrypted ~ '^v2:[A-Za-z0-9_-]{1,32}:[A-Za-z0-9_-]+$');
