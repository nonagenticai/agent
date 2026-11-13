-- 0128 — `agent_runtime` grants on Platform Knowledge's objects (0125–0127, agent#29; PK 0127's ruling Q3: B4 audits
-- the runtime's grants when it merges after PK). Forward-only (pre-prod), drift-tolerant (REVOKE ALL + GRANT).
--
-- Not in 0123: on a fresh database 0123 runs before 0125–0127, so it cannot name their objects. 0128 runs after them
-- in both orders (fresh number order; production: main's 0115 + 0125–0127 already applied, then B4's set).
--
-- The runner's `knowledge.search` / `knowledge.answer` (`runtime::runner::builtin` → `knowledge::kinds::search`,
-- connected as agent_runtime) reads, audited call by call:
--
-- - `release_pack_leaf` (0126): a release's (`Live`/`Release`) pinned pack leaves (`kinds::release_leaves`), and the
--   draft's fallback to the newest release (`kinds::leaf_release`). Immutable; read-only.
-- - `platform_pack_leaf` (0126): the draft's cached pack leaves (`kinds::cached_leaves`, `resolve::leaf_pins`).
--   Read-only: the cache refresh (`resolve::refresh_pins`, which rewrites `platform_pack` rows incl. `root_id` and
--   the leaf rows) stays agent_app's; in the runtime it fails 42501, is logged (`knowledge_pack_refresh_failed`) and
--   the uncached pins degrade, as when strata is down.
-- - `source`, `knowledge_upload` (0127 `supersedes`): the tenant's supersedes of the kind's tenant leaf
--   (`kinds::tenant_supersedes`): the filter and `supersedes` columns only, never a source's config or credentials
--   or an upload's object keys.
-- - `platform_pack.state`, `excluded_by` (0126): covered by 0065's table-level SELECT.

REVOKE ALL ON release_pack_leaf, platform_pack_leaf FROM agent_runtime;
GRANT SELECT ON release_pack_leaf, platform_pack_leaf TO agent_runtime;

REVOKE ALL ON source, knowledge_upload FROM agent_runtime;
GRANT SELECT (tenant_id, target_store_id, enabled, supersedes) ON source TO agent_runtime;
GRANT SELECT (tenant_id, store_id, supersedes) ON knowledge_upload TO agent_runtime;
