-- 0044 — the design ledger: `jobs` release columns, the draft ledger (change history) and the stored baseline of a
-- job never released (B2 T2; plan B2 §3 "0044", a port of fixtures/ledger.ts `DraftRow` + `liveSnapshot`).
--
-- A design place is (kind, ref) with a stable string key (`gate:GM:request`, `rule:RET-01`, …). A snap column holds
-- jsonb `{"v": value}` or SQL NULL (nothing at the place), exactly `design::snap_to_json`; `design_snap_ok` checks it.
-- The draft ledger is history only (who changed which place, when, and the value the first save replaced); it is
-- never read back to rebuild live. Live = the snapshot of `jobs.current_release_id` (0045), or, for a job never
-- released, its `release_baseline` rows.
--
-- Drift-tolerant: every statement re-runs on a database where it already (partly) applied — IF NOT EXISTS, one
-- canonical definition per constraint (DROP … IF EXISTS + ADD), CREATE OR REPLACE FUNCTION, DROP TRIGGER/POLICY IF
-- EXISTS + CREATE, DROP INDEX IF EXISTS + CREATE INDEX, REVOKE ALL + GRANT. It does not re-add a column missing
-- from an existing table (only the `jobs` columns use ADD COLUMN IF NOT EXISTS): a later ADD CONSTRAINT on that
-- column then fails loudly. Forward-only.

-- A stored snap: SQL NULL, or an object with exactly the key `v`.
CREATE OR REPLACE FUNCTION design_snap_ok(s jsonb) RETURNS boolean
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT s IS NULL OR (jsonb_typeof(s) = 'object' AND s ? 'v' AND (s - 'v') = '{}'::jsonb) $$;

-- jobs: 0033 owns pins, draft_rev and languages; B2 pins their invariants and adds the release pointer.
-- `current_release_id` gets its foreign key in 0045 (job_release does not exist yet). `baseline_kinds` = the snapshot
-- kinds the stored baseline covers; NULL = baseline not taken.
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS current_release_id text;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS baseline_kinds text[];
UPDATE jobs SET languages = '{en}' WHERE cardinality(languages) = 0;
ALTER TABLE jobs ALTER COLUMN languages SET DEFAULT '{en}';
ALTER TABLE jobs
    DROP CONSTRAINT IF EXISTS jobs_draft_rev_check,
    ADD CONSTRAINT jobs_draft_rev_check CHECK (draft_rev >= 0),
    DROP CONSTRAINT IF EXISTS jobs_languages_check,
    ADD CONSTRAINT jobs_languages_check CHECK (cardinality(languages) >= 1);

