-- 0184 — the known customer's display name on the web chat session (B7 staff view). The signed assertion's `name` was
-- verified at session create but never stored, so a KYC'd customer reached staff as "Web visitor · xxxx". Forward-only
-- (pre-prod), drift-tolerant like 0183: column IF NOT EXISTS, constraint dropped and re-added. References only 0124 and
-- 0183 (`web_chat_session`, `customer_ref`). `agent_app` already holds table-level INSERT/UPDATE/SELECT on
-- `web_chat_session` (0124), so no grant is needed.
--
-- `customer_name` is DISPLAY data only: set once with `customer_ref` at session create, copied onto the conversation's
-- `customer_name` (0061) by the inbound path, never logged and never put in an audit row. NULL = no name asserted.
ALTER TABLE web_chat_session ADD COLUMN IF NOT EXISTS customer_name text;

ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_customer_name_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_customer_name_check
    CHECK (customer_name IS NULL
           OR (customer_ref IS NOT NULL AND btrim(customer_name) <> '' AND char_length(customer_name) <= 200));
