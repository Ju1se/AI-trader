-- Portable ANSI SQL governance, optimistic-concurrency task state, DB-backed approvals,
-- security evaluations (veto source of truth), drafts, and audit ledgers.
-- Compatible with both SQLite and PostgreSQL via ANSI ON CONFLICT clauses.

CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT PRIMARY KEY,
    idempotency_trigger_key TEXT NOT NULL UNIQUE,
    task_type TEXT NOT NULL,
    config_version TEXT NOT NULL,
    requester_id TEXT NOT NULL,
    acting_principal TEXT NOT NULL,
    trigger_source TEXT NOT NULL,
    authorized_markets_json TEXT NOT NULL,
    state_code TEXT NOT NULL,
    state_detail TEXT,
    wait_owner TEXT,
    wait_deadline TEXT,
    wait_timeout_action TEXT,
    version BIGINT NOT NULL DEFAULT 1,
    turns_used BIGINT NOT NULL DEFAULT 0,
    spend_cents BIGINT NOT NULL DEFAULT 0,
    retries_used BIGINT NOT NULL DEFAULT 0,
    consecutive_denials BIGINT NOT NULL DEFAULT 0,
    reworks_used BIGINT NOT NULL DEFAULT 0,
    reopened_count BIGINT NOT NULL DEFAULT 0,
    updated_epoch_s BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS task_transitions (
    transition_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    from_state TEXT,
    to_state TEXT NOT NULL,
    version BIGINT NOT NULL,
    reason TEXT NOT NULL,
    actor TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS contracts (
    task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
    contract_json TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS security_evaluations (
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    ticker TEXT NOT NULL,
    as_of_date TEXT NOT NULL,
    industry TEXT NOT NULL,
    adv_usd DOUBLE PRECISION NOT NULL,
    quant_score DOUBLE PRECISION NOT NULL,
    consensus_score DOUBLE PRECISION NOT NULL,
    dispersion DOUBLE PRECISION NOT NULL,
    hard_veto BIGINT NOT NULL,
    veto_reasons_json TEXT NOT NULL,
    escalate_review BIGINT NOT NULL,
    PRIMARY KEY (task_id, ticker)
);

CREATE TABLE IF NOT EXISTS worker_evidences (
    evidence_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    ticker TEXT NOT NULL,
    agent TEXT NOT NULL,
    output_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_worker_evidences_task
    ON worker_evidences(task_id);

CREATE TABLE IF NOT EXISTS approvals (
    approval_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    requester_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    target TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    args_json TEXT NOT NULL,
    status TEXT NOT NULL,
    approver_id TEXT,
    decision_note TEXT,
    expires_at_epoch_s BIGINT NOT NULL,
    created_at_epoch_s BIGINT NOT NULL,
    decided_at_epoch_s BIGINT,
    consumed_at_epoch_s BIGINT
);

CREATE INDEX IF NOT EXISTS idx_approvals_task
    ON approvals(task_id, status);

CREATE TABLE IF NOT EXISTS action_ledger (
    idempotency_key TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    tool_name TEXT NOT NULL,
    target TEXT NOT NULL,
    args_json TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    approval_id TEXT,
    status TEXT NOT NULL,
    backend_receipt_id TEXT,
    readback_verified BIGINT NOT NULL DEFAULT 0,
    updated_epoch_s BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS rebalance_drafts (
    draft_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    as_of_date TEXT NOT NULL,
    weights_json TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS cost_ledger (
    cost_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(task_id),
    category TEXT NOT NULL,
    amount_cents BIGINT NOT NULL,
    human_minutes DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    note TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS agent_calibration (
    agent_name TEXT PRIMARY KEY,
    calibrated_confidence DOUBLE PRECISION NOT NULL,
    historical_ic DOUBLE PRECISION NOT NULL DEFAULT 0.0
);

CREATE TABLE IF NOT EXISTS curated_memory (
    memory_id TEXT PRIMARY KEY,
    source_task_id TEXT NOT NULL REFERENCES tasks(task_id),
    summary TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending_review',
    reviewed_by TEXT
);
