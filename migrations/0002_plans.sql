-- PLAN (Diagrams 2 and 8): every version of a task's plan. The newest is the plan in force;
-- earlier ones stay as the record of how it changed, such as a step a person refused.
CREATE TABLE plans (
    task_id TEXT NOT NULL REFERENCES tasks (task_id),
    version INTEGER NOT NULL,
    plan_json TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_epoch_s BIGINT NOT NULL,
    PRIMARY KEY (task_id, version)
);
