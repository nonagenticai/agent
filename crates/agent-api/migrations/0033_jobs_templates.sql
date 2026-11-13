-- 0033 — jobs, the job template tags a job is created from, and template upgrade requests (B1 T6; ports
-- care-bus-agents@96c608c 0006 `jobs` + 0007 `app_current_job`, without the bootstrap seed). Forward-only.
--
-- `jobs (tenant_id, id)` is UNIQUE: every B2–B8 job table references it with a composite foreign key, so a row can
-- never point at another tenant's job. B2 adds the release columns by ALTER in its own range.

CREATE TABLE jobs (
    id               uuid PRIMARY KEY DEFAULT uuidv7(),
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    key              text NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]{1,62}$'),
    label            text NOT NULL CHECK (char_length(btrim(label)) BETWEEN 1 AND 120),
    north_star       text NOT NULL DEFAULT '' CHECK (char_length(north_star) <= 2000),
    settings         jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(settings) = 'object'),
    -- TemplatePin[]: [{layer, version}], the template layers the job is built on.
    pins             jsonb NOT NULL CHECK (jsonb_typeof(pins) = 'array'),
    template_layer   text,
    template_version integer CHECK (template_version IS NULL OR template_version >= 0),
    draft_rev        bigint NOT NULL DEFAULT 0 CHECK (draft_rev >= 0),
    languages        text[] NOT NULL DEFAULT '{}',
    created_at       timestamptz NOT NULL DEFAULT now(),
    created_by       text NOT NULL CHECK (btrim(created_by) <> ''),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT jobs_tenant_key UNIQUE (tenant_id, key),
    CONSTRAINT jobs_tenant_id UNIQUE (tenant_id, id)
);
ALTER TABLE jobs ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON jobs FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON jobs TO agent_app;

-- The job a request acts on (crate::tenancy::JobCtx sets it transaction-locally next to app.current_tenant).
CREATE FUNCTION app_current_job() RETURNS uuid
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT nullif(current_setting('app.current_job', true), '')::uuid $$;

-- The tagged (and draft) job templates (P): platform data, readable by every tenant; written by B6 as
-- agent_platform. `key` is the job key a job created from the tag gets.
CREATE TABLE job_template_tag (
    layer      text NOT NULL CHECK (layer ~ '^[a-z][a-z0-9_]*(/[a-z][a-z0-9_]*)*$'),
    version    integer NOT NULL CHECK (version >= 0),
    key        text NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]{1,62}$'),
    label      text NOT NULL CHECK (char_length(btrim(label)) BETWEEN 1 AND 120),
    industry   text,
    pins       jsonb NOT NULL CHECK (jsonb_typeof(pins) = 'array'),
    status     text NOT NULL CHECK (status IN ('tagged', 'draft')),
    created_at timestamptz NOT NULL DEFAULT now(),
    created_by text NOT NULL CHECK (btrim(created_by) <> ''),
    PRIMARY KEY (layer, version)
);
GRANT SELECT ON job_template_tag TO agent_app, agent_platform;

-- seed:begin (owner D58: start with the UI fixture values, web/src/lib/fixtures/platform.ts jobTemplates())
INSERT INTO job_template_tag (layer, version, key, label, industry, pins, status, created_by) VALUES
    ('job/customer_care_agent', 4, 'customer_care_agent', 'Customer care agent', 'retail',
     '[{"layer":"base","version":3},{"layer":"industry/retail","version":2},{"layer":"industry_dept/retail/customer_service","version":2},{"layer":"job/customer_care_agent","version":4}]',
     'tagged', 'seed:0033'),
    ('job/customer_care_agent', 3, 'customer_care_agent', 'Customer care agent (previous tag)', 'retail',
     '[{"layer":"base","version":3},{"layer":"industry/retail","version":2},{"layer":"industry_dept/retail/customer_service","version":2},{"layer":"job/customer_care_agent","version":3}]',
     'tagged', 'seed:0033'),
    ('industry_dept/utilities/billing', 1, 'billing', 'Billing assistant (utilities billing department)', 'utilities',
     '[{"layer":"base","version":3},{"layer":"industry/utilities","version":1},{"layer":"industry_dept/utilities/billing","version":1}]',
     'tagged', 'seed:0033'),
    ('job/it_service_desk_agent', 0, 'it_service_desk_agent', 'IT service desk agent (generator run #7 in progress)', 'it_services',
     '[{"layer":"base","version":3},{"layer":"job/it_service_desk_agent","version":0}]',
     'draft', 'seed:0033')
ON CONFLICT (layer, version) DO NOTHING;
-- seed:end

-- A tenant admin asking for a job to move to a newer template tag (T). B2 closes it when a release adopts the pins.
CREATE TABLE template_upgrade_request (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id       uuid NOT NULL,
    from_pins    jsonb NOT NULL CHECK (jsonb_typeof(from_pins) = 'array'),
    to_pins      jsonb NOT NULL CHECK (jsonb_typeof(to_pins) = 'array'),
    requested_by text NOT NULL CHECK (btrim(requested_by) <> ''),
    requested_at timestamptz NOT NULL DEFAULT now(),
    status       text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'done', 'withdrawn')),
    CONSTRAINT template_upgrade_request_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE UNIQUE INDEX template_upgrade_request_one_open
    ON template_upgrade_request (tenant_id, job_id) WHERE status = 'open';
ALTER TABLE template_upgrade_request ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON template_upgrade_request FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON template_upgrade_request TO agent_app;
