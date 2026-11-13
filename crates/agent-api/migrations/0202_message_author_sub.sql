-- 0202 — the staff author of a message (Staff Workspace T6, a T4 review follow-up). Forward-only (pre-prod). References
-- only 0061 `conversation_message`, so it applies after the reserved B8 range 0191–0199 and after 0200 / 0201 in either
-- order.
--
-- `author_sub`: the sub of the staff member who wrote a staff reply or a note (`store::append_client`, written by the
-- one staff-message path, crate::conversations::staff::send). `author` keeps the display name the transcript shows. A
-- failed reply is delivered again only by its author: the retry reads this column (it used to read the
-- `conversation.staff_reply` audit row). NULL for every other row, and for staff rows stored before 0202.
--
-- Grants: none. 0061 grants `agent_app` SELECT, INSERT, UPDATE and 0065 grants `agent_runtime` SELECT, INSERT on the
-- whole table, which covers the new column.

ALTER TABLE conversation_message ADD COLUMN IF NOT EXISTS author_sub text;
ALTER TABLE conversation_message DROP CONSTRAINT IF EXISTS conversation_message_author_sub_check;
ALTER TABLE conversation_message ADD CONSTRAINT conversation_message_author_sub_check
    CHECK (author_sub IS NULL OR btrim(author_sub) <> '');
