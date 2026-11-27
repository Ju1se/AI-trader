//! The 13 task states of Diagram 4a and the only legal moves between them.
//!
//! The state is private to `TaskRecord` and changes only through `TaskRecord::transition`,
//! which checks `TaskState::can_transition`. Cycles in the diagram (waiting and running,
//! reopening after completion) are just rows in that table; no state holds a reference
//! to another.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{IdempotencyKey, TaskId};
use crate::types::{SchemaError, UsdCents, require_non_empty};

/// What happens when nobody acts on a waiting task before its deadline.
/// A timeout is never a silent approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeoutAction {
    /// Hand the task to a person.
    HandOff,
    /// Treat the pending approval as rejected.
    Decline,
    /// Close the settlement window as accepted.
    Complete,
    /// Re-assign the hand-off to the backup owner.
    Escalate,
    /// Verify and deliver what is done (a task out of budget nobody gave more).
    Deliver,
}

impl TimeoutAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HandOff => "HAND_OFF",
            Self::Decline => "DECLINE",
            Self::Complete => "COMPLETE",
            Self::Escalate => "ESCALATE",
            Self::Deliver => "DELIVER",
        }
    }

    pub fn from_db_str(s: &str) -> Result<Self, SchemaError> {
        match s {
            "HAND_OFF" => Ok(Self::HandOff),
            "DECLINE" => Ok(Self::Decline),
            "COMPLETE" => Ok(Self::Complete),
            "ESCALATE" => Ok(Self::Escalate),
            "DELIVER" => Ok(Self::Deliver),
            other => Err(SchemaError(format!("unknown_timeout_action:{other}"))),
        }
    }
}

/// Every waiting state names who must act, by when, and what happens at the deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitMetadata {
    pub owner_id: String,
    pub deadline_epoch_s: i64,
    pub timeout_action: TimeoutAction,
}

impl WaitMetadata {
    pub fn new(
        owner_id: impl Into<String>,
        deadline_epoch_s: i64,
        timeout_action: TimeoutAction,
    ) -> Result<Self, SchemaError> {
        let wait = Self {
            owner_id: owner_id.into(),
            deadline_epoch_s,
            timeout_action,
        };
        require_non_empty(&wait.owner_id, "wait.owner_id")?;
        if wait.deadline_epoch_s <= 0 {
            return Err(SchemaError("wait.deadline_epoch_s must be > 0".into()));
        }
        Ok(wait)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Queued,
    Running,
    AwaitingInput(WaitMetadata),
    AwaitingApproval(WaitMetadata),
    OutcomeUnknown {
        frozen_action_key: IdempotencyKey,
    },
    /// Out of turns, spend or time. The owner grants more or has what is done delivered.
    LimitReached {
        reason: String,
        wait: WaitMetadata,
    },
    Verifying,
    Settling(WaitMetadata),
    Completed,
    Failed {
        reason: String,
    },
    HandedOff(WaitMetadata),
    Declined {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
}

impl TaskState {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Running => "RUNNING",
            Self::AwaitingInput(_) => "AWAITING_INPUT",
            Self::AwaitingApproval(_) => "AWAITING_APPROVAL",
            Self::OutcomeUnknown { .. } => "OUTCOME_UNKNOWN",
            Self::LimitReached { .. } => "LIMIT_REACHED",
            Self::Verifying => "VERIFYING",
            Self::Settling(_) => "SETTLING",
            Self::Completed => "COMPLETED",
            Self::Failed { .. } => "FAILED",
            Self::HandedOff(_) => "HANDED_OFF",
            Self::Declined { .. } => "DECLINED",
            Self::Cancelled { .. } => "CANCELLED",
        }
    }

    pub fn wait_meta(&self) -> Option<&WaitMetadata> {
        match self {
            Self::AwaitingInput(w)
            | Self::AwaitingApproval(w)
            | Self::Settling(w)
            | Self::HandedOff(w)
            | Self::LimitReached { wait: w, .. } => Some(w),
            _ => None,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::OutcomeUnknown { frozen_action_key } => Some(frozen_action_key.as_str()),
            Self::LimitReached { reason, .. }
            | Self::Failed { reason }
            | Self::Declined { reason }
            | Self::Cancelled { reason } => Some(reason),
            _ => None,
        }
    }

