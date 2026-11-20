-- Report and per-turn read queries, planned and executed as the application role with RLS.
-- Invoked by scripts/perf/run-sql.sh with -v tenant=<uuid>.
\set ON_ERROR_STOP on
SET ROLE agent_app;
SET app.current_tenant = :'tenant';
SET app.current_job = '';
\timing on

\echo '=== usage report SLICES (30 days) ==='
EXPLAIN (ANALYZE, BUFFERS)
SELECT (u.occurred_at AT TIME ZONE 'UTC')::date AS day, j.key AS job_key, j.label AS job_label,
       u.actor, u.project, u.provider_id, u.model, u.funding, u.source,
       count(*) FILTER (WHERE u.use = 'qualify')::bigint AS turns,
       COALESCE(sum(u.tokens_in + u.tokens_out), 0)::bigint AS tokens
  FROM usage_event u LEFT JOIN jobs j ON j.tenant_id = u.tenant_id AND j.id = u.job_id
 WHERE u.occurred_at >= now() - interval '30 days' AND u.occurred_at < now()
 GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9;

\echo '=== usage report DRIVERS (30 days) ==='
EXPLAIN (ANALYZE, BUFFERS)
SELECT j.key AS job, COALESCE(u.activity, c1.activity, c2.activity, c1.request_type, c2.request_type, 'unqualified') AS activity,
       count(*) FILTER (WHERE u.use = 'qualify')::bigint AS turns,
       COALESCE(sum(u.tokens_in + u.tokens_out), 0)::bigint AS tokens
  FROM usage_event u JOIN jobs j ON j.tenant_id = u.tenant_id AND j.id = u.job_id
  LEFT JOIN LATERAL (
       SELECT NULLIF(ac.activity, '') AS activity, NULLIF(ac.request_type, '') AS request_type
         FROM agent_case ac
        WHERE ac.tenant_id = u.tenant_id AND ac.job_id = u.job_id AND ac.case_id = u.case_id
        ORDER BY ac.started_at LIMIT 1) c1 ON u.case_id IS NOT NULL
  LEFT JOIN LATERAL (
       SELECT NULLIF(ac.activity, '') AS activity, NULLIF(ac.request_type, '') AS request_type
         FROM agent_case ac
        WHERE ac.tenant_id = u.tenant_id AND ac.job_id = u.job_id
          AND ac.conversation_id = u.conversation_id AND ac.started_at >= u.occurred_at
        ORDER BY ac.started_at LIMIT 1) c2 ON u.case_id IS NULL
 WHERE u.occurred_at >= now() - interval '30 days' AND u.occurred_at < now()
   AND NOT u.source = ANY (ARRAY['generator', 'discovery'])
 GROUP BY 1, 2;

\echo '=== usage report SLICES (30 days, rollup) ==='
EXPLAIN (ANALYZE, BUFFERS)
SELECT r.day AS day, j.key AS job_key, j.label AS job_label, r.actor, r.project, r.provider_id,
       r.model, r.funding, r.source,
       COALESCE(sum(r.events) FILTER (WHERE r.use = 'qualify'), 0)::bigint AS turns,
       COALESCE(sum(r.tokens_in + r.tokens_out), 0)::bigint AS tokens
  FROM usage_rollup_daily r LEFT JOIN jobs j ON j.tenant_id = r.tenant_id AND j.id = r.job_id
 WHERE r.day >= (now() AT TIME ZONE 'UTC')::date - 29
   AND r.day < (now() AT TIME ZONE 'UTC')::date + 1
 GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9;

\echo '=== rollup vs raw ledger (all five sums must match) ==='
SELECT (SELECT count(*) FROM usage_event) AS raw_events,
       (SELECT sum(events) FROM usage_rollup_daily) AS rollup_events,
       (SELECT sum(calls) FROM usage_event) AS raw_calls,
       (SELECT sum(calls) FROM usage_rollup_daily) AS rollup_calls,
       (SELECT sum(tokens_in) FROM usage_event) AS raw_in,
       (SELECT sum(tokens_in) FROM usage_rollup_daily) AS rollup_in,
       (SELECT sum(tokens_out) FROM usage_event) AS raw_out,
       (SELECT sum(tokens_out) FROM usage_rollup_daily) AS rollup_out,
       (SELECT sum(latency_ms) FROM usage_event) AS raw_latency,
       (SELECT sum(latency_ms) FROM usage_rollup_daily) AS rollup_latency;

\echo '=== SLICES raw vs rollup (old_rows = new_rows, both diffs 0) ==='
WITH old AS (
    SELECT (u.occurred_at AT TIME ZONE 'UTC')::date AS day, j.key AS job_key, j.label AS job_label,
           u.actor, u.project, u.provider_id, u.model, u.funding, u.source,
           count(*) FILTER (WHERE u.use = 'qualify')::bigint AS turns,
           COALESCE(sum(u.tokens_in + u.tokens_out), 0)::bigint AS tokens
      FROM usage_event u LEFT JOIN jobs j ON j.tenant_id = u.tenant_id AND j.id = u.job_id
     WHERE u.occurred_at >= (now() AT TIME ZONE 'UTC')::date - 29
       AND u.occurred_at < (now() AT TIME ZONE 'UTC')::date + 1
     GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9),
new AS (
    SELECT r.day AS day, j.key AS job_key, j.label AS job_label, r.actor, r.project, r.provider_id,
           r.model, r.funding, r.source,
           COALESCE(sum(r.events) FILTER (WHERE r.use = 'qualify'), 0)::bigint AS turns,
           COALESCE(sum(r.tokens_in + r.tokens_out), 0)::bigint AS tokens
      FROM usage_rollup_daily r LEFT JOIN jobs j ON j.tenant_id = r.tenant_id AND j.id = r.job_id
     WHERE r.day >= (now() AT TIME ZONE 'UTC')::date - 29
       AND r.day < (now() AT TIME ZONE 'UTC')::date + 1
     GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9)
SELECT (SELECT count(*) FROM old) AS old_rows, (SELECT count(*) FROM new) AS new_rows,
       (SELECT count(*) FROM (SELECT * FROM old EXCEPT SELECT * FROM new) a) AS old_minus_new,
       (SELECT count(*) FROM (SELECT * FROM new EXCEPT SELECT * FROM old) b) AS new_minus_old;

\echo '=== conversation history (last 20 lines, as last_views reads it) ==='
EXPLAIN (ANALYZE, BUFFERS)
SELECT * FROM (
    SELECT message_id, role, author, text, case_id, at_ms, client_msg_id, delivered_at,
           delivery_error, mirror_state
      FROM conversation_message
     WHERE conversation_id = 'perf-conv-00001'
     ORDER BY at_ms DESC, message_id DESC
     LIMIT 20) t
 ORDER BY at_ms, message_id;

\echo '=== reply wait: first agent line after a customer message ==='
EXPLAIN (ANALYZE, BUFFERS)
SELECT message_id, text, case_id
  FROM conversation_message
 WHERE conversation_id = 'perf-conv-00001' AND role = 'agent'
   AND author IS DISTINCT FROM 'task_relay'
   AND (at_ms, message_id) > ((extract(epoch FROM now()) * 1000)::bigint - 3600000,
                              '00000000-0000-0000-0000-000000000000'::uuid)
 ORDER BY at_ms, message_id
 LIMIT 1;
