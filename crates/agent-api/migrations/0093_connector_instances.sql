-- 0093 — tenant connector instances and credential custody (B7 T8; plan B7 "Data model" 0093, from P6 T3 + T4).
-- Forward-only (pre-prod).
--
-- 1. `tenant_data_key` (Definer class): one data-encryption key (DEK) per tenant, stored only WRAPPED by the
--    TenantData keyring (crate::crypto::keyring; AAD tenant ‖ kek_id). No table grant to anyone: it is reached
--    through three SECURITY DEFINER functions. `agent_tenant_dek()` / `agent_tenant_dek_put()` act on the caller's
--    tenant (`app_current_tenant()`, agent_app); `agent_tenant_dek_destroy(tenant)` (agent_platform, from the
--    offboard transaction) wipes the wrapped key and stamps `destroyed_at`: a crypto-shred of every credential of the
--    tenant (spec 12 §7 caveat: effective after the offsite dump retention or a KEK rotation). A destroyed row stays,
--    so a later put cannot mint a new key for an offboarded tenant.
-- 2. `connector_instance` (Tenant class): a tenant's configured system (D69 environment resource, live in both envs).
-- 3. `connector_credential`: one sealed JSON object of the template's secret fields per instance (AES-256-GCM
--    under the tenant DEK, AAD tenant ‖ instance ‖ credential_version; crate::connectors::custody). Write-only to
--    the UI: only `fingerprint` (8 hex of HMAC-SHA256 under the DEK) is ever shown.
-- 4. `connector_contract_result`, `connector_sample`: T3 contract test results and the sandbox sample a JSONPath
--    override is tried against.
-- All tenant tables: RLS `tenant_isolation`, grants to agent_app; children cascade with their instance.

CREATE TABLE tenant_data_key (
    tenant_id    uuid        PRIMARY KEY REFERENCES tenants (id),
    -- The keyring-sealed DEK (`v2:<kek_id>:<b64url>` as UTF-8 bytes); NULL once destroyed.
    wrapped_dek  bytea,
    kek_id       text        CHECK (kek_id IS NULL OR kek_id ~ '^[A-Za-z0-9_-]{1,32}$'),
    dek_version  integer     NOT NULL DEFAULT 1 CHECK (dek_version >= 1),
    created_at   timestamptz NOT NULL DEFAULT now(),
    destroyed_at timestamptz,
    CONSTRAINT tenant_data_key_live CHECK (
        CASE WHEN destroyed_at IS NULL THEN wrapped_dek IS NOT NULL AND kek_id IS NOT NULL
             ELSE wrapped_dek IS NULL END)
);
ALTER TABLE tenant_data_key ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON tenant_data_key FROM PUBLIC;
REVOKE ALL ON tenant_data_key FROM agent_app;

-- The caller tenant's key row (none when it has no key yet).
CREATE FUNCTION agent_tenant_dek()
    RETURNS TABLE (wrapped_dek bytea, kek_id text, dek_version integer, destroyed_at timestamptz)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT k.wrapped_dek, k.kek_id, k.dek_version, k.destroyed_at
          FROM tenant_data_key k
         WHERE k.tenant_id = app_current_tenant()
    $$;
REVOKE ALL ON FUNCTION agent_tenant_dek() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_tenant_dek() TO agent_app;

-- Store the caller tenant's first key. Insert-only: an existing row (live or destroyed) is kept and `false` is
-- returned, so two replicas minting at once converge on whichever key landed first (the caller re-reads it).
CREATE FUNCTION agent_tenant_dek_put(p_wrapped bytea, p_kek_id text, p_dek_version integer)
    RETURNS boolean
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
DECLARE
    t uuid := app_current_tenant();
    n integer;
BEGIN
    IF t IS NULL THEN
        RAISE EXCEPTION 'tenant_not_found' USING DETAIL = 'agent_tenant_dek_put needs app.current_tenant';
    END IF;
    INSERT INTO tenant_data_key (tenant_id, wrapped_dek, kek_id, dek_version)
         VALUES (t, p_wrapped, p_kek_id, p_dek_version)
    ON CONFLICT (tenant_id) DO NOTHING;
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n = 1;
END $$;
REVOKE ALL ON FUNCTION agent_tenant_dek_put(bytea, text, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_tenant_dek_put(bytea, text, integer) TO agent_app;

-- Crypto-shred `p_tenant`'s key (offboard). Idempotent; a tenant without a key gets a destroyed row.
CREATE FUNCTION agent_tenant_dek_destroy(p_tenant uuid)
    RETURNS void
    LANGUAGE sql SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        INSERT INTO tenant_data_key AS k (tenant_id, wrapped_dek, kek_id, destroyed_at)
             VALUES (p_tenant, NULL, NULL, now())
        ON CONFLICT (tenant_id) DO UPDATE
           SET wrapped_dek = NULL, kek_id = NULL, destroyed_at = coalesce(k.destroyed_at, now())
    $$;
REVOKE ALL ON FUNCTION agent_tenant_dek_destroy(uuid) FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_tenant_dek_destroy(uuid) FROM agent_app;
GRANT EXECUTE ON FUNCTION agent_tenant_dek_destroy(uuid) TO agent_platform;

CREATE TABLE connector_instance (
    tenant_id        uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    id               text        NOT NULL CHECK (id ~ '^[a-z][a-z0-9_-]{2,40}$'),
    label            text        NOT NULL CHECK (btrim(label) <> ''),
    template_scope   text        NOT NULL CHECK (template_scope IN ('platform', 'tenant')),
    template_key     text        NOT NULL,
    template_version text        NOT NULL,
    env              text        NOT NULL CHECK (env IN ('prod', 'test', 'all')),
    base_url         text        NOT NULL DEFAULT '',
    env_ids          jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(env_ids) = 'object'),
    max_rps          numeric     CHECK (max_rps IS NULL OR max_rps > 0),
    stub             boolean     NOT NULL DEFAULT false,
    platform         boolean     NOT NULL DEFAULT false,
    status           text        NOT NULL DEFAULT 'untested' CHECK (status IN
                         ('ok', 'auth_failed', 'invalid_target', 'draft_template', 'untested')),
    last_error       jsonb       CHECK (last_error IS NULL OR jsonb_typeof(last_error) = 'object'),
    created_by       text        NOT NULL CHECK (btrim(created_by) <> ''),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id),
    -- A stub serves the test env only; a platform connector (strata_rag, agent_cases) serves both, and only it does.
    CONSTRAINT connector_instance_stub_test CHECK (NOT stub OR env = 'test'),
    CONSTRAINT connector_instance_platform_all CHECK (platform = (env = 'all'))
);
CREATE INDEX connector_instance_template ON connector_instance (tenant_id, template_scope, template_key);
ALTER TABLE connector_instance ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON connector_instance FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON connector_instance TO agent_app;

