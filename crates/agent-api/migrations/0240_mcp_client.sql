-- 0240 — MCP client (durable side): per-tenant MCP binding token and long-running MCP tasks.
-- Forward-only (pre-prod). Ported from care-bus 0029_mcp_tasks.sql, adapted to the agent platform.
--
-- A tenant's MCP calls carry two credentials: its `mcp` licence (minted on demand, never stored: crate::tenants::app_licence)
-- and a BINDING token the MCP server issued for (tenant, product 'agent'). The binding token is a long-lived secret the
-- server shows once, so it is stored SEALED under the tenant DEK (crate::connectors::custody::seal_fields; AAD
-- tenant ‖ "mcp-binding" ‖ binding_id), exactly as a channel binding's credential. A long-running MCP tool comes back
-- as a durable TASK: the handle is kept in `mcp_tasks`, the runtime's poller polls it to a terminal state, staff answer
-- its questions and cancel it, and the outcome is handed to the conversation that started it (delivered_at).

-- 1. The sealed binding token. BOTH processes ensure it lazily (the API for the staff routes, the runtime for tool
--    calls during a conversation), so both get SELECT, INSERT and UPDATE; neither deletes. A runtime that has to seal
--    needs the tenant's DEK to exist already (it never mints one, 0123): the API process creates it.
CREATE TABLE tenant_mcp_binding (
    tenant_id    uuid        PRIMARY KEY DEFAULT app_current_tenant() REFERENCES tenants (id),
    binding_id   text        NOT NULL CHECK (btrim(binding_id) <> '' AND char_length(binding_id) <= 200),
    sealed_token bytea       NOT NULL,
    nonce        bytea       NOT NULL,
    dek_version  integer     NOT NULL CHECK (dek_version >= 1),
    -- 8 hex of HMAC under the DEK (custody::SealedBlob): the only thing ever shown of the token.
    fingerprint  text        NOT NULL,
    licence_id   text        CHECK (licence_id IS NULL OR btrim(licence_id) <> ''),
    product      text        NOT NULL DEFAULT 'agent' CHECK (btrim(product) <> ''),
    ensured_at   timestamptz NOT NULL DEFAULT now(),
    rotated_at   timestamptz
);
ALTER TABLE tenant_mcp_binding ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_mcp_binding FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON tenant_mcp_binding FROM PUBLIC;
REVOKE ALL ON tenant_mcp_binding FROM agent_app, agent_runtime;
GRANT SELECT, INSERT, UPDATE ON tenant_mcp_binding TO agent_app, agent_runtime;

-- 2. The task handle and its outcome.
CREATE TABLE mcp_tasks (
    tenant_id        uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id           uuid NOT NULL DEFAULT app_current_job(),
    -- The server's taskId; unique for the life of the task.
    task_id          text PRIMARY KEY CHECK (length(task_id) BETWEEN 1 AND 200),
    -- The operation (op_key) whose call started the task, and the OpRequest idempotency key that links it to the
    -- conversation / case that is waiting on it.
    operation        text NOT NULL CHECK (length(operation) BETWEEN 1 AND 200),
    idempotency_key  text NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 300),
    case_id          text,
    tool             text NOT NULL CHECK (length(tool) BETWEEN 1 AND 200),
    status           text NOT NULL DEFAULT 'working'
                     CHECK (status IN ('working', 'input_required', 'completed', 'cancelled', 'failed', 'lost')),
    status_message   text,
    -- When the poller next asks the server; a claim pushes it forward (the lease).
    poll_after       timestamptz NOT NULL DEFAULT now(),
    -- The last tasks/get answer (DetailedTask wire object: inputRequests while input_required).
    last_detail      jsonb,
    -- The terminal outcome (TaskOutcome::to_json), written in the statement that makes the row terminal.
    result           jsonb,
    cancel_requested boolean NOT NULL DEFAULT false,
    -- tasks/cancel was acknowledged; until then the poller retries it.
    cancel_sent      boolean NOT NULL DEFAULT false,
    created_by       text NOT NULL CHECK (btrim(created_by) <> ''),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    terminal_at      timestamptz,
    -- When the terminal outcome was handed to the conversation that started the task.
    delivered_at     timestamptz,
    CONSTRAINT mcp_tasks_tenant_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CONSTRAINT mcp_tasks_tenant_task_key UNIQUE (tenant_id, task_id),
    CONSTRAINT mcp_tasks_terminal CHECK ((status IN ('completed', 'cancelled', 'failed', 'lost')) = (terminal_at IS NOT NULL)),
    CONSTRAINT mcp_tasks_delivered CHECK (delivered_at IS NULL OR terminal_at IS NOT NULL)
);
CREATE INDEX mcp_tasks_due ON mcp_tasks (poll_after) WHERE status IN ('working', 'input_required');
CREATE INDEX mcp_tasks_job ON mcp_tasks (job_id, created_at DESC);
CREATE INDEX mcp_tasks_undelivered ON mcp_tasks (tenant_id, job_id, case_id) WHERE terminal_at IS NOT NULL AND delivered_at IS NULL;
CREATE INDEX mcp_tasks_idem ON mcp_tasks (tenant_id, job_id, idempotency_key);

