//! Exact 13-state algebraic state machine per Diagram 4a:
//! `QUEUED`, `RUNNING`, `AWAITING_INPUT`, `AWAITING_APPROVAL`, `OUTCOME_UNKNOWN`,
//! `LIMIT_REACHED`, `VERIFYING`, `SETTLING`, `COMPLETED`, `FAILED`, `HANDED_OFF`,
//! `DECLINED`, `CANCELLED`.
//! `TaskRecord.state` and `TaskRecord.version` are strictly private and can only change
//! via `TaskRecord::transition()`, which validates Diagram 4a's transition matrix.

use std::fmt;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::types::{require_non_empty, SchemaError, UsdCents};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitMetadata {
    pub owner_id: String,
    pub deadline_iso: String,
    pub timeout_action: String,
}

impl WaitMetadata {
    pub fn new(
        owner_id: impl Into<String>,
        deadline_iso: impl Into<String>,
        timeout_action: impl Into<String>,
    ) -> Result<Self, SchemaError> {
        let owner_id = owner_id.into();
        let deadline_iso = deadline_iso.into();
        let timeout_action = timeout_action.into();
        require_non_empty(&owner_id, "wait_meta.owner_id")?;
        require_non_empty(&deadline_iso, "wait_meta.deadline_iso")?;
        require_non_empty(&timeout_action, "wait_meta.timeout_action")?;
        Ok(Self {
            owner_id,
            deadline_iso,
            timeout_action,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Queued,
    Running,
    AwaitingInput(WaitMetadata),
    AwaitingApproval(WaitMetadata),
    OutcomeUnknown { frozen_action_key: String },
    LimitReached { reason: String },
    Verifying,
    Settling(WaitMetadata),
    Completed,
    Failed { reason: String },
    HandedOff(WaitMetadata),
    Declined { reason: String },
    Cancelled { reason: String },
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
            | Self::HandedOff(w) => Some(w),
            _ => None,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::OutcomeUnknown { frozen_action_key } => Some(frozen_action_key.as_str()),
            Self::LimitReached { reason }
            | Self::Failed { reason }
            | Self::Declined { reason }
            | Self::Cancelled { reason } => Some(reason.as_str()),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Failed { .. } | Self::Declined { .. } | Self::Cancelled { .. }
        )
    }

    /// Strict DB deserialization: NEVER defaults unrecognized status strings to `Failed`!
    pub fn from_db_parts(
        code: &str,
        detail: Option<String>,
        wait_meta: Option<WaitMetadata>,
    ) -> Result<Self, SchemaError> {
        let require_wait = |state_name: &str| -> Result<WaitMetadata, SchemaError> {
            wait_meta.ok_or_else(|| {
                SchemaError(format!(
                    "waiting state {state_name} missing required WaitMetadata"
                ))
            })
        };

        match code {
            "QUEUED" => Ok(Self::Queued),
            "RUNNING" => Ok(Self::Running),
            "AWAITING_INPUT" => Ok(Self::AwaitingInput(require_wait("AWAITING_INPUT")?)),
            "AWAITING_APPROVAL" => Ok(Self::AwaitingApproval(require_wait("AWAITING_APPROVAL")?)),
            "OUTCOME_UNKNOWN" => {
                let key = detail.unwrap_or_default();
                require_non_empty(&key, "outcome_unknown.frozen_action_key")?;
                Ok(Self::OutcomeUnknown {
                    frozen_action_key: key,
                })
            }
            "LIMIT_REACHED" => Ok(Self::LimitReached {
                reason: detail.unwrap_or_default(),
            }),
            "VERIFYING" => Ok(Self::Verifying),
            "SETTLING" => Ok(Self::Settling(require_wait("SETTLING")?)),
            "COMPLETED" => Ok(Self::Completed),
            "FAILED" => Ok(Self::Failed {
                reason: detail.unwrap_or_default(),
            }),
            "HANDED_OFF" => Ok(Self::HandedOff(require_wait("HANDED_OFF")?)),
            "DECLINED" => Ok(Self::Declined {
                reason: detail.unwrap_or_default(),
            }),
            "CANCELLED" => Ok(Self::Cancelled {
                reason: detail.unwrap_or_default(),
            }),
            unknown => Err(SchemaError(format!("unknown_task_state:{unknown}"))),
        }
    }

    /// Diagram 4a explicit legal state transition matrix.
    pub fn can_transition(&self, next: &Self) -> bool {
        match (self, next) {
            // QUEUED -> RUNNING | DECLINED | CANCELLED | HANDED_OFF
            (
                Self::Queued,
                Self::Running
                | Self::Declined { .. }
                | Self::Cancelled { .. }
                | Self::HandedOff(_),
            ) => true,

            // RUNNING -> active wait / error / verification / close states
            (
                Self::Running,
                Self::AwaitingInput(_)
                | Self::AwaitingApproval(_)
                | Self::OutcomeUnknown { .. }
                | Self::LimitReached { .. }
                | Self::Verifying
                | Self::HandedOff(_)
                | Self::Failed { .. }
                | Self::Declined { .. }
                | Self::Cancelled { .. },
            ) => true,

            // AWAITING_INPUT -> RUNNING | HANDED_OFF | DECLINED | CANCELLED | LIMIT_REACHED
            (
                Self::AwaitingInput(_),
                Self::Running
                | Self::HandedOff(_)
                | Self::Declined { .. }
                | Self::Cancelled { .. }
                | Self::LimitReached { .. },
            ) => true,

            // AWAITING_APPROVAL -> RUNNING | DECLINED | HANDED_OFF | CANCELLED | LIMIT_REACHED
            (
                Self::AwaitingApproval(_),
                Self::Running
                | Self::Declined { .. }
                | Self::HandedOff(_)
                | Self::Cancelled { .. }
                | Self::LimitReached { .. },
            ) => true,

            // OUTCOME_UNKNOWN -> strictly frozen until reconciled (RUNNING) or escalated (HANDED_OFF / FAILED)
            (
                Self::OutcomeUnknown { .. },
                Self::Running | Self::HandedOff(_) | Self::Failed { .. },
            ) => true,

            // LIMIT_REACHED -> HANDED_OFF | RUNNING (human limit extension) | CANCELLED | FAILED
            (
                Self::LimitReached { .. },
                Self::HandedOff(_)
                | Self::Running
                | Self::Cancelled { .. }
                | Self::Failed { .. },
            ) => true,

            // VERIFYING -> SETTLING | RUNNING (rework) | LIMIT_REACHED | HANDED_OFF | FAILED
            (
                Self::Verifying,
                Self::Settling(_)
                | Self::Running
                | Self::LimitReached { .. }
                | Self::HandedOff(_)
                | Self::Failed { .. },
            ) => true,

            // SETTLING -> COMPLETED | RUNNING (disputed) | HANDED_OFF | FAILED
            (
                Self::Settling(_),
                Self::Completed | Self::Running | Self::HandedOff(_) | Self::Failed { .. },
            ) => true,

            // COMPLETED -> RUNNING (post-settlement reopen per Agent 7 5.3)
            (Self::Completed, Self::Running) => true,

            // HANDED_OFF -> RUNNING | COMPLETED | DECLINED | CANCELLED | FAILED
            (
                Self::HandedOff(_),
                Self::Running
                | Self::Completed
                | Self::Declined { .. }
                | Self::Cancelled { .. }
                | Self::Failed { .. },
            ) => true,

            // Terminal states (FAILED, DECLINED, CANCELLED) and all unlisted transitions are illegal
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: &'static str,
    pub to: &'static str,
    pub detail: String,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Illegal state transition {} -> {}: {}",
            self.from, self.to, self.detail
        )
    }
}

impl std::error::Error for TransitionError {}

/// External task record with private `state` and optimistic-concurrency `version`.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub idempotency_trigger_key: String,
    pub task_type: String,
    pub config_version: String,
    pub requester_id: String,
    pub acting_principal: String,
    pub trigger_source: String,
    pub authorized_markets: Vec<String>,
    state: TaskState,
    version: i64,
    pub turns_used: u32,
    pub spend: UsdCents,
    pub retries_used: u32,
    pub consecutive_denials: u32,
    pub reworks_used: u32,
    pub reopened_count: u32,
}

