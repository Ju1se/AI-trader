#![forbid(unsafe_code)]
//! The action gate of Diagram 3a.
//!
//! `decide` is a pure function of a request and a context that the store read in one go.
//! `run` is the thin shell around it: load, decide, write the verdict in one transaction
//! guarded by the task's version, execute, read back, record. Nothing holds a borrow of the
//! task across an `.await`; if someone changes the task in between, the write fails with
//! `GateError::Conflict` and `run` reads again and decides again. That retry only happens
//! before the request leaves, so an action is never sent twice.

use std::future::Future;

use trade_core::{
    ActionIntentRecord, ActionIntentStatus, ApprovalId, ApprovalRecord, ApprovalStatus,
    AutonomyLevel, ExecError, GateError, IdempotencyKey, PreDispatchError, RiskTier, TaskId,
    TaskRecord, TaskState, TaskTypeConfig, TimeoutAction, WaitMetadata, detect_prompt_injection,
    sha256_hex,
};

/// Proof that the gate allowed this execution. Only this crate can create one, so a tool's
/// side effect cannot be triggered from outside the gate.
pub struct VerifiedGateTicket(());

/// One proposed tool call. The task and the idempotency key come from the harness, never
/// from the model.
#[derive(Debug, Clone)]
pub struct GateRequest<A> {
    pub task_id: TaskId,
    pub target: String,
    pub args: A,
    pub idempotency_key: IdempotencyKey,
    pub approval_id: Option<ApprovalId>,
}

/// Everything `decide` looks at, read from the store in one go. Nothing in it comes from
/// the proposer.
#[derive(Debug, Clone)]
pub struct GateContext {
    pub task: TaskRecord,
    pub config: TaskTypeConfig,
    /// The approval the request names, if any.
    pub approval: Option<ApprovalRecord>,
    /// The action already registered under the request's idempotency key, if any.
    pub existing_intent: Option<ActionIntentRecord>,
    /// Any action of this task that was dispatched and not yet settled.
    pub unresolved_intent: Option<IdempotencyKey>,
    pub now_epoch_s: i64,
}

/// An allowed action, ready to be registered and sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    pub tier: RiskTier,
    pub canonical_args: String,
    pub payload_hash: String,
    /// The approval this dispatch uses up, when it needed one.
    pub approval_id: Option<ApprovalId>,
    /// Sending again an action that never left counts as a retry.
    pub is_retry: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Proceed(Dispatch),
    NeedsApproval(Dispatch),
    /// The same action already ran and was verified; nothing is sent again.
    Replay {
        receipt_id: String,
        payload_hash: String,
    },
    /// Step 1: a limit is used up. The task moves to LIMIT_REACHED.
    Limit(String),
    /// Step 1: the task is frozen or not running. Not the proposer's fault, not counted.
    Stop(PreDispatchError),
    /// Refused. The reason goes back to the proposer as data and counts as a denial.
    Deny(PreDispatchError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    Executed {
        receipt_id: String,
        payload_hash: String,
        replayed: bool,
    },
    NeedsApproval {
        approval_id: ApprovalId,
        payload_hash: String,
    },
}

/// What happened after the intent was marked DISPATCHED.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchResult {
    Verified {
        receipt_id: String,
    },
    /// The request never left: the intent is marked FAILED and the task keeps running.
    NotSent {
        detail: String,
    },
    /// It may have happened: the intent is UNKNOWN and the task goes to OUTCOME_UNKNOWN.
    Unknown {
        receipt_id: Option<String>,
        detail: String,
    },
}

/// An approval request for the store to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApproval {
    pub tool_name: String,
    pub target: String,
    pub payload_hash: String,
    pub args_json: String,
    pub expires_at_epoch_s: i64,
}

/// The persistence the gate needs. `trade-store` implements it; each method is one
/// transaction.
pub trait GateLedger {
    /// Read the task and everything else `decide` needs. Fails if the task does not exist.
    fn load_context(
        &self,
        task_id: &TaskId,
        key: &IdempotencyKey,
        approval_id: Option<&ApprovalId>,
    ) -> impl Future<Output = Result<GateContext, GateError>>;

