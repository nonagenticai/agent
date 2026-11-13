-- 0203 — canned replies of a job (Staff Workspace T7). Forward-only (pre-prod). References only 0001 `tenants`, 0033
-- `jobs` and the 0061 RLS helpers, so it applies after the reserved B8 range 0191–0199 and after 0200–0202 in any order.
--
-- A saved reply a staff member inserts into the composer (`GET /api/jobs/{job}/canned-replies`, any member of the
-- job's tenant; `POST` / `DELETE`, `support_manager`). Title 1–80 characters, body 1–4000 (trimmed, checked by the
-- route as 422 `invalid_canned_reply` and here as the backstop).
--
-- Grants: `agent_app` only. The runtime never reads canned replies.

CREATE TABLE support_canned_reply (
    id         uuid PRIMARY KEY DEFAULT uuidv7(),
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    title      text NOT NULL CHECK (char_length(btrim(title)) BETWEEN 1 AND 80),
    body       text NOT NULL CHECK (char_length(btrim(body)) BETWEEN 1 AND 4000),
    created_by text NOT NULL CHECK (btrim(created_by) <> ''),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT support_canned_reply_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE INDEX support_canned_reply_job_order ON support_canned_reply (tenant_id, job_id, updated_at, id);
ALTER TABLE support_canned_reply ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON support_canned_reply FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON support_canned_reply AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON support_canned_reply FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON support_canned_reply TO agent_app;
