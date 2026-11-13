-- 0171 — generator runs (B8 T9a; plan 2026-10-20-b8-journeys-generator-discovery, § Revision 2026-10-22 R15/R16).
--
-- A run is a P18 pipeline run: `scope = 'platform'` drafts template layers (super_admin), `scope = 'tenant'` drafts
-- refinements of one job (D97). Every row carries a tenant (R15): a platform run carries the platform-owned tenant
-- (`AGENT_PLATFORM_TENANT_ID`), so the tables are class Tenant. RLS: `tenant_isolation` for `agent_app` (a tenant sees
-- its own runs); `platform_read` lets `agent_platform` (super_admin routes, inside `begin_platform`) read and write
-- every run. `blocked` is never stored: it is `running` + a red finding (derived on read).
--
-- Child rows (steps, stages, items, cost lines) repeat the run's tenant for RLS and go with their run (CASCADE).
-- A stage row snapshots its label and order at start (B8-10: stages are an open set; the run keeps what it saw).
--
-- Drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT). Forward-only. References only B1
-- tables (`tenants`, `jobs`), so it applies whichever of KH / B6b / DR merges first.

CREATE TABLE IF NOT EXISTS generator_run (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    scope         text NOT NULL,
    industry      text NOT NULL,
    department    text NOT NULL,
    job_key       text NOT NULL,
    -- tenant runs only: the job the run refines (D97)
    job_id        uuid,
    status        text NOT NULL DEFAULT 'running',
    current_step  text NOT NULL,
    description   text NOT NULL DEFAULT '',
    objects       text[] NOT NULL DEFAULT '{}',
    output        text[] NOT NULL DEFAULT '{}',
    -- what Package wrote; NULL while open
    tagged        text,
    -- funding lane of the drafting provider (D56): what the run's usage is recorded under
    funding       text NOT NULL,
    -- tenant runs (D89): `{provider_id, model, funding}` of the tenant `generator` use at start; never a key
    provider      jsonb,
    started_by    text NOT NULL,
    started_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE generator_run
    DROP CONSTRAINT IF EXISTS generator_run_tenant,
    ADD CONSTRAINT generator_run_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS generator_run_job,
    ADD CONSTRAINT generator_run_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS generator_run_scope_check,
    ADD CONSTRAINT generator_run_scope_check
        CHECK (scope IN ('platform', 'tenant') AND ((scope = 'tenant') = (job_id IS NOT NULL))),
    DROP CONSTRAINT IF EXISTS generator_run_keys_check,
    ADD CONSTRAINT generator_run_keys_check
        CHECK (industry ~ '^[a-z][a-z0-9_]{1,40}$' AND department ~ '^[a-z][a-z0-9_]{1,40}$'
               AND job_key ~ '^[a-z][a-z0-9_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS generator_run_status_check,
    ADD CONSTRAINT generator_run_status_check CHECK (status IN ('running', 'packaged', 'abandoned')),
    DROP CONSTRAINT IF EXISTS generator_run_step_check,
    ADD CONSTRAINT generator_run_step_check
        CHECK (current_step IN ('input', 'business_model', 'journey', 'objects', 'operations', 'harmonise',
                                'validate', 'use_cases', 'package')),
    DROP CONSTRAINT IF EXISTS generator_run_description_check,
    ADD CONSTRAINT generator_run_description_check CHECK (char_length(description) <= 4000),
    DROP CONSTRAINT IF EXISTS generator_run_funding_check,
    ADD CONSTRAINT generator_run_funding_check
        CHECK (funding IN ('local_gpu', 'tenant_byo', 'platform_external')),
    DROP CONSTRAINT IF EXISTS generator_run_provider_check,
    ADD CONSTRAINT generator_run_provider_check CHECK (provider IS NULL OR jsonb_typeof(provider) = 'object'),
    DROP CONSTRAINT IF EXISTS generator_run_started_by_check,
    ADD CONSTRAINT generator_run_started_by_check CHECK (btrim(started_by) <> ''),
    DROP CONSTRAINT IF EXISTS generator_run_tagged_check,
    ADD CONSTRAINT generator_run_tagged_check CHECK ((status = 'packaged') = (tagged IS NOT NULL));
CREATE INDEX IF NOT EXISTS generator_run_recent ON generator_run (tenant_id, id DESC);

CREATE TABLE IF NOT EXISTS generator_step (
    run_id    bigint NOT NULL,
    step      text NOT NULL,
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    ord       smallint NOT NULL,
    status    text NOT NULL,
    summary   text NOT NULL DEFAULT '',
    PRIMARY KEY (run_id, step)
);
ALTER TABLE generator_step
    DROP CONSTRAINT IF EXISTS generator_step_run,
    ADD CONSTRAINT generator_step_run FOREIGN KEY (run_id) REFERENCES generator_run (id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS generator_step_tenant,
    ADD CONSTRAINT generator_step_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS generator_step_step_check,
    ADD CONSTRAINT generator_step_step_check
        CHECK (step IN ('input', 'business_model', 'journey', 'objects', 'operations', 'harmonise',
                        'validate', 'use_cases', 'package')),
    DROP CONSTRAINT IF EXISTS generator_step_status_check,
    ADD CONSTRAINT generator_step_status_check CHECK (status IN ('done', 'current', 'todo', 'blocked')),
    DROP CONSTRAINT IF EXISTS generator_step_summary_check,
    ADD CONSTRAINT generator_step_summary_check CHECK (char_length(summary) <= 2000);

CREATE TABLE IF NOT EXISTS generator_stage (
    run_id       bigint NOT NULL,
    stage        text NOT NULL,
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    ord          smallint NOT NULL,
    label        text NOT NULL,
    out_of_scope boolean NOT NULL DEFAULT false,
    PRIMARY KEY (run_id, stage)
);
ALTER TABLE generator_stage
    DROP CONSTRAINT IF EXISTS generator_stage_run,
    ADD CONSTRAINT generator_stage_run FOREIGN KEY (run_id) REFERENCES generator_run (id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS generator_stage_tenant,
    ADD CONSTRAINT generator_stage_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS generator_stage_key_check,
    ADD CONSTRAINT generator_stage_key_check CHECK (stage ~ '^[a-z][a-z0-9_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS generator_stage_label_check,
    ADD CONSTRAINT generator_stage_label_check CHECK (btrim(label) <> '' AND char_length(label) <= 200);

CREATE TABLE IF NOT EXISTS generator_item (
    run_id         bigint NOT NULL,
    item_id        text NOT NULL,
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    ord            integer NOT NULL,
    step           text NOT NULL,
    kind           text NOT NULL,
    title          text NOT NULL,
    detail         text NOT NULL DEFAULT '',
    example        jsonb,
    -- the typed draft (T8 schema of the step)
    payload        jsonb,
    decision       text NOT NULL DEFAULT 'pending',
    decision_note  text,
    new_object     text,
    proposal       text NOT NULL DEFAULT 'none',
    llm_confidence real NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, item_id)
);
ALTER TABLE generator_item
    DROP CONSTRAINT IF EXISTS generator_item_run,
    ADD CONSTRAINT generator_item_run FOREIGN KEY (run_id) REFERENCES generator_run (id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS generator_item_tenant,
    ADD CONSTRAINT generator_item_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS generator_item_id_check,
    ADD CONSTRAINT generator_item_id_check CHECK (item_id ~ '^[0-9]+-[a-z_]+-[0-9]+$'),
    DROP CONSTRAINT IF EXISTS generator_item_step_check,
    ADD CONSTRAINT generator_item_step_check
        CHECK (step IN ('business_model', 'journey', 'objects', 'operations', 'harmonise', 'use_cases')),
    DROP CONSTRAINT IF EXISTS generator_item_kind_check,
    ADD CONSTRAINT generator_item_kind_check CHECK (kind ~ '^[a-z][a-z0-9_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS generator_item_title_check,
    ADD CONSTRAINT generator_item_title_check CHECK (btrim(title) <> '' AND char_length(title) <= 300),
    DROP CONSTRAINT IF EXISTS generator_item_json_check,
    ADD CONSTRAINT generator_item_json_check
        CHECK ((example IS NULL OR jsonb_typeof(example) = 'object')
               AND (payload IS NULL OR jsonb_typeof(payload) = 'object')),
    DROP CONSTRAINT IF EXISTS generator_item_decision_check,
    ADD CONSTRAINT generator_item_decision_check
        CHECK (decision IN ('pending', 'accepted', 'merged', 'renamed', 'dropped')),
    DROP CONSTRAINT IF EXISTS generator_item_proposal_check,
    ADD CONSTRAINT generator_item_proposal_check CHECK (proposal IN ('none', 'proposed', 'approved', 'rejected')),
    DROP CONSTRAINT IF EXISTS generator_item_confidence_check,
    ADD CONSTRAINT generator_item_confidence_check CHECK (llm_confidence >= 0 AND llm_confidence <= 1);

CREATE TABLE IF NOT EXISTS generator_cost_line (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id         bigint NOT NULL,
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    step           text NOT NULL,
    -- the provider row id, or `internal` (D70)
    provider_id    text NOT NULL,
    model          text NOT NULL,
    tokens         bigint NOT NULL,
    funding        text NOT NULL,
    -- the B4 `usage_event.event_key` this line mirrors
    usage_event_id uuid,
    at             timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE generator_cost_line
    DROP CONSTRAINT IF EXISTS generator_cost_line_run,
    ADD CONSTRAINT generator_cost_line_run FOREIGN KEY (run_id) REFERENCES generator_run (id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS generator_cost_line_tenant,
    ADD CONSTRAINT generator_cost_line_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS generator_cost_line_step_check,
    ADD CONSTRAINT generator_cost_line_step_check
        CHECK (step IN ('business_model', 'journey', 'objects', 'operations', 'harmonise', 'use_cases')),
    DROP CONSTRAINT IF EXISTS generator_cost_line_tokens_check,
    ADD CONSTRAINT generator_cost_line_tokens_check CHECK (tokens >= 0),
    DROP CONSTRAINT IF EXISTS generator_cost_line_funding_check,
    ADD CONSTRAINT generator_cost_line_funding_check
        CHECK (funding IN ('local_gpu', 'tenant_byo', 'platform_external'));
CREATE INDEX IF NOT EXISTS generator_cost_line_run_at ON generator_cost_line (run_id, at, id);

-- A child row always carries its run's tenant (a platform transaction has no tenant GUC to default from).
CREATE OR REPLACE FUNCTION generator_child_tenant() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    NEW.tenant_id := (SELECT r.tenant_id FROM generator_run r WHERE r.id = NEW.run_id);
    IF NEW.tenant_id IS NULL THEN
        RAISE EXCEPTION 'generator run % not found', NEW.run_id USING ERRCODE = '23503';
    END IF;
    RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION generator_child_tenant() FROM PUBLIC;
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['generator_step', 'generator_stage', 'generator_item', 'generator_cost_line'] LOOP
        EXECUTE format('DROP TRIGGER IF EXISTS generator_child_tenant ON %I', t);
        EXECUTE format('CREATE TRIGGER generator_child_tenant BEFORE INSERT OR UPDATE OF run_id, tenant_id ON %I '
                       'FOR EACH ROW EXECUTE FUNCTION generator_child_tenant()', t);
    END LOOP;
END $$;

-- RLS: a tenant's own runs for `agent_app`; every run for `agent_platform` (super_admin routes).
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['generator_run', 'generator_step', 'generator_stage', 'generator_item',
                             'generator_cost_line'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL TO agent_app '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('DROP POLICY IF EXISTS platform_read ON %I', t);
        EXECUTE format('CREATE POLICY platform_read ON %I FOR ALL TO agent_platform USING (true) WITH CHECK (true)', t);
    END LOOP;
END $$;

REVOKE ALL ON generator_run, generator_step, generator_stage, generator_item, generator_cost_line FROM PUBLIC;
REVOKE ALL ON generator_run, generator_step, generator_stage, generator_item, generator_cost_line FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON generator_run, generator_step, generator_stage, generator_item, generator_cost_line TO agent_app;
REVOKE ALL ON generator_run, generator_step, generator_stage, generator_item, generator_cost_line
    FROM agent_platform;
GRANT SELECT, INSERT, UPDATE
    ON generator_run, generator_step, generator_stage, generator_item, generator_cost_line TO agent_platform;
REVOKE ALL ON SEQUENCE generator_run_id_seq, generator_cost_line_id_seq FROM PUBLIC;
GRANT USAGE ON SEQUENCE generator_run_id_seq, generator_cost_line_id_seq TO agent_app, agent_platform;
