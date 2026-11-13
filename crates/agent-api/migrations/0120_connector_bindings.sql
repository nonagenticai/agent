-- 0120 — channel connector bindings (B12 §2, B11 phase B). Forward-only (pre-prod), drift-tolerant: every object
-- is created IF NOT EXISTS and every constraint / policy is dropped and re-added, so the migration applies over a
-- hand-made copy of the table too.
--
-- A binding attaches a channel account to a job, customer side (where customers write in: Teams tenant id, Slack
-- team id, WhatsApp phone_number_id, an email address, a web chat key `wk_…`) or worker side (where staff are
-- notified: `settings` carries the target, `external_id` its id). A BYO credential is sealed (`secret_enc` /
-- `secret_nonce`, AAD `binding_cred:<id>`) and never returned. The edge reads all enabled bindings across tenants
-- through `agent_connector_bindings()`. References only ≤0033 objects.

CREATE TABLE IF NOT EXISTS connector_bindings (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    env          text NOT NULL DEFAULT 'prod',
    kind         text NOT NULL,
    side         text NOT NULL,
    external_id  text NOT NULL,
    label        text NOT NULL DEFAULT '',
    settings     jsonb NOT NULL DEFAULT '{}',
    secret_enc   bytea,
    secret_nonce bytea,
    secret_fpr   text,
    secret_dek_version integer,
    -- Ownership proof (B12 review #8): a non-web-chat binding is created unverified with a one-time code the
    -- account owner sends from the bound workspace/address (`verify <code>`); web chat keys are ours, verified at once.
    verify_code      text,
    verify_failures  integer NOT NULL DEFAULT 0,
    verified_at      timestamptz,
    enabled      boolean NOT NULL DEFAULT true,
    created_by   text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_tenant;
ALTER TABLE connector_bindings
    ADD CONSTRAINT connector_bindings_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_job;
ALTER TABLE connector_bindings
    ADD CONSTRAINT connector_bindings_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id);
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_env_check;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_env_check CHECK (env IN ('prod', 'test'));
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_kind_check;
ALTER TABLE connector_bindings
    ADD CONSTRAINT connector_bindings_kind_check CHECK (kind IN ('teams', 'slack', 'whatsapp', 'email', 'web_chat'));
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_side_check;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_side_check CHECK (side IN ('customer', 'worker'));
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_external_id_check;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_external_id_check
    CHECK (btrim(external_id) <> '' AND char_length(external_id) <= 512);
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_label_check;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_label_check CHECK (char_length(label) <= 120);
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_settings_check;
ALTER TABLE connector_bindings
    ADD CONSTRAINT connector_bindings_settings_check CHECK (jsonb_typeof(settings) = 'object');
-- A sealed credential is all three columns or none.
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_secret_check;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_secret_check CHECK (
    (secret_enc IS NULL) = (secret_nonce IS NULL) AND (secret_enc IS NULL) = (secret_fpr IS NULL)
    AND (secret_enc IS NULL) = (secret_dek_version IS NULL));
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_created_by_check;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_created_by_check CHECK (btrim(created_by) <> '');
-- One binding per channel account, platform-wide: the edge routes an inbound message by it.
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_account;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_account UNIQUE (kind, side, external_id);
CREATE INDEX IF NOT EXISTS connector_bindings_job_idx ON connector_bindings (tenant_id, job_id, kind, side);

ALTER TABLE connector_bindings ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON connector_bindings;
CREATE POLICY tenant_isolation ON connector_bindings FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON connector_bindings;
CREATE POLICY job_scope ON connector_bindings AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON connector_bindings FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON connector_bindings TO agent_app;

-- Every enabled binding of every tenant, with its job key: the edge's cross-tenant lookup (pattern
-- `agent_signup_homes`, 0040). The sealed credential columns go out sealed; the caller opens them.
ALTER TABLE connector_bindings DROP CONSTRAINT IF EXISTS connector_bindings_verify_check;
ALTER TABLE connector_bindings ADD CONSTRAINT connector_bindings_verify_check CHECK (
    (verified_at IS NULL) = (verify_code IS NOT NULL) AND verify_failures >= 0
    AND (verify_code IS NULL OR verify_code ~ '^[A-Z2-9]{6}$'));

