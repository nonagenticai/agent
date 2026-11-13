-- 0067 — CSAT capture (F-13; B4 T16b shape, pulled forward by B12 §1). Forward-only (pre-prod).
--
-- One request per case; the answer is a number 1–5 (no free text, spec 17 F-13). A request expires 24 h after it
-- was sent. State is derived: requested (answered_at NULL, before expires_at), answered, expired. Deltas to B4:
-- `release_id` / `draft_rev` nullable with no FK; widened channel CHECK. References only ≤0033 objects and 0060.

CREATE TABLE case_csat (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    case_id         text NOT NULL,
    conversation_id text NOT NULL,
    channel         text NOT NULL,
    env             text NOT NULL CHECK (env IN ('prod', 'test')),
    release_id      text,
    draft_rev       bigint CHECK (draft_rev IS NULL OR draft_rev >= 0),
    requested_at    timestamptz NOT NULL DEFAULT now(),
    answered_at     timestamptz,
    rating          smallint CHECK (rating IS NULL OR rating BETWEEN 1 AND 5),
    expires_at      timestamptz NOT NULL DEFAULT now() + interval '24 hours',
    PRIMARY KEY (tenant_id, job_id, case_id),
    CONSTRAINT case_csat_channel_check CHECK (channel IN (
        'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api')),
    CONSTRAINT case_csat_answer_check CHECK ((rating IS NULL) = (answered_at IS NULL)),
    CONSTRAINT case_csat_expiry_check CHECK (expires_at > requested_at),
    CONSTRAINT case_csat_case FOREIGN KEY (tenant_id, job_id, case_id)
        REFERENCES agent_case (tenant_id, job_id, case_id)
);
CREATE INDEX case_csat_answered ON case_csat (tenant_id, job_id, answered_at) WHERE answered_at IS NOT NULL;
CREATE INDEX case_csat_conversation ON case_csat (tenant_id, job_id, conversation_id);
ALTER TABLE case_csat ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON case_csat FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON case_csat AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
-- The request is inserted; only the answer may change afterwards.
GRANT SELECT, INSERT ON case_csat TO agent_app;
GRANT UPDATE (rating, answered_at) ON case_csat TO agent_app;
