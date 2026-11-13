-- 0132 — KH T1 (D106/D111/D115): the stored selections take the hierarchy shape. Forward-only (pre-prod).
--
-- D65's `industry_depts[]` (bare department keys) becomes `departments[]` — the department layer keys
-- `industry_dept/<industry>/<dept>`, in declared order, the first the primary — `industry: null` becomes `""`, and
-- `dimensions.region[]` becomes the one optional `region` (its first value; else a `region` already there). A row
-- with only `dimensions.region` (no `industry_depts`) is rewritten too, and so is a non-object `dimensions` naming
-- `region` (dropped): the WHEREs below use the CHECKs' own predicate, so the CHECKs never meet an unconverted row
-- (KH-F3; else ADD CONSTRAINT fails and the API does not boot). A selection without an industry or a
-- department stays incomplete: its readers refuse it 422 `hierarchy_incomplete`; nothing turns it into base only.
-- `kh_selection_v2` is the boot seeder's `platform::seeder::selection_v2` in SQL (seed v1 is sha-pinned and keeps
-- the old shape; the seeder rewrites it on apply).
--
-- Rewritten where a selection is stored and still read as the working state:
--   - `layer.template_selection` (0041) — a template head's default selection;
--   - `compile_reference_seed.selection` (0042);
--   - `job_target.selection` and `job_target_history.old/new` (0047) — the design store's `selection`;
--   - `draft_ledger.before/after/live_moved`, `release_bundled_draft.after`, `published_change.before/after/
--     rolled_back/restored_to_draft` (0044/0045) — design snapshots `{"v": <selection>}` of kind `selection`.
-- NOT rewritten: the release identity — `job_release.selection`, `release_snapshot.snap`, `release_baseline.snap`
-- (immutable by trigger; a release's hash covers them). A frozen release keeps the shape it was cut with; it reads
-- as a selection without departments, so the next approve needs the job's (rewritten) selection.
--
-- CHECKs: the three working stores refuse the old shape from now on (`industry_depts` / `dimensions.region`).
-- The pins rule of D115 (one `industry/<i>` pin and one `industry_dept` pin per selected department, none of another
-- industry) spans `jobs.pins` and the selection, so it is enforced in Rust (`platform::templates::pin_issues`) where
-- both are known: job creation (`routes::jobs`, template pins vs the template's selection) and every later
-- selection change, `PUT /api/jobs/{job}/target` (`routes::release::target`, `jobs.pins` vs the new selection).
-- Template registration (`platform::templates::project_job_template_tags`) checks the selection's hierarchy only.
-- A restored old-shape value (rollback / discard / restore of a pre-KH release) is rewritten by the same
-- conversion before it is written (`release::targets::SelectionTarget::set`); one without the hierarchy is a 422.

CREATE OR REPLACE FUNCTION kh_selection_v2(s jsonb) RETURNS jsonb
    LANGUAGE sql IMMUTABLE
    SET search_path = pg_catalog, public, pg_temp
    AS $$
    SELECT CASE
        -- the CHECKs' predicate below: a selection the CHECKs would refuse is always rewritten
        WHEN s IS NULL OR jsonb_typeof(s) <> 'object'
             OR NOT (s ? 'industry_depts' OR coalesce(s -> 'dimensions' ? 'region', false)) THEN s
        ELSE (s - 'industry_depts' - 'dimensions')
             || CASE WHEN s ? 'industry_depts'
                     THEN jsonb_build_object(
                              'industry', coalesce(s ->> 'industry', ''),
                              'departments', coalesce(
                                  (SELECT jsonb_agg('industry_dept/' || coalesce(s ->> 'industry', '') || '/' || d.v
                                                    ORDER BY d.n)
                                     FROM jsonb_array_elements_text(
                                              CASE WHEN jsonb_typeof(s -> 'industry_depts') = 'array'
                                                   THEN s -> 'industry_depts' ELSE '[]'::jsonb END)
                                          WITH ORDINALITY AS d(v, n)),
                                  '[]'::jsonb))
                     ELSE '{}'::jsonb END
             -- an object keeps its other dimensions; anything else (no dimensions, or a non-object one) is dropped
             || CASE WHEN jsonb_typeof(s -> 'dimensions') = 'object'
                     THEN jsonb_build_object(
                              'dimensions', (s -> 'dimensions') - 'region',
                              'region', coalesce(s -> 'dimensions' -> 'region' -> 0, s -> 'region', 'null'::jsonb))
                     ELSE '{}'::jsonb END
    END
$$;

-- Every object inside `j` (a design snapshot `{"v": …}`, `live_moved {was: …}`) rewritten with kh_selection_v2.
CREATE OR REPLACE FUNCTION kh_selections_v2(j jsonb) RETURNS jsonb
    LANGUAGE plpgsql IMMUTABLE
    SET search_path = pg_catalog, public, pg_temp
    AS $$
DECLARE
    out jsonb;
    k text;
    v jsonb;
BEGIN
    IF j IS NULL THEN
        RETURN NULL;
    ELSIF jsonb_typeof(j) = 'object' THEN
        IF j ? 'industry_depts' THEN
            RETURN kh_selection_v2(j);
        END IF;
        out := '{}'::jsonb;
        FOR k, v IN SELECT * FROM jsonb_each(j) LOOP
            out := out || jsonb_build_object(k, kh_selections_v2(v));
        END LOOP;
        RETURN out;
    ELSIF jsonb_typeof(j) = 'array' THEN
        SELECT coalesce(jsonb_agg(kh_selections_v2(e) ORDER BY n), '[]'::jsonb) INTO out
          FROM jsonb_array_elements(j) WITH ORDINALITY AS a(e, n);
        RETURN out;
    END IF;
    RETURN j;
END $$;

UPDATE layer SET template_selection = kh_selection_v2(template_selection)
 WHERE template_selection ? 'industry_depts' OR template_selection -> 'dimensions' ? 'region';
UPDATE compile_reference_seed SET selection = kh_selection_v2(selection)
 WHERE selection ? 'industry_depts' OR selection -> 'dimensions' ? 'region';
UPDATE job_target SET selection = kh_selection_v2(selection)
 WHERE selection ? 'industry_depts' OR selection -> 'dimensions' ? 'region';
UPDATE job_target_history SET old = kh_selections_v2(old), new = kh_selections_v2(new)
 WHERE old::text LIKE '%"industry_depts"%' OR new::text LIKE '%"industry_depts"%';
UPDATE draft_ledger
   SET before = kh_selections_v2(before), after = kh_selections_v2(after), live_moved = kh_selections_v2(live_moved)
 WHERE concat(before::text, after::text, live_moved::text) LIKE '%"industry_depts"%';
UPDATE release_bundled_draft SET after = kh_selections_v2(after)
 WHERE after::text LIKE '%"industry_depts"%';
UPDATE published_change
   SET before = kh_selections_v2(before), after = kh_selections_v2(after),
       rolled_back = kh_selections_v2(rolled_back), restored_to_draft = kh_selections_v2(restored_to_draft)
 WHERE concat(before::text, after::text, rolled_back::text, restored_to_draft::text) LIKE '%"industry_depts"%';

-- The old shape is refused from now on in the working stores.
ALTER TABLE layer
    DROP CONSTRAINT IF EXISTS layer_template_selection_v2,
    ADD CONSTRAINT layer_template_selection_v2
        CHECK (template_selection IS NULL
               OR NOT (template_selection ? 'industry_depts' OR template_selection -> 'dimensions' ? 'region'));
ALTER TABLE compile_reference_seed
    DROP CONSTRAINT IF EXISTS compile_reference_seed_selection_v2,
    ADD CONSTRAINT compile_reference_seed_selection_v2
        CHECK (NOT (selection ? 'industry_depts' OR selection -> 'dimensions' ? 'region'));
ALTER TABLE job_target
    DROP CONSTRAINT IF EXISTS job_target_selection_v2,
    ADD CONSTRAINT job_target_selection_v2
        CHECK (selection IS NULL OR NOT (selection ? 'industry_depts' OR selection -> 'dimensions' ? 'region'));
