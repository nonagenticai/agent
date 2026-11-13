-- 0126 (PK T1, plan PK-4..PK-7, PK-11): what a tenant observed of the strata platform packs, and the pack leaves a
-- release cut froze.
--
-- - `platform_pack` (0054, the tenant's listing cache) gains `state` (the pack's state as the platform owner's
--   listing classifies it: published | retired | draft | missing; NULL = not classified yet) and `excluded_by` (the
--   tenant's strata exclusion prefix that hides it, NULL = not excluded).
-- - `platform_pack_leaf`: the tenant's last `GET /api/platform/packs/{key}/{version}/stores` answer, one row per
--   leaf of the pack subtree. An observed cache (refreshed outside any transaction that calls strata), so
--   agent_app may delete and re-insert it; never updated in place.
-- - `release_pack_leaf`: the pack leaves a cut resolved, per pin `key@version`, frozen with the release
--   (`job_release`): SELECT + INSERT only, and a trigger refuses UPDATE and DELETE for the owner too
--   (`release_snapshot_immutable`, SQLSTATE 23514, as 0044/0045 `release_snapshot`).
--
-- Only observed per-tenant state is tenant-scoped (RLS `tenant_isolation`, as `jobs`); pack references themselves
-- are platform `layer_row`s (PK-11). References only main's tables (0001 tenants, 0045 job_release, 0054
-- platform_pack and `knowledge_pack_pins_valid`). Drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD,
-- CREATE OR REPLACE FUNCTION, DROP TRIGGER IF EXISTS + CREATE, DROP POLICY IF EXISTS + CREATE, REVOKE ALL + GRANT).
-- Forward-only.

ALTER TABLE platform_pack
    ADD COLUMN IF NOT EXISTS state text,
    ADD COLUMN IF NOT EXISTS excluded_by text;
ALTER TABLE platform_pack
    DROP CONSTRAINT IF EXISTS platform_pack_state_check,
    ADD CONSTRAINT platform_pack_state_check
        CHECK (state IS NULL OR state IN ('published', 'retired', 'draft', 'missing')),
    DROP CONSTRAINT IF EXISTS platform_pack_excluded_by_check,
    ADD CONSTRAINT platform_pack_excluded_by_check
        CHECK (excluded_by IS NULL OR excluded_by ~ '^[a-z0-9_-]+(/[a-z0-9_-]+){0,5}$');

CREATE TABLE IF NOT EXISTS platform_pack_leaf (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    pack_key    text NOT NULL,
    version     integer NOT NULL,
    leaf_id     uuid NOT NULL,
    parent_id   uuid,
    path        text[] NOT NULL,
    embed_model text,
    chunk_count bigint,
    checked_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, pack_key, version, leaf_id)
);
ALTER TABLE platform_pack_leaf
    DROP CONSTRAINT IF EXISTS platform_pack_leaf_tenant,
    ADD CONSTRAINT platform_pack_leaf_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS platform_pack_leaf_key_check,
    ADD CONSTRAINT platform_pack_leaf_key_check CHECK (pack_key ~ '^[a-z0-9_-]+(/[a-z0-9_-]+){0,5}$'),
    DROP CONSTRAINT IF EXISTS platform_pack_leaf_version_check,
    ADD CONSTRAINT platform_pack_leaf_version_check CHECK (version BETWEEN 1 AND 999999999),
    DROP CONSTRAINT IF EXISTS platform_pack_leaf_path_check,
    ADD CONSTRAINT platform_pack_leaf_path_check
        CHECK (cardinality(path) <= 64 AND array_position(path, NULL) IS NULL),
    DROP CONSTRAINT IF EXISTS platform_pack_leaf_embed_model_check,
    ADD CONSTRAINT platform_pack_leaf_embed_model_check
        CHECK (embed_model IS NULL OR char_length(embed_model) BETWEEN 1 AND 256),
    DROP CONSTRAINT IF EXISTS platform_pack_leaf_chunk_count_check,
    ADD CONSTRAINT platform_pack_leaf_chunk_count_check CHECK (chunk_count IS NULL OR chunk_count >= 0);
ALTER TABLE platform_pack_leaf ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON platform_pack_leaf;
CREATE POLICY tenant_isolation ON platform_pack_leaf FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON platform_pack_leaf FROM agent_app;
GRANT SELECT, INSERT, DELETE ON platform_pack_leaf TO agent_app;

CREATE TABLE IF NOT EXISTS release_pack_leaf (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL,
    release_id  text NOT NULL,
    pin         text NOT NULL,
    leaf_id     uuid NOT NULL,
    embed_model text,
    PRIMARY KEY (tenant_id, job_id, release_id, pin, leaf_id)
);
ALTER TABLE release_pack_leaf
    DROP CONSTRAINT IF EXISTS release_pack_leaf_release,
    ADD CONSTRAINT release_pack_leaf_release FOREIGN KEY (tenant_id, job_id, release_id)
        REFERENCES job_release (tenant_id, job_id, release_id),
    DROP CONSTRAINT IF EXISTS release_pack_leaf_pin_check,
    ADD CONSTRAINT release_pack_leaf_pin_check CHECK (knowledge_pack_pins_valid(ARRAY[pin])),
    DROP CONSTRAINT IF EXISTS release_pack_leaf_embed_model_check,
    ADD CONSTRAINT release_pack_leaf_embed_model_check
        CHECK (embed_model IS NULL OR char_length(embed_model) BETWEEN 1 AND 256);
ALTER TABLE release_pack_leaf ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON release_pack_leaf;
CREATE POLICY tenant_isolation ON release_pack_leaf FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON release_pack_leaf FROM agent_app;
GRANT SELECT, INSERT ON release_pack_leaf TO agent_app;

-- 0044's `release_snapshot_immutable()` names `OLD.key`, which this table lacks; same error, own detail.
CREATE OR REPLACE FUNCTION release_pack_leaf_immutable() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    RAISE EXCEPTION 'release_snapshot_immutable'
        USING ERRCODE = '23514', CONSTRAINT = 'release_snapshot_immutable',
              DETAIL = format('release_pack_leaf %s %s %s: a stored snapshot row never changes',
                              OLD.release_id, OLD.pin, OLD.leaf_id);
END $$;
DROP TRIGGER IF EXISTS release_pack_leaf_immutable ON release_pack_leaf;
CREATE TRIGGER release_pack_leaf_immutable BEFORE UPDATE OR DELETE ON release_pack_leaf
    FOR EACH ROW EXECUTE FUNCTION release_pack_leaf_immutable();
