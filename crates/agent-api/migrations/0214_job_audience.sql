-- 0214 — D129 (tenant job design): who a job serves is a job setting. Forward-only (pre-prod).
--
-- `jobs.audience`: `customers` (default; the tenant's customers write in) or `employees` (the tenant's own signed-in
-- staff: IT desk, HR). 'staff' stays reserved for the escalation side (D125). Set by a tenant_admin on
-- `POST /api/jobs` and `PATCH /api/jobs/{job}`; the runtime reads this column instead of inferring the audience
-- from layer keys.

ALTER TABLE jobs ADD COLUMN audience text NOT NULL DEFAULT 'customers' CHECK (audience IN ('customers', 'employees'));
