-- 0122 (B3 T11, plan §4 + §5.7; was 0055 before the T11-vs-B12 ruling): the grounding of agent replies and staff
-- ratings of them, both keyed to B12's delivered message log (0061 `conversation_message`).
--
-- Numbering deviation (B3's range is 0050–0059): both tables reference `conversation_message` (0061), so they must
-- run after it; 0122 is the next free number above every migration on the branch (0121 = B12). The old 0055 never
-- ran anywhere (B3 was unmerged) and is deleted. References only ≤0033 objects and 0061 — nothing from 0041–0059
-- or 0091–0093 — so both production orders (tests/migration_order.rs, tests/b12_migration_order.rs) hold.
--
-- `answer_grounding` is the grounding of ONE agent reply, 1:1 with its `conversation_message` row (PK = the
-- message id; the FK carries tenant and job, so a row cannot point at another tenant's or job's message). It stores
-- only what the message does not: the knowledge query the turn ran, whether it searched knowledge, the chunk ids it
-- cites, the cited sources (`AnswerSource` JSON; R10 platform hits carry `layer: "platform"` and
-- `pack: "<key>@<version>"`), the "I don't know" flag and the verdict. The question, the answer text, the time,
-- the conversation, the case and the env are read from `conversation_message` / `conversation`. The verdict is a
-- pure function of three columns (web `groundingOf`), pinned by a CHECK. Written once by the conversation runtime
-- (`knowledge::analytics::record_grounding`, in the reply's transaction); never changed.
--
-- `answer_feedback` is a staff rating of one agent reply (operational data, not design: never in the D54 draft),
-- referencing the message the same way. One rating per person per reply: rating again replaces it (upsert). A
-- reason only on a down vote; a note of at most 500 characters.
--
-- Both may reference an AGENT reply only (trigger). Forward-only; drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS
-- + ADD, CREATE OR REPLACE FUNCTION, DROP TRIGGER/POLICY IF EXISTS + CREATE, REVOKE ALL + GRANT).

-- The composite FK target (`message_id` alone is the PK; a unique index is a valid FK target).
CREATE UNIQUE INDEX IF NOT EXISTS conversation_message_tenant_job_message
    ON conversation_message (tenant_id, job_id, message_id);

CREATE OR REPLACE FUNCTION answer_references_agent_reply() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM conversation_message m
                    WHERE m.message_id = NEW.message_id AND m.tenant_id = NEW.tenant_id
                      AND m.job_id = NEW.job_id AND m.role = 'agent') THEN
        RAISE EXCEPTION 'answer_not_agent_reply' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;

CREATE TABLE IF NOT EXISTS answer_grounding (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL,
    message_id      uuid NOT NULL,
    knowledge_query text,
    used_knowledge  boolean NOT NULL DEFAULT false,
    citations       text[] NOT NULL DEFAULT '{}',
    sources         jsonb NOT NULL DEFAULT '[]',
    dont_know       boolean NOT NULL DEFAULT false,
    grounding       text NOT NULL,
    PRIMARY KEY (message_id)
);
ALTER TABLE answer_grounding
    DROP CONSTRAINT IF EXISTS answer_grounding_tenant,
    ADD CONSTRAINT answer_grounding_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS answer_grounding_message,
    ADD CONSTRAINT answer_grounding_message FOREIGN KEY (tenant_id, job_id, message_id)
        REFERENCES conversation_message (tenant_id, job_id, message_id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS answer_grounding_sources_check,
    ADD CONSTRAINT answer_grounding_sources_check CHECK (jsonb_typeof(sources) = 'array'),
    DROP CONSTRAINT IF EXISTS answer_grounding_grounding_check,
    ADD CONSTRAINT answer_grounding_grounding_check CHECK (grounding = CASE
        WHEN NOT used_knowledge THEN 'no_knowledge'
        WHEN cardinality(citations) > 0 THEN 'grounded'
        WHEN dont_know THEN 'dont_know'
        ELSE 'not_grounded' END);
CREATE INDEX IF NOT EXISTS answer_grounding_job ON answer_grounding (tenant_id, job_id);
DROP TRIGGER IF EXISTS answer_grounding_agent_reply ON answer_grounding;
CREATE TRIGGER answer_grounding_agent_reply BEFORE INSERT OR UPDATE OF message_id ON answer_grounding
    FOR EACH ROW EXECUTE FUNCTION answer_references_agent_reply();
ALTER TABLE answer_grounding ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON answer_grounding;
CREATE POLICY tenant_isolation ON answer_grounding FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
-- written once with its reply, never changed
REVOKE ALL ON answer_grounding FROM agent_app;
GRANT SELECT, INSERT ON answer_grounding TO agent_app;

CREATE TABLE IF NOT EXISTS answer_feedback (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL,
    id              uuid NOT NULL,
    message_id      uuid NOT NULL,
    surface         text NOT NULL,
    vote            text NOT NULL,
    reason          text,
    note            text,
    by_sub          text NOT NULL,
    at              timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (id)
);
ALTER TABLE answer_feedback
    DROP CONSTRAINT IF EXISTS answer_feedback_tenant,
    ADD CONSTRAINT answer_feedback_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS answer_feedback_message,
    ADD CONSTRAINT answer_feedback_message FOREIGN KEY (tenant_id, job_id, message_id)
        REFERENCES conversation_message (tenant_id, job_id, message_id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS answer_feedback_one_per_person,
    ADD CONSTRAINT answer_feedback_one_per_person UNIQUE (tenant_id, message_id, by_sub),
    DROP CONSTRAINT IF EXISTS answer_feedback_surface_check,
    ADD CONSTRAINT answer_feedback_surface_check CHECK (surface IN ('test_chat', 'case')),
    DROP CONSTRAINT IF EXISTS answer_feedback_vote_check,
    ADD CONSTRAINT answer_feedback_vote_check CHECK (vote IN ('up', 'down')),
    DROP CONSTRAINT IF EXISTS answer_feedback_reason_check,
    ADD CONSTRAINT answer_feedback_reason_check CHECK (
        reason IS NULL OR (vote = 'down' AND reason IN ('wrong', 'incomplete', 'not_grounded', 'tone', 'other'))),
    DROP CONSTRAINT IF EXISTS answer_feedback_note_check,
    ADD CONSTRAINT answer_feedback_note_check CHECK (note IS NULL OR char_length(note) BETWEEN 1 AND 500),
    DROP CONSTRAINT IF EXISTS answer_feedback_by_check,
    ADD CONSTRAINT answer_feedback_by_check CHECK (btrim(by_sub) <> '');
CREATE INDEX IF NOT EXISTS answer_feedback_job_at ON answer_feedback (tenant_id, job_id, at DESC);
DROP TRIGGER IF EXISTS answer_feedback_agent_reply ON answer_feedback;
CREATE TRIGGER answer_feedback_agent_reply BEFORE INSERT OR UPDATE OF message_id ON answer_feedback
    FOR EACH ROW EXECUTE FUNCTION answer_references_agent_reply();
ALTER TABLE answer_feedback ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON answer_feedback;
CREATE POLICY tenant_isolation ON answer_feedback FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
-- upsert = insert, or replace my own rating; ratings are never deleted through the API
REVOKE ALL ON answer_feedback FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON answer_feedback TO agent_app;
