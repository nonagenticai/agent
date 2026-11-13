-- 0208 — CSAT lifecycle (SW-6, owner 2026-10-27: "think critically about the CSAT functionality"; crate::
-- conversations::csat). Forward-only (pre-prod), drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS before ADD).
--
-- Order: it references only 0067's `case_csat` and the `agent_app` role, so it runs after 0067 in every order.
--
-- One request per CONVERSATION now (the per-case PK stays; the at-most-once rule is `NOT EXISTS` under the
-- conversation's control lock in `csat::arm` / `csat::ask_staff`). A row's `state`:
-- - `awaiting_confirmation`: the agent resolved the case; nothing is sent until the customer's next message
--   acknowledges it (then `asked`); any other message dismisses it; silence lets it expire (`asked_at` NULL);
-- - `asked`: the prompt was sent (`asked_at`); a rating answers it, any other message dismisses it;
-- - `answered`: `rating` / `answered_at` as before; a rating of 1 or 2 asks one follow-up (`comment_asked_at`) and the
--   customer's next message is kept as `comment` (customer text: never in a log, an audit detail or a metric label);
-- - `dismissed`: the customer moved on (`dismissed_at`), before the prompt (`asked_at` NULL) or after it; the reason
--   is a metric label only.
-- `asked_by` (`agent` | `staff`, with the staff member's `asked_by_sub`) splits the support report by resolver.
-- Existing rows become `asked` (or `answered`), asked when requested; a row whose prompt line was the agent's
-- (`external_id` `csat:{case}`) is `agent`, every other `staff`.

ALTER TABLE case_csat
    ADD COLUMN IF NOT EXISTS state text NOT NULL DEFAULT 'asked',
    ADD COLUMN IF NOT EXISTS asked_by text NOT NULL DEFAULT 'staff',
    ADD COLUMN IF NOT EXISTS asked_by_sub text,
    ADD COLUMN IF NOT EXISTS asked_at timestamptz DEFAULT now(),
    ADD COLUMN IF NOT EXISTS dismissed_at timestamptz,
    ADD COLUMN IF NOT EXISTS comment text,
    ADD COLUMN IF NOT EXISTS comment_asked_at timestamptz,
    ADD COLUMN IF NOT EXISTS comment_at timestamptz;

UPDATE case_csat SET
    state = CASE WHEN answered_at IS NOT NULL THEN 'answered' ELSE 'asked' END,
    asked_at = requested_at
 WHERE state = 'asked' AND (asked_at IS NULL OR asked_at <> requested_at);

UPDATE case_csat c SET asked_by = 'agent'
 WHERE asked_by = 'staff'
   AND EXISTS (SELECT 1 FROM conversation_message m
                WHERE m.tenant_id = c.tenant_id AND m.job_id = c.job_id AND m.conversation_id = c.conversation_id
                  AND m.external_id = 'csat:' || c.case_id);

ALTER TABLE case_csat
    DROP CONSTRAINT IF EXISTS case_csat_state_check,
    ADD CONSTRAINT case_csat_state_check CHECK (
        state IN ('awaiting_confirmation', 'asked', 'answered', 'dismissed')
        AND (state = 'answered') = (answered_at IS NOT NULL)
        -- an armed request has not been sent; a sent one has its time; a dismissed one may be either (the customer
        -- moved on before or after the prompt)
        AND (state <> 'awaiting_confirmation' OR asked_at IS NULL)
        AND (state NOT IN ('asked', 'answered') OR asked_at IS NOT NULL)
        AND (state = 'dismissed') = (dismissed_at IS NOT NULL)),
    DROP CONSTRAINT IF EXISTS case_csat_asked_by_check,
    ADD CONSTRAINT case_csat_asked_by_check CHECK (
        asked_by IN ('agent', 'staff') AND (asked_by_sub IS NULL OR asked_by = 'staff')),
    DROP CONSTRAINT IF EXISTS case_csat_comment_check,
    ADD CONSTRAINT case_csat_comment_check CHECK (
        (comment IS NULL) = (comment_at IS NULL)
        AND (comment IS NULL OR char_length(comment) BETWEEN 1 AND 1000)
        AND (comment_asked_at IS NULL OR state = 'answered')
        AND (comment_at IS NULL OR comment_asked_at IS NOT NULL));

CREATE INDEX IF NOT EXISTS case_csat_asked
    ON case_csat (tenant_id, job_id, asked_at) WHERE asked_at IS NOT NULL;

-- the request moves through its states; the rest of the row stays as inserted
GRANT UPDATE (state, asked_at, expires_at, dismissed_at, comment, comment_asked_at, comment_at)
    ON case_csat TO agent_app;
