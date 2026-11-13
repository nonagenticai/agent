-- 0236 — A1 (caller-identity plan), last step: the scattered identity columns move into conversation state and are
-- dropped. Forward-only (pre-prod), drift-tolerant (every statement is re-runnable). References 0060 `agent_case`,
-- 0061 `conversation`, 0112 `party`, 0124 `web_chat_session`, 0182/0183/0196/0235 functions and 0231
-- `conversation_state`.
--
-- Backfill (ON CONFLICT DO NOTHING, `produced_by = 'backfill:0236'`, a state row never overwrites one the runtime
-- already wrote): `conversation.customer_id / customer_name / customer_email / identity_source` of every identified
-- conversation (identity_source IN assertion, staff_pick, staff_test) -> `subject / name / email / identity_source`;
-- `conversation.customer_name` of an unidentified one -> `name`; its wire party id -> `channel_subject` (+
-- `identity_source = channel`, never `api`); `web_chat_session.customer_ref / customer_email / customer_name` onto the
-- session's conversation (an `assertion` identity).
--
-- Dropped:
--   conversation:  customer_name, customer_tier, identity_source, customer_email   (+ their CHECK constraints)
--   agent_case:    identity_level, verified_party, verified_email, verify_failures (+ CHECKs, column grants); a case's
--                  level / verified party / email are derived from its conversation's state where read
--   party:         kind, external_customer_ref (+ CHECKs)
--   functions:     party_mark_verified(uuid, text), party_mark_session_verified(text) (they only wrote
--                  external_customer_ref)
-- Kept: conversation.customer_id (the conversation's WIRE party id: `api:{key}`, `int:{id}`, a Telegram chat id, the
-- web chat `wc:<session>` placeholder; the identified customer is state `subject`), agent_case.customer_id /
-- customer_name (the case's display snapshot), web_chat_session.customer_ref / customer_email / customer_name /
-- identified_at (a session exists BEFORE its conversation, so the verified assertion rides on the session row until
-- the first message binds it into state; `identified_at` also anchors `session_hours`), party_identity (the channel
-- subject -> party mapping).

-- 1. backfill ---------------------------------------------------------------------------------------------------------
INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT c.tenant_id, c.job_id, c.conversation_id, 'subject', 'value', btrim(c.customer_id), 'backfill:0236', now()
  FROM conversation c
 WHERE c.identity_source IN ('assertion', 'staff_pick', 'staff_test')
   AND c.customer_id IS NOT NULL AND btrim(c.customer_id) <> '' AND char_length(c.customer_id) <= 8000
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT c.tenant_id, c.job_id, c.conversation_id, 'email', 'value', btrim(c.customer_email), 'backfill:0236', now()
  FROM conversation c
 WHERE c.identity_source IN ('assertion', 'staff_pick', 'staff_test')
   AND c.customer_email IS NOT NULL AND btrim(c.customer_email) <> '' AND char_length(c.customer_email) <= 8000
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT c.tenant_id, c.job_id, c.conversation_id, 'identity_source', 'value', c.identity_source, 'backfill:0236', now()
  FROM conversation c
 WHERE c.identity_source IN ('assertion', 'staff_pick', 'staff_test')
   AND c.customer_id IS NOT NULL AND btrim(c.customer_id) <> '' AND char_length(c.customer_id) <= 8000
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

-- a display name, identified or not
INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT c.tenant_id, c.job_id, c.conversation_id, 'name', 'value', btrim(c.customer_name), 'backfill:0236', now()
  FROM conversation c
 WHERE c.customer_name IS NOT NULL AND btrim(c.customer_name) <> '' AND char_length(c.customer_name) <= 8000
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

-- a channel that does not identify: the wire party id
INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT c.tenant_id, c.job_id, c.conversation_id, 'channel_subject', 'value', btrim(c.customer_id), 'backfill:0236', now()
  FROM conversation c
 WHERE c.identity_source IS NULL AND c.channel NOT IN ('api', 'staff')
   AND c.customer_id IS NOT NULL AND btrim(c.customer_id) <> '' AND char_length(c.customer_id) <= 8000
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT c.tenant_id, c.job_id, c.conversation_id, 'identity_source', 'value', 'channel', 'backfill:0236', now()
  FROM conversation c
 WHERE c.identity_source IS NULL AND c.channel NOT IN ('api', 'staff')
   AND c.customer_id IS NOT NULL AND btrim(c.customer_id) <> '' AND char_length(c.customer_id) <= 8000
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

-- an identified web chat session onto its conversation (an assertion identity), where state holds none yet
INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT s.tenant_id, s.job_id, s.conversation_id, 'subject', 'value', btrim(s.customer_ref), 'backfill:0236', now()
  FROM web_chat_session s
 WHERE s.conversation_id IS NOT NULL AND s.customer_ref IS NOT NULL AND btrim(s.customer_ref) <> ''
   AND NOT EXISTS (SELECT 1 FROM conversation_state x
                    WHERE x.tenant_id = s.tenant_id AND x.job_id = s.job_id
                      AND x.conversation_id = s.conversation_id AND x.key = 'identity_source'
                      AND x.value IN ('assertion', 'staff_pick', 'staff_test'))
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO UPDATE
    SET value = EXCLUDED.value, produced_by = EXCLUDED.produced_by, updated_at = now()
    WHERE conversation_state.produced_by = 'backfill:0236';

INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT s.tenant_id, s.job_id, s.conversation_id, 'email', 'value', btrim(s.customer_email), 'backfill:0236', now()
  FROM web_chat_session s
 WHERE s.conversation_id IS NOT NULL AND s.customer_ref IS NOT NULL AND s.customer_email IS NOT NULL
   AND btrim(s.customer_email) <> ''
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT s.tenant_id, s.job_id, s.conversation_id, 'name', 'value', btrim(s.customer_name), 'backfill:0236', now()
  FROM web_chat_session s
 WHERE s.conversation_id IS NOT NULL AND s.customer_ref IS NOT NULL AND s.customer_name IS NOT NULL
   AND btrim(s.customer_name) <> ''
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING;

INSERT INTO conversation_state (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at)
SELECT s.tenant_id, s.job_id, s.conversation_id, 'identity_source', 'value', 'assertion', 'backfill:0236', now()
  FROM web_chat_session s
 WHERE s.conversation_id IS NOT NULL AND s.customer_ref IS NOT NULL AND btrim(s.customer_ref) <> ''
ON CONFLICT (tenant_id, job_id, conversation_id, key) DO UPDATE
    SET value = EXCLUDED.value, produced_by = EXCLUDED.produced_by, updated_at = now()
    WHERE conversation_state.value = 'channel' AND conversation_state.produced_by = 'backfill:0236';

-- 2. functions that read or write the dropped columns -----------------------------------------------------------------
DROP FUNCTION IF EXISTS party_mark_session_verified(text);
DROP FUNCTION IF EXISTS party_mark_verified(uuid, text);

-- 3. constraints, then columns ----------------------------------------------------------------------------------------
ALTER TABLE conversation DROP CONSTRAINT IF EXISTS conversation_identity_source_check;
ALTER TABLE conversation DROP CONSTRAINT IF EXISTS conversation_customer_email_check;
ALTER TABLE conversation DROP COLUMN IF EXISTS customer_name;
ALTER TABLE conversation DROP COLUMN IF EXISTS customer_tier;
ALTER TABLE conversation DROP COLUMN IF EXISTS identity_source;
ALTER TABLE conversation DROP COLUMN IF EXISTS customer_email;

ALTER TABLE agent_case DROP CONSTRAINT IF EXISTS agent_case_identity_level_check;
ALTER TABLE agent_case DROP CONSTRAINT IF EXISTS agent_case_verified_party_check;
ALTER TABLE agent_case DROP CONSTRAINT IF EXISTS agent_case_verified_email_check;
ALTER TABLE agent_case DROP CONSTRAINT IF EXISTS agent_case_verify_failures_check;
ALTER TABLE agent_case DROP COLUMN IF EXISTS identity_level;
ALTER TABLE agent_case DROP COLUMN IF EXISTS verified_party;
ALTER TABLE agent_case DROP COLUMN IF EXISTS verified_email;
ALTER TABLE agent_case DROP COLUMN IF EXISTS verify_failures;

ALTER TABLE party DROP CONSTRAINT IF EXISTS party_kind_check;
ALTER TABLE party DROP CONSTRAINT IF EXISTS party_external_customer_ref_check;
ALTER TABLE party DROP COLUMN IF EXISTS kind;
ALTER TABLE party DROP COLUMN IF EXISTS external_customer_ref;
