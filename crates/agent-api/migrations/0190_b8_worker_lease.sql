-- 0190 — leased claims for the B8 background work (grid runs, discovery imports), so agent-api can run more than one
-- replica without two replicas running the same run / import (B8 boot resume, 0177 + 0173).
--
-- - `lease_owner` / `lease_until` on `validation_grid_run` and `discovery_import`: the process (one id per process
--   start) that holds the row and when its hold lapses. Both null or both set. A holder renews after every chunk /
--   batch and clears the lease when it ends; a crashed holder's lease simply lapses and the next sweep takes the row.
-- - `*_claim(id, owner, ttl)` is the only way to take a row: true = granted (the row was unleased or its lease had
--   lapsed, and is still active). It never re-grants to the current holder: a second task of the same process must
--   not start the same work twice. `*_renew` extends the holder's lease, `*_release` clears it; both match the owner.
-- - `validation_grid_active()` / `discovery_classifying_imports()` (same signatures) now return only rows with no live
--   lease, so a sweep skips work a live replica holds. They still only read: resume never creates work.
--
-- SECURITY DEFINER like the boot-resume readers (no tenant scope at boot); every function takes the row id, which is
-- globally unique for both tables (`discovery_import.id` has UNIQUE (id), 0173). Drift-tolerant, forward-only.
-- Needs 0173, 0177.

ALTER TABLE validation_grid_run
    ADD COLUMN IF NOT EXISTS lease_owner text,
    ADD COLUMN IF NOT EXISTS lease_until timestamptz;
ALTER TABLE validation_grid_run
    DROP CONSTRAINT IF EXISTS validation_grid_run_lease_check,
    ADD CONSTRAINT validation_grid_run_lease_check CHECK ((lease_owner IS NULL) = (lease_until IS NULL));

ALTER TABLE discovery_import
    ADD COLUMN IF NOT EXISTS lease_owner text,
    ADD COLUMN IF NOT EXISTS lease_until timestamptz;
ALTER TABLE discovery_import
    DROP CONSTRAINT IF EXISTS discovery_import_lease_check,
    ADD CONSTRAINT discovery_import_lease_check CHECK ((lease_owner IS NULL) = (lease_until IS NULL));

CREATE OR REPLACE FUNCTION validation_grid_claim(p_id uuid, p_owner text, p_ttl_secs integer)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        WITH u AS (
            UPDATE validation_grid_run
               SET lease_owner = p_owner, lease_until = now() + make_interval(secs => p_ttl_secs)
             WHERE id = p_id AND status IN ('queued', 'running')
               AND (lease_until IS NULL OR lease_until < now())
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM u)
    $$;

CREATE OR REPLACE FUNCTION validation_grid_renew(p_id uuid, p_owner text, p_ttl_secs integer)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        WITH u AS (
            UPDATE validation_grid_run
               SET lease_until = now() + make_interval(secs => p_ttl_secs)
             WHERE id = p_id AND lease_owner = p_owner AND status IN ('queued', 'running')
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM u)
    $$;

CREATE OR REPLACE FUNCTION validation_grid_release(p_id uuid, p_owner text)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        WITH u AS (
            UPDATE validation_grid_run SET lease_owner = NULL, lease_until = NULL
             WHERE id = p_id AND lease_owner = p_owner
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM u)
    $$;

CREATE OR REPLACE FUNCTION discovery_import_claim(p_id text, p_owner text, p_ttl_secs integer)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        WITH u AS (
            UPDATE discovery_import
               SET lease_owner = p_owner, lease_until = now() + make_interval(secs => p_ttl_secs)
             WHERE id = p_id AND status = 'classifying'
               AND (lease_until IS NULL OR lease_until < now())
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM u)
    $$;

CREATE OR REPLACE FUNCTION discovery_import_renew(p_id text, p_owner text, p_ttl_secs integer)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        WITH u AS (
            UPDATE discovery_import
               SET lease_until = now() + make_interval(secs => p_ttl_secs)
             WHERE id = p_id AND lease_owner = p_owner AND status = 'classifying'
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM u)
    $$;

CREATE OR REPLACE FUNCTION discovery_import_release(p_id text, p_owner text)
    RETURNS boolean
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        WITH u AS (
            UPDATE discovery_import SET lease_owner = NULL, lease_until = NULL
             WHERE id = p_id AND lease_owner = p_owner
            RETURNING 1)
        SELECT EXISTS (SELECT 1 FROM u)
    $$;

-- The boot / sweep readers: only rows no live replica holds.
CREATE OR REPLACE FUNCTION validation_grid_active()
    RETURNS TABLE (id uuid, tenant_id uuid, job_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        SELECT g.id, g.tenant_id, g.job_id FROM validation_grid_run g
         WHERE g.status IN ('queued', 'running') AND (g.lease_until IS NULL OR g.lease_until < now())
         ORDER BY g.queued_at, g.id
    $$;

CREATE OR REPLACE FUNCTION discovery_classifying_imports()
    RETURNS TABLE (tenant_id uuid, job_id uuid, id text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT i.tenant_id, i.job_id, i.id FROM discovery_import i
         WHERE i.status = 'classifying' AND (i.lease_until IS NULL OR i.lease_until < now())
         ORDER BY i.seq
    $$;

REVOKE ALL ON FUNCTION validation_grid_claim(uuid, text, integer), validation_grid_renew(uuid, text, integer),
    validation_grid_release(uuid, text), discovery_import_claim(text, text, integer),
    discovery_import_renew(text, text, integer), discovery_import_release(text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION validation_grid_claim(uuid, text, integer), validation_grid_renew(uuid, text, integer),
    validation_grid_release(uuid, text), discovery_import_claim(text, text, integer),
    discovery_import_renew(text, text, integer), discovery_import_release(text, text) TO agent_app;
REVOKE ALL ON FUNCTION validation_grid_active(), discovery_classifying_imports() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION validation_grid_active(), discovery_classifying_imports() TO agent_app;
