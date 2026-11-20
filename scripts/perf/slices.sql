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
SELECT j.key AS job, COALESCE(u.activity, c.activity, c.request_type, 'unqualified') AS activity,
       count(*) FILTER (WHERE u.use = 'qualify')::bigint AS turns,
       COALESCE(sum(u.tokens_in + u.tokens_out), 0)::bigint AS tokens
  FROM usage_event u JOIN jobs j ON j.tenant_id = u.tenant_id AND j.id = u.job_id
  LEFT JOIN LATERAL (
       SELECT NULLIF(ac.activity, '') AS activity, NULLIF(ac.request_type, '') AS request_type
         FROM agent_case ac
        WHERE ac.tenant_id = u.tenant_id AND ac.job_id = u.job_id
          AND (ac.case_id = u.case_id
               OR (u.case_id IS NULL AND ac.conversation_id = u.conversation_id
                   AND ac.started_at >= u.occurred_at))
        ORDER BY ac.started_at LIMIT 1) c ON true
 WHERE u.occurred_at >= now() - interval '30 days' AND u.occurred_at < now()
   AND NOT u.source = ANY (ARRAY['generator', 'discovery'])
 GROUP BY 1, 2;

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
