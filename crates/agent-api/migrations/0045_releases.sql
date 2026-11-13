-- 0045 — releases: the cut (compiled bundle + immutable design snapshot), what each cut bundled, what each apply
-- published, and the rollback/reapply log (B2 T2; plan B2 §3 "0045", a port of care-bus-agents 0009 `job_releases`,
-- reshaped for D62, plus fixtures/release.ts `ReleaseExtra` / `PublishedDraftRow` / the rollback log).
--
-- A release row is immutable except for its lifecycle: `state`, `released_*` (set once), `applied_*`, `readiness`
-- and an append-only `snapshot_kinds` (coverage backfill, §2.5). Trigger `job_release_immutable` enforces that for
-- the owner too; the runtime role's column grants say the same. `release_snapshot` rows never change.
--
-- Drift-tolerant as 0044 (IF NOT EXISTS, DROP … IF EXISTS + ADD, CREATE OR REPLACE, REVOKE ALL + GRANT).
-- Forward-only.

CREATE TABLE IF NOT EXISTS job_release (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL,
    release_id      text NOT NULL,
    -- release identity: bundle_hash + the design snapshot (§5)
    hash            text NOT NULL,
    bundle_hash     text NOT NULL,
    bundle          jsonb NOT NULL,
    -- ResolvedSelection, profile values inlined
    selection       jsonb NOT NULL,
    pins            jsonb NOT NULL,
    -- {overlay_revs, seed_versions}
    provenance      jsonb NOT NULL,
    requires        jsonb NOT NULL,
    causes          jsonb NOT NULL,
    state           text NOT NULL,
    approved_by     text NOT NULL,
    approved_at     timestamptz NOT NULL,
    released_by     text,
    released_at     timestamptz,
    applied_by      text,
    last_applied_at timestamptz,
    -- ReadinessResult at the last apply
    readiness       jsonb,
    snapshot_kinds  text[] NOT NULL,
    rollbacks_seen  integer NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, job_id, release_id)
);
ALTER TABLE job_release
    DROP CONSTRAINT IF EXISTS job_release_job,
    ADD CONSTRAINT job_release_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS job_release_id_check,
    ADD CONSTRAINT job_release_id_check CHECK (release_id ~ '^cfg-\d{4}-\d{2}-\d{2}\.[1-9][0-9]{0,3}$'),
    DROP CONSTRAINT IF EXISTS job_release_hash_check,
    ADD CONSTRAINT job_release_hash_check
        CHECK (hash ~ '^sha256:[0-9a-f]{64}$' AND bundle_hash ~ '^sha256:[0-9a-f]{64}$'),
    DROP CONSTRAINT IF EXISTS job_release_json_check,
    ADD CONSTRAINT job_release_json_check
        CHECK (jsonb_typeof(bundle) = 'object' AND jsonb_typeof(selection) = 'object'
               AND jsonb_typeof(pins) = 'array' AND jsonb_typeof(provenance) = 'object'
               AND (readiness IS NULL OR jsonb_typeof(readiness) = 'object')),
    DROP CONSTRAINT IF EXISTS job_release_state_check,
    ADD CONSTRAINT job_release_state_check CHECK (state IN ('approved', 'current', 'applied_earlier')),
    DROP CONSTRAINT IF EXISTS job_release_approved_by_check,
    ADD CONSTRAINT job_release_approved_by_check CHECK (btrim(approved_by) <> ''),
    DROP CONSTRAINT IF EXISTS job_release_released_check,
    ADD CONSTRAINT job_release_released_check CHECK ((released_by IS NULL) = (released_at IS NULL)),
    DROP CONSTRAINT IF EXISTS job_release_state_released_check,
    ADD CONSTRAINT job_release_state_released_check CHECK (state = 'approved' OR released_at IS NOT NULL),
    DROP CONSTRAINT IF EXISTS job_release_applied_check,
    ADD CONSTRAINT job_release_applied_check CHECK ((applied_by IS NULL) = (last_applied_at IS NULL)),
    DROP CONSTRAINT IF EXISTS job_release_rollbacks_seen_check,
    ADD CONSTRAINT job_release_rollbacks_seen_check CHECK (rollbacks_seen >= 0);
DROP INDEX IF EXISTS job_release_one_current;
CREATE UNIQUE INDEX job_release_one_current ON job_release (tenant_id, job_id) WHERE state = 'current';
ALTER TABLE job_release ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_release;
CREATE POLICY tenant_isolation ON job_release FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON job_release FROM agent_app;
GRANT SELECT, INSERT ON job_release TO agent_app;
GRANT UPDATE (state, released_by, released_at, applied_by, last_applied_at, readiness, snapshot_kinds)
    ON job_release TO agent_app;

