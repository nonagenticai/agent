-- 0124 — web chat sessions, durable channel rate limits and abuse events (B13 §1 rows 2–3, §3). Forward-only
-- (pre-prod), drift-tolerant like 0120: every object is created IF NOT EXISTS and every constraint / policy is dropped
-- and re-added.
--
-- 1. `web_chat_session` (Tenant class): one anonymous widget session on a core web chat binding. The token is shown
--    once (`<session_id>.<base64url 32 bytes>`); only its sha256 is stored. It slides 24 h on every message, capped at
--    7 days after creation. One session is one conversation (`conversation_id`, set on its first message).
--    `blocked_at` + `blocked_reason`: `rotated` (the widget key was rotated: for good) or `rate_limited` /
--    `repeat_text` (abuse: for 1 h).
-- 2. `channel_rate_counter` (Definer class): fixed-window counters (`key`, `window_start`), reached only through
--    `agent_channel_rate_hit`; rows older than 2 h are swept every 10 minutes (`agent_channel_counters_sweep`, which
--    also deletes sessions expired for over a day).
-- 3. `channel_abuse_event` (Definer class): one row per refusal of a session (`rate_limited`) or repeated text
--    (`repeat_text`), reached only through `agent_channel_abuse`.
--
-- No Redis (B4 T1 is not built): the counters live here, so they survive an edge or agent-api restart. References
-- only ≤0033 objects, 0061 (`conversation`) and 0120 (`connector_bindings`).

CREATE TABLE IF NOT EXISTS web_chat_session (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    binding_id      uuid NOT NULL,
    token_hash      bytea NOT NULL,
    party_id        uuid,
    conversation_id text,
    origin          text NOT NULL,
    ip_hash         bytea NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_seen_at    timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    blocked_at      timestamptz,
    blocked_reason  text
);

ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_tenant;
ALTER TABLE web_chat_session
    ADD CONSTRAINT web_chat_session_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_job;
ALTER TABLE web_chat_session
    ADD CONSTRAINT web_chat_session_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id);
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_binding;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_binding
    FOREIGN KEY (binding_id) REFERENCES connector_bindings (id) ON DELETE CASCADE;
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_conversation;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_conversation
    FOREIGN KEY (tenant_id, job_id, conversation_id) REFERENCES conversation (tenant_id, job_id, conversation_id);
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_token;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_token UNIQUE (token_hash);
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_hash_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_hash_check
    CHECK (octet_length(token_hash) = 32 AND octet_length(ip_hash) = 32);
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_origin_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_origin_check
    CHECK (btrim(origin) <> '' AND char_length(origin) <= 300);
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_blocked_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_blocked_check CHECK (
    (blocked_at IS NULL) = (blocked_reason IS NULL)
    AND (blocked_reason IS NULL OR blocked_reason IN ('rotated', 'rate_limited', 'repeat_text')));
ALTER TABLE web_chat_session DROP CONSTRAINT IF EXISTS web_chat_session_expiry_check;
ALTER TABLE web_chat_session ADD CONSTRAINT web_chat_session_expiry_check
    CHECK (expires_at <= created_at + interval '7 days');
CREATE INDEX IF NOT EXISTS web_chat_session_binding_idx ON web_chat_session (tenant_id, binding_id, expires_at);
CREATE INDEX IF NOT EXISTS web_chat_session_expires_idx ON web_chat_session (expires_at);

ALTER TABLE web_chat_session ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON web_chat_session;
CREATE POLICY tenant_isolation ON web_chat_session FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON web_chat_session;
CREATE POLICY job_scope ON web_chat_session AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON web_chat_session FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON web_chat_session TO agent_app;

CREATE TABLE IF NOT EXISTS channel_rate_counter (
    key          text NOT NULL,
    window_start timestamptz NOT NULL,
    count        integer NOT NULL DEFAULT 0,
    PRIMARY KEY (key, window_start)
);
ALTER TABLE channel_rate_counter DROP CONSTRAINT IF EXISTS channel_rate_counter_key_check;
ALTER TABLE channel_rate_counter ADD CONSTRAINT channel_rate_counter_key_check
    CHECK (btrim(key) <> '' AND char_length(key) <= 200 AND count >= 0);
