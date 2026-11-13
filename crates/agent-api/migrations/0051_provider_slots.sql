-- 0051 — B3 T4: the tenant's rag/ingest provider slots (D69 draft, interim for W5 A1/A2; plan B3 §4, §5.1, G2).
--
-- `provider_slot` is the WORKING (draft) row of the tenant-wide ledger kind `provider_slot` (key `slot:<slot>`): written
-- only through `design::track_tenant` or the kind's target (crate::providers::targets, tests/design_antidrift.rs).
-- No row, or `configured = false`, is "nothing at the place". `version` is the agent draft version a save names as
-- `expected_version` (409 `VERSION_CONFLICT`); every write of the row bumps it. `secret_id` names the immutable sealed
-- key the value uses; `key_kept` = strata holds a key agent never saw (a drift adopted from another app): the next
-- push sends no `api_key` for the slot and strata keeps its own.
--
-- `provider_slot_secret`: immutable sealed slot keys (plan §0 R2: crate::crypto::keyring `Purpose::ProviderSecret`,
-- `v2:<kid>:…`, AAD `<tenant>:slot_key:<id>`), referenced by working rows, ledger values and release snapshots.
-- Never updated or deleted here (unreferenced rows are B2 retention's).
--
-- `provider_slot_remote`: what agent last pushed to strata (outbox op `connections_put`) and what strata last reported
-- (`GET /api/tenant/providers`), as fingerprints of `base_url|dialect|model|has_api_key` (`none` = no tenant
-- registration). Drift (G2, interim for A2 versions) = `observed_fpr <> pushed_fpr`; `observed` keeps the reported
-- registration (never a key: strata never returns one) so a drift can be adopted.
--
-- Drift-tolerant as 0050 (IF NOT EXISTS, DROP … IF EXISTS + ADD, DROP POLICY IF EXISTS + CREATE, REVOKE ALL + GRANT).
-- References nothing from 0091–0093 (tests/migration_order.rs). Forward-only.

CREATE TABLE IF NOT EXISTS provider_slot_secret (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    id          uuid NOT NULL,
    slot        text NOT NULL,
    key_sealed  text NOT NULL,
    fingerprint text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    created_by  text NOT NULL,
    PRIMARY KEY (id)
);
ALTER TABLE provider_slot_secret
    DROP CONSTRAINT IF EXISTS provider_slot_secret_tenant,
    ADD CONSTRAINT provider_slot_secret_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS provider_slot_secret_tenant_id_key,
    ADD CONSTRAINT provider_slot_secret_tenant_id_key UNIQUE (tenant_id, id),
    DROP CONSTRAINT IF EXISTS provider_slot_secret_slot_check,
    ADD CONSTRAINT provider_slot_secret_slot_check
        CHECK (slot IN ('embed', 'rerank', 'chat', 'ocr', 'vision', 'transcribe')),
    DROP CONSTRAINT IF EXISTS provider_slot_secret_sealed_check,
    ADD CONSTRAINT provider_slot_secret_sealed_check CHECK (key_sealed LIKE 'v2:%'),
    DROP CONSTRAINT IF EXISTS provider_slot_secret_fingerprint_check,
    ADD CONSTRAINT provider_slot_secret_fingerprint_check CHECK (fingerprint ~ '^[0-9a-f]{8}$'),
    DROP CONSTRAINT IF EXISTS provider_slot_secret_created_by_check,
    ADD CONSTRAINT provider_slot_secret_created_by_check CHECK (btrim(created_by) <> '');
ALTER TABLE provider_slot_secret ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON provider_slot_secret;
CREATE POLICY tenant_isolation ON provider_slot_secret FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON provider_slot_secret FROM agent_app;
GRANT SELECT, INSERT ON provider_slot_secret TO agent_app;

CREATE TABLE IF NOT EXISTS provider_slot (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    slot       text NOT NULL,
    service    text NOT NULL,
    configured boolean NOT NULL DEFAULT false,
    kind       text,
    base_url   text,
    model      text,
    region     text,
    version    integer NOT NULL DEFAULT 0,
    secret_id  uuid,
    key_kept   boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by text NOT NULL,
    PRIMARY KEY (tenant_id, slot)
);
ALTER TABLE provider_slot
    DROP CONSTRAINT IF EXISTS provider_slot_tenant,
    ADD CONSTRAINT provider_slot_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS provider_slot_secret_fkey,
    ADD CONSTRAINT provider_slot_secret_fkey FOREIGN KEY (tenant_id, secret_id)
        REFERENCES provider_slot_secret (tenant_id, id),
    DROP CONSTRAINT IF EXISTS provider_slot_slot_check,
    ADD CONSTRAINT provider_slot_slot_check
        CHECK (slot IN ('embed', 'rerank', 'chat', 'ocr', 'vision', 'transcribe')),
    DROP CONSTRAINT IF EXISTS provider_slot_service_check,
    ADD CONSTRAINT provider_slot_service_check CHECK (
        service IN ('strata_rag', 'strata_ingest')
        AND (service = 'strata_rag') = (slot IN ('embed', 'rerank', 'chat'))),
    DROP CONSTRAINT IF EXISTS provider_slot_value_check,
    ADD CONSTRAINT provider_slot_value_check CHECK (
        CASE WHEN configured
             THEN kind IS NOT NULL AND char_length(kind) BETWEEN 1 AND 64
                  AND model IS NOT NULL AND char_length(model) BETWEEN 1 AND 200
             ELSE kind IS NULL AND base_url IS NULL AND model IS NULL AND secret_id IS NULL AND NOT key_kept
        END),
    DROP CONSTRAINT IF EXISTS provider_slot_base_url_check,
    ADD CONSTRAINT provider_slot_base_url_check
        CHECK (base_url IS NULL OR (base_url ~ '^https?://' AND char_length(base_url) <= 2048)),
    DROP CONSTRAINT IF EXISTS provider_slot_key_check,
    ADD CONSTRAINT provider_slot_key_check CHECK (NOT (key_kept AND secret_id IS NOT NULL)),
    DROP CONSTRAINT IF EXISTS provider_slot_region_check,
    ADD CONSTRAINT provider_slot_region_check CHECK (region IS NULL OR char_length(region) BETWEEN 1 AND 64),
    DROP CONSTRAINT IF EXISTS provider_slot_version_check,
    ADD CONSTRAINT provider_slot_version_check CHECK (version >= 0),
    DROP CONSTRAINT IF EXISTS provider_slot_updated_by_check,
    ADD CONSTRAINT provider_slot_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE provider_slot ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON provider_slot;
CREATE POLICY tenant_isolation ON provider_slot FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON provider_slot FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON provider_slot TO agent_app;

CREATE TABLE IF NOT EXISTS provider_slot_remote (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    slot           text NOT NULL,
    pushed_version integer NOT NULL DEFAULT 0,
    pushed_fpr     text NOT NULL DEFAULT 'none',
    pushed_at      timestamptz,
    pushed_release text,
    observed_fpr   text,
    observed       jsonb,
    observed_at    timestamptz,
    PRIMARY KEY (tenant_id, slot)
);
ALTER TABLE provider_slot_remote
    DROP CONSTRAINT IF EXISTS provider_slot_remote_tenant,
    ADD CONSTRAINT provider_slot_remote_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS provider_slot_remote_slot_check,
    ADD CONSTRAINT provider_slot_remote_slot_check
        CHECK (slot IN ('embed', 'rerank', 'chat', 'ocr', 'vision', 'transcribe')),
    DROP CONSTRAINT IF EXISTS provider_slot_remote_version_check,
    ADD CONSTRAINT provider_slot_remote_version_check CHECK (pushed_version >= 0),
    DROP CONSTRAINT IF EXISTS provider_slot_remote_observed_check,
    ADD CONSTRAINT provider_slot_remote_observed_check CHECK (
        (observed_fpr IS NULL) = (observed_at IS NULL)
        AND (observed IS NULL OR jsonb_typeof(observed) = 'object'));
ALTER TABLE provider_slot_remote ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON provider_slot_remote;
CREATE POLICY tenant_isolation ON provider_slot_remote FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON provider_slot_remote FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON provider_slot_remote TO agent_app;
