-- 0247 — a blank job pins the base department its department composes on (D116). Forward-only.
--
-- Blank jobs (D128) were created pinning base, `industry/<i>` and each `industry_dept/<i>/<d>`, never `base_dept/<d>`.
-- When a pinned tag was cut after that base department existed, the compile names it `unpinned_layer` and the job's
-- design never compiles: no agent roles, no activities, so the assistant never runs. `POST /api/jobs` now completes
-- the pins (`templates::complete_pins`); this repairs the jobs created before, pinning each missing `base_dept/<d>`
-- at its newest tag. A job whose pinned tags all predate that base department compiles without it (the compile's own
-- rule) and is left alone. On a fresh database `layer_tag` is empty here (the seeder runs later): nothing changes.
WITH dept AS (
    SELECT DISTINCT j.id, 'base_dept/' || split_part(p.pin ->> 'layer', '/', 3) AS family
      FROM jobs j
     CROSS JOIN LATERAL jsonb_array_elements(j.pins) AS p (pin)
     WHERE p.pin ->> 'layer' LIKE 'industry_dept/%/%'
),
need AS (
    SELECT d.id, d.family, (SELECT max(t.version) FROM layer_tag t WHERE t.family = d.family AND NOT t.provisional)
               AS version
      FROM dept d
      JOIN jobs j ON j.id = d.id
      JOIN layer l ON l.key = d.family AND l.family = l.key
     WHERE NOT j.pins @> jsonb_build_array(jsonb_build_object('layer', d.family))
       AND l.created_at <= (SELECT max(t.tagged_at)
                              FROM jsonb_array_elements(j.pins) AS p (pin)
                              JOIN layer_tag t ON t.family = p.pin ->> 'layer'
                                              AND t.version = (p.pin ->> 'version')::integer)
)
UPDATE jobs j
   SET pins = j.pins || (SELECT jsonb_agg(jsonb_build_object('layer', n.family, 'version', n.version)
                                          ORDER BY n.family)
                           FROM need n
                          WHERE n.id = j.id AND n.version IS NOT NULL)
 WHERE j.id IN (SELECT id FROM need WHERE version IS NOT NULL);
