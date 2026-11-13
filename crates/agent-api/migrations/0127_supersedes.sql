-- 0127 (PK T1, plan PK-8): tenant precedence at authoring. `supersedes` = the platform topic keys a tenant's
-- knowledge source / upload replaces; the leaf-set search sends their union as the request's `supersedes` (strata
-- drops matching platform chunks). Limits as strata (README "Document sources"): at most 32 keys, each 1–128
-- characters and not blank. The API sorts and de-duplicates; the CHECK is the floor.
--
-- No `agent_runtime` grants here (coordinator ruling Q3, 2026-10-21): B4 audits the runtime's grants when it merges
-- after PK. References only main's tables (0052 knowledge_upload, 0053 source). Drift-tolerant (ADD COLUMN IF NOT
-- EXISTS, CREATE OR REPLACE FUNCTION, DROP CONSTRAINT IF EXISTS + ADD). Forward-only.

CREATE OR REPLACE FUNCTION knowledge_supersedes_valid(p text[]) RETURNS boolean
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    SET search_path = pg_catalog, public, pg_temp
    AS $$
    SELECT p IS NOT NULL
       AND coalesce(array_ndims(p), 1) = 1
       AND cardinality(p) <= 32
       AND NOT EXISTS (
           SELECT 1 FROM unnest(p) AS e
            WHERE e IS NULL OR char_length(e) NOT BETWEEN 1 AND 128 OR btrim(e) = '')
$$;

ALTER TABLE source ADD COLUMN IF NOT EXISTS supersedes text[] NOT NULL DEFAULT '{}';
ALTER TABLE source
    DROP CONSTRAINT IF EXISTS source_supersedes_check,
    ADD CONSTRAINT source_supersedes_check CHECK (knowledge_supersedes_valid(supersedes));

ALTER TABLE knowledge_upload ADD COLUMN IF NOT EXISTS supersedes text[] NOT NULL DEFAULT '{}';
ALTER TABLE knowledge_upload
    DROP CONSTRAINT IF EXISTS knowledge_upload_supersedes_check,
    ADD CONSTRAINT knowledge_upload_supersedes_check CHECK (knowledge_supersedes_valid(supersedes));
