-- 0094 — job operation routes and tenant field-mapping overrides (B7 T12/T13; plan B7 "Data model" 0094).
-- Forward-only (pre-prod).
--
-- Both tables hold DRAFT working rows (D54/D62/D69): the test env runs them; Production resolves through the job's
-- active release snapshot (B2 `design::read` with `DesignOf::Live`), never through these rows.
--
-- 1. `operation_route`: per job and env, where an operation (object, verb, variant) goes. Ledger kind `route` (per
--    job). `target_kind`: `instance` (a connector_instance of the tenant), `builtin`, `agent_cases`, `deep_link` (an
--    https URL handed to the customer) or `not_offered`. The FK to `connector_instance` is RESTRICT: a system that a
--    route serves cannot be deleted from under it.
-- 2. `field_mapping_override`: a tenant JSONPath override of one field of one op on one instance. Ledger kind
--    `mapping_override` (tenant-wide: every job's draft). `default_path` is the template path at save time.
-- Both: RLS `tenant_isolation`, tenant_id defaults to `app_current_tenant()`, grants to agent_app. They reference
-- only `tenants`, `jobs` (0033), `connector_instance` (0093) and `app_current_tenant()` (0001), so the migration-order
-- cut of tests/migration_order.rs (everything below 0041 plus everything from 0091) still holds.

CREATE TABLE operation_route (
    tenant_id   uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id      uuid        NOT NULL,
    object      text        NOT NULL CHECK (object ~ '^[a-z_]+$'),
    verb        text        NOT NULL CHECK (verb ~ '^[a-z_]+$'),
    variant     text        NOT NULL DEFAULT '*' CHECK (btrim(variant) <> ''),
    env         text        NOT NULL CHECK (env IN ('prod', 'test')),
    target_kind text        NOT NULL CHECK (target_kind IN
                    ('instance', 'builtin', 'agent_cases', 'deep_link', 'not_offered')),
    instance_id text,
    detail      text        NOT NULL DEFAULT '',
    url         text,
    updated_by  text        NOT NULL CHECK (btrim(updated_by) <> ''),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id, object, verb, variant, env),
    CONSTRAINT operation_route_job_fk FOREIGN KEY (tenant_id, job_id)
        REFERENCES jobs (tenant_id, id) ON DELETE CASCADE,
    CONSTRAINT operation_route_instance_fk FOREIGN KEY (tenant_id, instance_id)
        REFERENCES connector_instance (tenant_id, id) ON DELETE RESTRICT,
    -- Target shape: an instance route names its instance; a deep link carries an https URL; nothing else names
    -- an instance.
    CONSTRAINT operation_route_target_shape CHECK (
        CASE target_kind
            WHEN 'instance'  THEN instance_id IS NOT NULL
            WHEN 'deep_link' THEN instance_id IS NULL AND url IS NOT NULL AND url LIKE 'https://%'
            ELSE instance_id IS NULL
        END)
);
-- Usage lookups: which job routes serve an instance.
CREATE INDEX operation_route_instance ON operation_route (tenant_id, instance_id);
ALTER TABLE operation_route ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON operation_route FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON operation_route TO agent_app;

CREATE TABLE field_mapping_override (
    tenant_id    uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    instance_id  text        NOT NULL,
    op           text        NOT NULL CHECK (btrim(op) <> ''),
    field        text        NOT NULL CHECK (btrim(field) <> ''),
    default_path text        NOT NULL,
    path         text        NOT NULL CHECK (path LIKE '$%' AND char_length(path) <= 512),
    updated_by   text        NOT NULL CHECK (btrim(updated_by) <> ''),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, instance_id, op, field),
    CONSTRAINT field_mapping_override_instance_fk FOREIGN KEY (tenant_id, instance_id)
        REFERENCES connector_instance (tenant_id, id) ON DELETE RESTRICT
);
ALTER TABLE field_mapping_override ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON field_mapping_override FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON field_mapping_override TO agent_app;

-- 3. B2's draft ledger restores tenant templates through their design target (D69; crate::connectors::places):
--    discarding the creation of a template version deletes that DRAFT version. The 0092 guard still refuses deleting
--    a published (active / retired) version, and the target refuses a version a system pins.
GRANT DELETE ON tenant_connector_template TO agent_app;
