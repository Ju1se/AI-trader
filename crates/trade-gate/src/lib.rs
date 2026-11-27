#![forbid(unsafe_code)]
//! Generic 9-step fixed-order `ActionGate` depending ONLY on `trade-core`.
//! - Zero hardcoded tool names: interacts with concrete tools strictly via the `Tool` trait.
//! - Unconfigured tools are rejected (`ToolNotConfigured`); `R4` is permanently prohibited (`R4PermanentlyProhibited`).
//! - Approvals are DB-backed (`approval_id` only; atomic consumption + `DISPATCHED` intent commit).
//! - Any error after `DISPATCHED` transitions the task to `OUTCOME_UNKNOWN` (`GateError::PostDispatchUnknown`).

use std::collections::BTreeSet;
use std::future::Future;

use trade_core::{
    detect_prompt_injection, sha256_hex, ActionIntentRecord, ActionIntentStatus, AutonomyLevel,
    GateError, PreDispatchError, RiskTier, TaskRecord, TaskState, TaskTypeConfig, WaitMetadata,
};

/// Unforgeable compile-time capability proof that steps 1..=8 of `ActionGate` succeeded.
/// Concrete `Tool::execute` implementations require `&VerifiedGateTicket`.
pub struct VerifiedGateTicket {
    _private: (),
}

impl VerifiedGateTicket {
    fn mint() -> Self {
        Self { _private: () }
    }
}

/// Abstract persistence ledger implemented by `trade-store`.
pub trait GateLedger {
    fn save_task(
        &self,
        task: &mut TaskRecord,
        reason: &str,
        actor: &str,
    ) -> impl Future<Output = Result<(), GateError>>;

    fn get_action_intent(
        &self,
        idempotency_key: &str,
    ) -> impl Future<Output = Result<Option<ActionIntentRecord>, GateError>>;

    fn create_pending_approval(
        &self,
        task_id: &str,
        requester_id: &str,
        tool_name: &str,
        target: &str,
        payload_hash: &str,
        canonical_args: &str,
    ) -> impl Future<Output = Result<String, GateError>>;

    /// Atomically verifies `approval_id` (if `Some`: status == APPROVED, hash, expiry, separation of duties),
    /// marks the approval `CONSUMED` in the same DB transaction, and writes `ActionIntentStatus::Dispatched`!
    fn consume_approval_and_mark_dispatched(
        &self,
        approval_id: Option<&str>,
        task: &TaskRecord,
        config: &TaskTypeConfig,
        intent: &ActionIntentRecord,
    ) -> impl Future<Output = Result<(), GateError>>;

    fn update_action_post_dispatch(
        &self,
        intent: &ActionIntentRecord,
        approval_id: Option<&str>,
    ) -> impl Future<Output = Result<(), GateError>>;
}

/// Generic governed tool interface (`Requirement #2`: risk tier, arg validation, execution, readback).
pub trait Tool {
    type Args;
    type Receipt: AsRef<str> + Clone;