CREATE INDEX IF NOT EXISTS channel_rate_counter_window_idx ON channel_rate_counter (window_start);
ALTER TABLE channel_rate_counter ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON channel_rate_counter FROM PUBLIC;
REVOKE ALL ON channel_rate_counter FROM agent_app;
REVOKE ALL ON channel_rate_counter FROM agent_platform;

CREATE TABLE IF NOT EXISTS channel_abuse_event (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    session_id uuid NOT NULL,
    kind       text NOT NULL,
    text_hash  bytea,
    at         timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE channel_abuse_event DROP CONSTRAINT IF EXISTS channel_abuse_event_kind_check;
ALTER TABLE channel_abuse_event ADD CONSTRAINT channel_abuse_event_kind_check
    CHECK (kind IN ('rate_limited', 'repeat_text'));
CREATE INDEX IF NOT EXISTS channel_abuse_event_session_idx ON channel_abuse_event (session_id, kind, at);
ALTER TABLE channel_abuse_event ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON channel_abuse_event FROM PUBLIC;
REVOKE ALL ON channel_abuse_event FROM agent_app;
REVOKE ALL ON channel_abuse_event FROM agent_platform;

-- One hit on the fixed window of `p_window_s` seconds that holds now (`INSERT … ON CONFLICT DO UPDATE SET count =
-- count + 1`): the count after the hit and the seconds until the window ends (at least 1).
DROP FUNCTION IF EXISTS agent_channel_rate_hit(text, integer);
CREATE FUNCTION agent_channel_rate_hit(p_key text, p_window_s integer)
    RETURNS TABLE (hits integer, retry_after_s integer)
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        INSERT INTO channel_rate_counter AS c (key, window_start, count)
        VALUES (p_key,
                date_bin(make_interval(secs => p_window_s), now(), timestamptz '1970-01-01 00:00:00+00'),
                1)
        ON CONFLICT (key, window_start) DO UPDATE SET count = c.count + 1
        RETURNING c.count,
                  greatest(1, ceil(extract(epoch FROM (c.window_start + make_interval(secs => p_window_s) - now()))))
                      ::integer
    $$;
REVOKE ALL ON FUNCTION agent_channel_rate_hit(text, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_channel_rate_hit(text, integer) TO agent_app;

-- Record one abuse event of a session; returns how many events of that kind the session has within `p_within_s`
-- seconds (this one included).
DROP FUNCTION IF EXISTS agent_channel_abuse(uuid, text, bytea, integer);
CREATE FUNCTION agent_channel_abuse(p_session uuid, p_kind text, p_text_hash bytea, p_within_s integer)
    RETURNS integer
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        n integer;
    BEGIN
        INSERT INTO channel_abuse_event (session_id, kind, text_hash) VALUES (p_session, p_kind, p_text_hash);
        SELECT count(*) INTO n FROM channel_abuse_event
         WHERE session_id = p_session AND kind = p_kind AND at > now() - make_interval(secs => p_within_s);
        RETURN n;
    END
    $$;
REVOKE ALL ON FUNCTION agent_channel_abuse(uuid, text, bytea, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_channel_abuse(uuid, text, bytea, integer) TO agent_app;

-- The 10-minute tick: counter windows that began more than 2 h ago, abuse events older than 7 days, sessions expired
-- more than a day ago (nothing references a session row: `channel_abuse_event.session_id` is a plain uuid that ages
-- out on its own; the day of grace keeps a just-expired token answering `session_invalid` from a known row). Owner
-- rights (no FORCE ROW LEVEL SECURITY), so the sweep spans every tenant. Returns the number of counter rows deleted.
DROP FUNCTION IF EXISTS agent_channel_counters_sweep();
CREATE FUNCTION agent_channel_counters_sweep()
    RETURNS integer
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        n integer;
    BEGIN
        DELETE FROM channel_rate_counter WHERE window_start < now() - interval '2 hours';
        GET DIAGNOSTICS n = ROW_COUNT;
        DELETE FROM channel_abuse_event WHERE at < now() - interval '7 days';
        DELETE FROM web_chat_session WHERE expires_at < now() - interval '1 day';
        RETURN n;
    END
    $$;
REVOKE ALL ON FUNCTION agent_channel_counters_sweep() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_channel_counters_sweep() TO agent_app;