-- Only the lifecycle columns change; `released_*` once set never change; `snapshot_kinds` only grows. Compared as
-- the row's JSON text minus the mutable columns (as 0041 `layer_row_immutable`), so a column added later is covered
-- and `1` → `1.0` inside the bundle counts as a change.
CREATE OR REPLACE FUNCTION job_release_immutable() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
DECLARE
    mutable CONSTANT text[] := ARRAY['state', 'released_by', 'released_at', 'applied_by', 'last_applied_at',
                                     'readiness', 'snapshot_kinds'];
    why text;
BEGIN
    IF (to_jsonb(NEW) - mutable)::text <> (to_jsonb(OLD) - mutable)::text THEN
        why := 'only state, released_*, applied_*, readiness and snapshot_kinds may change';
    ELSIF OLD.released_at IS NOT NULL
          AND (NEW.released_by IS DISTINCT FROM OLD.released_by
               OR NEW.released_at IS DISTINCT FROM OLD.released_at) THEN
        why := 'released_by and released_at are set once';
    ELSIF NOT (NEW.snapshot_kinds @> OLD.snapshot_kinds) THEN
        why := 'snapshot_kinds is append-only';
    ELSE
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'job_release_immutable'
        USING ERRCODE = '23514', CONSTRAINT = 'job_release_immutable',
              DETAIL = format('job_release %s: %s', OLD.release_id, why);
END $$;
DROP TRIGGER IF EXISTS job_release_immutable ON job_release;
CREATE TRIGGER job_release_immutable BEFORE UPDATE ON job_release
    FOR EACH ROW EXECUTE FUNCTION job_release_immutable();

-- The live release of a job (MATCH SIMPLE: NULL = never released, live = its baseline).
ALTER TABLE jobs
    DROP CONSTRAINT IF EXISTS jobs_current_release_fkey,
    ADD CONSTRAINT jobs_current_release_fkey FOREIGN KEY (tenant_id, id, current_release_id)
        REFERENCES job_release (tenant_id, job_id, release_id);