    /// No way out at all.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Failed { .. } | Self::Declined { .. } | Self::Cancelled { .. }
        )
    }

    /// The agent's work has ended. Completed tasks can still be reopened and handed-off
    /// tasks handed back, but neither needs the agent until that happens.
    pub fn is_closed(&self) -> bool {
        self.is_terminal() || matches!(self, Self::Completed | Self::HandedOff(_))
    }

    /// Rebuild a state read from the database. Unknown codes are an error, never a default.
    pub fn from_db_parts(
        code: &str,
        detail: Option<String>,
        wait: Option<WaitMetadata>,
    ) -> Result<Self, SchemaError> {
        let wait = |name: &str| {
            wait.clone()
                .ok_or_else(|| SchemaError(format!("waiting state {name} has no wait metadata")))
        };
        let detail = detail.unwrap_or_default();
        Ok(match code {
            "QUEUED" => Self::Queued,
            "RUNNING" => Self::Running,
            "AWAITING_INPUT" => Self::AwaitingInput(wait(code)?),
            "AWAITING_APPROVAL" => Self::AwaitingApproval(wait(code)?),
            "OUTCOME_UNKNOWN" => {
                require_non_empty(&detail, "outcome_unknown.frozen_action_key")?;
                Self::OutcomeUnknown {
                    frozen_action_key: IdempotencyKey::new(detail),
                }
            }
            "LIMIT_REACHED" => Self::LimitReached {
                reason: detail,
                wait: wait(code)?,
            },
            "VERIFYING" => Self::Verifying,
            "SETTLING" => Self::Settling(wait(code)?),
            "COMPLETED" => Self::Completed,
            "FAILED" => Self::Failed { reason: detail },
            "HANDED_OFF" => Self::HandedOff(wait(code)?),
            "DECLINED" => Self::Declined { reason: detail },
            "CANCELLED" => Self::Cancelled { reason: detail },
            unknown => return Err(SchemaError(format!("unknown_task_state:{unknown}"))),
        })
    }

    /// One example of every state, in Diagram 4a order.
    fn examples() -> [Self; 13] {
        let wait = WaitMetadata {
            owner_id: "-".into(),
            deadline_epoch_s: 1,
            timeout_action: TimeoutAction::HandOff,
        };
        let reason = String::new;
        [
            Self::Queued,
            Self::Running,
            Self::AwaitingInput(wait.clone()),
            Self::AwaitingApproval(wait.clone()),
            Self::OutcomeUnknown {
                frozen_action_key: IdempotencyKey::new("-"),
            },
            Self::LimitReached {
                reason: reason(),
                wait: wait.clone(),
            },
            Self::Verifying,
            Self::Settling(wait.clone()),
            Self::Completed,
            Self::Failed { reason: reason() },
            Self::HandedOff(wait),
            Self::Declined { reason: reason() },
            Self::Cancelled { reason: reason() },
        ]
    }

    /// Codes of every state, in Diagram 4a order.
    pub fn all_codes() -> [&'static str; 13] {
        Self::examples().map(|s| s.code())
    }

    /// The states this one may move to, by code.
    pub fn next_codes(&self) -> Vec<&'static str> {
        Self::examples()
            .iter()
            .filter(|next| self.can_transition(next))
            .map(Self::code)
            .collect()
    }

    /// The legal moves of Diagram 4a, row by row. Anything not listed is illegal.
    pub fn can_transition(&self, next: &Self) -> bool {
        use TaskState as S;
        match (self, next) {
            // Any state the agent is still working on can be handed to a person.
            (from, S::HandedOff(_)) if !from.is_closed() => true,
            // Cancelling is allowed while working, except when an action's outcome is
            // unknown: that must be reconciled first.
            (from, S::Cancelled { .. }) => {
                !from.is_closed() && !matches!(from, S::OutcomeUnknown { .. })
            }
            (S::Queued, S::Running | S::Failed { .. }) => true,
            (
                S::Running,
                S::AwaitingInput(_)
                | S::AwaitingApproval(_)
                | S::OutcomeUnknown { .. }
                | S::LimitReached { .. }
                | S::Verifying
                | S::Failed { .. },
            ) => true,
            (S::AwaitingInput(_), S::Running) => true,
            // Approved, or rejected but the goal is still reachable without the action.
            (S::AwaitingApproval(_), S::Running | S::Declined { .. }) => true,
            (S::OutcomeUnknown { .. }, S::Running) => true,
            // More budget granted, or verify and deliver what is already done.
            (S::LimitReached { .. }, S::Running | S::Verifying) => true,
            (S::Verifying, S::Running | S::Settling(_) | S::Failed { .. }) => true,
            (S::Settling(_), S::Completed | S::Running) => true,
            // Reopened after a later check finds the outcome wrong.
            (S::Completed, S::Running) => true,
            // A person answered and handed the task back.
            (S::HandedOff(_), S::Running) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: &'static str,
    pub to: &'static str,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "illegal state transition {} -> {}", self.from, self.to)
    }
}

impl std::error::Error for TransitionError {}