-- One staff answer per input request key: a double submit is a no-op. `sent_at` is set once tasks/update was
-- acknowledged; the key is never reused by the server.
CREATE TABLE mcp_task_answers (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    task_id     text NOT NULL,
    input_key   text NOT NULL CHECK (length(input_key) BETWEEN 1 AND 300),
    -- The human who answered; the server is told `agent:<tenant>:<answered_by>`.
    answered_by text NOT NULL,
    -- The ElicitResult sent: {"action", "content"?}.
    response    jsonb NOT NULL,
    sent_at     timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, input_key),
    CONSTRAINT mcp_task_answers_task_fkey FOREIGN KEY (tenant_id, task_id) REFERENCES mcp_tasks (tenant_id, task_id)
        ON DELETE CASCADE,
    CONSTRAINT mcp_task_answers_tenant_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);

-- RLS as agent_case (0060): tenant isolation + restrictive job scope. Grants: the API (staff answer / cancel / list)
-- and the runtime (record_started during a turn, the poller) both read and write; nobody deletes.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['mcp_tasks', 'mcp_task_answers'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
        EXECUTE format('REVOKE ALL ON %I FROM PUBLIC', t);
        EXECUTE format('REVOKE ALL ON %I FROM agent_app, agent_runtime', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE ON %I TO agent_app, agent_runtime', t);
    END LOOP;
END $$;

-- The poller is not a tenant: this definer function is its one cross-tenant read+write. It leases up to p_limit due
-- tasks (non-terminal, poll_after passed) by pushing poll_after p_lease_s ahead, and returns only the scope needed to
-- continue under RLS. A poller that dies leaves the row to be claimed again when the lease passes.
CREATE FUNCTION agent_mcp_task_claim(p_lease_s integer, p_limit integer)
    RETURNS TABLE (tenant_id uuid, job_id uuid, task_id text)
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    BEGIN
        IF p_lease_s IS NULL OR p_lease_s < 5 OR p_lease_s > 3600 OR p_limit IS NULL OR p_limit < 1 OR p_limit > 100 THEN
            RAISE EXCEPTION 'agent_mcp_task_claim: a lease of 5..3600 s and a limit of 1..100 are required'
                USING ERRCODE = 'invalid_parameter_value';
        END IF;
        RETURN QUERY
        UPDATE mcp_tasks m
           SET poll_after = now() + make_interval(secs => p_lease_s)
         WHERE m.task_id IN (
                SELECT c.task_id FROM mcp_tasks c
                 WHERE c.status IN ('working', 'input_required') AND c.poll_after <= now()
                 ORDER BY c.poll_after
                 LIMIT p_limit
                 FOR UPDATE SKIP LOCKED)
        RETURNING m.tenant_id, m.job_id, m.task_id;
    END
    $$;
REVOKE ALL ON FUNCTION agent_mcp_task_claim(integer, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION agent_mcp_task_claim(integer, integer) FROM agent_app;
GRANT EXECUTE ON FUNCTION agent_mcp_task_claim(integer, integer) TO agent_runtime;