DROP FUNCTION IF EXISTS agent_connector_bindings();
CREATE FUNCTION agent_connector_bindings()
    RETURNS TABLE (id uuid, tenant_id uuid, job_id uuid, job_key text, env text, kind text, side text,
                   external_id text, label text, settings jsonb, secret_enc bytea, secret_nonce bytea,
                   secret_fpr text, secret_dek_version integer)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT b.id, b.tenant_id, b.job_id, j.key, b.env, b.kind, b.side, b.external_id, b.label, b.settings,
               b.secret_enc, b.secret_nonce, b.secret_fpr, b.secret_dek_version
          FROM connector_bindings b
          JOIN jobs j ON j.tenant_id = b.tenant_id AND j.id = b.job_id
         WHERE b.enabled AND b.verified_at IS NOT NULL
         ORDER BY b.created_at, b.id
    $$;
REVOKE ALL ON FUNCTION agent_connector_bindings() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_connector_bindings() TO agent_app;

-- The pending (unverified, enabled) binding of one account, for the edge's `verify <code>` door. Returns the code so
-- the caller compares it in constant time; never exposed beyond the key-gated internal route.
DROP FUNCTION IF EXISTS agent_connector_binding_pending(text, text, text);
CREATE FUNCTION agent_connector_binding_pending(p_kind text, p_side text, p_external_id text)
    RETURNS TABLE (id uuid, tenant_id uuid, verify_code text, verify_failures integer)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT b.id, b.tenant_id, b.verify_code, b.verify_failures
          FROM connector_bindings b
         WHERE b.kind = p_kind AND b.side = p_side AND b.external_id = p_external_id
           AND b.enabled AND b.verified_at IS NULL
    $$;
REVOKE ALL ON FUNCTION agent_connector_binding_pending(text, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_connector_binding_pending(text, text, text) TO agent_app;

-- The same by binding id (an email worker binding is verified at `verify+<id>@<reply domain>`, B12): kind, side and
-- settings come back so the caller checks the side and the sender against `settings.to`.
DROP FUNCTION IF EXISTS agent_connector_binding_pending_id(uuid);
CREATE FUNCTION agent_connector_binding_pending_id(p_id uuid)
    RETURNS TABLE (id uuid, tenant_id uuid, verify_code text, verify_failures integer, kind text, side text,
                   settings jsonb)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT b.id, b.tenant_id, b.verify_code, b.verify_failures, b.kind, b.side, b.settings
          FROM connector_bindings b
         WHERE b.id = p_id AND b.enabled AND b.verified_at IS NULL
    $$;
REVOKE ALL ON FUNCTION agent_connector_binding_pending_id(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_connector_binding_pending_id(uuid) TO agent_app;

-- The outcome of one verification attempt: `p_ok` marks the binding verified; otherwise the failure is counted and,
-- at 5, the code is replaced by `p_new_code` (the owner reads the new one in the admin UI). Returns the new state.
DROP FUNCTION IF EXISTS agent_connector_binding_attempt(uuid, boolean, text);
CREATE FUNCTION agent_connector_binding_attempt(p_id uuid, p_ok boolean, p_new_code text)
    RETURNS text
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        n integer;
    BEGIN
        IF p_ok THEN
            UPDATE connector_bindings SET verified_at = now(), verify_code = NULL, verify_failures = 0,
                   updated_at = now()
             WHERE id = p_id AND verified_at IS NULL;
            RETURN 'verified';
        END IF;
        UPDATE connector_bindings SET verify_failures = verify_failures + 1, updated_at = now()
         WHERE id = p_id AND verified_at IS NULL
        RETURNING verify_failures INTO n;
        IF n IS NOT NULL AND n >= 5 THEN
            UPDATE connector_bindings SET verify_code = p_new_code, verify_failures = 0 WHERE id = p_id;
            RETURN 'rotated';
        END IF;
        RETURN 'failed';
    END
    $$;
REVOKE ALL ON FUNCTION agent_connector_binding_attempt(uuid, boolean, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_connector_binding_attempt(uuid, boolean, text) TO agent_app;
