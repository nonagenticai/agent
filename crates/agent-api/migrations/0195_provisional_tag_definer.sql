-- 0195 (B8 T11; integration review 2026-10-27): the platform role stays INSERT-only on tags. 0192 granted
-- `agent_platform` DELETE on `layer_tag` / `layer_tag_row` so a generator run could replace its provisional `v0`
-- (0041 anticipates that: "a generator draft, B8 replaces it"); a table-wide DELETE grant is broader than that one
-- need, and `platform_role_inserts_never_deletes` pins the platform role as insert-only (seeds and tags are history).
--
-- - REVOKE the DELETE again.
-- - `platform_drop_provisional_tag(family)`: SECURITY DEFINER, deletes ONLY the family's provisional v0 and its
--   members; the platform role gets EXECUTE on it and nothing else. The 0041 immutability triggers stay as a backstop.
--
-- Forward-only. Needs 0041, 0192.

REVOKE DELETE ON layer_tag, layer_tag_row FROM agent_platform;

CREATE OR REPLACE FUNCTION platform_drop_provisional_tag(p_family text)
    RETURNS void
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = pg_catalog, public, pg_temp
    AS $$
        DELETE FROM layer_tag_row
         WHERE family = p_family AND version = 0
           AND EXISTS (SELECT 1 FROM layer_tag t WHERE t.family = p_family AND t.version = 0 AND t.provisional);
        DELETE FROM layer_tag WHERE family = p_family AND version = 0 AND provisional;
    $$;

REVOKE ALL ON FUNCTION platform_drop_provisional_tag(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION platform_drop_provisional_tag(text) TO agent_platform;
