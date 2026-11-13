-- 0050 — B3 T3: the tenant's LLM providers (D70, live data) and which provider serves which use (D69 design: the
-- working row of the tenant-wide ledger kind `llm_use`, written only through `design::track_tenant` or the kind's
-- target, crate::providers::targets). Uses: `qualify`, `gates`, `generator` (D89) and the speech uses `asr`, `tts`
-- (D101/D102, served from T4d; written once here so 0050 never changes). `voice` is a `tts` binding's voice (T4d).
--
-- `tenants.platform_owned` marks the D70 internal tenant (nonagentic's own, `gpuHostingOf`): set at boot for
-- `AGENT_PLATFORM_TENANT_ID` (crate::state::build). That tenant stores no provider: its LLM is the pgx vLLM from config.
--
-- The provider key (plan §0 R2) is one `secret_sealed` text column (crate::crypto::keyring `Purpose::ProviderSecret`,
-- `v2:<kid>:…`, AAD `<tenant>:llm_key:<id>`) with its fingerprint `secret_fpr`; both are set or neither.
--
-- `llm_use_binding.provider_id` has NO foreign key to `llm_provider`, on purpose: the binding is a design value that
-- B2 restores from snapshots through the target's `set` (discard, follow-live), and the design contract
-- (tests/common/design_contract.rs) writes it on a fresh tenant. A provider is therefore never removed: delete is a
-- SOFT delete (`deleted_at`; the key is dropped with it), so a snapshot that still names it (a rollback, an old cut)
-- resolves to an explicit 409 `provider_deleted`, never to "unbound". A deleted provider is hidden from the list, its
-- name is free again (the name is unique among live providers only) and it cannot be bound. The handler still refuses
-- a delete while the provider is bound in the draft, live in any job, or bound by an approved, unapplied cut (409
-- `provider_in_use`); bind holds the provider row FOR KEY SHARE, delete takes it FOR UPDATE and then every job FOR
-- SHARE (apply locks its job FOR UPDATE) before it reads the bindings.
--
-- `tenants.platform_owned` is reconciled at every boot (cleared everywhere when the variable is unset) and only the
-- migration owner may change it: `tenants_guard` refuses a change made as `agent_app`.
--
-- Drift-tolerant as 0044–0047 (IF NOT EXISTS, DROP … IF EXISTS + ADD, DROP POLICY IF EXISTS + CREATE, REVOKE ALL +
-- GRANT). References nothing from 0091–0093 (tests/migration_order.rs). Forward-only.

ALTER TABLE tenants ADD COLUMN IF NOT EXISTS platform_owned boolean NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS llm_provider (
    tenant_id         uuid NOT NULL DEFAULT app_current_tenant(),
    id                uuid NOT NULL,
    name              text NOT NULL,
    kind              text NOT NULL,
    base_url          text NOT NULL,
    models            text[] NOT NULL,
    region            text,
    secret_sealed     text,
    secret_fpr        text,
    secret_updated_at timestamptz,
    last_test         jsonb,
    updated_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by        text NOT NULL,
    deleted_at        timestamptz,
    PRIMARY KEY (id)
);
ALTER TABLE llm_provider ADD COLUMN IF NOT EXISTS deleted_at timestamptz;
ALTER TABLE llm_provider
    DROP CONSTRAINT IF EXISTS llm_provider_tenant,
    ADD CONSTRAINT llm_provider_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS llm_provider_tenant_id_key,
    ADD CONSTRAINT llm_provider_tenant_id_key UNIQUE (tenant_id, id),
    DROP CONSTRAINT IF EXISTS llm_provider_tenant_name_key,
    DROP CONSTRAINT IF EXISTS llm_provider_name_check,
    ADD CONSTRAINT llm_provider_name_check CHECK (char_length(btrim(name)) BETWEEN 1 AND 120),
    DROP CONSTRAINT IF EXISTS llm_provider_kind_check,
    ADD CONSTRAINT llm_provider_kind_check CHECK (kind IN ('openai', 'laya', 'w2v')),
    DROP CONSTRAINT IF EXISTS llm_provider_base_url_check,
    ADD CONSTRAINT llm_provider_base_url_check
        CHECK (base_url ~ '^https?://' AND char_length(base_url) <= 2048),
    DROP CONSTRAINT IF EXISTS llm_provider_models_check,
    ADD CONSTRAINT llm_provider_models_check
        CHECK (cardinality(models) BETWEEN 1 AND 100 AND array_position(models, NULL) IS NULL),
    DROP CONSTRAINT IF EXISTS llm_provider_region_check,
    ADD CONSTRAINT llm_provider_region_check CHECK (region IS NULL OR char_length(region) BETWEEN 1 AND 64),
    DROP CONSTRAINT IF EXISTS llm_provider_secret_check,
    ADD CONSTRAINT llm_provider_secret_check CHECK (
        (secret_sealed IS NULL OR secret_sealed LIKE 'v2:%')
        AND (secret_sealed IS NULL) = (secret_fpr IS NULL)
        AND (secret_sealed IS NULL) = (secret_updated_at IS NULL)),
    DROP CONSTRAINT IF EXISTS llm_provider_deleted_check,
    ADD CONSTRAINT llm_provider_deleted_check CHECK (deleted_at IS NULL OR secret_sealed IS NULL),
    DROP CONSTRAINT IF EXISTS llm_provider_last_test_check,
    ADD CONSTRAINT llm_provider_last_test_check
        CHECK (last_test IS NULL OR jsonb_typeof(last_test) = 'object'),
    DROP CONSTRAINT IF EXISTS llm_provider_updated_by_check,
    ADD CONSTRAINT llm_provider_updated_by_check CHECK (btrim(updated_by) <> '');
-- a name is unique among the tenant's live (not deleted) providers
CREATE UNIQUE INDEX IF NOT EXISTS llm_provider_tenant_name_live ON llm_provider (tenant_id, name)
    WHERE deleted_at IS NULL;
ALTER TABLE llm_provider ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON llm_provider;
CREATE POLICY tenant_isolation ON llm_provider FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON llm_provider FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON llm_provider TO agent_app;

-- The working (draft) binding of one use; no row = the use is unbound (its fallback runs). The live binding of a job
-- is that job's active release snapshot (D62).
CREATE TABLE IF NOT EXISTS llm_use_binding (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    "use"       text NOT NULL,
    provider_id uuid,
    model       text,
    voice       text,
    updated_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by  text NOT NULL,
    PRIMARY KEY (tenant_id, "use")
);
ALTER TABLE llm_use_binding
    DROP CONSTRAINT IF EXISTS llm_use_binding_tenant,
    ADD CONSTRAINT llm_use_binding_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS llm_use_binding_use_check,
    ADD CONSTRAINT llm_use_binding_use_check CHECK ("use" IN ('qualify', 'gates', 'generator', 'asr', 'tts')),
    DROP CONSTRAINT IF EXISTS llm_use_binding_model_check,
    ADD CONSTRAINT llm_use_binding_model_check
        CHECK (model IS NULL OR (provider_id IS NOT NULL AND char_length(model) BETWEEN 1 AND 200)),
    DROP CONSTRAINT IF EXISTS llm_use_binding_voice_check,
    ADD CONSTRAINT llm_use_binding_voice_check
        CHECK (voice IS NULL OR ("use" = 'tts' AND char_length(voice) BETWEEN 1 AND 200)),
    DROP CONSTRAINT IF EXISTS llm_use_binding_updated_by_check,
    ADD CONSTRAINT llm_use_binding_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE llm_use_binding ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON llm_use_binding;
CREATE POLICY tenant_isolation ON llm_use_binding FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON llm_use_binding FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON llm_use_binding TO agent_app;

-- 0032's guard + D70: `platform_owned` is set by the migration owner at boot (crate::state::build), never by the
-- runtime role (which holds a table-wide UPDATE grant on `tenants`).
CREATE OR REPLACE FUNCTION tenants_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = public, pg_temp
    AS $$
BEGIN
    IF NEW.residency IS DISTINCT FROM OLD.residency THEN
        RAISE EXCEPTION 'residency_immutable';
    END IF;
    IF OLD.status = 'archived'
       AND (NEW.status IS DISTINCT FROM OLD.status OR NEW.archived_at IS DISTINCT FROM OLD.archived_at) THEN
        RAISE EXCEPTION 'tenant_archived';
    END IF;
    IF NEW.platform_owned IS DISTINCT FROM OLD.platform_owned AND current_user = 'agent_app' THEN
        RAISE EXCEPTION 'platform_owned_immutable';
    END IF;
    RETURN NEW;
END $$;