-- The pinned template must exist in its scope. Platform scope: `connector_template` (key, version). Tenant scope:
-- the same tenant's `tenant_connector_template` (0092, B7 T11; migrated before this file, so it is looked up here).
CREATE FUNCTION connector_instance_template_exists() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    IF NEW.template_scope = 'platform' AND NOT EXISTS (
        SELECT 1 FROM connector_template t WHERE t.key = NEW.template_key AND t.version = NEW.template_version
    ) THEN
        RAISE EXCEPTION 'connector_template_not_found'
            USING DETAIL = format('no platform connector template %s@%s', NEW.template_key, NEW.template_version);
    END IF;
    IF NEW.template_scope = 'tenant' AND NOT EXISTS (
        SELECT 1 FROM tenant_connector_template t
         WHERE t.tenant_id = NEW.tenant_id AND t.key = NEW.template_key AND t.version = NEW.template_version
    ) THEN
        RAISE EXCEPTION 'connector_template_not_found'
            USING DETAIL = format('this tenant has no connector template %s@%s', NEW.template_key,
                                  NEW.template_version);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER connector_instance_template_exists
    BEFORE INSERT OR UPDATE OF template_scope, template_key, template_version ON connector_instance
    FOR EACH ROW EXECUTE FUNCTION connector_instance_template_exists();

CREATE TABLE connector_credential (
    tenant_id          uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    instance_id        text        NOT NULL,
    ciphertext         bytea       NOT NULL CHECK (octet_length(ciphertext) >= 16),
    nonce              bytea       NOT NULL CHECK (octet_length(nonce) = 12),
    credential_version integer     NOT NULL CHECK (credential_version >= 1),
    dek_version        integer     NOT NULL CHECK (dek_version >= 1),
    fingerprint        char(8)     NOT NULL CHECK (fingerprint ~ '^[0-9a-f]{8}$'),
    updated_by         text        NOT NULL CHECK (btrim(updated_by) <> ''),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, instance_id),
    CONSTRAINT connector_credential_instance_fk FOREIGN KEY (tenant_id, instance_id)
        REFERENCES connector_instance (tenant_id, id) ON DELETE CASCADE
);
ALTER TABLE connector_credential ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON connector_credential FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON connector_credential TO agent_app;

-- One T3 result per (instance, op); `response` is the redacted body kept for samples of W ops. The primary key
-- serves the plan's (tenant_id, instance_id) lookup.
CREATE TABLE connector_contract_result (
    tenant_id   uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    instance_id text        NOT NULL,
    op          text        NOT NULL CHECK (btrim(op) <> ''),
    env         text        NOT NULL CHECK (env IN ('prod', 'test')),
    side_effect text        NOT NULL CHECK (side_effect IN ('R', 'W', 'X', 'P')),
    outcome     text        NOT NULL CHECK (outcome IN ('green', 'red', 'write_unverified', 'needs_ok', 'not_run')),
    code        text,
    note        text        NOT NULL DEFAULT '',
    at          timestamptz,
    ran_by      text,
    response    jsonb       CHECK (response IS NULL OR octet_length(response::text) <= 65536),
    PRIMARY KEY (tenant_id, instance_id, op),
    CONSTRAINT connector_contract_result_instance_fk FOREIGN KEY (tenant_id, instance_id)
        REFERENCES connector_instance (tenant_id, id) ON DELETE CASCADE
);
ALTER TABLE connector_contract_result ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON connector_contract_result FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON connector_contract_result TO agent_app;

CREATE TABLE connector_sample (
    tenant_id   uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    instance_id text        NOT NULL,
    op          text        NOT NULL CHECK (btrim(op) <> ''),
    fetched_at  timestamptz NOT NULL DEFAULT now(),
    body        jsonb       NOT NULL CHECK (octet_length(body::text) <= 262144),
    PRIMARY KEY (tenant_id, instance_id),
    CONSTRAINT connector_sample_instance_fk FOREIGN KEY (tenant_id, instance_id)
        REFERENCES connector_instance (tenant_id, id) ON DELETE CASCADE
);
ALTER TABLE connector_sample ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON connector_sample FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON connector_sample TO agent_app;
