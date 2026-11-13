-- 0156 — retire B1 0033's hand-built job template rows (DR T5, ruling DR-R3). Forward-only.
--
-- The four `seed:0033` rows of `job_template_tag` (customer_care_agent@3 and @4, utilities billing@1, the draft
-- it_service_desk_agent@0) were typed in by hand from the UI fixtures. Seed v3 (KH) supersedes them: the boot
-- seeder's projection (`platform::templates::project`) writes the same (layer, version) keys from the seeded layer
-- tags, as `seed:platform:vN`, and updates a surviving row's pins in place.
--
-- A row a job still stands on is NOT deleted: `jobs.template_layer/template_version` is the job's pointer to its
-- template (what `newest_tag` and the upgrade board read), and an open `template_upgrade_request` names its target
-- template in `to_pins`. Prod's Demo Retail customer_care_agent is on `job/customer_care_agent@4`, so that row
-- stays (pins kept in step by the projection) until the job is re-created on `@5` (DR T7) or upgraded; a later
-- migration may then drop it. Rows the migration does not delete are left untouched, never repointed: a job's
-- pins are its own and move only through a release.
--
-- Accepted (DR ruling): the seeder re-creates the rows it still wants as `seed:platform:vN` with head labels; the
-- hand-written labels (e.g. "generator run #7 in progress") are lost on purpose (D113: no hand-built SQL literals).
--
-- Order: migrations run before the boot seeder, so on a fresh database all four rows go and the seeder re-projects
-- the ones the seed wants (with head labels instead of the hand-written ones); on prod the unreferenced rows go and
-- are re-projected the same way where the seed still wants them (latest and previous tag, or provisional).
DELETE FROM job_template_tag t
 WHERE t.created_by = 'seed:0033'
   AND NOT EXISTS (SELECT 1 FROM jobs j
                    WHERE j.template_layer = t.layer AND j.template_version = t.version)
   AND NOT EXISTS (SELECT 1 FROM template_upgrade_request r
                    WHERE r.status = 'open'
                      AND r.to_pins @> jsonb_build_array(jsonb_build_object('layer', t.layer, 'version', t.version)));