    /// Write `task` if the stored version still equals `task.version()`, otherwise fail
    /// with `GateError::Conflict`.
    fn save_task(
        &self,
        task: &TaskRecord,
        reason: &str,
    ) -> impl Future<Output = Result<(), GateError>>;

    /// Count a denied proposal (one turn and one consecutive denial) and log the reason.
    /// The state does not change.
    fn record_denial(
        &self,
        task_id: &TaskId,
        reason: &PreDispatchError,
    ) -> impl Future<Output = Result<(), GateError>>;

    /// Create a pending approval and save `task`, now AWAITING_APPROVAL, together.
    fn request_approval(
        &self,
        task: &TaskRecord,
        approval: &NewApproval,
    ) -> impl Future<Output = Result<ApprovalId, GateError>>;

    /// In one transaction: use up the approval if it is still APPROVED for this payload,
    /// register the intent as DISPATCHED, and save `task`. If any part fails, none happens.
    fn begin_dispatch(
        &self,
        task: &TaskRecord,
        approval_id: Option<&ApprovalId>,
        intent: &ActionIntentRecord,
    ) -> impl Future<Output = Result<(), GateError>>;

    /// Record what happened after dispatch. `Verified` also clears the consecutive-denial
    /// count. `NotSent` gives back the approval the action used, since nothing was sent.
    /// `Unknown` also moves the task to OUTCOME_UNKNOWN, whatever its version is now.
    fn finish_dispatch(
        &self,
        task_id: &TaskId,
        key: &IdempotencyKey,
        result: &DispatchResult,
    ) -> impl Future<Output = Result<(), GateError>>;
}

/// A governed tool. `decide` uses only the pure methods; `load_facts`, `execute` and
/// `readback` are the ones with effects.
pub trait Tool {
    type Args;
    /// What the business rules are checked against, such as the stored veto list.
    type Facts;

    fn name(&self) -> &str;
    /// The lowest tier this tool may run at. Configuration can raise it, never lower it.
    fn min_tier(&self) -> RiskTier;
    /// Canonical JSON of the arguments, which the approval hash covers. An error means
    /// the arguments are malformed.
    fn canonical_args(&self, args: &Self::Args) -> Result<String, PreDispatchError>;
    /// Read what `check` needs, from the store or the target system.
    fn load_facts(
        &self,
        task: &TaskRecord,
        args: &Self::Args,
    ) -> impl Future<Output = Result<Self::Facts, GateError>>;
    /// Scope and business rules. Pure.
    fn check(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
        facts: &Self::Facts,
    ) -> Result<(), PreDispatchError>;
    /// The side effect. Returns the target system's receipt ID.
    fn execute(
        &self,
        ticket: &VerifiedGateTicket,
        task_id: &TaskId,
        key: &IdempotencyKey,
        args: &Self::Args,
    ) -> impl Future<Output = Result<String, ExecError>>;
    /// Read the target system independently. `Ok` only if it shows exactly what was asked.
    fn readback(
        &self,
        args: &Self::Args,
        receipt_id: &str,
    ) -> impl Future<Output = Result<(), String>>;
}

pub fn payload_hash(tool: &str, target: &str, canonical_args: &str) -> String {
    sha256_hex(format!("{tool}|{target}|{canonical_args}").as_bytes())
}

/// Diagram 3a in its fixed order. Every refusal comes before any request for approval.
pub fn decide<T: Tool>(
    tool: &T,
    req: &GateRequest<T::Args>,
    ctx: &GateContext,
    facts: &T::Facts,
) -> Verdict {
    match evaluate(tool, req, ctx, facts) {
        Ok(verdict) | Err(verdict) => verdict,
    }
}

