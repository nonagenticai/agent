-- 0061 — conversations, the delivered message log, console takeover, saved test cases (B4 T3 shapes, pulled
-- forward by B12 §1). Forward-only (pre-prod).
--
-- Deltas to B4: `release_id` / `draft_rev` nullable with no FK (B2 is not merged); widened channel CHECK;
-- `conversation_message.external_id` (the provider's message id) with a partial UNIQUE so a provider retry is
-- deduplicated by the database, not by edge memory. References only ≤0033 objects and 0060.

CREATE TABLE conversation (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    conversation_id text NOT NULL CHECK (btrim(conversation_id) <> '' AND char_length(conversation_id) <= 200),
    env             text NOT NULL CHECK (env IN ('prod', 'test')),
    channel         text NOT NULL,
    customer_id     text,
    customer_name   text,
    customer_tier   text,
    initiator_sub   text,
    started_at      timestamptz NOT NULL DEFAULT now(),
    ended_at        timestamptz,
    PRIMARY KEY (tenant_id, job_id, conversation_id),
    CONSTRAINT conversation_channel_check CHECK (channel IN (
        'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api')),
    CONSTRAINT conversation_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CHECK (ended_at IS NULL OR ended_at >= started_at)
);
CREATE INDEX conversation_recent ON conversation (tenant_id, job_id, started_at DESC);
ALTER TABLE conversation ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON conversation FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON conversation AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON conversation TO agent_app;

-- The delivered log (`chatConversation.messages`).
CREATE TABLE conversation_message (
    message_id      uuid PRIMARY KEY DEFAULT uuidv7(),
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    conversation_id text NOT NULL,
    role            text NOT NULL CHECK (role IN ('customer', 'agent', 'staff', 'system')),
    author          text NOT NULL DEFAULT '',
    text            text NOT NULL,
    case_id         text,
    refused         boolean NOT NULL DEFAULT false,
    grounding       jsonb,
    -- `ReplyTrace`, test env only.
    trace           jsonb,
    at_ms           bigint NOT NULL CHECK (at_ms >= 0),
    -- The provider's message id (Teams activity id, Slack ts, WhatsApp wamid, …): a retry is a duplicate.
    external_id     text CHECK (external_id IS NULL OR (btrim(external_id) <> '' AND char_length(external_id) <= 512)),
    -- A staff message reached the customer's channel (B12 review #6): a retry re-delivers while this is NULL.
    delivered_at    timestamptz,
    CONSTRAINT conversation_message_conversation FOREIGN KEY (tenant_id, job_id, conversation_id)
        REFERENCES conversation (tenant_id, job_id, conversation_id)
);
CREATE INDEX conversation_message_log ON conversation_message (tenant_id, job_id, conversation_id, at_ms);
CREATE UNIQUE INDEX conversation_message_external
    ON conversation_message (tenant_id, job_id, conversation_id, external_id) WHERE external_id IS NOT NULL;
ALTER TABLE conversation_message ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON conversation_message FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON conversation_message AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON conversation_message TO agent_app;

-- Console takeover. No row: the agent has had control from the start.
CREATE TABLE conversation_control (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    conversation_id text NOT NULL,
    mode            text NOT NULL CHECK (mode IN ('agent', 'staff')),
    by_sub          text,
    since           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id, conversation_id),
    CONSTRAINT conversation_control_conversation FOREIGN KEY (tenant_id, job_id, conversation_id)
        REFERENCES conversation (tenant_id, job_id, conversation_id)
);
ALTER TABLE conversation_control ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON conversation_control FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON conversation_control AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON conversation_control TO agent_app;

-- A staff test chat saved as a regression case. `stale` (draft_fp <> B2's current fingerprint) is computed on read.
CREATE TABLE test_case (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    title           text NOT NULL CHECK (char_length(btrim(title)) BETWEEN 1 AND 120),
    conversation_id text NOT NULL,
    customer_id     text,
    release_id      text,
    draft_rev       bigint NOT NULL CHECK (draft_rev >= 0),
    draft_fp        text NOT NULL,
    draft_keys      text[] NOT NULL DEFAULT '{}',
    exercised       text[] NOT NULL DEFAULT '{}',
    differs         integer NOT NULL DEFAULT 0 CHECK (differs >= 0),
    turns           jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(turns) = 'array'),
    saved_by        text NOT NULL CHECK (btrim(saved_by) <> ''),
    saved_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT test_case_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE INDEX test_case_job_saved ON test_case (tenant_id, job_id, saved_at DESC);
ALTER TABLE test_case ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON test_case FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON test_case AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON test_case TO agent_app;
