-- 0154 — `agent_runtime` may record WHERE a write ran (`operation_call.instance_id`, `template`).
-- Forward-only (pre-prod), drift-tolerant (REVOKE + GRANT of exactly these columns). Column-level only.
--
-- A write's row is inserted `pending` before the upstream call (`runtime::runner::ops::insert_pending`) and finished
-- after it (`finish_pending`). 0065 granted UPDATE on the outcome columns only, so a finished write never carried the
-- instance and pinned template the executor answered: live 2026-10-22, `delivery.report_problem` on
-- `demo-retail-backends-test` listed `instance=null` while the reads of the same case named it. A read is inserted
-- finished (INSERT covers both columns) and was never affected.
--
-- References only 0062/0065 objects: the same grants in both orders (fresh: number order; production: after ≤0150).

REVOKE UPDATE (instance_id, template) ON operation_call FROM agent_runtime;
GRANT UPDATE (instance_id, template) ON operation_call TO agent_runtime;
