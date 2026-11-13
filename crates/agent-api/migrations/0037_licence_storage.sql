-- 0037 — licence storage the commonpractice way (B1 T5; owner D77, amendment L1–L3). Numbered last of B1's range so
-- it applies after 0033–0036.
--
-- agent mints a licence per tenant AND per member and stores the sealed tokens in its own database
-- (commonpractice `license-provisioning.ts`, 0031/0089/0095/0097). Licence ids are TEXT, as licences returns
-- them and as 0030's `tenant_license` stored them. Tokens are `v1:` sealed (AES-256-GCM, tenant id as associated
-- data, crate::license_token); no token is ever in SOPS. Nothing mints here: the tenant licence of a tenant whose
-- `licence_id IS NULL` is minted by the provisioning tick (L7).

-- L1: the tenant's own licence (holder `agent-tenant:<id>:a`) and the ids whose revoke must be retried.
ALTER TABLE tenants
    ADD COLUMN licence_id              text CHECK (licence_id IS NULL OR licence_id <> ''),
    ADD COLUMN licence_token_encrypted text
        CHECK (licence_token_encrypted IS NULL OR licence_token_encrypted LIKE 'v1:%'),
    ADD COLUMN licence_revoke_pending  text[] NOT NULL DEFAULT '{}';

-- L2: one entitlement per member (T). Written `pending` inside the membership transaction, minted after COMMIT
-- and stored by a conditional UPDATE (`WHERE status = 'pending' AND license_id IS NULL`); revoked on disable.
-- Rows are never deleted.
CREATE TABLE member_entitlement (
    tenant_id          uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    user_sub           text NOT NULL CHECK (btrim(user_sub) <> ''),
    license_id         text CHECK (license_id IS NULL OR license_id <> ''),
    license_subject_id text CHECK (license_subject_id IS NULL OR license_subject_id <> ''),
    token_encrypted    text CHECK (token_encrypted IS NULL OR token_encrypted LIKE 'v1:%'),
    status             text NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'active', 'revoking', 'revoked', 'error')),
    error_code         text CHECK (error_code IS NULL OR char_length(error_code) <= 80),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    revoked_at         timestamptz,
    CONSTRAINT member_entitlement_tenant_sub UNIQUE (tenant_id, user_sub),
    CONSTRAINT member_entitlement_active_has_licence CHECK (status <> 'active' OR license_id IS NOT NULL)
);
CREATE INDEX member_entitlement_work ON member_entitlement (status) WHERE status IN ('pending', 'revoking');
ALTER TABLE member_entitlement ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON member_entitlement FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON member_entitlement TO agent_app;

-- L3: every member licence 0030 stored becomes an active entitlement, ids and token copied as they are.
INSERT INTO member_entitlement
       (tenant_id, user_sub, license_id, license_subject_id, token_encrypted, status, created_at, updated_at)
SELECT tenant_id, holder_sub, license_id, subject_id, token_encrypted, 'active', issued_at, issued_at
  FROM tenant_license
    ON CONFLICT (tenant_id, user_sub) DO NOTHING;
DROP TABLE tenant_license;