/// `Err` carries an early verdict, so each step can end the evaluation with `?`.
fn evaluate<T: Tool>(
    tool: &T,
    req: &GateRequest<T::Args>,
    ctx: &GateContext,
    facts: &T::Facts,
) -> Result<Verdict, Verdict> {
    use PreDispatchError as E;
    use Verdict::{Deny, Stop};
    let (task, config, name) = (&ctx.task, &ctx.config, tool.name());

    // 0. The tool is in this task type's configuration.
    let tier = config
        .lookup_tool_risk(name)
        .ok_or_else(|| Deny(E::ToolNotConfigured(name.into())))?
        .max(tool.min_tier());

    // 1. The task is running, not frozen, and within every limit.
    if let TaskState::OutcomeUnknown { frozen_action_key } = task.state() {
        return Err(Stop(E::TaskFrozenOutcomeUnknown(
            frozen_action_key.to_string(),
        )));
    }
    if let Some(key) = &ctx.unresolved_intent {
        return Err(Stop(E::UnreconciledIntentExists(key.to_string())));
    }
    if !matches!(task.state(), TaskState::Running) {
        return Err(Stop(E::TaskNotActive(task.state().code().into())));
    }
    let is_retry = ctx
        .existing_intent
        .as_ref()
        .is_some_and(|intent| intent.status == ActionIntentStatus::Failed);
    if let Some(limit) = used_up_limit(task, config, ctx.now_epoch_s, is_retry) {
        return Err(Verdict::Limit(limit.into()));
    }

    // 2. The arguments are well formed.
    if req.target.trim().is_empty() {
        return Err(Deny(E::InvalidArguments("empty_target".into())));
    }
    let canonical_args = tool.canonical_args(&req.args).map_err(Deny)?;

    // 3. Hard denials: never allowed, or text trying to steer the system.
    if tier == RiskTier::R4 {
        return Err(Deny(E::R4PermanentlyProhibited(name.into())));
    }
    if detect_prompt_injection(&req.target) || detect_prompt_injection(&canonical_args) {
        return Err(Deny(E::PromptInjectionDetected(name.into())));
    }

    // 4. Permission: what this task type's autonomy level allows.
    let blocked = match (config.autonomy_level, tier) {
        (AutonomyLevel::Shadow, RiskTier::R2 | RiskTier::R3) => Some("shadow_blocks_writes"),
        (AutonomyLevel::DraftOnly, RiskTier::R3) => Some("draft_only_blocks_r3"),
        _ => None,
    };
    if let Some(why) = blocked {
        return Err(Deny(E::AutonomyModeBlocked(why.into())));
    }

    // 5. Scope and business rules, checked against facts from the store.
    tool.check(&req.args, task, facts).map_err(Deny)?;

    // 7a. An action already registered under this key is replayed or refused, never sent
    // again. Checked before approval so that a replay does not ask for a second approval.
    let payload_hash = payload_hash(name, &req.target, &canonical_args);
    if let Some(intent) = &ctx.existing_intent {
        let key = || req.idempotency_key.to_string();
        match (&intent.status, &intent.backend_receipt_id) {
            _ if intent.payload_hash != payload_hash => {
                return Err(Deny(E::IdempotencyKeyReused(key())));
            }
            (ActionIntentStatus::Verified, Some(receipt_id)) => {
                return Err(Verdict::Replay {
                    receipt_id: receipt_id.clone(),
                    payload_hash,
                });
            }
            (ActionIntentStatus::Failed, _) => {}
            _ => return Err(Stop(E::UnreconciledIntentExists(key()))),
        }
    }

    // 6. A person approves R3 actions, and R2 actions under per-action approval.
    let needs_approval = tier == RiskTier::R3
        || (tier == RiskTier::R2 && config.autonomy_level == AutonomyLevel::PerActionApproval);
    let mut dispatch = Dispatch {
        tier,
        canonical_args,
        payload_hash,
        approval_id: None,
        is_retry,
    };
    if needs_approval {
        let Some(approval_id) = &req.approval_id else {
            return Ok(Verdict::NeedsApproval(dispatch));
        };
        check_approval(approval_id, ctx, &dispatch.payload_hash).map_err(Deny)?;
        dispatch.approval_id = Some(approval_id.clone());
    }

    // 7–8. Register and send. The store checks the approval and the task version again in
    // the same transaction that marks the intent DISPATCHED.
    Ok(Verdict::Proceed(dispatch))
}

