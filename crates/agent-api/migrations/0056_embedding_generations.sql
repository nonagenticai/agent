-- 0056 — B3 T4a: embeddings (and every other knowledge model) per job (D83, widened to all six capabilities by D93;
-- plan B3 §4, §5.8, §0 R7).
--
-- `store_generation`: one strata ROOT per model set of one agent root store (`store_id`, a root `knowledge_store`).
-- Generation `initial` is the store's own strata binding (`strata_store_id` NULL here: it is
-- `knowledge_store.strata_store_id`); every rebuild (`reembed` / `copy` / `reingest`) is a NEW strata root whose subtree
-- strata mirrors from the source generation (R7: one embed model per tree, never re-embedded in place). `members` maps
-- each agent store of the tree (its id) to its strata store in this generation (rebuilds only). Model columns hold the
-- slot's model name, NULL = the deployment default. `provider_ref` = `{<capability>: {source: tenant|default,
-- slot_version}}` (never a key). At most one `building` / `ready` generation per model set; a `failed` or `retired`
-- one stays as history.
--
-- `job_embedding_pin`: the WORKING (draft) row of the job ledger kind `embedding_pin` (key `embedding:<store_key>`),
-- written only through `design::track` or the kind's target (crate::knowledge::targets, tests/design_antidrift.rs).
-- No row = the job uses the store's `initial` generation. The row carries the pinned value as the ledger sees it
-- (`generation`, `models`), so a release restore never depends on a generation row still existing.
--
-- `model_change` / `model_change_job`: one row per tenant slot model change, fanned out to every job whose pinned
-- generation runs the old model (`stores` = `[{store_id, from_generation, to_generation?}]`). `reembedding` = a rebuild
-- is in flight; `ready` = every target generation is built; `accepted` = the pins are in the job's draft; `published`
-- = the job published them (the kind's `published` hook).
--
-- Drift-tolerant as 0050 (IF NOT EXISTS, DROP … IF EXISTS + ADD, DROP POLICY IF EXISTS + CREATE, REVOKE ALL + GRANT).
-- References nothing from 0091–0093 (tests/migration_order.rs). Forward-only.

CREATE TABLE IF NOT EXISTS store_generation (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    id              uuid NOT NULL,
    store_id        uuid NOT NULL,
    strata_store_id uuid,
    embed           text,
    rerank          text,
    chat            text,
    ocr             text,
    vision          text,
    transcribe      text,
    provider_ref    jsonb NOT NULL DEFAULT '{}',
    build           text NOT NULL,
    state           text NOT NULL,
    source_id       uuid,
    reembed_job     text,
    members         jsonb NOT NULL DEFAULT '{}',
    chunks          integer,
    missing         jsonb NOT NULL DEFAULT '[]',
    error           text,
    created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    created_by      text NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (id)
);
ALTER TABLE IF EXISTS model_change_job DROP CONSTRAINT IF EXISTS model_change_job_change;
ALTER TABLE store_generation
    DROP CONSTRAINT IF EXISTS store_generation_tenant,
    ADD CONSTRAINT store_generation_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS store_generation_tenant_id_key,
    ADD CONSTRAINT store_generation_tenant_id_key UNIQUE (tenant_id, id),
    DROP CONSTRAINT IF EXISTS store_generation_store,
    ADD CONSTRAINT store_generation_store FOREIGN KEY (tenant_id, store_id)
        REFERENCES knowledge_store (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS store_generation_build_check,
    ADD CONSTRAINT store_generation_build_check CHECK (build IN ('initial', 'reembed', 'copy', 'reingest')),
    DROP CONSTRAINT IF EXISTS store_generation_state_check,
    ADD CONSTRAINT store_generation_state_check CHECK (state IN ('building', 'ready', 'failed', 'retired')),
    DROP CONSTRAINT IF EXISTS store_generation_initial_check,
    ADD CONSTRAINT store_generation_initial_check
        CHECK (CASE WHEN build = 'initial'
                    THEN strata_store_id IS NULL AND source_id IS NULL AND reembed_job IS NULL
                         AND state IN ('ready', 'retired')
                    ELSE source_id IS NOT NULL AND (strata_store_id IS NOT NULL OR state IN ('building', 'failed'))
               END),
    DROP CONSTRAINT IF EXISTS store_generation_members_check,
    ADD CONSTRAINT store_generation_members_check CHECK (jsonb_typeof(members) = 'object'),
    DROP CONSTRAINT IF EXISTS store_generation_missing_check,
    ADD CONSTRAINT store_generation_missing_check CHECK (jsonb_typeof(missing) = 'array'),
    DROP CONSTRAINT IF EXISTS store_generation_ref_check,
    ADD CONSTRAINT store_generation_ref_check CHECK (jsonb_typeof(provider_ref) = 'object'),
    DROP CONSTRAINT IF EXISTS store_generation_created_by_check,
    ADD CONSTRAINT store_generation_created_by_check CHECK (btrim(created_by) <> '');
-- one building / ready generation per model set of a store
CREATE UNIQUE INDEX IF NOT EXISTS store_generation_one_live_set
    ON store_generation (tenant_id, store_id, embed, rerank, chat, ocr, vision, transcribe) NULLS NOT DISTINCT
    WHERE state IN ('building', 'ready');
-- one initial generation per store, whatever its state
CREATE UNIQUE INDEX IF NOT EXISTS store_generation_one_initial
    ON store_generation (tenant_id, store_id) WHERE build = 'initial';
ALTER TABLE store_generation ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON store_generation;
CREATE POLICY tenant_isolation ON store_generation FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON store_generation FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON store_generation TO agent_app;

CREATE TABLE IF NOT EXISTS job_embedding_pin (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    job_id        uuid NOT NULL,
    store_key     text NOT NULL,
    generation_id uuid NOT NULL,
    models        jsonb NOT NULL DEFAULT '{}',
    updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by    text NOT NULL,
    PRIMARY KEY (tenant_id, job_id, store_key)
);
ALTER TABLE job_embedding_pin
    DROP CONSTRAINT IF EXISTS job_embedding_pin_tenant,
    ADD CONSTRAINT job_embedding_pin_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS job_embedding_pin_job,
    ADD CONSTRAINT job_embedding_pin_job FOREIGN KEY (tenant_id, job_id)
        REFERENCES jobs (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS job_embedding_pin_store_key_check,
    ADD CONSTRAINT job_embedding_pin_store_key_check CHECK (store_key ~ '^[a-z][a-z0-9_.-]{1,62}$'),
    DROP CONSTRAINT IF EXISTS job_embedding_pin_models_check,
    ADD CONSTRAINT job_embedding_pin_models_check CHECK (jsonb_typeof(models) = 'object'),
    DROP CONSTRAINT IF EXISTS job_embedding_pin_updated_by_check,
    ADD CONSTRAINT job_embedding_pin_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE job_embedding_pin ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_embedding_pin;
CREATE POLICY tenant_isolation ON job_embedding_pin FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_embedding_pin FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON job_embedding_pin TO agent_app;

CREATE TABLE IF NOT EXISTS model_change (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    id         uuid NOT NULL,
    scope      text NOT NULL,
    slot       text NOT NULL,
    from_model text,
    to_model   text,
    rebuild    text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    created_by text NOT NULL,
    PRIMARY KEY (id)
);
ALTER TABLE model_change
    DROP CONSTRAINT IF EXISTS model_change_tenant,
    ADD CONSTRAINT model_change_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS model_change_tenant_id_key,
    ADD CONSTRAINT model_change_tenant_id_key UNIQUE (tenant_id, id),
    DROP CONSTRAINT IF EXISTS model_change_scope_check,
    ADD CONSTRAINT model_change_scope_check CHECK (scope IN ('platform', 'tenant')),
    DROP CONSTRAINT IF EXISTS model_change_slot_check,
    ADD CONSTRAINT model_change_slot_check
        CHECK (slot IN ('embed', 'rerank', 'chat', 'ocr', 'vision', 'transcribe')),
    DROP CONSTRAINT IF EXISTS model_change_rebuild_check,
    ADD CONSTRAINT model_change_rebuild_check CHECK (
        (slot = 'embed' AND rebuild = 'reembed')
        OR (slot IN ('rerank', 'chat') AND rebuild = 'copy')
        OR (slot IN ('ocr', 'vision', 'transcribe') AND rebuild = 'reingest')),
    DROP CONSTRAINT IF EXISTS model_change_moves_check,
    ADD CONSTRAINT model_change_moves_check CHECK (from_model IS DISTINCT FROM to_model),
    DROP CONSTRAINT IF EXISTS model_change_created_by_check,
    ADD CONSTRAINT model_change_created_by_check CHECK (btrim(created_by) <> '');
CREATE INDEX IF NOT EXISTS model_change_created_idx ON model_change (tenant_id, created_at DESC);
ALTER TABLE model_change ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON model_change;
CREATE POLICY tenant_isolation ON model_change FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON model_change FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON model_change TO agent_app;

CREATE TABLE IF NOT EXISTS model_change_job (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    model_change_id uuid NOT NULL,
    job_id          uuid NOT NULL,
    state           text NOT NULL DEFAULT 'pending',
    stores          jsonb NOT NULL DEFAULT '[]',
    chunks          integer,
    updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    updated_by      text NOT NULL,
    PRIMARY KEY (tenant_id, model_change_id, job_id)
);
ALTER TABLE model_change_job
    DROP CONSTRAINT IF EXISTS model_change_job_tenant,
    ADD CONSTRAINT model_change_job_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS model_change_job_change,
    ADD CONSTRAINT model_change_job_change FOREIGN KEY (tenant_id, model_change_id)
        REFERENCES model_change (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS model_change_job_job,
    ADD CONSTRAINT model_change_job_job FOREIGN KEY (tenant_id, job_id)
        REFERENCES jobs (tenant_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS model_change_job_state_check,
    ADD CONSTRAINT model_change_job_state_check
        CHECK (state IN ('pending', 'reembedding', 'ready', 'accepted', 'published', 'dismissed')),
    DROP CONSTRAINT IF EXISTS model_change_job_stores_check,
    ADD CONSTRAINT model_change_job_stores_check
        CHECK (jsonb_typeof(stores) = 'array' AND jsonb_array_length(stores) >= 1),
    DROP CONSTRAINT IF EXISTS model_change_job_updated_by_check,
    ADD CONSTRAINT model_change_job_updated_by_check CHECK (btrim(updated_by) <> '');
CREATE INDEX IF NOT EXISTS model_change_job_job_idx ON model_change_job (tenant_id, job_id, state);
ALTER TABLE model_change_job ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON model_change_job;
CREATE POLICY tenant_isolation ON model_change_job FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON model_change_job FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON model_change_job TO agent_app;

-- The `store_delete` outbox op (crate::provision::senders::store_delete): a retired or orphaned generation root is
-- deleted in strata through the outbox, so the op CHECK of 0035 admits it.
ALTER TABLE provision_outbox
    DROP CONSTRAINT IF EXISTS provision_outbox_op_check,
    ADD CONSTRAINT provision_outbox_op_check
        CHECK (op IN ('licence_ensure', 'licence_revoke', 'connections_put', 'store_ensure', 'store_delete'));