-- The draft ledger: one row per changed place (fixture DraftRow). API id `d-<id>`.
CREATE TABLE IF NOT EXISTS draft_ledger (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    job_id     uuid NOT NULL,
    key        text NOT NULL,
    kind       text NOT NULL,
    ref        jsonb NOT NULL,
    area       text NOT NULL,
    page       text NOT NULL,
    label      text NOT NULL,
    before     jsonb,
    after      jsonb,
    at         timestamptz NOT NULL DEFAULT clock_timestamp(),
    by_sub     text NOT NULL,
    -- a rollback moved the live value under this unpublished change: {release, at, was}
    live_moved jsonb
);
ALTER TABLE draft_ledger
    DROP CONSTRAINT IF EXISTS draft_ledger_job,
    ADD CONSTRAINT draft_ledger_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS draft_ledger_place,
    ADD CONSTRAINT draft_ledger_place UNIQUE (tenant_id, job_id, key),
    DROP CONSTRAINT IF EXISTS draft_ledger_key_check,
    ADD CONSTRAINT draft_ledger_key_check CHECK (char_length(key) BETWEEN 1 AND 512),
    DROP CONSTRAINT IF EXISTS draft_ledger_kind_check,
    ADD CONSTRAINT draft_ledger_kind_check CHECK (kind ~ '^[a-z][a-z_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS draft_ledger_ref_check,
    ADD CONSTRAINT draft_ledger_ref_check CHECK (jsonb_typeof(ref) = 'object'),
    DROP CONSTRAINT IF EXISTS draft_ledger_area_check,
    ADD CONSTRAINT draft_ledger_area_check CHECK (btrim(area) <> ''),
    DROP CONSTRAINT IF EXISTS draft_ledger_page_check,
    ADD CONSTRAINT draft_ledger_page_check CHECK (page ~ '^[A-Z][0-9A-Z]{1,5}$'),
    DROP CONSTRAINT IF EXISTS draft_ledger_label_check,
    ADD CONSTRAINT draft_ledger_label_check CHECK (char_length(label) <= 500),
    DROP CONSTRAINT IF EXISTS draft_ledger_snap_check,
    ADD CONSTRAINT draft_ledger_snap_check CHECK (design_snap_ok(before) AND design_snap_ok(after)),
    DROP CONSTRAINT IF EXISTS draft_ledger_by_check,
    ADD CONSTRAINT draft_ledger_by_check CHECK (btrim(by_sub) <> ''),
    DROP CONSTRAINT IF EXISTS draft_ledger_live_moved_check,
    ADD CONSTRAINT draft_ledger_live_moved_check CHECK (live_moved IS NULL OR jsonb_typeof(live_moved) = 'object');
ALTER TABLE draft_ledger ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON draft_ledger;
CREATE POLICY tenant_isolation ON draft_ledger FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON draft_ledger FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON draft_ledger TO agent_app;
GRANT USAGE ON SEQUENCE draft_ledger_id_seq TO agent_app;

-- The baseline of a job never released: its design as it was before the first draft write (fixture
-- `liveSnapshot`), one row per place. Immutable once taken (`release_snapshot_immutable`, which binds the owner too);
-- coverage backfill adds rows for a newly registered kind with INSERT … ON CONFLICT DO NOTHING.
CREATE TABLE IF NOT EXISTS release_baseline (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL,
    key       text NOT NULL,
    kind      text NOT NULL,
    ref       jsonb NOT NULL,
    snap      jsonb,
    taken_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, job_id, key)
);
ALTER TABLE release_baseline
    DROP CONSTRAINT IF EXISTS release_baseline_job,
    ADD CONSTRAINT release_baseline_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    DROP CONSTRAINT IF EXISTS release_baseline_key_check,
    ADD CONSTRAINT release_baseline_key_check CHECK (char_length(key) BETWEEN 1 AND 512),
    DROP CONSTRAINT IF EXISTS release_baseline_kind_check,
    ADD CONSTRAINT release_baseline_kind_check CHECK (kind ~ '^[a-z][a-z_]{1,40}$'),
    DROP CONSTRAINT IF EXISTS release_baseline_ref_check,
    ADD CONSTRAINT release_baseline_ref_check CHECK (jsonb_typeof(ref) = 'object'),
    DROP CONSTRAINT IF EXISTS release_baseline_snap_check,
    ADD CONSTRAINT release_baseline_snap_check CHECK (design_snap_ok(snap));
ALTER TABLE release_baseline ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON release_baseline;
CREATE POLICY tenant_isolation ON release_baseline FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON release_baseline FROM agent_app;
GRANT SELECT, INSERT ON release_baseline TO agent_app;

-- Stored snapshots (release_baseline here, release_snapshot in 0045) never change. A trigger, not only the missing
-- grant, so the owner is bound too. SQLSTATE 23514 with the constraint name, so a caller can tell it apart.
CREATE OR REPLACE FUNCTION release_snapshot_immutable() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    RAISE EXCEPTION 'release_snapshot_immutable'
        USING ERRCODE = '23514', CONSTRAINT = 'release_snapshot_immutable',
              DETAIL = format('%s %s: a stored snapshot row never changes', TG_TABLE_NAME, OLD.key);
END $$;
DROP TRIGGER IF EXISTS release_baseline_immutable ON release_baseline;
CREATE TRIGGER release_baseline_immutable BEFORE UPDATE ON release_baseline
    FOR EACH ROW EXECUTE FUNCTION release_snapshot_immutable();
