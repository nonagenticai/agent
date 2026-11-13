-- 0211 — handover items: `urgent` priority and `collected` as the array the staff console reads. Forward-only,
-- drift-tolerant (IF EXISTS / re-runnable). References only 0063 `agent_handover_items`.
--
-- 1. `collected` was a JSON object (0063); the console reads an ARRAY of {label, value, verified} and the runtime now
--    inserts that array. Existing rows are converted, not dropped: `{}` becomes `[]`, a non-empty object becomes one
--    {label: key, value: value-as-text, verified: false} entry per key.
-- 2. `priority` gains `urgent` (above high). The SLA due time is not stored by priority in SQL: it is stamped by
--    `handover::policy::Targets::of`, where urgent is half of high. The runtime INSERT grant (0065) is table-wide, so
--    it already covers `collected` and `priority`.
ALTER TABLE agent_handover_items DROP CONSTRAINT IF EXISTS agent_handover_items_collected_check;
ALTER TABLE agent_handover_items ALTER COLUMN collected SET DEFAULT '[]'::jsonb;

UPDATE agent_handover_items
   SET collected = CASE
           WHEN collected = '{}'::jsonb THEN '[]'::jsonb
           ELSE (SELECT COALESCE(jsonb_agg(jsonb_build_object('label', e.key, 'value', e.value::text, 'verified', false)
                                           ORDER BY e.key), '[]'::jsonb)
                   FROM jsonb_each(collected) AS e)
       END
 WHERE jsonb_typeof(collected) = 'object';

ALTER TABLE agent_handover_items
    ADD CONSTRAINT agent_handover_items_collected_check CHECK (jsonb_typeof(collected) = 'array');

ALTER TABLE agent_handover_items DROP CONSTRAINT IF EXISTS agent_handover_items_priority_check;
ALTER TABLE agent_handover_items
    ADD CONSTRAINT agent_handover_items_priority_check CHECK (priority IN ('urgent', 'high', 'normal', 'low'));
