-- 0145 — grants on B5's job-design tables (0140, 0142–0144; plan 2026-10-20-b5-job-design, Data model `0145`, T3).
-- Forward-only (pre-prod), drift-tolerant (REVOKE ALL + GRANT).
--
-- Order: production has applied ≤0134 and applies 0140+ after it; a fresh database applies in number order. Every
-- object named here is ≤0144, and the roles exist from 0001 (`agent_app`) and 0065 (`agent_runtime`).
--
-- agent_app (routes): read/write on the design rows; append-only on `procedure_history` and the gate log, plus DELETE
-- on `gate_evaluation` for `job_design_purge_gate_log()` (its answers go by the FK cascade).
--
-- agent_runtime (R7; runtime processes, as 0123/0128/0131), audited by path:
-- - `RuntimeDesign::read` (B4 `runtime/design.rs`, T12) for the Test env (`DesignOf::Draft`) reads the working rows:
--   `ontology_entity`, `ontology_entity_refinement`, `ontology_relation` (ontology), `procedure_version` (effective
--   procedures), `job_gate_question`, `job_gate_option_refinement`, `job_gate_rule_refinement` (gate set),
--   `toolset_narrowing` (toolsets). SELECT only. Production (`Live`) reads `release_snapshot` (already granted, 0065).
-- - the gate hooks (`job_design::gates::hooks`, T12) write one `gate_evaluation` row per gate-point call and its
--   `gate_answer` rows: SELECT, INSERT (+ the identity sequence). Never UPDATE or DELETE.
-- - nothing on `procedure_history` (authoring history, not runtime).

-- Every B5 table: nothing for PUBLIC.
REVOKE ALL ON ontology_entity, ontology_entity_refinement, ontology_relation, procedure_version, procedure_history,
              job_gate_question, job_gate_option_refinement, job_gate_rule_refinement, gate_evaluation, gate_answer,
              toolset_narrowing
    FROM PUBLIC;
REVOKE ALL ON SEQUENCE procedure_history_id_seq, gate_evaluation_id_seq FROM PUBLIC;

-- agent_app
REVOKE ALL ON ontology_entity, ontology_entity_refinement, ontology_relation, procedure_version, procedure_history,
              job_gate_question, job_gate_option_refinement, job_gate_rule_refinement, gate_evaluation, gate_answer,
              toolset_narrowing
    FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON ontology_entity, ontology_entity_refinement, ontology_relation, procedure_version,
       job_gate_question, job_gate_option_refinement, job_gate_rule_refinement, toolset_narrowing
    TO agent_app;
GRANT SELECT, INSERT ON procedure_history, gate_evaluation, gate_answer TO agent_app;
GRANT DELETE ON gate_evaluation TO agent_app;
REVOKE ALL ON SEQUENCE procedure_history_id_seq, gate_evaluation_id_seq FROM agent_app;
GRANT USAGE ON SEQUENCE procedure_history_id_seq, gate_evaluation_id_seq TO agent_app;

-- agent_runtime
REVOKE ALL ON ontology_entity, ontology_entity_refinement, ontology_relation, procedure_version, procedure_history,
              job_gate_question, job_gate_option_refinement, job_gate_rule_refinement, gate_evaluation, gate_answer,
              toolset_narrowing
    FROM agent_runtime;
GRANT SELECT
    ON ontology_entity, ontology_entity_refinement, ontology_relation, procedure_version,
       job_gate_question, job_gate_option_refinement, job_gate_rule_refinement, toolset_narrowing
    TO agent_runtime;
GRANT SELECT, INSERT ON gate_evaluation, gate_answer TO agent_runtime;
REVOKE ALL ON SEQUENCE procedure_history_id_seq, gate_evaluation_id_seq FROM agent_runtime;
GRANT USAGE ON SEQUENCE gate_evaluation_id_seq TO agent_runtime;

-- The gate-log purge: agent_app only.
REVOKE ALL ON FUNCTION job_design_purge_gate_log() FROM PUBLIC;
REVOKE ALL ON FUNCTION job_design_purge_gate_log() FROM agent_runtime;
GRANT EXECUTE ON FUNCTION job_design_purge_gate_log() TO agent_app;
