-- 0230 — retire (and restore) an industry or a department (plan 2026-10-30-super-admin-platform-fixes T3). Forward-only.
--
-- A super_admin creates industries and departments as `industry` / `industry_dept` layers (createLayer) and may
-- later take one back without deleting it: `layer.retired_at` marks it. A retired industry or department leaves the
-- choices a tenant gets for a job (`release::compile::db_options`, its departments with a retired industry too);
-- tags, pins and releases that hold it keep compiling. Retiring is refused while a tenant job selects it
-- (409 `in_use`), so no job's saved selection names a retired layer. Restore clears the marker.
--
-- A marker column, as `job_template_tag.retired_at` (0228): nothing else on a layer row changes.
ALTER TABLE layer
    ADD COLUMN retired_at timestamptz,
    ADD COLUMN retired_by text CHECK (retired_by IS NULL OR btrim(retired_by) <> ''),
    ADD CONSTRAINT layer_retired_pair CHECK ((retired_at IS NULL) = (retired_by IS NULL)),
    ADD CONSTRAINT layer_retired_scope CHECK (retired_at IS NULL OR scope IN ('industry', 'industry_dept'));
GRANT UPDATE (retired_at, retired_by) ON layer TO agent_platform;
