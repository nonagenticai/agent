-- 0063 — the handover queue (D64; B4 T2/T16a shapes, pulled forward by B12 §1). Forward-only (pre-prod).
--
-- Deltas to B4: `release_id` / `draft_rev` nullable with no FK (B2 is not merged); widened channel CHECK; B4's 0066
-- ALTER columns (`outside_hours`, `callback`, `clock_starts_at`) are created here. The SLA policy is B2 ledger kind
-- `handover_policy`, not a table. References only ≤0033 objects and 0060.

CREATE TABLE agent_handover_items (
    tenant_id          uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id             uuid NOT NULL DEFAULT app_current_job(),
    id                 text NOT NULL CHECK (id ~ '^HO-[1-9][0-9]*$'),
    case_id            text NOT NULL,
    conversation_id    text NOT NULL,
    env                text NOT NULL CHECK (env IN ('prod', 'test')),
    case_type          text NOT NULL DEFAULT 'handover'
                            CHECK (case_type IN ('handover', 'complaint', 'return', 'warranty_claim')),
    subject            text NOT NULL DEFAULT '' CHECK (char_length(subject) <= 300),
    customer           text NOT NULL DEFAULT '',
    order_id           text,
    channel            text NOT NULL,
    -- The 8 reasons: D64's six, `staff_decision` (D85), `gate_unavailable` (D94).
    reason             text NOT NULL CHECK (reason IN (
                            'identity_failed', 'gate_blocked', 'customer_asked', 'tool_failed', 'unknown_outcome',
                            'out_of_scope', 'staff_decision', 'gate_unavailable')),
    trigger            text NOT NULL DEFAULT '',
    priority           text NOT NULL DEFAULT 'normal' CHECK (priority IN ('high', 'normal', 'low')),
    state              text NOT NULL DEFAULT 'queued' CHECK (state IN (
                            'queued', 'assigned', 'waiting_customer', 'resolved', 'returned_to_agent')),
    queue              text NOT NULL DEFAULT '',
    queued_at          timestamptz NOT NULL DEFAULT now(),
    first_response_at  timestamptz,
    closed_at          timestamptz,
    first_response_due timestamptz,
    resolve_due        timestamptz,
    assignee           text,
    release_id         text,
    draft_rev          bigint CHECK (draft_rev IS NULL OR draft_rev >= 0),
    confidence         real CHECK (confidence IS NULL OR confidence BETWEEN 0 AND 1),
    agent_did          jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(agent_did) = 'array'),
    collected          jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(collected) = 'object'),
    suggested_next     text,
    proposal_hint      jsonb,
    -- F-14 (B4 0066): raised out of hours; the SLA clock then starts at the next open time.
    outside_hours      boolean NOT NULL DEFAULT false,
    -- The call-back the customer accepted out of hours (NULL = none).
    callback           jsonb CHECK (callback IS NULL OR jsonb_typeof(callback) = 'object'),
    clock_starts_at    timestamptz,
    PRIMARY KEY (tenant_id, job_id, id),
    CONSTRAINT agent_handover_items_channel_check CHECK (channel IN (
        'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api')),
    CONSTRAINT agent_handover_items_closed_check
        CHECK ((state IN ('resolved', 'returned_to_agent')) = (closed_at IS NOT NULL)),
    CONSTRAINT agent_handover_items_case FOREIGN KEY (tenant_id, job_id, case_id)
        REFERENCES agent_case (tenant_id, job_id, case_id)
);
-- One open item per case.
CREATE UNIQUE INDEX agent_handover_items_one_open ON agent_handover_items (tenant_id, job_id, case_id)
    WHERE state NOT IN ('resolved', 'returned_to_agent');
CREATE INDEX agent_handover_items_queue ON agent_handover_items (tenant_id, job_id, state, queued_at);
CREATE INDEX agent_handover_items_conversation ON agent_handover_items (tenant_id, job_id, conversation_id);
ALTER TABLE agent_handover_items ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON agent_handover_items FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON agent_handover_items AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON agent_handover_items TO agent_app;

-- Every transition and (re)assignment (D64). Append-only: no UPDATE or DELETE grant.
CREATE TABLE agent_handover_events (
    seq        bigserial PRIMARY KEY,
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    item_id    text NOT NULL,
    at         timestamptz NOT NULL DEFAULT now(),
    -- `agent` for the enqueue, otherwise the person's sub.
    actor      text NOT NULL CHECK (btrim(actor) <> ''),
    from_state text CHECK (from_state IS NULL OR from_state IN (
                   'queued', 'assigned', 'waiting_customer', 'resolved', 'returned_to_agent')),
    to_state   text NOT NULL CHECK (to_state IN (
                   'queued', 'assigned', 'waiting_customer', 'resolved', 'returned_to_agent')),
    assignee   text,
    note       text NOT NULL DEFAULT '',
    CONSTRAINT agent_handover_events_item FOREIGN KEY (tenant_id, job_id, item_id)
        REFERENCES agent_handover_items (tenant_id, job_id, id)
);
CREATE INDEX agent_handover_events_item_seq ON agent_handover_events (tenant_id, job_id, item_id, seq);
ALTER TABLE agent_handover_events ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON agent_handover_events FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON agent_handover_events AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT ON agent_handover_events TO agent_app;
GRANT USAGE ON SEQUENCE agent_handover_events_seq_seq TO agent_app;

-- Staff feedback on a handover (live data; B8 drafts proposals from it).
CREATE TABLE agent_handover_feedback (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    item_id    text NOT NULL,
    verdict    text NOT NULL CHECK (verdict IN ('agent_should_handle', 'right_to_hand_over')),
    note       text NOT NULL DEFAULT '' CHECK (char_length(note) <= 2000),
    by_sub     text NOT NULL CHECK (btrim(by_sub) <> ''),
    at         timestamptz NOT NULL DEFAULT now(),
    proposal   jsonb,
    drafted_at timestamptz,
    PRIMARY KEY (tenant_id, job_id, item_id),
    CONSTRAINT agent_handover_feedback_item FOREIGN KEY (tenant_id, job_id, item_id)
        REFERENCES agent_handover_items (tenant_id, job_id, id)
);
ALTER TABLE agent_handover_feedback ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON agent_handover_feedback FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON agent_handover_feedback AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON agent_handover_feedback TO agent_app;

-- The per-job item counter behind `HO-<n>`.
CREATE TABLE agent_handover_seq (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id    uuid NOT NULL DEFAULT app_current_job(),
    next      bigint NOT NULL CHECK (next >= 1),
    PRIMARY KEY (tenant_id, job_id),
    CONSTRAINT agent_handover_seq_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
ALTER TABLE agent_handover_seq ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON agent_handover_seq FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY job_scope ON agent_handover_seq AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
GRANT SELECT, INSERT, UPDATE ON agent_handover_seq TO agent_app;

-- The next handover id of the scoped job: `HO-1`, `HO-2`, … per job (as `agent_next_case_id`).
CREATE FUNCTION agent_next_handover_id() RETURNS text
    LANGUAGE sql VOLATILE
    SET search_path = public, pg_temp
    AS $$
        INSERT INTO agent_handover_seq AS s (tenant_id, job_id, next)
             VALUES (app_current_tenant(), app_current_job(), 2)
        ON CONFLICT (tenant_id, job_id) DO UPDATE SET next = s.next + 1
        RETURNING 'HO-' || (s.next - 1)
    $$;
REVOKE ALL ON FUNCTION agent_next_handover_id() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_next_handover_id() TO agent_app;
