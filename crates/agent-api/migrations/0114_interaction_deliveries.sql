-- 0114 — durable async interactions (B13 §1 row 6, §2): one row per `POST /api/jobs/{job}/interactions` with a
-- `callback_url`, so a queued answer and its signed callback survive an agent-api restart. Forward-only (pre-prod),
-- drift-tolerant like 0120: IF NOT EXISTS, every constraint / policy dropped and re-added. References only ≤0033
-- objects (`tenants`, `jobs`, `app_current_tenant()`, `app_current_job()`).
--
-- `id` is the interaction id (`int_<id>` on the wire, the `webhook-id` of every attempt). `request` is the translated
-- request (customer text: never logged), `caller` who submitted it, `answer` the callback body once computed (a resume
-- never recomputes it). States: queued → answering → delivering → delivered | failed. A worker claims due rows across
-- tenants through `agent_interaction_deliveries_due` and holds them by `lease_until` (60 s, renewed by the worker's
-- heartbeat); an expired lease is claimed again (restart resume). `claims` counts the claims (the worker's fence
-- token, and a row claimed too often while answering is failed `too_many_claims`).

CREATE TABLE IF NOT EXISTS interaction_deliveries (
    id              uuid PRIMARY KEY,
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL,
    caller          jsonb NOT NULL DEFAULT '{}',
    request         jsonb NOT NULL,
    callback_url    text NOT NULL,
    state           text NOT NULL DEFAULT 'queued',
    answer          jsonb,
    attempts        integer NOT NULL DEFAULT 0,
    claims          integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    lease_until     timestamptz,
    last_error      text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE interaction_deliveries ADD COLUMN IF NOT EXISTS claims integer NOT NULL DEFAULT 0;

ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_tenant;
ALTER TABLE interaction_deliveries
    ADD CONSTRAINT interaction_deliveries_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_job;
ALTER TABLE interaction_deliveries
    ADD CONSTRAINT interaction_deliveries_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id);
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_state_check;
ALTER TABLE interaction_deliveries ADD CONSTRAINT interaction_deliveries_state_check
    CHECK (state IN ('queued', 'answering', 'delivering', 'delivered', 'failed'));
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_caller_check;
ALTER TABLE interaction_deliveries
    ADD CONSTRAINT interaction_deliveries_caller_check CHECK (jsonb_typeof(caller) = 'object');
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_request_check;
ALTER TABLE interaction_deliveries
    ADD CONSTRAINT interaction_deliveries_request_check CHECK (jsonb_typeof(request) = 'object');
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_url_check;
ALTER TABLE interaction_deliveries ADD CONSTRAINT interaction_deliveries_url_check
    CHECK (btrim(callback_url) <> '' AND char_length(callback_url) <= 2048);
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_attempts_check;
ALTER TABLE interaction_deliveries
    ADD CONSTRAINT interaction_deliveries_attempts_check CHECK (attempts >= 0);
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_claims_check;
ALTER TABLE interaction_deliveries
    ADD CONSTRAINT interaction_deliveries_claims_check CHECK (claims >= 0);
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_last_error_check;
ALTER TABLE interaction_deliveries ADD CONSTRAINT interaction_deliveries_last_error_check
    CHECK (last_error IS NULL OR char_length(last_error) <= 500);
-- An answer exists once the row has left `answering`.
ALTER TABLE interaction_deliveries DROP CONSTRAINT IF EXISTS interaction_deliveries_answer_check;
ALTER TABLE interaction_deliveries ADD CONSTRAINT interaction_deliveries_answer_check
    CHECK (state NOT IN ('delivering', 'delivered') OR answer IS NOT NULL);

-- The claim scan (non-final rows by due time) and the per-tenant pending count.
CREATE INDEX IF NOT EXISTS interaction_deliveries_due_idx
    ON interaction_deliveries (next_attempt_at) WHERE state IN ('queued', 'answering', 'delivering');
CREATE INDEX IF NOT EXISTS interaction_deliveries_pending_idx
    ON interaction_deliveries (tenant_id) WHERE state IN ('queued', 'answering', 'delivering');
