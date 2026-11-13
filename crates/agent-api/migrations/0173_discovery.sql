-- 0173 — B8 discovery (plan 2026-10-20-b8-journeys-generator-discovery, Data model `0103` → `0173` per § Revision
-- 2026-10-22 R16; T14a import + parsing, T15 shadow half). Forward-only (pre-prod), drift-tolerant (IF NOT EXISTS,
-- DROP … IF EXISTS + ADD, CREATE OR REPLACE FUNCTION). References only B1 tables (`jobs`, `app_current_tenant()`,
-- `app_current_job()`, ≤0033), so it applies whichever of KH / B6b / DR merges first.
--
-- Every table is live data (D69), one job's, under RLS `tenant_isolation` plus a RESTRICTIVE `job_scope` (as 0143).
-- PII: `discovery_row.excerpt` and `discovery_observation.excerpt` hold scrubbed text only (≤1024 bytes; digit runs
-- ≥8 → `[number]`, e-mail → `[email]`); a row with a card number / password / one-time code is never stored
-- (crate::discovery::scrub). The cascade FKs from rows/clusters to their import are 0178's (T15a, F-52).

-- One uploaded ticket/chat export. `id` = `imp-NNNN` from a global identity (never reused after a delete, so a
-- decision keyed on an old import's cluster can never point at a new one).
CREATE TABLE IF NOT EXISTS discovery_import (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    job_id        uuid NOT NULL DEFAULT app_current_job(),
    seq           bigint GENERATED ALWAYS AS IDENTITY,
    id            text GENERATED ALWAYS AS (
                      'imp-' || CASE WHEN seq < 10000 THEN lpad(seq::text, 4, '0') ELSE seq::text END
                  ) STORED,
    file          text NOT NULL,
    rows          integer NOT NULL,
    distinct_rows integer NOT NULL,
    status        text NOT NULL DEFAULT 'classifying',
    done          integer NOT NULL DEFAULT 0,
    coverage      real,
    long_tail     integer NOT NULL DEFAULT 0,
    volumes       jsonb NOT NULL DEFAULT '[]'::jsonb,
    uploaded_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
    uploaded_by   text NOT NULL,
    error         text,
    PRIMARY KEY (tenant_id, job_id, id)
);
ALTER TABLE discovery_import
    DROP CONSTRAINT IF EXISTS discovery_import_job,
    ADD CONSTRAINT discovery_import_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS discovery_import_id_key,
    ADD CONSTRAINT discovery_import_id_key UNIQUE (id),
    DROP CONSTRAINT IF EXISTS discovery_import_status_check,
    ADD CONSTRAINT discovery_import_status_check CHECK (status IN ('classifying', 'classified', 'failed')),
    DROP CONSTRAINT IF EXISTS discovery_import_counts_check,
    ADD CONSTRAINT discovery_import_counts_check
        CHECK (rows >= 0 AND distinct_rows >= 0 AND distinct_rows <= rows AND done >= 0 AND done <= distinct_rows
               AND long_tail >= 0),
    DROP CONSTRAINT IF EXISTS discovery_import_coverage_check,
    ADD CONSTRAINT discovery_import_coverage_check CHECK (coverage IS NULL OR (coverage >= 0 AND coverage <= 1)),
    DROP CONSTRAINT IF EXISTS discovery_import_file_check,
    ADD CONSTRAINT discovery_import_file_check CHECK (char_length(file) BETWEEN 1 AND 255),
    DROP CONSTRAINT IF EXISTS discovery_import_volumes_check,
    ADD CONSTRAINT discovery_import_volumes_check CHECK (jsonb_typeof(volumes) = 'array'),
    DROP CONSTRAINT IF EXISTS discovery_import_uploaded_by_check,
    ADD CONSTRAINT discovery_import_uploaded_by_check CHECK (btrim(uploaded_by) <> ''),
    DROP CONSTRAINT IF EXISTS discovery_import_error_check,
    ADD CONSTRAINT discovery_import_error_check CHECK (error IS NULL OR char_length(error) <= 200);
ALTER TABLE discovery_import ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON discovery_import;
CREATE POLICY tenant_isolation ON discovery_import FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON discovery_import;
CREATE POLICY job_scope ON discovery_import AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- One distinct row of an import (dedupe key = sha256 hex of the lowercased, whitespace-collapsed text). `weight` =
-- how many input rows it stands for (× the count column, F-52). `bucket` NULL = not classified yet; `ord` = first
-- occurrence in the file (the worker's batch order).
CREATE TABLE IF NOT EXISTS discovery_row (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    import_id  text NOT NULL,
    text_hash  text NOT NULL,
    ord        integer NOT NULL,
    excerpt    text,
    weight     integer NOT NULL,
    stage      text,
    activity   text,
    bucket     text,
    confidence real,
    cluster_id text,
    PRIMARY KEY (tenant_id, job_id, import_id, text_hash)
);
ALTER TABLE discovery_row
    DROP CONSTRAINT IF EXISTS discovery_row_import,
    ADD CONSTRAINT discovery_row_import FOREIGN KEY (tenant_id, job_id, import_id)
        REFERENCES discovery_import (tenant_id, job_id, id),
    DROP CONSTRAINT IF EXISTS discovery_row_hash_check,
    ADD CONSTRAINT discovery_row_hash_check CHECK (text_hash ~ '^[0-9a-f]{64}$'),
    DROP CONSTRAINT IF EXISTS discovery_row_excerpt_check,
    ADD CONSTRAINT discovery_row_excerpt_check CHECK (excerpt IS NULL OR octet_length(excerpt) <= 1024),
    DROP CONSTRAINT IF EXISTS discovery_row_weight_check,
    ADD CONSTRAINT discovery_row_weight_check CHECK (weight >= 1),
    DROP CONSTRAINT IF EXISTS discovery_row_bucket_check,
    ADD CONSTRAINT discovery_row_bucket_check
        CHECK (bucket IS NULL OR bucket IN ('covered', 'hidden', 'uncovered', 'unclear', 'out_of_scope')),
    DROP CONSTRAINT IF EXISTS discovery_row_confidence_check,
    ADD CONSTRAINT discovery_row_confidence_check CHECK (confidence IS NULL OR (confidence >= 0 AND confidence <= 1));
CREATE INDEX IF NOT EXISTS discovery_row_pending ON discovery_row (tenant_id, job_id, import_id, ord)
    WHERE bucket IS NULL;
ALTER TABLE discovery_row ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON discovery_row;
CREATE POLICY tenant_isolation ON discovery_row FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON discovery_row;
CREATE POLICY job_scope ON discovery_row AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- A cluster of an import (T14b): hidden rows per hidden activity (`enable_activity`) or uncovered rows grouped by
-- one LLM call (`tenant_extension`). `examples` = scrubbed excerpts.
CREATE TABLE IF NOT EXISTS discovery_cluster (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant(),
    job_id         uuid NOT NULL DEFAULT app_current_job(),
    import_id      text NOT NULL,
    id             text NOT NULL,
    label          text NOT NULL,
    tickets        integer NOT NULL DEFAULT 0,
    examples       jsonb NOT NULL DEFAULT '[]'::jsonb,
    suggestion     jsonb NOT NULL,
    drafted_tokens integer NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, job_id, import_id, id)
);
ALTER TABLE discovery_cluster
    DROP CONSTRAINT IF EXISTS discovery_cluster_import,
    ADD CONSTRAINT discovery_cluster_import FOREIGN KEY (tenant_id, job_id, import_id)
        REFERENCES discovery_import (tenant_id, job_id, id),
    DROP CONSTRAINT IF EXISTS discovery_cluster_counts_check,
    ADD CONSTRAINT discovery_cluster_counts_check CHECK (tickets >= 0 AND drafted_tokens >= 0),
    DROP CONSTRAINT IF EXISTS discovery_cluster_examples_check,
    ADD CONSTRAINT discovery_cluster_examples_check CHECK (jsonb_typeof(examples) = 'array'),
    DROP CONSTRAINT IF EXISTS discovery_cluster_suggestion_check,
    ADD CONSTRAINT discovery_cluster_suggestion_check
        CHECK (jsonb_typeof(suggestion) = 'object'
               AND suggestion->>'kind' IN ('enable_activity', 'tenant_extension'));
ALTER TABLE discovery_cluster ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON discovery_cluster;
CREATE POLICY tenant_isolation ON discovery_cluster FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON discovery_cluster;
CREATE POLICY job_scope ON discovery_cluster AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- A decision on a discovery proposal (T15 accept/reject). Survives an import delete (its draft keys are B2's).
CREATE TABLE IF NOT EXISTS discovery_decision (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    proposal_id text NOT NULL,
    state       text NOT NULL,
    at          timestamptz NOT NULL DEFAULT clock_timestamp(),
    by          text NOT NULL,
    reason      text,
    keys        text[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (tenant_id, job_id, proposal_id)
);
ALTER TABLE discovery_decision
    DROP CONSTRAINT IF EXISTS discovery_decision_job,
    ADD CONSTRAINT discovery_decision_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS discovery_decision_state_check,
    ADD CONSTRAINT discovery_decision_state_check CHECK (state IN ('accepted', 'rejected')),
    DROP CONSTRAINT IF EXISTS discovery_decision_reason_check,
    ADD CONSTRAINT discovery_decision_reason_check
        CHECK (state <> 'rejected' OR (reason IS NOT NULL AND btrim(reason) <> '')),
    DROP CONSTRAINT IF EXISTS discovery_decision_by_check,
    ADD CONSTRAINT discovery_decision_by_check CHECK (btrim(by) <> '');
ALTER TABLE discovery_decision ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON discovery_decision;
CREATE POLICY tenant_isolation ON discovery_decision FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON discovery_decision;
CREATE POLICY job_scope ON discovery_decision AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- Shadow mode of a job (D63: a runtime flag the B4 router consults; it writes no route row).
CREATE TABLE IF NOT EXISTS shadow_mode (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    active     boolean NOT NULL,
    env        text NOT NULL,
    started_at timestamptz,
    started_by text,
    stopped_at timestamptz,
    PRIMARY KEY (tenant_id, job_id)
);
ALTER TABLE shadow_mode
    DROP CONSTRAINT IF EXISTS shadow_mode_job,
    ADD CONSTRAINT shadow_mode_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS shadow_mode_env_check,
    ADD CONSTRAINT shadow_mode_env_check CHECK (env IN ('test', 'prod')),
    DROP CONSTRAINT IF EXISTS shadow_mode_started_check,
    ADD CONSTRAINT shadow_mode_started_check
        CHECK (NOT active OR (started_at IS NOT NULL AND started_by IS NOT NULL AND btrim(started_by) <> ''));
ALTER TABLE shadow_mode ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON shadow_mode;
CREATE POLICY tenant_isolation ON shadow_mode FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON shadow_mode;
CREATE POLICY job_scope ON shadow_mode AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- One shadow-mode conversation turn as the runtime saw it (B4's post-turn hook, `discovery::shadow::observe`): the
-- stage/activity the turn's GA already computed, no model call. Purged after 30 days.
CREATE TABLE IF NOT EXISTS discovery_observation (
    id        bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL DEFAULT app_current_job(),
    case_id   text NOT NULL,
    env       text NOT NULL,
    at        timestamptz NOT NULL DEFAULT clock_timestamp(),
    stage     text,
    activity  text,
    labelled  boolean NOT NULL DEFAULT false,
    excerpt   text
);
ALTER TABLE discovery_observation
    DROP CONSTRAINT IF EXISTS discovery_observation_job,
    ADD CONSTRAINT discovery_observation_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS discovery_observation_env_check,
    ADD CONSTRAINT discovery_observation_env_check CHECK (env IN ('test', 'prod')),
    DROP CONSTRAINT IF EXISTS discovery_observation_case_check,
    ADD CONSTRAINT discovery_observation_case_check CHECK (btrim(case_id) <> ''),
    DROP CONSTRAINT IF EXISTS discovery_observation_excerpt_check,
    ADD CONSTRAINT discovery_observation_excerpt_check CHECK (excerpt IS NULL OR octet_length(excerpt) <= 1024);
CREATE INDEX IF NOT EXISTS discovery_observation_job_at ON discovery_observation (tenant_id, job_id, at);
CREATE INDEX IF NOT EXISTS discovery_observation_at ON discovery_observation (at);
ALTER TABLE discovery_observation ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON discovery_observation;
CREATE POLICY tenant_isolation ON discovery_observation FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON discovery_observation;
CREATE POLICY job_scope ON discovery_observation AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- Boot purge (no model call, DELETE only): observations older than `p_older`, every tenant (owner rights, no FORCE
-- ROW LEVEL SECURITY). Returns the number deleted.
CREATE OR REPLACE FUNCTION discovery_purge_observations(p_older interval)
    RETURNS integer
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        n integer;
    BEGIN
        IF p_older IS NULL OR p_older < interval '1 day' THEN
            RAISE EXCEPTION 'invalid_purge_window';
        END IF;
        DELETE FROM discovery_observation WHERE at < now() - p_older;
        GET DIAGNOSTICS n = ROW_COUNT;
        RETURN n;
    END
    $$;

-- Boot resume: the imports still `classifying`, every tenant (ids only; the worker reads each under its tenant).
CREATE OR REPLACE FUNCTION discovery_classifying_imports()
    RETURNS TABLE (tenant_id uuid, job_id uuid, id text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT i.tenant_id, i.job_id, i.id FROM discovery_import i WHERE i.status = 'classifying' ORDER BY i.seq
    $$;

-- Grants. agent_app (routes + the import worker): read/write on every table. agent_runtime (B4 runtime processes,
-- the shadow hook only): SELECT `shadow_mode` (`shadow::active`), INSERT `discovery_observation` (`shadow::observe`).
REVOKE ALL ON discovery_import, discovery_row, discovery_cluster, discovery_decision, shadow_mode,
              discovery_observation
    FROM PUBLIC;
REVOKE ALL ON SEQUENCE discovery_import_seq_seq, discovery_observation_id_seq FROM PUBLIC;
REVOKE ALL ON discovery_import, discovery_row, discovery_cluster, discovery_decision, shadow_mode,
              discovery_observation
    FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON discovery_import, discovery_row, discovery_cluster, discovery_decision, shadow_mode, discovery_observation
    TO agent_app;
REVOKE ALL ON SEQUENCE discovery_import_seq_seq, discovery_observation_id_seq FROM agent_app;
GRANT USAGE ON SEQUENCE discovery_import_seq_seq, discovery_observation_id_seq TO agent_app;
REVOKE ALL ON discovery_import, discovery_row, discovery_cluster, discovery_decision, shadow_mode,
              discovery_observation
    FROM agent_runtime;
GRANT SELECT ON shadow_mode TO agent_runtime;
GRANT INSERT ON discovery_observation TO agent_runtime;
REVOKE ALL ON SEQUENCE discovery_import_seq_seq, discovery_observation_id_seq FROM agent_runtime;
GRANT USAGE ON SEQUENCE discovery_observation_id_seq TO agent_runtime;

REVOKE ALL ON FUNCTION discovery_purge_observations(interval) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION discovery_purge_observations(interval) TO agent_app;
REVOKE ALL ON FUNCTION discovery_classifying_imports() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION discovery_classifying_imports() TO agent_app;
