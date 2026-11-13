-- 0098 — external ticket reference on a case (B7 remaining, workstream E, R10). Forward-only (pre-prod),
-- drift-tolerant like 0120 (IF NOT EXISTS).
--
-- `external_ref`: the vendor ticket a connector `case.create` opened for this case (e.g. a Frappe `HD Ticket` name,
-- a Zendesk ticket id), so the case can be linked to it later (B4 wires the write; R9 resolve/close finds the case by
-- it). Free text: the id shape is the vendor's. NULL = no external ticket. One case links at most one ticket; the
-- index serves the reverse lookup (vendor webhook → case) per tenant and job.
--
-- Production applies this AFTER 0124 (fresh databases: in number order). References only 0060 (`agent_case`).

ALTER TABLE agent_case ADD COLUMN IF NOT EXISTS external_ref text
    CHECK (external_ref IS NULL OR (btrim(external_ref) <> '' AND char_length(external_ref) <= 200));
CREATE INDEX IF NOT EXISTS agent_case_external_ref ON agent_case (tenant_id, job_id, external_ref)
    WHERE external_ref IS NOT NULL;
