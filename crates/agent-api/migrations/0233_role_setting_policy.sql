-- 0233 — A5 per-job runtime policy (plan 2026-10-30-caller-identity-iam, A5). Forward-only (pre-prod), drift-tolerant
-- (IF NOT EXISTS, DROP … IF EXISTS + ADD).
--
-- job_agent_role_setting (0148) gains the assistant's per-job policy:
--   turn_budget_s     seconds one assistant turn may run, 5..600 (NULL = the runtime default, 60)
--   system_prompt     the assistant's task prompt, at most 4000 characters (NULL = the audience's default)
--   case_per_thread   a turn on a conversation whose latest case is open continues that case
-- The grants of 0148 cover the new columns (table-level).

ALTER TABLE job_agent_role_setting
    ADD COLUMN IF NOT EXISTS turn_budget_s   integer NULL,
    ADD COLUMN IF NOT EXISTS system_prompt   text NULL,
    ADD COLUMN IF NOT EXISTS case_per_thread boolean NOT NULL DEFAULT false;
ALTER TABLE job_agent_role_setting
    DROP CONSTRAINT IF EXISTS job_agent_role_setting_turn_budget_s_check,
    ADD CONSTRAINT job_agent_role_setting_turn_budget_s_check
        CHECK (turn_budget_s IS NULL OR turn_budget_s BETWEEN 5 AND 600),
    DROP CONSTRAINT IF EXISTS job_agent_role_setting_system_prompt_check,
    ADD CONSTRAINT job_agent_role_setting_system_prompt_check
        CHECK (system_prompt IS NULL OR char_length(system_prompt) <= 4000);

-- Metering: the end-user caller is no longer only a customer. Source `customer_turn` → `turn`, actor
-- `customers:<channel>` → `callers:<channel>` (0064's source check lists the allowed sources).
ALTER TABLE usage_event DROP CONSTRAINT IF EXISTS usage_event_source_check;
UPDATE usage_event SET source = 'turn' WHERE source = 'customer_turn';
UPDATE usage_event SET actor = 'callers:' || substr(actor, char_length('customers:') + 1)
 WHERE actor LIKE 'customers:%';
ALTER TABLE usage_event
    ADD CONSTRAINT usage_event_source_check CHECK (source IN (
        'turn', 'staff_test', 'generator', 'discovery', 'validation', 'gate'));
