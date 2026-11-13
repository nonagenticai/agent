-- 0231 — conversation state (caller identity / IAM plan, A1). Forward-only (pre-prod), drift-tolerant.
--
-- `conversation_state` is a conversation's key/value state: the channel's identity (`subject`, `email`, `name`,
-- `channel_subject`, `caller`, `caller_label`, `identity_source`) and whatever a tool declaration produces (a token, a
-- customer id). `kind = 'secret'` rows carry a Keyring-sealed `secret` (AAD = tenant|conversation|key), never a
-- `value`; the model sees only present/absent. An expired row (`expires_at <= now()`) is absent to every reader.
-- `subject_state` is the opt-in carry-over (`persist_for_subject`): a key kept against the conversation's subject
-- (`subject:<id>` or `channel:<id>`) until `expires_at`, sealed with AAD = tenant|subject_key|key.
-- Dual-write phase: the `conversation.customer_*` columns stay until readers move (0234 backfills and drops).
-- References only 0001 `tenants`, 0033 `jobs`, 0061 `conversation` and the 0065 `agent_runtime` role.

CREATE TABLE IF NOT EXISTS conversation_state (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    conversation_id text NOT NULL,
    key             text NOT NULL,
    kind            text NOT NULL,
    value           text,
    secret          text,
    produced_by     text NOT NULL DEFAULT '',
    expires_at      timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id, conversation_id, key)
);
ALTER TABLE conversation_state DROP CONSTRAINT IF EXISTS conversation_state_conversation;
ALTER TABLE conversation_state ADD CONSTRAINT conversation_state_conversation
    FOREIGN KEY (tenant_id, job_id, conversation_id) REFERENCES conversation (tenant_id, job_id, conversation_id)
    ON DELETE CASCADE;
ALTER TABLE conversation_state DROP CONSTRAINT IF EXISTS conversation_state_job;
ALTER TABLE conversation_state ADD CONSTRAINT conversation_state_job
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id);
ALTER TABLE conversation_state DROP CONSTRAINT IF EXISTS conversation_state_key_check;
ALTER TABLE conversation_state ADD CONSTRAINT conversation_state_key_check CHECK (key ~ '^[a-z][a-z0-9_]{0,63}$');
ALTER TABLE conversation_state DROP CONSTRAINT IF EXISTS conversation_state_kind_check;
ALTER TABLE conversation_state ADD CONSTRAINT conversation_state_kind_check CHECK (
    (kind = 'value' AND value IS NOT NULL AND secret IS NULL)
    OR (kind = 'secret' AND secret IS NOT NULL AND value IS NULL));
ALTER TABLE conversation_state DROP CONSTRAINT IF EXISTS conversation_state_size_check;
ALTER TABLE conversation_state ADD CONSTRAINT conversation_state_size_check CHECK (
    char_length(coalesce(value, '')) <= 8000 AND char_length(coalesce(secret, '')) <= 16000
    AND char_length(produced_by) <= 200);
CREATE INDEX IF NOT EXISTS conversation_state_expiry
    ON conversation_state (expires_at) WHERE expires_at IS NOT NULL;

ALTER TABLE conversation_state ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON conversation_state;
CREATE POLICY tenant_isolation ON conversation_state FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON conversation_state;
CREATE POLICY job_scope ON conversation_state AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

CREATE TABLE IF NOT EXISTS subject_state (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    subject_key text NOT NULL,
    key         text NOT NULL,
    kind        text NOT NULL,
    value       text,
    secret      text,
    produced_by text NOT NULL DEFAULT '',
    expires_at  timestamptz,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id, subject_key, key)
);
ALTER TABLE subject_state DROP CONSTRAINT IF EXISTS subject_state_job;
ALTER TABLE subject_state ADD CONSTRAINT subject_state_job
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id);
ALTER TABLE subject_state DROP CONSTRAINT IF EXISTS subject_state_subject_key_check;
ALTER TABLE subject_state ADD CONSTRAINT subject_state_subject_key_check CHECK (
    btrim(subject_key) <> '' AND char_length(subject_key) <= 260);
ALTER TABLE subject_state DROP CONSTRAINT IF EXISTS subject_state_key_check;
ALTER TABLE subject_state ADD CONSTRAINT subject_state_key_check CHECK (key ~ '^[a-z][a-z0-9_]{0,63}$');
ALTER TABLE subject_state DROP CONSTRAINT IF EXISTS subject_state_kind_check;
ALTER TABLE subject_state ADD CONSTRAINT subject_state_kind_check CHECK (
    (kind = 'value' AND value IS NOT NULL AND secret IS NULL)
    OR (kind = 'secret' AND secret IS NOT NULL AND value IS NULL));
ALTER TABLE subject_state DROP CONSTRAINT IF EXISTS subject_state_size_check;
ALTER TABLE subject_state ADD CONSTRAINT subject_state_size_check CHECK (
    char_length(coalesce(value, '')) <= 8000 AND char_length(coalesce(secret, '')) <= 16000
    AND char_length(produced_by) <= 200);
CREATE INDEX IF NOT EXISTS subject_state_expiry ON subject_state (expires_at) WHERE expires_at IS NOT NULL;

ALTER TABLE subject_state ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON subject_state;
CREATE POLICY tenant_isolation ON subject_state FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON subject_state;
CREATE POLICY job_scope ON subject_state AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());

-- agent_app writes channel identity; agent_runtime (orchestrator, runner, interaction) reads and writes tool state.
REVOKE ALL ON conversation_state, subject_state FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON conversation_state, subject_state TO agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON conversation_state, subject_state TO agent_runtime;