fn check_approval(
    id: &ApprovalId,
    ctx: &GateContext,
    payload_hash: &str,
) -> Result<(), PreDispatchError> {
    use PreDispatchError as E;
    let approval = ctx
        .approval
        .as_ref()
        .filter(|a| &a.approval_id == id)
        .ok_or_else(|| E::ApprovalNotFound(id.to_string()))?;
    if approval.task_id != ctx.task.task_id {
        return Err(E::ApprovalTaskMismatch);
    }
    match approval.status {
        ApprovalStatus::Approved => {}
        ApprovalStatus::Consumed => return Err(E::ApprovalAlreadyUsed(id.to_string())),
        ApprovalStatus::Rejected => return Err(E::ApprovalRejected(id.to_string())),
        ApprovalStatus::Expired => return Err(E::ApprovalExpired(id.to_string())),
        ApprovalStatus::Pending => return Err(E::ApprovalNotApproved("PENDING".into())),
    }
    if ctx.now_epoch_s >= approval.expires_at_epoch_s {
        return Err(E::ApprovalExpired(id.to_string()));
    }
    if approval.payload_hash != payload_hash {
        return Err(E::ApprovalPayloadMismatch);
    }
    // The approver is neither the requester nor the person who configured the task type.
    let approver = approval.approver_id.as_deref().unwrap_or_default();
    if approver.is_empty()
        || approver == ctx.task.requester_id
        || approver == ctx.config.config_admin_id
    {
        return Err(E::SeparationOfDutiesViolation(approver.into()));
    }
    Ok(())
}

/// The first limit this task has used up, if any. Execution may not touch the spend
/// reserved for closing.
pub fn used_up_limit(
    task: &TaskRecord,
    config: &TaskTypeConfig,
    now_epoch_s: i64,
    is_retry: bool,
) -> Option<&'static str> {
    let limits = &config.budget_limits;
    let running_ms = task.running_since_epoch_s.map_or(0, |since| {
        (now_epoch_s - since).max(0).saturating_mul(1_000)
    });
    let max_wall_ms = i64::try_from(limits.max_wall_ms).unwrap_or(i64::MAX);
    [
        (task.turns_used >= limits.max_turns, "max_turns"),
        (task.spend >= limits.usable_execution_spend(), "max_spend"),
        (running_ms > max_wall_ms, "max_wall_clock"),
        (
            is_retry && task.retries_used >= limits.max_retries,
            "max_retries",
        ),
        (
            task.consecutive_denials >= limits.max_consecutive_denials,
            "max_consecutive_denials",
        ),
        (task.reworks_used > limits.max_reworks, "max_reworks"),
    ]
    .into_iter()
    .find_map(|(used_up, limit)| used_up.then_some(limit))
}

/// How many times `run` reads again and decides again when the task changed under it.
const MAX_ATTEMPTS: usize = 3;

/// One proposed call, end to end: decide, dispatch, execute, read back, record.
pub async fn run<L: GateLedger, T: Tool>(
    ledger: &L,
    tool: &T,
    req: &GateRequest<T::Args>,
) -> Result<GateOutcome, GateError> {
    let dispatch = match prepare(ledger, tool, req).await? {
        Prepared::Done(outcome) => return Ok(outcome),
        Prepared::Dispatched(dispatch) => dispatch,
    };

    // The request may leave from here on: no retries, and every outcome is recorded.
    let ticket = VerifiedGateTicket(());
    let result = match tool
        .execute(&ticket, &req.task_id, &req.idempotency_key, &req.args)
        .await
    {
        Ok(receipt_id) => match tool.readback(&req.args, &receipt_id).await {
            Ok(()) => DispatchResult::Verified { receipt_id },
            Err(why) => DispatchResult::Unknown {
                receipt_id: Some(receipt_id),
                detail: format!("readback:{why}"),
            },
        },
        Err(ExecError::NotSent(why)) => DispatchResult::NotSent { detail: why },
        Err(ExecError::Unknown(why)) => DispatchResult::Unknown {
            receipt_id: None,
            detail: why,
        },
    };
    // If this write fails the intent stays DISPATCHED, and recovery reconciles it.
    ledger
        .finish_dispatch(&req.task_id, &req.idempotency_key, &result)
        .await?;
    match result {
        DispatchResult::Verified { receipt_id } => Ok(GateOutcome::Executed {
            receipt_id,
            payload_hash: dispatch.payload_hash,
            replayed: false,
        }),
        DispatchResult::NotSent { detail } => Err(GateError::NotSent(detail)),
        DispatchResult::Unknown { detail, .. } => Err(GateError::PostDispatchUnknown {
            idempotency_key: req.idempotency_key.to_string(),
            detail,
        }),
    }
}

