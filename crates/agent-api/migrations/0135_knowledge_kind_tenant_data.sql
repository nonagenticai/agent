-- 0135 — KH T3 (D107/D110): a job's knowledge kind records whether it is the tenant's own data. Forward-only.
--
-- `stage_knowledge_kind` rows gained `tenant_data` (order status, delivery slots: frozen platform packs alone do not
-- satisfy such a kind, D110). The job's working rows carry it from the template fold (`knowledge::kinds::
-- materialise`), so readiness (KH T8) reads it with the rest of the kind. Existing rows predate the field: false.
-- A kind's stage may now be `*` (every stage, D107); the 0054 stage CHECK already accepts it.
ALTER TABLE job_knowledge_kind ADD COLUMN IF NOT EXISTS tenant_data boolean NOT NULL DEFAULT false;
