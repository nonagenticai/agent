-- 0046 — release conflicts: a tenant refinement whose base moved underneath it (B2 T2; plan B2 §3 "0046", a port
-- of care-bus-agents 0009 `release_conflicts` plus the J16 columns `cause`, `raised_by_cut` and `decision`).
--
-- A conflict is `open` until the release carrying its decision's draft is applied (`resolved`, `resolution` = the
-- choice) or another apply replaces it (`superseded`). D15: `raised_by_cut` = the approved release whose template
-- upgrade raised it at the cut, before apply.
--
-- Drift-tolerant as 0044 (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT). Forward-only.

CREATE TABLE IF NOT EXISTS release_conflict (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant(),
    job_id           uuid NOT NULL,
    release_id       text NOT NULL,
    kind             text NOT NULL,
    object           text NOT NULL,
    key              text NOT NULL,
    tenant_row       text NOT NULL,
    detail           text NOT NULL,
    base_hash_before text,
    base_hash_after  text,
    base_before      jsonb,
    base_after       jsonb,
    -- ConflictCause
    cause            jsonb,
    raised_by_cut    text,
    detected_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    status           text NOT NULL DEFAULT 'open',
    resolution       text,
    resolved_by      text,
    resolved_at      timestamptz,
    -- ConflictDecision: {choice, draft_key, draft_id, by, at, published_in}
    decision         jsonb
);
ALTER TABLE release_conflict
    DROP CONSTRAINT IF EXISTS release_conflict_job,
    ADD CONSTRAINT release_conflict_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS release_conflict_release,
    ADD CONSTRAINT release_conflict_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS release_conflict_raised_by_cut,
    ADD CONSTRAINT release_conflict_raised_by_cut FOREIGN KEY (tenant_id, job_id, raised_by_cut)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS release_conflict_kind_check,
    ADD CONSTRAINT release_conflict_kind_check
        CHECK (kind IN ('base_changed', 'base_removed', 'base_added', 'parent_missing', 'base_shadowed',
                        'job_setting')),
    DROP CONSTRAINT IF EXISTS release_conflict_object_check,
    ADD CONSTRAINT release_conflict_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                          'job_setting')),
    DROP CONSTRAINT IF EXISTS release_conflict_tenant_row_check,
    ADD CONSTRAINT release_conflict_tenant_row_check CHECK (tenant_row IN ('override', 'extension', 'tombstone')),
    DROP CONSTRAINT IF EXISTS release_conflict_status_check,
    ADD CONSTRAINT release_conflict_status_check CHECK (status IN ('open', 'superseded', 'resolved')),
    DROP CONSTRAINT IF EXISTS release_conflict_resolution_check,
    ADD CONSTRAINT release_conflict_resolution_check CHECK (resolution IN ('reverted', 'rebased')),
    DROP CONSTRAINT IF EXISTS release_conflict_resolved_check,
    ADD CONSTRAINT release_conflict_resolved_check CHECK ((status = 'resolved') = (resolution IS NOT NULL)),
    DROP CONSTRAINT IF EXISTS release_conflict_json_check,
    ADD CONSTRAINT release_conflict_json_check
        CHECK ((cause IS NULL OR jsonb_typeof(cause) = 'object')
               AND (decision IS NULL OR jsonb_typeof(decision) = 'object'));
DROP INDEX IF EXISTS release_conflict_open;
CREATE INDEX release_conflict_open ON release_conflict (tenant_id, job_id) WHERE status = 'open';
ALTER TABLE release_conflict ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON release_conflict;
CREATE POLICY tenant_isolation ON release_conflict FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON release_conflict FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON release_conflict TO agent_app;
GRANT USAGE ON SEQUENCE release_conflict_id_seq TO agent_app;
