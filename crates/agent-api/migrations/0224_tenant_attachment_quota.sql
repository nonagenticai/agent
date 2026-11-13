-- 0224 — SW-9 follow-up (owner 2026-10-28): the attachment storage quota is a per-tenant setting, not only an env.
-- Forward-only.
--
-- `tenants.attachment_quota_bytes` NULL = the platform default (env `ATTACHMENT_QUOTA_BYTES`, 1 GiB unset). Only a
-- platform super_admin sets it (`PATCH /api/tenants/{id}`, crate::tenants::lifecycle::set_attachment_quota); the
-- tenant's own admin reads it with the current usage on the job's Channels page. `agent_app` already holds a
-- table-wide UPDATE grant on `tenants` (0001), so the database cannot tell that handler from other app code: the
-- handler is the only writer, by construction (as for 0207 `support_auto_assign`).

ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS attachment_quota_bytes bigint
        CHECK (attachment_quota_bytes IS NULL OR attachment_quota_bytes BETWEEN 1 AND 1099511627776),
    ADD COLUMN IF NOT EXISTS attachment_quota_set_by text
        CHECK (attachment_quota_set_by IS NULL OR btrim(attachment_quota_set_by) <> ''),
    ADD COLUMN IF NOT EXISTS attachment_quota_set_at timestamptz;
