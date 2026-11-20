-- Stage 0 SQL benchmark seed. Applied to the dedicated perf database AFTER migrations, by
-- scripts/perf/run-sql.sh (schema is recreated each run, so this file is not idempotent).
--
-- Volume is deliberately larger than any single tenant today: it makes a scan-vs-rollup
-- difference visible in EXPLAIN (ANALYZE, BUFFERS) instead of noise. Adjust the generate_series
-- bounds for a smaller machine; keep the ratios (20 messages/conversation, 10 jobs).
\set ON_ERROR_STOP on
BEGIN;

INSERT INTO tenants (id, name, updated_by, residency)
VALUES (:'tenant', 'perf benchmark tenant', 'perf:seed', 'eu');

-- 10 jobs so the usage report's job dimension is not constant.
CREATE TEMP TABLE perf_jobs ON COMMIT DROP AS
SELECT n, gen_random_uuid() AS id
FROM generate_series(0, 9) AS n;

INSERT INTO jobs (id, tenant_id, key, label, pins, created_by)
SELECT id, :'tenant', 'perf_job_' || n, 'Perf job ' || n, '[]', 'perf:seed'
FROM perf_jobs;

-- 1M metered calls over 90 days, spread over 10 jobs and the dimensions the report groups by.
INSERT INTO usage_event
    (tenant_id, job_id, event_key, occurred_at, source, use, actor, provider_id, model, funding,
     tokens_in, tokens_out)
SELECT :'tenant',
       pj.id,
       gen_random_uuid(),
       now() - (random() * interval '90 days'),
       'turn',
       CASE g % 5 WHEN 0 THEN 'qualify' WHEN 1 THEN 'answer' WHEN 2 THEN 'knowledge'
                  WHEN 3 THEN 'qualify' ELSE 'answer' END,
       'user-' || (g % 50),
       'provider-' || (g % 4),
       'model-' || (g % 7),
       CASE g % 3 WHEN 0 THEN 'local_gpu' WHEN 1 THEN 'platform_external' ELSE 'tenant_byo' END,
       (random() * 500)::bigint,
       (random() * 500)::bigint
FROM generate_series(1, 1000000) AS g
JOIN perf_jobs pj ON pj.n = g % 10;

-- 10k conversations x 20 messages.
INSERT INTO conversation
    (tenant_id, job_id, conversation_id, env, channel, customer_id, started_at)
SELECT :'tenant', pj.id, 'perf-conv-' || lpad(g::text, 5, '0'), 'prod', 'web_chat',
       'cust-' || g, now() - (random() * interval '30 days')
FROM generate_series(1, 10000) AS g
JOIN perf_jobs pj ON pj.n = (g - 1) / 1000;

INSERT INTO conversation_message
    (message_id, tenant_id, job_id, conversation_id, role, author, text, at_ms)
SELECT uuidv7(), c.tenant_id, c.job_id, c.conversation_id,
       CASE WHEN g % 2 = 0 THEN 'customer' ELSE 'agent' END,
       CASE WHEN g % 2 = 0 THEN 'cust-' || (((g - 1) / 20) + 1) ELSE 'agent' END,
       'message ' || g,
       (extract(epoch FROM now()) * 1000)::bigint - (200000 - g) * 1000
FROM generate_series(1, 200000) AS g
JOIN conversation c
  ON c.conversation_id = 'perf-conv-' || lpad((((g - 1) / 20) + 1)::text, 5, '0');

-- 100k operation calls, the SEARCHES read model's input.
INSERT INTO operation_call
    (tenant_id, job_id, call_id, operation, route, env, state, started_at, finished_at,
     initiator, release_id)
SELECT :'tenant', pj.id, 'opc_' || gen_random_uuid(), 'knowledge.search', 'builtin', 'prod', 'ok',
       now() - (random() * interval '90 days'), now(), 'perf:seed', NULL
FROM generate_series(1, 100000) AS g
JOIN perf_jobs pj ON pj.n = g % 10;

COMMIT;
