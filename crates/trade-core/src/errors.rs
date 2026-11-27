//! Algebraic error hierarchy distinguishing pre-dispatch rejections from post-dispatch
//! uncertain outcomes (`OUTCOME_UNKNOWN`).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::state::TransitionError;
use crate::types::SchemaError;

/// Failure occurring strictly BEFORE an external/consequential request is dispatched.
/// Safe to deny without putting the task into `OUTCOME_UNKNOWN`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreDispatchError {
    ToolNotConfigured(String),
    R4PermanentlyProhibited(String),
    LimitReached(String),
    TaskFrozenOutcomeUnknown(String),
    /// The task is not in RUNNING (waiting, closed, or out of budget).
    TaskNotActive(String),
    InvalidArguments(String),
    PromptInjectionDetected(String),
    UnauthorizedMarket(String),
    BusinessRuleViolation(String),
    AutonomyModeBlocked(String),
    ApprovalNotFound(String),
    ApprovalNotApproved(String),
    ApprovalRejected(String),
    ApprovalAlreadyUsed(String),
    ApprovalExpired(String),
    ApprovalTaskMismatch,
    ApprovalPayloadMismatch,
    SeparationOfDutiesViolation(String),
    UnreconciledIntentExists(String),
    /// The idempotency key was already used for a different payload.
    IdempotencyKeyReused(String),
    MissingCredential(String),
    CheckFailed(String),
}

impl PreDispatchError {
    pub fn code(&self) -> String {
        match self {
            Self::ToolNotConfigured(t) => format!("tool_not_in_config:{t}"),
            Self::R4PermanentlyProhibited(t) => format!("r4_permanently_prohibited:{t}"),
            Self::LimitReached(r) => format!("limit_reached:{r}"),
            Self::TaskFrozenOutcomeUnknown(k) => {
                format!("frozen_outcome_unknown_reconcile_first:{k}")
            }
            Self::TaskNotActive(s) => format!("task_not_running:{s}"),
            Self::InvalidArguments(r) => format!("invalid_arguments:{r}"),
            Self::PromptInjectionDetected(r) => format!("prompt_injection_detected:{r}"),
            Self::UnauthorizedMarket(m) => format!("unauthorized_market:{m}"),
            Self::BusinessRuleViolation(r) => format!("business_rule_violation:{r}"),
            Self::AutonomyModeBlocked(r) => format!("autonomy_blocked:{r}"),
            Self::ApprovalNotFound(id) => format!("approval_not_found:{id}"),
            Self::ApprovalNotApproved(st) => format!("approval_not_approved:{st}"),
            Self::ApprovalRejected(id) => format!("approval_rejected:{id}"),
            Self::ApprovalAlreadyUsed(id) => format!("approval_already_used:{id}"),
            Self::ApprovalExpired(id) => format!("approval_expired:{id}"),
            Self::ApprovalTaskMismatch => "approval_task_mismatch".into(),
            Self::ApprovalPayloadMismatch => "approval_payload_hash_mismatch".into(),
            Self::SeparationOfDutiesViolation(r) => format!("separation_of_duties:{r}"),
            Self::UnreconciledIntentExists(k) => format!("existing_intent_unreconciled:{k}"),
            Self::IdempotencyKeyReused(k) => {
                format!("idempotency_key_reused_with_other_payload:{k}")
            }
            Self::MissingCredential(c) => format!("missing_credential:{c}"),
            Self::CheckFailed(r) => format!("gate_check_failed:{r}"),
        }
    }
}

impl fmt::Display for PreDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

impl std::error::Error for PreDispatchError {}

/// Top-level gate & workflow error distinguishing pre-dispatch failures from post-dispatch
/// uncertain outcomes (`PostDispatchUnknown` -> `TaskState::OutcomeUnknown`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// Failed before dispatching the action; task does NOT enter `OUTCOME_UNKNOWN`.
    PreDispatch(PreDispatchError),
    /// Failed AFTER dispatching the action (timeout, transport drop, broker error, or read-back failure).
    /// Task is unconditionally transitioned to `OUTCOME_UNKNOWN` and frozen until reconciled!
    PostDispatchUnknown {
        idempotency_key: String,
        detail: String,
    },
    /// The tool failed before its request left; the action is marked failed and may be
    /// retried under the same idempotency key. The task stays in RUNNING.
    NotSent(String),
    /// Storage or schema integrity error.
    Storage(String),
    /// Another writer changed the task first; reload and decide again.
    Conflict(String),
    /// Illegal state transition per Diagram 4a.
    StateTransition(TransitionError),
    /// Domain schema validation error.
    Schema(SchemaError),
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreDispatch(e) => write!(f, "PreDispatch({e})"),
            Self::PostDispatchUnknown {
                idempotency_key,
                detail,
            } => write!(
                f,
                "PostDispatchUnknown(idempotency_key={idempotency_key}, detail={detail})"
            ),
            Self::NotSent(s) => write!(f, "NotSent({s})"),
            Self::Storage(s) => write!(f, "Storage({s})"),
            Self::Conflict(s) => write!(f, "Conflict({s})"),
            Self::StateTransition(e) => write!(f, "StateTransition({e})"),
            Self::Schema(e) => write!(f, "Schema({e})"),
        }
    }
}

impl std::error::Error for GateError {}

impl From<PreDispatchError> for GateError {
    fn from(v: PreDispatchError) -> Self {
        Self::PreDispatch(v)
    }
}

impl From<TransitionError> for GateError {
    fn from(v: TransitionError) -> Self {
        Self::StateTransition(v)
    }
}

impl From<SchemaError> for GateError {
    fn from(v: SchemaError) -> Self {
        Self::Schema(v)
    }
}

/// How a tool's execution failed. The difference decides whether the task must be frozen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// The request never left; nothing can have happened. The action is marked failed.
    NotSent(String),
    /// The request may have taken effect; the task goes to `OUTCOME_UNKNOWN`.
    Unknown(String),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(s) => write!(f, "not_sent:{s}"),
            Self::Unknown(s) => write!(f, "outcome_unknown:{s}"),
        }
    }
}

impl std::error::Error for ExecError {}