-- The design the cut saw: one row per place of every snapshot kind in `job_release.snapshot_kinds`.
CREATE TABLE IF NOT EXISTS release_snapshot (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL,
    release_id text NOT NULL,
    key        text NOT NULL,
    kind       text NOT NULL,
    ref        jsonb NOT NULL,
    snap       jsonb,
    PRIMARY KEY (tenant_id, job_id, release_id, key)
);
ALTER TABLE release_snapshot
    DROP CONSTRAINT IF EXISTS release_snapshot_release,
    ADD CONSTRAINT release_snapshot_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS release_snapshot_key_check,
    ADD CONSTRAINT release_snapshot_key_check CHECK (char_length(key) BETWEEN 1 AND 512),
    DROP CONSTRAINT IF EXISTS release_snapshot_kind_check,
    ADD CONSTRAINT release_snapshot_kind_check CHECK (kind ~ '^[a-z][a-z_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS release_snapshot_ref_check,
    ADD CONSTRAINT release_snapshot_ref_check CHECK (jsonb_typeof(ref) = 'object'),
    DROP CONSTRAINT IF EXISTS release_snapshot_snap_check,
    ADD CONSTRAINT release_snapshot_snap_check CHECK (design_snap_ok(snap));
ALTER TABLE release_snapshot ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON release_snapshot;
CREATE POLICY tenant_isolation ON release_snapshot FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON release_snapshot FROM agent_app;
GRANT SELECT, INSERT ON release_snapshot TO agent_app;
DROP TRIGGER IF EXISTS release_snapshot_immutable ON release_snapshot;
CREATE TRIGGER release_snapshot_immutable BEFORE UPDATE ON release_snapshot
    FOR EACH ROW EXECUTE FUNCTION release_snapshot_immutable();

-- The ledger rows a cut bundled (fixture `ReleaseExtra.drafts`). `ledger_id` has no foreign key: apply deletes the
-- ledger rows it published.
CREATE TABLE IF NOT EXISTS release_bundled_draft (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL,
    release_id text NOT NULL,
    key        text NOT NULL,
    ledger_id  bigint NOT NULL,
    kind       text NOT NULL,
    ref        jsonb NOT NULL,
    area       text NOT NULL,
    page       text NOT NULL,
    label      text NOT NULL,
    after      jsonb,
    at         timestamptz NOT NULL,
    by_sub     text NOT NULL,
    PRIMARY KEY (tenant_id, job_id, release_id, key)
);
ALTER TABLE release_bundled_draft
    DROP CONSTRAINT IF EXISTS release_bundled_draft_release,
    ADD CONSTRAINT release_bundled_draft_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS release_bundled_draft_kind_check,
    ADD CONSTRAINT release_bundled_draft_kind_check CHECK (kind ~ '^[a-z][a-z_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS release_bundled_draft_ref_check,
    ADD CONSTRAINT release_bundled_draft_ref_check CHECK (jsonb_typeof(ref) = 'object'),
    DROP CONSTRAINT IF EXISTS release_bundled_draft_snap_check,
    ADD CONSTRAINT release_bundled_draft_snap_check CHECK (design_snap_ok(after));
ALTER TABLE release_bundled_draft ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON release_bundled_draft;
CREATE POLICY tenant_isolation ON release_bundled_draft FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON release_bundled_draft FROM agent_app;
GRANT SELECT, INSERT ON release_bundled_draft TO agent_app;

-- A change an apply published (fixture `PublishedDraftRow`). API id `pub-<id>`. Only the rollback marks change:
-- `rolled_back` = {to, at, by, rollback}, `restored_to_draft` = {at, by}.
CREATE TABLE IF NOT EXISTS published_change (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id         uuid NOT NULL DEFAULT app_current_tenant(),
    job_id            uuid NOT NULL,
    key               text NOT NULL,
    kind              text NOT NULL,
    ref               jsonb NOT NULL,
    area              text NOT NULL,
    page              text NOT NULL,
    label             text NOT NULL,
    before            jsonb,
    after             jsonb,
    at                timestamptz NOT NULL,
    by_sub            text NOT NULL,
    published_in      text NOT NULL,
    published_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    published_by      text NOT NULL,
    rolled_back       jsonb,
    restored_to_draft jsonb
);
ALTER TABLE published_change
    DROP CONSTRAINT IF EXISTS published_change_job,
    ADD CONSTRAINT published_change_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS published_change_release,
    ADD CONSTRAINT published_change_release FOREIGN KEY (tenant_id, job_id, published_in)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS published_change_kind_check,
    ADD CONSTRAINT published_change_kind_check CHECK (kind ~ '^[a-z][a-z_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS published_change_ref_check,
    ADD CONSTRAINT published_change_ref_check CHECK (jsonb_typeof(ref) = 'object'),
    DROP CONSTRAINT IF EXISTS published_change_snap_check,
    ADD CONSTRAINT published_change_snap_check CHECK (design_snap_ok(before) AND design_snap_ok(after)),
    DROP CONSTRAINT IF EXISTS published_change_marks_check,
    ADD CONSTRAINT published_change_marks_check
        CHECK ((rolled_back IS NULL OR jsonb_typeof(rolled_back) = 'object')
               AND (restored_to_draft IS NULL OR jsonb_typeof(restored_to_draft) = 'object'));
DROP INDEX IF EXISTS published_change_job_idx;
CREATE INDEX published_change_job_idx ON published_change (tenant_id, job_id, id);
ALTER TABLE published_change ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON published_change;
CREATE POLICY tenant_isolation ON published_change FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON published_change FROM agent_app;
GRANT SELECT, INSERT ON published_change TO agent_app;
GRANT UPDATE (rolled_back, restored_to_draft) ON published_change TO agent_app;
GRANT USAGE ON SEQUENCE published_change_id_seq TO agent_app;

-- The rollback / reapply log (fixture rollbacks). API id `rb-<id>`. `from_release` NULL = nothing was live.
CREATE TABLE IF NOT EXISTS release_rollback (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL,
    at           timestamptz NOT NULL DEFAULT clock_timestamp(),
    by_sub       text NOT NULL,
    direction    text NOT NULL,
    from_release text,
    to_release   text NOT NULL,
    undone       bigint[] NOT NULL DEFAULT '{}',
    reapplied    bigint[] NOT NULL DEFAULT '{}',
    draft_moved  text[] NOT NULL DEFAULT '{}'
);
ALTER TABLE release_rollback
    DROP CONSTRAINT IF EXISTS release_rollback_job,
    ADD CONSTRAINT release_rollback_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS release_rollback_from,
    ADD CONSTRAINT release_rollback_from FOREIGN KEY (tenant_id, job_id, from_release)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS release_rollback_to,
    ADD CONSTRAINT release_rollback_to FOREIGN KEY (tenant_id, job_id, to_release)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS release_rollback_direction_check,
    ADD CONSTRAINT release_rollback_direction_check CHECK (direction IN ('rollback', 'reapply', 'apply'));
DROP INDEX IF EXISTS release_rollback_job_idx;
CREATE INDEX release_rollback_job_idx ON release_rollback (tenant_id, job_id, id);
ALTER TABLE release_rollback ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON release_rollback;
CREATE POLICY tenant_isolation ON release_rollback FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON release_rollback FROM agent_app;
GRANT SELECT, INSERT ON release_rollback TO agent_app;
GRANT USAGE ON SEQUENCE release_rollback_id_seq TO agent_app;