/// A task as stored outside the model. `state` and `version` are private: the state moves
/// only through `transition`, and the store bumps `version` on every write so a stale copy
/// can never overwrite a newer one.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRecord {
    pub task_id: TaskId,
    pub idempotency_trigger_key: String,
    pub task_type: String,
    pub config_version: String,
    pub requester_id: String,
    pub acting_principal: String,
    pub trigger_source: String,
    pub authorized_markets: Vec<String>,
    state: TaskState,
    version: i64,
    /// When the task last entered RUNNING. The wall-clock limit counts agent working time
    /// from here, so time spent waiting for a person does not use it up.
    pub running_since_epoch_s: Option<i64>,
    pub turns_used: u32,
    pub spend: UsdCents,
    pub retries_used: u32,
    pub consecutive_denials: u32,
    pub reworks_used: u32,
    pub reopened_count: u32,
    /// Turns and spend the owner added after a limit was reached (Diagram 4a).
    pub granted_turns: u32,
    pub granted_spend: UsdCents,
}

impl TaskRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new_queued(
        task_id: TaskId,
        idempotency_trigger_key: impl Into<String>,
        task_type: impl Into<String>,
        config_version: impl Into<String>,
        requester_id: impl Into<String>,
        acting_principal: impl Into<String>,
        trigger_source: impl Into<String>,
        authorized_markets: Vec<String>,
    ) -> Result<Self, SchemaError> {
        let task = Self {
            task_id,
            idempotency_trigger_key: idempotency_trigger_key.into(),
            task_type: task_type.into(),
            config_version: config_version.into(),
            requester_id: requester_id.into(),
            acting_principal: acting_principal.into(),
            trigger_source: trigger_source.into(),
            authorized_markets,
            state: TaskState::Queued,
            version: 1,
            running_since_epoch_s: None,
            turns_used: 0,
            spend: UsdCents::ZERO,
            retries_used: 0,
            consecutive_denials: 0,
            reworks_used: 0,
            reopened_count: 0,
            granted_turns: 0,
            granted_spend: UsdCents::ZERO,
        };
        task.validate()?;
        Ok(task)
    }

    /// Put back the state and version of a row read from the database. Only the store calls
    /// this; it skips the transition check because the row is already persisted.
    pub fn restored(mut self, state: TaskState, version: i64) -> Result<Self, SchemaError> {
        if version < 1 {
            return Err(SchemaError(format!("invalid task version {version}")));
        }
        self.state = state;
        self.version = version;
        Ok(self)
    }

    pub fn state(&self) -> &TaskState {
        &self.state
    }

    pub fn version(&self) -> i64 {
        self.version
    }

    /// Called by the store after its `UPDATE ... WHERE version = expected` succeeded.
    pub fn set_persisted_version(&mut self, version: i64) {
        self.version = version;
    }

    /// The only way to change the state. `now_epoch_s` is an input, not read from a clock,
    /// so this stays a pure function.
    pub fn transition(&mut self, next: TaskState, now_epoch_s: i64) -> Result<(), TransitionError> {
        let illegal = TransitionError {
            from: self.state.code(),
            to: next.code(),
        };
        if !self.state.can_transition(&next) {
            return Err(illegal);
        }
        if let TaskState::OutcomeUnknown { frozen_action_key } = &next
            && frozen_action_key.trim().is_empty()
        {
            return Err(illegal);
        }
        if matches!(next, TaskState::Running) {
            if !matches!(self.state, TaskState::Running) {
                self.running_since_epoch_s = Some(now_epoch_s);
            }
            // Back to work after verification, settlement or completion is a rework.
            if matches!(
                self.state,
                TaskState::Verifying | TaskState::Settling(_) | TaskState::Completed
            ) {
                self.reworks_used += 1;
            }
            if matches!(self.state, TaskState::Completed) {
                self.reopened_count += 1;
            }
        }
        self.state = next;
        Ok(())
    }

    /// Give the current wait a new owner and deadline without changing the state, as when
    /// a hand-off is not taken up in time and passes to the next person.
    pub fn reassign_wait(
        &mut self,
        owner_id: impl Into<String>,
        deadline_epoch_s: i64,
    ) -> Result<(), SchemaError> {
        let wait = match &mut self.state {
            TaskState::AwaitingInput(w)
            | TaskState::AwaitingApproval(w)
            | TaskState::Settling(w)
            | TaskState::HandedOff(w) => w,
            other => return Err(SchemaError(format!("{} has no wait", other.code()))),
        };
        *wait = WaitMetadata::new(owner_id, deadline_epoch_s, wait.timeout_action)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), SchemaError> {
        require_non_empty(&self.task_id, "task_id")?;
        require_non_empty(&self.idempotency_trigger_key, "idempotency_trigger_key")?;
        require_non_empty(&self.task_type, "task_type")?;
        require_non_empty(&self.config_version, "config_version")?;
        require_non_empty(&self.requester_id, "requester_id")?;
        require_non_empty(&self.acting_principal, "acting_principal")?;
        require_non_empty(&self.trigger_source, "trigger_source")?;
        if self.authorized_markets.is_empty() {
            return Err(SchemaError("authorized_markets must not be empty".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    fn all_states() -> Vec<TaskState> {
        TaskState::examples().to_vec()
    }

    /// Diagram 4a written out edge by edge, independent of `can_transition`.
    fn diagram_edges() -> BTreeSet<(&'static str, &'static str)> {
        let mut edges = BTreeSet::new();
        let rows: [(&str, &[&str]); 11] = [
            ("QUEUED", &["RUNNING", "FAILED"]),
            (
                "RUNNING",
                &[
                    "AWAITING_INPUT",
                    "AWAITING_APPROVAL",
                    "OUTCOME_UNKNOWN",
                    "LIMIT_REACHED",
                    "VERIFYING",
                    "FAILED",
                ],
            ),
            ("AWAITING_INPUT", &["RUNNING"]),
            ("AWAITING_APPROVAL", &["RUNNING", "DECLINED"]),
            ("OUTCOME_UNKNOWN", &["RUNNING"]),
            ("LIMIT_REACHED", &["RUNNING", "VERIFYING"]),
            ("VERIFYING", &["RUNNING", "SETTLING", "FAILED"]),
            ("SETTLING", &["COMPLETED", "RUNNING"]),
            ("COMPLETED", &["RUNNING"]),
            ("HANDED_OFF", &["RUNNING"]),
            ("FAILED", &[]),
        ];
        for (from, tos) in rows {
            for to in tos {
                edges.insert((from, *to));
            }
        }
        let working = [
            "QUEUED",
            "RUNNING",
            "AWAITING_INPUT",
            "AWAITING_APPROVAL",
            "OUTCOME_UNKNOWN",
            "LIMIT_REACHED",
            "VERIFYING",
            "SETTLING",
        ];
        for from in working {
            edges.insert((from, "HANDED_OFF"));
            if from != "OUTCOME_UNKNOWN" {
                edges.insert((from, "CANCELLED"));
            }
        }
        edges
    }

    #[test]
    fn transition_table_matches_diagram_4a_exactly() {
        let expected = diagram_edges();
        for from in all_states() {
            for to in all_states() {
                let edge = (from.code(), to.code());
                assert_eq!(
                    from.can_transition(&to),
                    expected.contains(&edge),
                    "{} -> {}",
                    edge.0,
                    edge.1
                );
            }
        }
    }

    #[test]
    fn unknown_codes_are_errors() {
        assert!(TaskState::from_db_parts("BOGUS", None, None).is_err());
        assert!(TaskState::from_db_parts("SETTLING", None, None).is_err());
        assert!(TimeoutAction::from_db_str("LATER").is_err());
    }

    fn new_task() -> TaskRecord {
        TaskRecord::new_queued(
            TaskId::new("T-1"),
            "trigger",
            "rebalance",
            "1",
            "alice",
            "svc",
            "cron",
            vec!["US".into()],
        )
        .unwrap()
    }

    proptest! {
        /// Random walks through the machine never break the invariants of Diagram 4a.
        #[test]
        fn random_walks_keep_diagram_invariants(choices in prop::collection::vec(0usize..13, 0..60)) {
            let states = all_states();
            let mut task = new_task();
            let (mut reopen_moves, mut rework_moves) = (0, 0);
            for (now, i) in choices.into_iter().enumerate() {
                let from = task.state().clone();
                let to = states[i].clone();
                let legal = from.can_transition(&to);
                prop_assert_eq!(task.transition(to.clone(), now as i64).is_ok(), legal);
                if !legal {
                    prop_assert_eq!(task.state(), &from);
                    continue;
                }
                prop_assert!(!from.is_terminal(), "left terminal state {}", from.code());
                match to.code() {
                    "COMPLETED" => prop_assert_eq!(from.code(), "SETTLING"),
                    "SETTLING" => prop_assert_eq!(from.code(), "VERIFYING"),
                    "DECLINED" => prop_assert_eq!(from.code(), "AWAITING_APPROVAL"),
                    "FAILED" => prop_assert!(from.code() != "OUTCOME_UNKNOWN"),
                    _ => {}
                }
                if to.code() == "RUNNING" {
                    if from.code() != "RUNNING" {
                        prop_assert_eq!(task.running_since_epoch_s, Some(now as i64));
                    }
                    if matches!(from.code(), "VERIFYING" | "SETTLING" | "COMPLETED") {
                        rework_moves += 1;
                    }
                    if from.code() == "COMPLETED" {
                        reopen_moves += 1;
                    }
                }
            }
            prop_assert_eq!(task.reopened_count, reopen_moves);
            prop_assert_eq!(task.reworks_used, rework_moves);
        }
    }
}
