-- Diagram 4a: turns and spend the owner added after a task reached a limit. The task type's
-- limits stay as configured; these come on top, for this task only.
ALTER TABLE tasks ADD COLUMN granted_turns BIGINT NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN granted_spend_cents BIGINT NOT NULL DEFAULT 0;