impl TaskRecord {
    pub fn new_queued(
        task_id: impl Into<String>,
        idempotency_trigger_key: impl Into<String>,
        task_type: impl Into<String>,
        config_version: impl Into<String>,
        requester_id: impl Into<String>,
        acting_principal: impl Into<String>,
        trigger_source: impl Into<String>,
        authorized_markets: Vec<String>,
    ) -> Result<Self, SchemaError> {
        let rec = Self {
            task_id: task_id.into(),
            idempotency_trigger_key: idempotency_trigger_key.into(),
            task_type: task_type.into(),
            config_version: config_version.into(),
            requester_id: requester_id.into(),
            acting_principal: acting_principal.into(),
            trigger_source: trigger_source.into(),
            authorized_markets,
            state: TaskState::Queued,
            version: 1,
            turns_used: 0,
            spend: UsdCents::ZERO,
            retries_used: 0,
            consecutive_denials: 0,
            reworks_used: 0,
            reopened_count: 0,
        };
        rec.validate()?;
        Ok(rec)
    }

    /// Rehydrate a persisted `TaskRecord` from DB row columns (used only by repository layer).
    #[allow(clippy::too_many_arguments)]
    pub fn rehydrate_from_db(
        task_id: String,
        idempotency_trigger_key: String,
        task_type: String,
        config_version: String,
        requester_id: String,
        acting_principal: String,
        trigger_source: String,
        authorized_markets: Vec<String>,
        state: TaskState,
        version: i64,
        turns_used: u32,
        spend: UsdCents,
        retries_used: u32,
        consecutive_denials: u32,
        reworks_used: u32,
        reopened_count: u32,
    ) -> Result<Self, SchemaError> {
        if version < 1 {
            return Err(SchemaError(format!("invalid task version {version} < 1")));
        }
        let rec = Self {
            task_id,
            idempotency_trigger_key,
            task_type,
            config_version,
            requester_id,
            acting_principal,
            trigger_source,
            authorized_markets,
            state,
            version,
            turns_used,
            spend,
            retries_used,
            consecutive_denials,
            reworks_used,
            reopened_count,
        };
        rec.validate()?;
        Ok(rec)
    }

