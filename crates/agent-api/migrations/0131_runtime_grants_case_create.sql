-- 0131 — `agent_runtime` grants for the runner's connector `case.create` (B4 F4; B7 workstream E R10, 0098).
-- Forward-only (pre-prod), drift-tolerant (REVOKE + GRANT of exactly these columns). Column-level only.
--
-- - `agent_case.external_ref` (0098): the runner stores the vendor ticket id (the `case_id` output) an instance
--   `case.create` answered (`runtime::runner::case_effects`). The case row is already readable (0065 table SELECT,
--   which covers `party_id` from 0112).
-- - `party_identity` (0112): the runner fills `case.create`'s `customer_email` from the case party's `email` identity,
--   verified first, then the most recently seen (`runtime::runner::party_email`, the query of
--   `conversations::handover`'s worker notify). `tenant_id` for the `tenant_isolation` policy (FOR ALL, no TO clause:
--   it applies to agent_runtime as to agent_app). Never `first_seen_at`, never a write; `party` stays unreadable.
--
-- Runs after 0098 and 0112 in both orders (fresh: number order; production: main's ≤0130 already applied).

REVOKE UPDATE (external_ref) ON agent_case FROM agent_runtime;
GRANT UPDATE (external_ref) ON agent_case TO agent_runtime;

REVOKE ALL ON party_identity FROM agent_runtime;
GRANT SELECT (tenant_id, party_id, channel, subject, verified, last_seen_at) ON party_identity TO agent_runtime;