CREATE INDEX IF NOT EXISTS interaction_deliveries_final_idx
    ON interaction_deliveries (updated_at) WHERE state IN ('delivered', 'failed');

ALTER TABLE interaction_deliveries ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON interaction_deliveries;
CREATE POLICY tenant_isolation ON interaction_deliveries FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON interaction_deliveries;
CREATE POLICY job_scope ON interaction_deliveries AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON interaction_deliveries FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON interaction_deliveries TO agent_app;

-- Claim (pattern `provision_claim`, 0035): up to `p_limit` non-final rows of ACTIVE tenants (a suspended or archived
-- tenant's rows wait) that are due and not leased (or whose lease expired: a worker that died), oldest due first,
-- SKIP LOCKED; at most 8 rows per tenant are leased at once, the ones it already holds counted (one tenant cannot
-- take every worker). Each gets a 60 s lease (the worker renews it every 20 s while it works the row) and `claims`
-- + 1. Nothing else changes: the worker moves the state. Claims are serialized by an advisory lock so two
-- processes cannot both fill a tenant's 8. Cross-tenant, so definer.
DROP FUNCTION IF EXISTS agent_interaction_deliveries_due(int);
CREATE FUNCTION agent_interaction_deliveries_due(p_limit int) RETURNS SETOF interaction_deliveries
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    SELECT pg_advisory_xact_lock(hashtextextended('interaction-deliveries:claim', 0));
    WITH held AS (
        SELECT h.tenant_id, count(*) AS n
          FROM interaction_deliveries h
         WHERE h.state IN ('queued', 'answering', 'delivering')
           AND h.lease_until >= now()
         GROUP BY h.tenant_id
    ), ranked AS (
        SELECT d.id,
               row_number() OVER (PARTITION BY d.tenant_id ORDER BY d.next_attempt_at, d.id)
                 + coalesce(held.n, 0) AS slot
          FROM interaction_deliveries d
          JOIN tenants t ON t.id = d.tenant_id AND t.status = 'active'
          LEFT JOIN held ON held.tenant_id = d.tenant_id
         WHERE d.state IN ('queued', 'answering', 'delivering')
           AND d.next_attempt_at <= now()
           AND (d.lease_until IS NULL OR d.lease_until < now())
    ), due AS (
        SELECT d.id
          FROM interaction_deliveries d
          JOIN ranked r ON r.id = d.id
         WHERE r.slot <= 8
         ORDER BY d.next_attempt_at, d.id
         LIMIT greatest(p_limit, 0)
         FOR UPDATE OF d SKIP LOCKED
    )
    UPDATE interaction_deliveries u
       SET lease_until = now() + interval '60 seconds',
           claims = u.claims + 1,
           updated_at = now()
      FROM due
     WHERE u.id = due.id
    RETURNING u.*
$$;
REVOKE ALL ON FUNCTION agent_interaction_deliveries_due(int) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_interaction_deliveries_due(int) TO agent_app;

-- Retention: rows in a final state for more than 7 days are deleted (the worker's tick). Cross-tenant, so definer;
-- answers the number deleted.
DROP FUNCTION IF EXISTS agent_interaction_deliveries_prune();
CREATE FUNCTION agent_interaction_deliveries_prune() RETURNS bigint
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    WITH archived AS (
        -- an archived tenant's pending rows are never claimed (the claim takes active tenants only): close them
        UPDATE interaction_deliveries d
           SET state = 'failed', last_error = 'tenant_archived', lease_until = NULL, updated_at = now()
          FROM tenants t
         WHERE t.id = d.tenant_id AND t.status = 'archived'
           AND d.state NOT IN ('delivered', 'failed')
        RETURNING 1
    ), gone AS (
        DELETE FROM interaction_deliveries
         WHERE state IN ('delivered', 'failed') AND updated_at < now() - interval '7 days'
        RETURNING 1
    )
    SELECT count(*) FROM gone -- `archived` runs too: Postgres always executes data-modifying CTEs
$$;
REVOKE ALL ON FUNCTION agent_interaction_deliveries_prune() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_interaction_deliveries_prune() TO agent_app;
