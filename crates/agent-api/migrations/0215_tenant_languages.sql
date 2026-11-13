-- 0215 — the company's languages are a tenant setting (D132, spec 2026-10-27-tenant-job-design). Forward-only (pre-prod).
-- References only `tenants` (0001). `tenants` is T-class (RLS `tenant_self`): the runtime role already holds
-- table-wide SELECT/UPDATE (0001), so the new column needs no new grant.
--
-- * `languages`: the language keys (job-language vocabulary of `routes/release/settings.rs`) a new job serves by
--   default; `POST /api/jobs` copies them into `jobs.languages` unless the request names its own. `{en}` until set.
--   The vocabulary is checked by `PUT /api/tenant/profile`, not here; the CHECK only keeps it non-empty.
ALTER TABLE tenants
    ADD COLUMN languages text[] NOT NULL DEFAULT '{en}',
    ADD CONSTRAINT tenants_languages_check CHECK (
        cardinality(languages) BETWEEN 1 AND 16 AND array_position(languages, NULL) IS NULL);
