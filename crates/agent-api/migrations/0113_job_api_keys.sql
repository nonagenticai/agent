-- 0113 — per-job API keys for machine callers (B13 §2/§4). Forward-only (pre-prod), drift-tolerant: every object is
-- created IF NOT EXISTS and every constraint / policy is dropped and re-added.
--
-- A key answers the interaction routes of ONE job (crate::interaction::apikeys). The secret (`ak_` + 40 base32
-- characters) is shown once at creation; only its sha256 is stored, with the first 10 characters (`prefix`) so a
-- person can tell keys apart. A request presents the secret before any tenant is known, so the lookup by hash goes
-- through the SECURITY DEFINER `agent_job_api_key` (RLS hides every row before the tenant GUC is set). References
-- only ≤0033 objects (`tenants`, `jobs`, `app_current_tenant()`, `app_current_job()`).

CREATE TABLE IF NOT EXISTS job_api_keys (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    label        text NOT NULL,
    prefix       text NOT NULL,
    key_hash     bytea NOT NULL,
    created_by   text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    expires_at   timestamptz,
    revoked_at   timestamptz
);

ALTER TABLE job_api_keys DROP CONSTRAINT IF EXISTS job_api_keys_tenant;
ALTER TABLE job_api_keys ADD CONSTRAINT job_api_keys_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE job_api_keys DROP CONSTRAINT IF EXISTS job_api_keys_job;
ALTER TABLE job_api_keys
    ADD CONSTRAINT job_api_keys_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id) ON DELETE CASCADE;
ALTER TABLE job_api_keys DROP CONSTRAINT IF EXISTS job_api_keys_key_hash_key;
ALTER TABLE job_api_keys ADD CONSTRAINT job_api_keys_key_hash_key UNIQUE (key_hash);
ALTER TABLE job_api_keys DROP CONSTRAINT IF EXISTS job_api_keys_key_hash_check;
ALTER TABLE job_api_keys ADD CONSTRAINT job_api_keys_key_hash_check CHECK (octet_length(key_hash) = 32);
ALTER TABLE job_api_keys DROP CONSTRAINT IF EXISTS job_api_keys_prefix_check;
ALTER TABLE job_api_keys ADD CONSTRAINT job_api_keys_prefix_check CHECK (prefix ~ '^ak_[a-z2-7]{7}$');
ALTER TABLE job_api_keys DROP CONSTRAINT IF EXISTS job_api_keys_label_check;
ALTER TABLE job_api_keys ADD CONSTRAINT job_api_keys_label_check
    CHECK (btrim(label) <> '' AND char_length(label) <= 120);
ALTER TABLE job_api_keys DROP CONSTRAINT IF EXISTS job_api_keys_created_by_check;
ALTER TABLE job_api_keys ADD CONSTRAINT job_api_keys_created_by_check CHECK (btrim(created_by) <> '');
CREATE INDEX IF NOT EXISTS job_api_keys_job_idx ON job_api_keys (tenant_id, job_id, created_at);

ALTER TABLE job_api_keys ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON job_api_keys;
CREATE POLICY tenant_isolation ON job_api_keys FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON job_api_keys;
CREATE POLICY job_scope ON job_api_keys AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON job_api_keys FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON job_api_keys TO agent_app;

-- The key with this hash, whatever its tenant (revoked and expired ones too: the caller refuses them, so a revoked
-- key is told apart from an unknown one only in the log, never on the wire).
DROP FUNCTION IF EXISTS agent_job_api_key(bytea);
CREATE FUNCTION agent_job_api_key(p_hash bytea)
    RETURNS TABLE (id uuid, tenant_id uuid, job_id uuid, revoked_at timestamptz, expires_at timestamptz)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT k.id, k.tenant_id, k.job_id, k.revoked_at, k.expires_at
          FROM job_api_keys k
         WHERE k.key_hash = p_hash
    $$;
REVOKE ALL ON FUNCTION agent_job_api_key(bytea) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_job_api_key(bytea) TO agent_app;
