-- A staff test chat over a customer job runs as a known customer whose email the staff member typed
-- (`identity_source = 'staff_test'`); the customer's data resolves through the job's real connectors, like an
-- `assertion` conversation. Nothing else about the 0183 column changes.
ALTER TABLE conversation DROP CONSTRAINT IF EXISTS conversation_identity_source_check;
ALTER TABLE conversation ADD CONSTRAINT conversation_identity_source_check
    CHECK (identity_source IS NULL OR identity_source IN ('staff_pick', 'assertion', 'staff_test'));
