-- 0115 — names of staff who are not tenant members (B13 live acceptance, 2026-10-21). Forward-only (pre-prod),
-- drift-tolerant like 0124: every object is created IF NOT EXISTS and every constraint is dropped and re-added.
--
-- A platform super_admin acting in a tenant has no `users` row there, so every name lookup that reads `users` only
-- (handover assignee, History, transcript author, answer feedback) showed the Keycloak sub. `actor_names` remembers
-- the name such a caller presented (`<preferred_username> (platform)`), keyed by sub, across tenants.
--
-- `actor_names` (Definer class): reached only through `agent_actor_name_seen` (write, from the staff write paths) and
-- `agent_actor_name` (read). Lookups resolve `users` (display name, else email) → `agent_actor_name(sub)` → the sub.
-- References nothing.

CREATE TABLE IF NOT EXISTS actor_names (
    sub        text PRIMARY KEY,
    name       text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE actor_names DROP CONSTRAINT IF EXISTS actor_names_name_check;
ALTER TABLE actor_names ADD CONSTRAINT actor_names_name_check CHECK (char_length(name) BETWEEN 1 AND 200);
ALTER TABLE actor_names ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON actor_names FROM PUBLIC;
REVOKE ALL ON actor_names FROM agent_app;
REVOKE ALL ON actor_names FROM agent_platform;

-- Remember the name of `p_sub` (trimmed, at most 200 characters). No-op when the name is blank, is the sub itself, or
-- is unchanged (no write, so `updated_at` keeps the last real change).
DROP FUNCTION IF EXISTS agent_actor_name_seen(text, text);
CREATE FUNCTION agent_actor_name_seen(p_sub text, p_name text)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        INSERT INTO actor_names AS a (sub, name)
        SELECT p_sub, left(btrim(p_name), 200)
         WHERE btrim(coalesce(p_sub, '')) <> ''
           AND btrim(coalesce(p_name, '')) <> ''
           AND btrim(p_name) <> p_sub
        ON CONFLICT (sub) DO UPDATE SET name = EXCLUDED.name, updated_at = now()
         WHERE a.name IS DISTINCT FROM EXCLUDED.name
    $$;
REVOKE ALL ON FUNCTION agent_actor_name_seen(text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_actor_name_seen(text, text) TO agent_app;

-- The remembered name of `p_sub`, else NULL.
DROP FUNCTION IF EXISTS agent_actor_name(text);
CREATE FUNCTION agent_actor_name(p_sub text)
    RETURNS text
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT name FROM actor_names WHERE sub = p_sub
    $$;
REVOKE ALL ON FUNCTION agent_actor_name(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_actor_name(text) TO agent_app;
-- platform read models resolve names too (`crate::members::names`)
GRANT EXECUTE ON FUNCTION agent_actor_name(text) TO agent_platform;