    pub fn state(&self) -> &TaskState {
        &self.state
    }

    pub fn version(&self) -> i64 {
        self.version
    }

    /// Advance in-memory version ONLY after the DB `UPDATE ... WHERE version = expected` succeeds.
    pub fn advance_persisted_version(&mut self, new_version: i64) {
        self.version = new_version;
    }

    /// The ONLY way to mutate `self.state`. Validates Diagram 4a's transition matrix!
    pub fn transition(&mut self, next: TaskState) -> Result<(), TransitionError> {
        if let TaskState::OutcomeUnknown { frozen_action_key } = &next {
            if frozen_action_key.trim().is_empty() {
                return Err(TransitionError {
                    from: self.state.code(),
                    to: next.code(),
                    detail: "OutcomeUnknown requires non-empty frozen_action_key".into(),
                });
            }
        }
        if !self.state.can_transition(&next) {
            return Err(TransitionError {
                from: self.state.code(),
                to: next.code(),
                detail: "violates Diagram 4a state transition table".into(),
            });
        }
        if matches!(self.state, TaskState::Completed) && matches!(next, TaskState::Running) {
            self.reopened_count = self.reopened_count.saturating_add(1);
            self.reworks_used = self.reworks_used.saturating_add(1);
        }
        self.state = next;
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

// --- Compile-Time Type-State Lifecycle Phases ---

#[derive(Debug, Clone, Copy)]
pub struct PhaseQueued;
#[derive(Debug, Clone, Copy)]
pub struct PhaseFramed;
#[derive(Debug, Clone, Copy)]
pub struct PhaseSettling;
#[derive(Debug, Clone, Copy)]
pub struct PhaseCompleted;

#[derive(Debug, Clone)]
pub struct TypedTask<Phase> {
    record: TaskRecord,
    _phase: PhantomData<Phase>,
}

impl<Phase> TypedTask<Phase> {
    pub fn record(&self) -> &TaskRecord {
        &self.record
    }

    pub fn into_record(self) -> TaskRecord {
        self.record
    }
}

impl TypedTask<PhaseQueued> {
    pub fn from_queued(record: TaskRecord) -> Result<Self, SchemaError> {
        if !matches!(record.state(), TaskState::Queued) {
            return Err(SchemaError(format!(
                "Expected QUEUED state, got {}",
                record.state().code()
            )));
        }
        Ok(Self {
            record,
            _phase: PhantomData,
        })
    }

    pub fn into_framed(mut self) -> Result<TypedTask<PhaseFramed>, TransitionError> {
        self.record.transition(TaskState::Running)?;
        Ok(TypedTask {
            record: self.record,
            _phase: PhantomData,
        })
    }
}

impl TypedTask<PhaseFramed> {
    pub fn from_verifying(record: TaskRecord) -> Result<Self, SchemaError> {
        if !matches!(record.state(), TaskState::Verifying) {
            return Err(SchemaError(format!(
                "Expected VERIFYING state, got {}",
                record.state().code()
            )));
        }
        Ok(Self {
            record,
            _phase: PhantomData,
        })
    }

    pub fn into_settling(
        mut self,
        confirm_wait: WaitMetadata,
    ) -> Result<TypedTask<PhaseSettling>, TransitionError> {
        self.record.transition(TaskState::Settling(confirm_wait))?;
        Ok(TypedTask {
            record: self.record,
            _phase: PhantomData,
        })
    }
}

impl TypedTask<PhaseSettling> {
    pub fn into_completed(mut self) -> Result<TypedTask<PhaseCompleted>, TransitionError> {
        self.record.transition(TaskState::Completed)?;
        Ok(TypedTask {
            record: self.record,
            _phase: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{ActionIntentStatus, ApprovalStatus, RiskTier, TaskTypeConfig, AutonomyLevel, BudgetLimits, ShapeLevel};
    use crate::types::sha256_hex;
    use std::collections::BTreeMap;

    #[test]
    fn sha256_via_sha2_matches_nist() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn diagram_4a_transitions_and_private_state_enforced() {
        let mut t = TaskRecord::new_queued(
            "T-1",
            "trig-1",
            "rebalance",
            "1.0",
            "alice",
            "svc",
            "cron",
            vec!["US".into()],
        )
        .unwrap();
        assert_eq!(t.state().code(), "QUEUED");
        assert_eq!(t.version(), 1);

        // Illegal jump QUEUED -> COMPLETED must fail!
        assert!(t.transition(TaskState::Completed).is_err());

        // Legal path: QUEUED -> RUNNING -> OUTCOME_UNKNOWN -> RUNNING -> VERIFYING -> SETTLING -> COMPLETED
        t.transition(TaskState::Running).unwrap();
        t.transition(TaskState::OutcomeUnknown {
            frozen_action_key: "act-1".into(),
        })
        .unwrap();
        // Cannot jump OUTCOME_UNKNOWN -> VERIFYING without reconciling first!
        assert!(t.transition(TaskState::Verifying).is_err());
        t.transition(TaskState::Running).unwrap();
        t.transition(TaskState::Verifying).unwrap();
        let wm = WaitMetadata::new("alice", "2099-01-01T00:00:00Z", "escalate").unwrap();
        t.transition(TaskState::Settling(wm)).unwrap();
        t.transition(TaskState::Completed).unwrap();
        assert_eq!(t.reopened_count, 0);

        // Reopen COMPLETED -> RUNNING increments reopened_count & reworks_used
        t.transition(TaskState::Running).unwrap();
        assert_eq!(t.reopened_count, 1);
        assert_eq!(t.reworks_used, 1);

        // Terminal state DECLINED blocks all outgoing transitions
        t.transition(TaskState::Declined {
            reason: "rejected".into(),
        })
        .unwrap();
        assert!(t.transition(TaskState::Running).is_err());
    }

    #[test]
    fn unknown_status_strings_error_instead_of_defaulting_to_failed() {
        assert!(TaskState::from_db_parts("BOGUS_STATE", None, None).is_err());
        assert!(ActionIntentStatus::from_db_str("GARBAGE").is_err());
        assert!(ApprovalStatus::from_db_str("MAYBE").is_err());
        assert_eq!(
            ActionIntentStatus::from_db_str("DISPATCHED").unwrap(),
            ActionIntentStatus::Dispatched
        );
    }

    #[test]
    fn diagram_3a_unlisted_tool_is_none_and_r4_exists() {
        let mut allowed = BTreeMap::new();
        allowed.insert("submit_paper_orders".into(), RiskTier::R3);
        allowed.insert("direct_wire_transfer".into(), RiskTier::R4);
        let cfg = TaskTypeConfig {
            task_type: "t".into(),
            version: "1".into(),
            shape_level: ShapeLevel::MultiAgent,
            shape_rationale: "r".into(),
            autonomy_level: AutonomyLevel::PerActionApproval,
            config_admin_id: "admin".into(),
            allowed_tools: allowed,
            budget_limits: BudgetLimits::default(),
            done_criteria: vec!["ok".into()],
        };
        assert_eq!(cfg.lookup_tool_risk("unlisted_tool"), None);
        assert_eq!(cfg.lookup_tool_risk("submit_paper_orders"), Some(RiskTier::R3));
        assert_eq!(cfg.lookup_tool_risk("direct_wire_transfer"), Some(RiskTier::R4));
    }
}
