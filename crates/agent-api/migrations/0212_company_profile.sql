-- 0212 — the company profile is a tenant setting (D124, spec 2026-10-27-tenant-job-design). Forward-only (pre-prod).
-- References only `tenants` (0001, 0032: `industry`, `description`). `tenants` is T-class (RLS `tenant_self`): the
-- runtime role already holds table-wide SELECT/UPDATE (0001), so the new columns need no new grant.
--
-- * `departments`: the department layer keys of the company (`industry_dept/<industry>/<dept>`), `{}` until set.
--   Which industry each belongs to is checked by `PUT /api/tenant/profile` against the selection options, not here.
-- * `region`: a `dimension_value` region key (D111), NULL until set; the API checks it against the vocabulary.
-- * `profile_updated_at` / `profile_updated_by`: the optimistic-concurrency token of the profile (`expected_updated_at`)
--   and who set it. `tenants.updated_at` is bumped by renames and lifecycle changes too, so it is no usable token.
ALTER TABLE tenants
    ADD COLUMN departments        text[] NOT NULL DEFAULT '{}',
    ADD COLUMN region             text,
    ADD COLUMN profile_updated_at timestamptz,
    ADD COLUMN profile_updated_by text;

-- Each element is `industry_dept/<industry>/<dept>`: the array is joined by newlines and matched as a whole (a CHECK
-- cannot hold a subquery); NULL elements are refused separately (array_to_string skips them).
ALTER TABLE tenants
    ADD CONSTRAINT tenants_departments_check CHECK (
        cardinality(departments) <= 64
        AND array_position(departments, NULL) IS NULL
        AND array_to_string(departments, E'\n')
            ~ E'^(industry_dept/[a-z0-9_]+/[a-z0-9_]+(\\nindustry_dept/[a-z0-9_]+/[a-z0-9_]+)*)?$'),
    ADD CONSTRAINT tenants_region_check CHECK (region IS NULL OR char_length(region) BETWEEN 1 AND 80),
    ADD CONSTRAINT tenants_profile_updated_by_check CHECK (profile_updated_by IS NULL OR btrim(profile_updated_by) <> '');
