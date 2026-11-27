-- One schema for SQLite and PostgreSQL. Only TEXT, BIGINT and DOUBLE PRECISION are used;
-- booleans are BIGINT 0/1, times are Unix seconds, structured values are JSON text.
-- Table comments name the entity of Diagram 8 each table holds.
--
-- Market data is not stored here: it comes from the market-data adapter (fixtures/ for
-- demos and tests). This database holds the control plane and the research results.

-- TASK_TYPE_PROFILE: risk tiers, limits, autonomy and waits per task type. Versioned and
-- never edited in place, so every task and decision names the rules it ran under.
CREATE TABLE task_types (
    task_type TEXT NOT NULL,
    version TEXT NOT NULL,
    config_json TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL,
    PRIMARY KEY (task_type, version)
);

-- TASK with its AUTHORITY (acting principal, market scope) and its current WAIT: a task
-- waits for at most one thing at a time. `version` guards every update.
CREATE TABLE tasks (
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
    wait_deadline_epoch_s BIGINT,
    wait_timeout_action TEXT,
    version BIGINT NOT NULL,
    running_since_epoch_s BIGINT,
    turns_used BIGINT NOT NULL,
    spend_cents BIGINT NOT NULL,
    retries_used BIGINT NOT NULL,
    consecutive_denials BIGINT NOT NULL,
    reworks_used BIGINT NOT NULL,
    reopened_count BIGINT NOT NULL,
    created_epoch_s BIGINT NOT NULL,
    updated_epoch_s BIGINT NOT NULL,
    FOREIGN KEY (task_type, config_version) REFERENCES task_types (task_type, version)
);

CREATE INDEX idx_tasks_state ON tasks (state_code);
CREATE INDEX idx_tasks_wait_deadline ON tasks (wait_deadline_epoch_s);

-- SESSION_EVENT and AUDIT_RECORD: every state change, denial, decision, action and cost,
-- append only, numbered per task.
CREATE TABLE task_events (
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    seq BIGINT NOT NULL,
    kind TEXT NOT NULL,
    from_state TEXT,
    to_state TEXT,
    version BIGINT NOT NULL,
    actor TEXT NOT NULL,
    ref_id TEXT,
    detail TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL,
    PRIMARY KEY (task_id, seq)
);

-- TASK_CONTRACT
CREATE TABLE contracts (
    task_id TEXT PRIMARY KEY REFERENCES tasks (task_id),
    contract_json TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL
);

-- ACTION: one row per idempotency key. A retry of an action that never left reuses it.
CREATE TABLE actions (
    idempotency_key TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    tool_name TEXT NOT NULL,
    target TEXT NOT NULL,
    args_json TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    approval_id TEXT,
    status TEXT NOT NULL,
    backend_receipt_id TEXT,
    readback_verified BIGINT NOT NULL,
    detail TEXT,
    created_epoch_s BIGINT NOT NULL,
    updated_epoch_s BIGINT NOT NULL
);

CREATE INDEX idx_actions_task_status ON actions (task_id, status);

-- APPROVAL: covers exactly one payload hash, is used at most once, and expires.
CREATE TABLE approvals (
    approval_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    requester_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    target TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    args_json TEXT NOT NULL,
    status TEXT NOT NULL,
    approver_id TEXT,
    decision_note TEXT,
    expires_at_epoch_s BIGINT NOT NULL,
    created_epoch_s BIGINT NOT NULL,
    decided_epoch_s BIGINT,
    consumed_epoch_s BIGINT
);

CREATE INDEX idx_approvals_task_status ON approvals (task_id, status);

-- DELIVERY, with the VERIFICATION result per criterion inside the manifest.
CREATE TABLE deliveries (
    task_id TEXT PRIMARY KEY REFERENCES tasks (task_id),
    completeness TEXT NOT NULL,
    manifest_json TEXT NOT NULL,
    delivered_epoch_s BIGINT NOT NULL
);

-- What a person needs to take a task over.
CREATE TABLE handoffs (
    task_id TEXT PRIMARY KEY REFERENCES tasks (task_id),
    owner_id TEXT NOT NULL,
    package_json TEXT NOT NULL,
    accepted_by TEXT,
    created_epoch_s BIGINT NOT NULL,
    accepted_epoch_s BIGINT
);

-- COST_ENTRY. API and tool spend also adds to tasks.spend_cents; human time does not.
CREATE TABLE costs (
    cost_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    stage TEXT NOT NULL,
    category TEXT NOT NULL,
    amount_cents BIGINT NOT NULL,
    human_minutes DOUBLE PRECISION NOT NULL,
    note TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL
);

CREATE INDEX idx_costs_task ON costs (task_id);

-- Committee results per security. The gate checks business rules against this stored
-- veto list, never against what a worker or the proposer says.
CREATE TABLE security_evaluations (
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    ticker TEXT NOT NULL,
    as_of_date TEXT NOT NULL,
    hard_veto BIGINT NOT NULL,
    decision_json TEXT NOT NULL,
    PRIMARY KEY (task_id, ticker)
);

CREATE TABLE drafts (
    draft_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    as_of_date TEXT NOT NULL,
    weights_json TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL
);

-- Learning: how far to trust each agent, and lessons waiting for a person's review.
CREATE TABLE agent_calibration (
    agent_name TEXT PRIMARY KEY,
    calibrated_confidence DOUBLE PRECISION NOT NULL,
    historical_ic DOUBLE PRECISION NOT NULL
);

CREATE TABLE curated_memory (
    memory_id TEXT PRIMARY KEY,
    source_task_id TEXT NOT NULL REFERENCES tasks (task_id),
    summary TEXT NOT NULL,
    status TEXT NOT NULL,
    reviewed_by TEXT
);