enum Prepared {
    Done(GateOutcome),
    Dispatched(Dispatch),
}

/// Everything before the request leaves. Nothing external has happened yet, so after a
/// version conflict it is safe to start over.
async fn prepare<L: GateLedger, T: Tool>(
    ledger: &L,
    tool: &T,
    req: &GateRequest<T::Args>,
) -> Result<Prepared, GateError> {
    for _ in 0..MAX_ATTEMPTS {
        match prepare_once(ledger, tool, req).await {
            Err(GateError::Conflict(_)) => continue,
            other => return other,
        }
    }
    Err(GateError::Conflict(format!(
        "task {} kept changing while the gate decided",
        req.task_id
    )))
}

async fn prepare_once<L: GateLedger, T: Tool>(
    ledger: &L,
    tool: &T,
    req: &GateRequest<T::Args>,
) -> Result<Prepared, GateError> {
    let ctx = ledger
        .load_context(&req.task_id, &req.idempotency_key, req.approval_id.as_ref())
        .await?;
    let verdict = match tool.load_facts(&ctx.task, &req.args).await {
        Ok(facts) => decide(tool, req, &ctx, &facts),
        // A check that cannot run fails closed.
        Err(e) => Verdict::Deny(PreDispatchError::CheckFailed(e.to_string())),
    };
    let now = ctx.now_epoch_s;
    let mut task = ctx.task;
    match verdict {
        Verdict::Stop(reason) => Err(reason.into()),
        Verdict::Deny(reason) => {
            ledger.record_denial(&task.task_id, &reason).await?;
            Err(reason.into())
        }
        Verdict::Limit(limit) => {
            task.transition(
                TaskState::LimitReached {
                    reason: limit.clone(),
                },
                now,
            )?;
            ledger
                .save_task(&task, &format!("limit_reached:{limit}"))
                .await?;
            Err(PreDispatchError::LimitReached(limit).into())
        }
        Verdict::Replay {
            receipt_id,
            payload_hash,
        } => Ok(Prepared::Done(GateOutcome::Executed {
            receipt_id,
            payload_hash,
            replayed: true,
        })),
        Verdict::NeedsApproval(dispatch) => {
            let waits = &ctx.config.waits;
            let deadline = now + waits.approval_ttl_s;
            let wait = WaitMetadata::new(&waits.approver_role, deadline, TimeoutAction::Decline)?;
            task.turns_used += 1;
            task.transition(TaskState::AwaitingApproval(wait), now)?;
            let approval = NewApproval {
                tool_name: tool.name().into(),
                target: req.target.clone(),
                payload_hash: dispatch.payload_hash.clone(),
                args_json: dispatch.canonical_args,
                expires_at_epoch_s: deadline,
            };
            let approval_id = ledger.request_approval(&task, &approval).await?;
            Ok(Prepared::Done(GateOutcome::NeedsApproval {
                approval_id,
                payload_hash: dispatch.payload_hash,
            }))
        }
        Verdict::Proceed(dispatch) => {
            task.turns_used += 1;
            if dispatch.is_retry {
                task.retries_used += 1;
            }
            let intent = ActionIntentRecord {
                idempotency_key: req.idempotency_key.clone(),
                task_id: task.task_id.clone(),
                tool_name: tool.name().into(),
                target: req.target.clone(),
                args_json: dispatch.canonical_args.clone(),
                payload_hash: dispatch.payload_hash.clone(),
                status: ActionIntentStatus::Dispatched,
                backend_receipt_id: None,
                readback_verified: false,
            };
            ledger
                .begin_dispatch(&task, dispatch.approval_id.as_ref(), &intent)
                .await?;
            Ok(Prepared::Dispatched(dispatch))
        }
    }
}

#[cfg(test)]
mod tests;
