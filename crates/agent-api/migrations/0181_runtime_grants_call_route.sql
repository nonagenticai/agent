-- 0181 — `agent_runtime` may re-point `operation_call.route` when a duplicate write key re-arms an `error` row.
-- Forward-only (pre-prod), drift-tolerant (REVOKE + GRANT of exactly this column). Column-level only.
--
-- `insert_pending` re-arms a failed write (same idempotency key) to `pending` for the retry. 0065/0154 granted UPDATE
-- on the outcome columns + instance_id/template only, so the row kept the FAILED attempt's route while the retry
-- may be served by another: the audit row named the wrong route. The re-arm now assigns the retry's route.
--
-- References only 0062/0065 objects: the same grants in both orders.

REVOKE UPDATE (route) ON operation_call FROM agent_runtime;
GRANT UPDATE (route) ON operation_call TO agent_runtime;