    fn name(&self) -> &str;
    fn risk_tier(&self) -> RiskTier;
    fn canonical_args_json(&self, args: &Self::Args) -> Result<String, PreDispatchError>;
    fn validate_args(&self, args: &Self::Args, task: &TaskRecord) -> Result<(), PreDispatchError>;
    fn check_business_rules(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
    ) -> impl Future<Output = Result<(), PreDispatchError>>;
    fn check_pre_dispatch_ready(&self) -> Result<(), PreDispatchError> {
        Ok(())
    }
    fn execute(
        &self,
        ticket: &VerifiedGateTicket,
        task_id: &str,
        idempotency_key: &str,
        args: &Self::Args,
    ) -> impl Future<Output = Result<Self::Receipt, String>>;
    fn readback_verify(
        &self,
        args: &Self::Args,
        receipt: &Self::Receipt,
    ) -> impl Future<Output = Result<bool, String>>;
    fn rehydrate_verified_receipt(
        &self,
        receipt_id: &str,
    ) -> impl Future<Output = Result<Option<Self::Receipt>, String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome<R> {
    Executed {
        receipt: R,
        idempotency_key: String,
        payload_hash: String,
        replayed: bool,
    },
    NeedsApproval {
        approval_id: String,
        payload_hash: String,
        risk_tier: RiskTier,
    },
}

fn compute_generic_payload_hash(tool_name: &str, target: &str, canonical_args: &str) -> String {
    let envelope = format!("{tool_name}|{target}|{canonical_args}");
    sha256_hex(envelope.as_bytes())
}

pub struct ActionGate;

impl ActionGate {
    async fn record_denial<L: GateLedger>(
        ledger: &L,
        task: &mut TaskRecord,
        err: PreDispatchError,
    ) -> GateError {
        task.consecutive_denials = task.consecutive_denials.saturating_add(1);
        let reason = format!("gate_denied:{}", err.code());
        if let Err(save_err) = ledger.save_task(task, &reason, "action_gate").await {
            return save_err;
        }
        GateError::PreDispatch(err)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate_and_execute<L, T>(
        ledger: &L,
        tool: &T,
        task: &mut TaskRecord,
        config: &TaskTypeConfig,
        mounted_tools: &BTreeSet<String>,
        target: &str,
        args: &T::Args,
        idempotency_key: &str,
        approval_id: Option<&str>,
        wall_elapsed_ms: u64,
        is_close_phase: bool,
    ) -> Result<GateOutcome<T::Receipt>, GateError>
    where
        L: GateLedger,
        T: Tool,
    {
        let tool_name = tool.name();

        // 1. Config Risk Lookup & R4 Permanent Prohibition (Diagram 3a) + Mount Check
        let Some(cfg_tier) = config.lookup_tool_risk(tool_name) else {
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::ToolNotConfigured(tool_name.into()),
            )
            .await);
        };
        let risk_tier = cfg_tier.max(tool.risk_tier());
        if risk_tier == RiskTier::R4 {
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::R4PermanentlyProhibited(tool_name.into()),
            )
            .await);
        }
        if !mounted_tools.contains(tool_name) {
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::ToolUnmounted(tool_name.into()),
            )
            .await);
        }

        // 2. OutcomeUnknown Freeze & Budget/Safety Limits
        if let TaskState::OutcomeUnknown { frozen_action_key } = task.state() {
            let k = frozen_action_key.clone();
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::TaskFrozenOutcomeUnknown(k),
            )
            .await);
        }

        let lim = &config.budget_limits;
        let spend_cap = if is_close_phase {
            lim.max_spend
        } else {
            lim.usable_execution_spend()
        };
        if task.turns_used >= lim.max_turns
            || task.spend >= spend_cap
            || wall_elapsed_ms > lim.max_wall_ms
            || task.retries_used > lim.max_retries
            || task.consecutive_denials >= lim.max_consecutive_denials
            || task.reworks_used > lim.max_reworks
        {
            if !matches!(task.state(), TaskState::LimitReached { .. }) {
                task.transition(TaskState::LimitReached {
                    reason: "budget_or_safety_limit_reached".into(),
                })?;
                ledger
                    .save_task(task, "limit_reached", "action_gate")
                    .await?;
            }
            return Err(GateError::PreDispatch(PreDispatchError::LimitReached(
                "budget_or_safety_limit_reached".into(),
            )));
        }

        // 3. Argument Schema Validation
        if let Err(e) = tool.validate_args(args, task) {
            return Err(Self::record_denial(ledger, task, e).await);
        }

        // 4. Hard Deny & Prompt Injection Guard
        if target.trim().is_empty() {
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::InvalidArguments("empty_target".into()),
            )
            .await);
        }
        let canonical_args = match tool.canonical_args_json(args) {
            Ok(s) => s,
            Err(e) => return Err(Self::record_denial(ledger, task, e).await),
        };
        if detect_prompt_injection(target) || detect_prompt_injection(&canonical_args) {
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::PromptInjectionDetected(tool_name.into()),
            )
            .await);
        }

        // 5 & 6. Scope, Authoritative DB Business Rules & Pre-Dispatch Readiness
        if let Err(e) = tool.check_business_rules(args, task).await {
            return Err(Self::record_denial(ledger, task, e).await);
        }
        if let Err(e) = tool.check_pre_dispatch_ready() {
            return Err(Self::record_denial(ledger, task, e).await);
        }

        // 7. Progressive Autonomy & DB-Backed Approval Lifecycle
        let payload_hash = compute_generic_payload_hash(tool_name, target, &canonical_args);

        if config.autonomy_level == AutonomyLevel::Shadow
            && matches!(risk_tier, RiskTier::R2 | RiskTier::R3)
        {
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::AutonomyModeBlocked("shadow_mode_blocks_writes".into()),
            )
            .await);
        }
        if config.autonomy_level == AutonomyLevel::DraftOnly && risk_tier == RiskTier::R3 {
            return Err(Self::record_denial(
                ledger,
                task,
                PreDispatchError::AutonomyModeBlocked("draft_only_blocks_r3_execution".into()),
            )
            .await);
        }

        // Check existing intent for exact verified idempotent replay or unreconciled state
        if let Some(existing) = ledger.get_action_intent(idempotency_key).await? {
            if existing.status == ActionIntentStatus::Verified
                && existing.readback_verified
                && existing.payload_hash == payload_hash
            {
                if let Some(rid) = &existing.backend_receipt_id {
                    if let Ok(Some(rehydrated)) = tool.rehydrate_verified_receipt(rid).await {
                        return Ok(GateOutcome::Executed {
                            receipt: rehydrated,
                            idempotency_key: idempotency_key.to_string(),
                            payload_hash,
                            replayed: true,
                        });
                    }
                }
            }
            if matches!(
                existing.status,
                ActionIntentStatus::Unknown | ActionIntentStatus::Dispatched
            ) {
                return Err(Self::record_denial(
                    ledger,
                    task,
                    PreDispatchError::UnreconciledIntentExists(idempotency_key.into()),
                )
                .await);
            }
        }

        let needs_approval = risk_tier == RiskTier::R3
            || (config.autonomy_level == AutonomyLevel::PerActionApproval
                && risk_tier == RiskTier::R2);

        if needs_approval && approval_id.is_none() {
            let created_id = ledger
                .create_pending_approval(
                    &task.task_id,
                    &task.requester_id,
                    tool_name,
                    target,
                    &payload_hash,
                    &canonical_args,
                )
                .await?;
            if !matches!(task.state(), TaskState::AwaitingApproval(_)) {
                let wm = WaitMetadata::new(
                    "portfolio_approver",
                    "2099-12-31T23:59:59Z",
                    "expire_and_decline",
                )?;
                task.transition(TaskState::AwaitingApproval(wm))?;
                ledger
                    .save_task(
                        task,
                        &format!("pending_approval:{created_id}"),
                        "action_gate",
                    )
                    .await?;
            }
            return Ok(GateOutcome::NeedsApproval {
                approval_id: created_id,
                payload_hash,
                risk_tier,
            });
        }

        // 8. Atomic DB Approval Verification/Consumption + Pre-Dispatch Intent (`DISPATCHED`)
        let mut intent = ActionIntentRecord {
            idempotency_key: idempotency_key.to_string(),
            task_id: task.task_id.clone(),
            tool_name: tool_name.to_string(),
            target: target.to_string(),
            args_json: canonical_args,
            payload_hash: payload_hash.clone(),
            status: ActionIntentStatus::Dispatched,
            backend_receipt_id: None,
            readback_verified: false,
        };

        let effective_approval_id = if needs_approval { approval_id } else { None };
        match ledger
            .consume_approval_and_mark_dispatched(effective_approval_id, task, config, &intent)
            .await
        {
            Ok(()) => {}
            Err(GateError::PreDispatch(PreDispatchError::ApprovalRejected(aid))) => {
                task.consecutive_denials = task.consecutive_denials.saturating_add(1);
                if matches!(
                    task.state(),
                    TaskState::AwaitingApproval(_) | TaskState::Running
                ) {
                    task.transition(TaskState::Declined {
                        reason: format!("approval_rejected:{aid}"),
                    })?;
                }
                ledger
                    .save_task(task, &format!("approval_rejected:{aid}"), "action_gate")
                    .await?;
                return Err(GateError::PreDispatch(PreDispatchError::ApprovalRejected(
                    aid,
                )));
            }
            Err(GateError::PreDispatch(pre_err)) => {
                return Err(Self::record_denial(ledger, task, pre_err).await);
            }
            Err(other) => return Err(other),
        }

        // Transition AWAITING_APPROVAL -> RUNNING now that approval is consumed and action is DISPATCHED
        if matches!(task.state(), TaskState::AwaitingApproval(_)) {
            task.transition(TaskState::Running)?;
            ledger
                .save_task(task, "approval_consumed_resumed_running", "action_gate")
                .await?;
        }

        // 9. Post-Dispatch Execution & Independent Read-Back
        // CRITICAL INVARIANT (`Requirement #3`): ANY error from this point onward MUST mark the
        // intent `UNKNOWN` and transition the task to `OUTCOME_UNKNOWN`!
        let ticket = VerifiedGateTicket::mint();
        let exec_res = tool
            .execute(&ticket, &task.task_id, idempotency_key, args)
            .await;

        let receipt = match exec_res {
            Ok(r) => r,
            Err(err_msg) => {
                return Err(Self::enter_outcome_unknown(
                    ledger,
                    task,
                    &mut intent,
                    effective_approval_id,
                    format!("execute_error:{err_msg}"),
                )
                .await);
            }
        };

        intent.status = ActionIntentStatus::Executed;
        intent.backend_receipt_id = Some(receipt.as_ref().to_string());
        if let Err(e) = ledger
            .update_action_post_dispatch(&intent, effective_approval_id)
            .await
        {
            return Err(Self::enter_outcome_unknown(
                ledger,
                task,
                &mut intent,
                effective_approval_id,
                format!("ledger_executed_update_error:{e}"),
            )
            .await);
        }

        match tool.readback_verify(args, &receipt).await {
            Ok(true) => {
                intent.status = ActionIntentStatus::Verified;
                intent.readback_verified = true;
                if let Err(e) = ledger
                    .update_action_post_dispatch(&intent, effective_approval_id)
                    .await
                {
                    return Err(Self::enter_outcome_unknown(
                        ledger,
                        task,
                        &mut intent,
                        effective_approval_id,
                        format!("ledger_verified_update_error:{e}"),
                    )
                    .await);
                }
                task.consecutive_denials = 0;
                ledger
                    .save_task(task, &format!("action_verified:{tool_name}"), "action_gate")
                    .await?;
                Ok(GateOutcome::Executed {
                    receipt,
                    idempotency_key: idempotency_key.to_string(),
                    payload_hash,
                    replayed: false,
                })
            }
            Ok(false) => Err(
                Self::enter_outcome_unknown(
                    ledger,
                    task,
                    &mut intent,
                    effective_approval_id,
                    "readback_verification_mismatch".into(),
                )
                .await,
            ),
            Err(rb_err) => Err(
                Self::enter_outcome_unknown(
                    ledger,
                    task,
                    &mut intent,
                    effective_approval_id,
                    format!("readback_error:{rb_err}"),
                )
                .await,
            ),
        }
    }

    async fn enter_outcome_unknown<L: GateLedger>(
        ledger: &L,
        task: &mut TaskRecord,
        intent: &mut ActionIntentRecord,
        approval_id: Option<&str>,
        detail: String,
    ) -> GateError {
        intent.status = ActionIntentStatus::Unknown;
        intent.readback_verified = false;
        let _ = ledger.update_action_post_dispatch(intent, approval_id).await;
        if !matches!(task.state(), TaskState::OutcomeUnknown { .. }) {
            let _ = task.transition(TaskState::OutcomeUnknown {
                frozen_action_key: intent.idempotency_key.clone(),
            });
            let _ = ledger
                .save_task(task, &format!("outcome_unknown:{detail}"), "action_gate")
                .await;
        }
        GateError::PostDispatchUnknown {
            idempotency_key: intent.idempotency_key.clone(),
            detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    use trade_core::{ApprovalStatus, BudgetLimits, ShapeLevel};

    fn block_on_ready<F: Future>(fut: F) -> F::Output {
        let mut pinned = pin!(fut);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(val) => val,
            Poll::Pending => panic!("mock future unexpectedly returned Pending"),
        }
    }

    #[derive(Default)]
    struct MockLedger {
        intents: RefCell<BTreeMap<String, ActionIntentRecord>>,
        approvals: RefCell<BTreeMap<String, (String, String, ApprovalStatus, String)>>, // id -> (task_id, hash, status, approver)
    }

    impl GateLedger for MockLedger {
        async fn save_task(
            &self,
            task: &mut TaskRecord,
            _reason: &str,
            _actor: &str,
        ) -> Result<(), GateError> {
            task.advance_persisted_version(task.version() + 1);
            Ok(())
        }

        async fn get_action_intent(
            &self,
            idempotency_key: &str,
        ) -> Result<Option<ActionIntentRecord>, GateError> {
            Ok(self.intents.borrow().get(idempotency_key).cloned())
        }

        async fn create_pending_approval(
            &self,
            task_id: &str,
            _requester_id: &str,
            _tool_name: &str,
            _target: &str,
            payload_hash: &str,
            _canonical_args: &str,
        ) -> Result<String, GateError> {
            let id = "APR-MOCK-1".to_string();
            self.approvals.borrow_mut().insert(
                id.clone(),
                (
                    task_id.to_string(),
                    payload_hash.to_string(),
                    ApprovalStatus::Pending,
                    String::new(),
                ),
            );
            Ok(id)
        }

        async fn consume_approval_and_mark_dispatched(
            &self,
            approval_id: Option<&str>,
            task: &TaskRecord,
            config: &TaskTypeConfig,
            intent: &ActionIntentRecord,
        ) -> Result<(), GateError> {
            if let Some(aid) = approval_id {
                let mut map = self.approvals.borrow_mut();
                let Some((tid, ph, st, approver)) = map.get_mut(aid) else {
                    return Err(PreDispatchError::ApprovalNotFound(aid.into()).into());
                };
                if tid != &task.task_id {
                    return Err(PreDispatchError::ApprovalTaskMismatch.into());
                }
                if *st == ApprovalStatus::Consumed {
                    return Err(PreDispatchError::ApprovalAlreadyUsed(aid.into()).into());
                }
                if *st != ApprovalStatus::Approved {
                    return Err(PreDispatchError::ApprovalNotApproved(st.as_str().into()).into());
                }
                if ph != &intent.payload_hash {
                    return Err(PreDispatchError::ApprovalPayloadMismatch.into());
                }
                if approver == &task.requester_id || approver == &config.config_admin_id {
                    return Err(
                        PreDispatchError::SeparationOfDutiesViolation(approver.clone()).into(),
                    );
                }
                *st = ApprovalStatus::Consumed;
            }
            self.intents
                .borrow_mut()
                .insert(intent.idempotency_key.clone(), intent.clone());
            Ok(())
        }

        async fn update_action_post_dispatch(
            &self,
            intent: &ActionIntentRecord,
            _approval_id: Option<&str>,
        ) -> Result<(), GateError> {
            self.intents
                .borrow_mut()
                .insert(intent.idempotency_key.clone(), intent.clone());
            Ok(())
        }
    }

    struct MockTool {
        tool_name: &'static str,
        tier: RiskTier,
        fail_execute: bool,
        fail_readback: bool,
    }

    impl Tool for MockTool {
        type Args = String;
        type Receipt = String;

        fn name(&self) -> &str {
            self.tool_name
        }
        fn risk_tier(&self) -> RiskTier {
            self.tier
        }
        fn canonical_args_json(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
            Ok(format!("\"{args}\""))
        }
        fn validate_args(
            &self,
            args: &Self::Args,
            _task: &TaskRecord,
        ) -> Result<(), PreDispatchError> {
            if args.is_empty() {
                return Err(PreDispatchError::InvalidArguments("empty".into()));
            }
            Ok(())
        }
        async fn check_business_rules(
            &self,
            _args: &Self::Args,
            _task: &TaskRecord,
        ) -> Result<(), PreDispatchError> {
            Ok(())
        }
        async fn execute(
            &self,
            _ticket: &VerifiedGateTicket,
            _task_id: &str,
            _idempotency_key: &str,
            _args: &Self::Args,
        ) -> Result<Self::Receipt, String> {
            if self.fail_execute {
                return Err("network_drop_after_dispatch".into());
            }
            Ok("RCPT-1".into())
        }
        async fn readback_verify(
            &self,
            _args: &Self::Args,
            _receipt: &Self::Receipt,
        ) -> Result<bool, String> {
            Ok(!self.fail_readback)
        }
        async fn rehydrate_verified_receipt(
            &self,
            receipt_id: &str,
        ) -> Result<Option<Self::Receipt>, String> {
            Ok(Some(receipt_id.to_string()))
        }
    }

    fn test_config() -> TaskTypeConfig {
        let mut allowed = BTreeMap::new();
        allowed.insert("r2_draft".into(), RiskTier::R2);
        allowed.insert("r3_order".into(), RiskTier::R3);
        allowed.insert("r4_banned".into(), RiskTier::R4);
        TaskTypeConfig {
            task_type: "test".into(),
            version: "1.0".into(),
            shape_level: ShapeLevel::MultiAgent,
            shape_rationale: "test".into(),
            autonomy_level: AutonomyLevel::DraftOnly,
            config_admin_id: "admin".into(),
            allowed_tools: allowed,
            budget_limits: BudgetLimits::default(),
            done_criteria: vec!["ok".into()],
        }
    }

    #[test]
    fn unconfigured_and_r4_tools_are_pre_dispatch_denied() {
        let ledger = MockLedger::default();
        let cfg = test_config();
        let mut task = TaskRecord::new_queued(
            "T1",
            "k1",
            "test",
            "1.0",
            "alice",
            "svc",
            "cli",
            vec!["US".into()],
        )
        .unwrap();
        task.transition(TaskState::Running).unwrap();
        let mut mounted = BTreeSet::new();
        mounted.insert("unlisted".into());
        mounted.insert("r4_banned".into());

        let unlisted = MockTool {
            tool_name: "unlisted",
            tier: RiskTier::R2,
            fail_execute: false,
            fail_readback: false,
        };
        let err1 = block_on_ready(ActionGate::evaluate_and_execute(
            &ledger,
            &unlisted,
            &mut task,
            &cfg,
            &mounted,
            "tgt",
            &"ok".to_string(),
            "idem-1",
            None,
            0,
            false,
        ))
        .unwrap_err();
        assert_eq!(
            err1,
            GateError::PreDispatch(PreDispatchError::ToolNotConfigured("unlisted".into()))
        );
        assert_eq!(task.state().code(), "RUNNING");

        let r4_tool = MockTool {
            tool_name: "r4_banned",
            tier: RiskTier::R4,
            fail_execute: false,
            fail_readback: false,
        };
        let err2 = block_on_ready(ActionGate::evaluate_and_execute(
            &ledger,
            &r4_tool,
            &mut task,
            &cfg,
            &mounted,
            "tgt",
            &"ok".to_string(),
            "idem-2",
            None,
            0,
            false,
        ))
        .unwrap_err();
        assert_eq!(
            err2,
            GateError::PreDispatch(PreDispatchError::R4PermanentlyProhibited("r4_banned".into()))
        );
    }

    #[test]
    fn post_dispatch_error_forces_outcome_unknown_and_freezes_task() {
        let ledger = MockLedger::default();
        let cfg = test_config();
        let mut task = TaskRecord::new_queued(
            "T2",
            "k2",
            "test",
            "1.0",
            "alice",
            "svc",
            "cli",
            vec!["US".into()],
        )
        .unwrap();
        task.transition(TaskState::Running).unwrap();
        let mut mounted = BTreeSet::new();
        mounted.insert("r2_draft".into());

        let flaky = MockTool {
            tool_name: "r2_draft",
            tier: RiskTier::R2,
            fail_execute: true,
            fail_readback: false,
        };
        let err = block_on_ready(ActionGate::evaluate_and_execute(
            &ledger,
            &flaky,
            &mut task,
            &cfg,
            &mounted,
            "tgt",
            &"payload".to_string(),
            "idem-flaky",
            None,
            0,
            false,
        ))
        .unwrap_err();

        assert!(matches!(err, GateError::PostDispatchUnknown { .. }));
        assert_eq!(task.state().code(), "OUTCOME_UNKNOWN");

        // Subsequent calls while OUTCOME_UNKNOWN are frozen with PreDispatch(TaskFrozenOutcomeUnknown)
        let frozen_err = block_on_ready(ActionGate::evaluate_and_execute(
            &ledger,
            &flaky,
            &mut task,
            &cfg,
            &mounted,
            "tgt",
            &"payload".to_string(),
            "idem-other",
            None,
            0,
            false,
        ))
        .unwrap_err();
        assert_eq!(
            frozen_err,
            GateError::PreDispatch(PreDispatchError::TaskFrozenOutcomeUnknown(
                "idem-flaky".into()
            ))
        );
    }
}
