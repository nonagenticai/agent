-- 0137 — KH T7: `agent_runtime` may read the two `provision_outbox` columns the runner's knowledge search needs.
-- Forward-only (pre-prod), drift-tolerant (REVOKE + GRANT of exactly these columns). Column-level only.
--
-- The runner's `knowledge.answer` / `knowledge.search` (`runtime::runner::builtin` → `knowledge::kinds::search` →
-- `knowledge::stores::all`) reads each store's `ensuring` flag: an EXISTS over `provision_outbox` filtered on `op`
-- and `dedupe_key`. 0123 granted the gate's columns only (id, tenant_id, target, status, created_at), so a kind bound
-- to a tenant leaf failed 42501 in the runtime (the case ended `failed`) while the same search as agent_app passed.
-- `payload` and every other column stay unreadable.
REVOKE SELECT (op, dedupe_key) ON provision_outbox FROM agent_runtime;
GRANT SELECT (op, dedupe_key) ON provision_outbox TO agent_runtime;
