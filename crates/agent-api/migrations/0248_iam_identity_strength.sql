-- 0248 — a tenant IAM procedure declares the identity strength it establishes (owner 2026-11-01). Forward-only
-- (pre-prod), drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, as 0232).
--
-- `identified`: the procedure established who the person is (e.g. their email) — conversation identity level 1.
-- `verified`: the procedure proved it (e.g. a one-time code) — level 2, the level at which the platform treats the
-- person as a known customer (KYC context reads, party matching). The strength is read with the procedure's steps
-- on every design read (the steps are unversioned; 0232's note), so editing it changes a live job exactly as
-- editing the steps does.

ALTER TABLE iam_procedure ADD COLUMN IF NOT EXISTS strength text NOT NULL DEFAULT 'identified';
ALTER TABLE iam_procedure DROP CONSTRAINT IF EXISTS iam_procedure_strength_check;
ALTER TABLE iam_procedure ADD CONSTRAINT iam_procedure_strength_check
    CHECK (strength IN ('identified', 'verified'));

-- The identity upgrade runs inside the runner's transaction (`agent_runtime`): it fills the web chat session with the
-- identity (0183's shape), which is the session's authority on a reload. Table-level, deliberately: a column grant
-- may only name columns that exist when it runs, and this migration also has to apply BEFORE the B4 migrations that
-- add them (the B4 migration-order test applies everything but B4's set first). The conversation's effective
-- customer id needs no grant: it is the state `subject` (`conversation::conv_columns` COALESCEs it over the column).
GRANT SELECT, UPDATE ON web_chat_session TO agent_runtime;
