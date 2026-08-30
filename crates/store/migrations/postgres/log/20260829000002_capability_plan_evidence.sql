-- Add the bounded evidence summary to capability plan diagnostics. Existing
-- installations may already have applied the initial capability-plan table.
ALTER TABLE request_capability_plans
    ADD COLUMN evidence_json TEXT NOT NULL DEFAULT '[]';
