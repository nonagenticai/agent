-- 0192 (B8 phase 2 T10a, plan 2026-10-20 `0106` → revision 2026-10-25 P2 / 0191–0199): F-23 operation proposals,
-- F-53 generator evidence, F-54 the draft hint; and the one grant the provisional `v0` tag needs.
--
-- - `generator_item.new_operation`: the proposed `operation` row (F-22 fields + object / verb / variant) of an
--   operations-step item (`proposal` then covers objects and operations).
-- - `generator_step.hint`: the last steering hint used for the step (at most 2000 characters). The audit never
--   carries it.
-- - `generator_evidence` + `generator_evidence_row` (Platform class: platform runs only, `agent_platform` writes,
--   `agent_app` nothing): an uploaded ticket export checked against a run's accepted journey. Rows hold scrubbed
--   excerpts only (≤ 1024 bytes; digit runs ≥ 8 → `[number]`, e-mail → `[email]`); a row with a card number /
--   password / one-time code is never stored (crate::discovery::scrub). `uncovered` holds the clusters the
--   Validate step lists as rule 7 information.
-- - `DELETE` on `layer_tag` / `layer_tag_row` for `agent_platform`: a generator run keeps one provisional `v0` tag
--   per output layer and replaces it on every advance and at Package (0041: "a generator draft, B8 replaces it").
--   The immutability triggers of 0041 still refuse deleting or changing anything but a provisional v0.
--
-- Drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD). Forward-only. Needs 0041, 0171.

ALTER TABLE generator_item ADD COLUMN IF NOT EXISTS new_operation jsonb;
ALTER TABLE generator_item
    DROP CONSTRAINT IF EXISTS generator_item_new_operation_check,
    ADD CONSTRAINT generator_item_new_operation_check
        CHECK (new_operation IS NULL OR jsonb_typeof(new_operation) = 'object');

ALTER TABLE generator_step ADD COLUMN IF NOT EXISTS hint text;
ALTER TABLE generator_step
    DROP CONSTRAINT IF EXISTS generator_step_hint_check,
    ADD CONSTRAINT generator_step_hint_check CHECK (hint IS NULL OR char_length(hint) <= 2000);

CREATE TABLE IF NOT EXISTS generator_evidence (
    seq           bigint GENERATED ALWAYS AS IDENTITY,
    id            text GENERATED ALWAYS AS (
                      'ev-' || CASE WHEN seq < 10000 THEN lpad(seq::text, 4, '0') ELSE seq::text END
                  ) STORED,
    run_id        bigint NOT NULL,
    file          text NOT NULL,
    format        text NOT NULL,
    text_column   text,
    count_column  text,
    rows          integer NOT NULL,
    distinct_rows integer NOT NULL,
    status        text NOT NULL DEFAULT 'classifying',
    done          integer NOT NULL DEFAULT 0,
    uncovered     jsonb NOT NULL DEFAULT '[]'::jsonb,
    uploaded_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
    uploaded_by   text NOT NULL,
    error         text,
    PRIMARY KEY (id)
);
ALTER TABLE generator_evidence
    DROP CONSTRAINT IF EXISTS generator_evidence_run,
    ADD CONSTRAINT generator_evidence_run FOREIGN KEY (run_id) REFERENCES generator_run (id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS generator_evidence_status_check,
    ADD CONSTRAINT generator_evidence_status_check CHECK (status IN ('classifying', 'classified', 'failed')),
    DROP CONSTRAINT IF EXISTS generator_evidence_format_check,
    ADD CONSTRAINT generator_evidence_format_check CHECK (format IN ('csv', 'json', 'xlsx')),
    DROP CONSTRAINT IF EXISTS generator_evidence_counts_check,
    ADD CONSTRAINT generator_evidence_counts_check
        CHECK (rows >= 0 AND distinct_rows >= 0 AND done >= 0 AND done <= distinct_rows),
    DROP CONSTRAINT IF EXISTS generator_evidence_uncovered_check,
    ADD CONSTRAINT generator_evidence_uncovered_check CHECK (jsonb_typeof(uncovered) = 'array'),
    DROP CONSTRAINT IF EXISTS generator_evidence_uploaded_by_check,
    ADD CONSTRAINT generator_evidence_uploaded_by_check CHECK (btrim(uploaded_by) <> '');
CREATE INDEX IF NOT EXISTS generator_evidence_run ON generator_evidence (run_id, seq);

CREATE TABLE IF NOT EXISTS generator_evidence_row (
    evidence_id text NOT NULL,
    text_hash   text NOT NULL,
    ord         integer NOT NULL,
    excerpt     text,
    weight      integer NOT NULL DEFAULT 1,
    stage       text,
    activity    text,
    bucket      text,
    confidence  real,
    PRIMARY KEY (evidence_id, text_hash)
);
ALTER TABLE generator_evidence_row
    DROP CONSTRAINT IF EXISTS generator_evidence_row_evidence,
    ADD CONSTRAINT generator_evidence_row_evidence
        FOREIGN KEY (evidence_id) REFERENCES generator_evidence (id) ON DELETE CASCADE,
    DROP CONSTRAINT IF EXISTS generator_evidence_row_excerpt_check,
    ADD CONSTRAINT generator_evidence_row_excerpt_check CHECK (excerpt IS NULL OR octet_length(excerpt) <= 1024),
    DROP CONSTRAINT IF EXISTS generator_evidence_row_weight_check,
    ADD CONSTRAINT generator_evidence_row_weight_check CHECK (weight >= 1),
    DROP CONSTRAINT IF EXISTS generator_evidence_row_bucket_check,
    ADD CONSTRAINT generator_evidence_row_bucket_check
        CHECK (bucket IS NULL OR bucket IN ('covered', 'uncovered', 'unclear', 'out_of_scope'));
CREATE INDEX IF NOT EXISTS generator_evidence_row_pending ON generator_evidence_row (evidence_id, ord);

REVOKE ALL ON generator_evidence, generator_evidence_row FROM PUBLIC;
REVOKE ALL ON generator_evidence, generator_evidence_row FROM agent_app;
REVOKE ALL ON generator_evidence, generator_evidence_row FROM agent_platform;
GRANT SELECT, INSERT, UPDATE, DELETE ON generator_evidence, generator_evidence_row TO agent_platform;
REVOKE ALL ON SEQUENCE generator_evidence_seq_seq FROM PUBLIC;
GRANT USAGE ON SEQUENCE generator_evidence_seq_seq TO agent_platform;

-- the provisional v0 is replaced, never changed in place; the 0041 triggers still refuse a real tag
GRANT DELETE ON layer_tag, layer_tag_row TO agent_platform;
