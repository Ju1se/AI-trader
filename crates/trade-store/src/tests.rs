//! Each test gets its own database: SQLite in memory by default, or a fresh PostgreSQL
//! database when `TRADE_TEST_PG_URL` points at a server (for example
//! `postgres://postgres:secret@localhost:5432/postgres`).

use std::collections::BTreeMap;

use sqlx::Connection;
use trade_core::{
    ActionIntentStatus, ApprovalStatus, AsOfDate, AutonomyLevel, BudgetLimits, Completeness,
    DeliveryManifest, ExecError, GateError, HandoffPackage, IdempotencyKey, PreDispatchError,
    RiskTier, ShapeLevel, TaskId, TaskRecord, TaskState, TaskTypeConfig, TimeoutAction, UsdCents,
    WaitMetadata,
};
use trade_gate::{
    DispatchResult, GateLedger, GateOutcome, GateRequest, Tool, VerifiedGateTicket, run,
};

use super::*;

async fn store() -> Store {
    let Ok(base) = std::env::var("TRADE_TEST_PG_URL") else {
        return Store::connect(":memory:").await.unwrap();
    };
    sqlx::any::install_default_drivers();
    let name = new_id("t").to_lowercase().replace('-', "_");
    let mut admin = sqlx::AnyConnection::connect(&base).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let (server, _) = base.rsplit_once('/').unwrap();
    Store::connect(&format!("{server}/{name}")).await.unwrap()
}

fn config() -> TaskTypeConfig {
    TaskTypeConfig {
        task_type: "rebalance".into(),
        version: "1".into(),
        shape_level: ShapeLevel::MultiAgent,
        shape_rationale: "test".into(),
        autonomy_level: AutonomyLevel::BoundedAuto,
        config_admin_id: "admin".into(),
        allowed_tools: BTreeMap::from([("order".to_string(), RiskTier::R3)]),
        budget_limits: BudgetLimits::default(),
        done_criteria: vec!["weights_delivered".into()],
        waits: Default::default(),
    }
}

fn queued(id: &str) -> TaskRecord {
    TaskRecord::new_queued(
        TaskId::new(id),
        format!("trigger-{id}"),
        "rebalance",
        "1",
        "alice",
        "svc",
        "cron",
        vec!["US".into()],
    )
    .unwrap()
}

/// A registered task, already RUNNING.
async fn running(store: &Store, id: &str) -> TaskRecord {
    store.put_task_type(&config()).await.unwrap();
    let (mut task, created) = store.create_task(&queued(id), "test").await.unwrap();
    assert!(created);
    task.transition(TaskState::Running, store.now()).unwrap();
    store.save(&mut task, "start", "test").await.unwrap();
    task
}

async fn reload(store: &Store, task: &TaskRecord) -> TaskRecord {
    store.task(&task.task_id).await.unwrap().unwrap()
}

/// An R3 tool whose executions follow a script, then succeed.
struct Order {
    script: std::cell::RefCell<Vec<Result<(), ExecError>>>,
}

impl Order {
    fn new(script: Vec<Result<(), ExecError>>) -> Self {
        Self {
            script: std::cell::RefCell::new(script.into_iter().rev().collect()),
        }
    }
}

impl Tool for Order {
    type Args = String;
    type Facts = ();

    fn name(&self) -> &str {
        "order"
    }
    fn min_tier(&self) -> RiskTier {
        RiskTier::R3
    }
    fn canonical_args(&self, args: &String) -> Result<String, PreDispatchError> {
        Ok(format!("\"{args}\""))
    }
    async fn load_facts(&self, _: &TaskRecord, _: &String) -> Result<(), GateError> {
        Ok(())
    }
    fn check(&self, _: &String, _: &TaskRecord, _: &()) -> Result<(), PreDispatchError> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &VerifiedGateTicket,
        _: &TaskId,
        key: &IdempotencyKey,
        _: &String,
    ) -> Result<String, ExecError> {
        self.script.borrow_mut().pop().unwrap_or(Ok(()))?;
        Ok(format!("RCPT-{key}"))
    }
    async fn readback(&self, _: &String, _: &str) -> Result<(), String> {
        Ok(())
    }
}

fn order_request(task: &TaskRecord, key: &str) -> GateRequest<String> {
    GateRequest {
        task_id: task.task_id.clone(),
        target: "paper-account".into(),
        args: "buy AAPL 10%".into(),
        idempotency_key: IdempotencyKey::new(key),
        approval_id: None,
    }
}

/// Run the order through the gate until it waits for approval, then approve it as `bob`.
async fn approved_order(store: &Store, task: &TaskRecord, tool: &Order) -> GateRequest<String> {
    let mut req = order_request(task, "K1");
    let outcome = run(store, tool, &req).await.unwrap();
    let GateOutcome::NeedsApproval { approval_id, .. } = outcome else {
        panic!("expected an approval request, got {outcome:?}");
    };
    let mut task = reload(store, task).await;
    task.transition(TaskState::Running, store.now()).unwrap();
    store
        .decide_approval(
            &approval_id,
            ApprovalStatus::Approved,
            Some("bob"),
            None,
            &mut task,
        )
        .await
        .unwrap();
    req.approval_id = Some(approval_id);
    req
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    let store = store().await;
    // The task type was never registered.
    assert!(store.create_task(&queued("T1"), "test").await.is_err());
}

#[tokio::test]
async fn intake_is_deduplicated_by_trigger_key() {
    let store = store().await;
    let first = running(&store, "T1").await;
    let mut again = queued("T2");
    again.idempotency_trigger_key = first.idempotency_trigger_key.clone();
    let (stored, created) = store.create_task(&again, "test").await.unwrap();
    assert!(!created);
    assert_eq!(stored.task_id, first.task_id);
}

#[tokio::test]
async fn task_type_versions_never_change() {
    let store = store().await;
    store.put_task_type(&config()).await.unwrap();
    store.put_task_type(&config()).await.unwrap();
    let mut edited = config();
    edited.budget_limits.max_turns = 99;
    assert!(store.put_task_type(&edited).await.is_err());
    edited.version = "2".into();
    store.put_task_type(&edited).await.unwrap();
    assert_eq!(
        store
            .task_type("rebalance", "2")
            .await
            .unwrap()
            .unwrap()
            .budget_limits
            .max_turns,
        99
    );
}

#[tokio::test]
async fn stale_copy_cannot_overwrite_a_newer_write() {
    let store = store().await;
    let task = running(&store, "T1").await;
    let mut first = task.clone();
    let mut stale = task.clone();
    first.turns_used = 5;
    store.save(&mut first, "first", "a").await.unwrap();
    stale.turns_used = 1;
    let err = store.save(&mut stale, "stale", "b").await.unwrap_err();
    assert!(matches!(err, GateError::Conflict(_)), "{err:?}");
    let stored = reload(&store, &task).await;
    assert_eq!((stored.turns_used, stored.version()), (5, first.version()));
}

#[tokio::test]
async fn approval_is_used_once_for_exactly_its_payload() {
    let store = store().await;
    let task = running(&store, "T1").await;
    let tool = Order::new(vec![]);
    let req = approved_order(&store, &task, &tool).await;
    let approval_id = req.approval_id.clone().unwrap();

    let outcome = run(&store, &tool, &req).await.unwrap();
    assert!(matches!(
        outcome,
        GateOutcome::Executed {
            replayed: false,
            ..
        }
    ));
    let approval = store.approval(&approval_id).await.unwrap().unwrap();
    assert_eq!(approval.status, ApprovalStatus::Consumed);
    let actions = store.actions(&task.task_id).await.unwrap();
    assert_eq!(actions[0].status, ActionIntentStatus::Verified);

    // The same request is a replay; a new action with the spent approval is refused.
    let again = run(&store, &tool, &req).await.unwrap();
    assert!(matches!(
        again,
        GateOutcome::Executed { replayed: true, .. }
    ));
    let mut other = req.clone();
    other.idempotency_key = IdempotencyKey::new("K2");
    assert_eq!(
        run(&store, &tool, &other).await.unwrap_err(),
        PreDispatchError::ApprovalAlreadyUsed(approval_id.to_string()).into()
    );

    let kinds: Vec<String> = store
        .events(&task.task_id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "created",
            "update",
            "approval_requested",
            "approval_decided",
            "dispatched",
            "action_finished",
            "denied"
        ]
    );
}

#[tokio::test]
async fn a_rejection_after_the_gate_read_wins() {
    let store = store().await;
    let task = running(&store, "T1").await;
    let tool = Order::new(vec![]);
    let req = approved_order(&store, &task, &tool).await;
    let approval_id = req.approval_id.clone().unwrap();

    // The gate has read its context and decided...
    let ctx = store
        .load_context(&task.task_id, &req.idempotency_key, Some(&approval_id))
        .await
        .unwrap();
    // ...then the approver changes their mind. Here the step could be bypassed, so the
    // task keeps running.
    let mut fresh = reload(&store, &task).await;
    store
        .decide_approval(
            &approval_id,
            ApprovalStatus::Rejected,
            Some("bob"),
            Some("changed my mind"),
            &mut fresh,
        )
        .await
        .unwrap();
    let intent = trade_core::ActionIntentRecord {
        idempotency_key: req.idempotency_key.clone(),
        task_id: task.task_id.clone(),
        tool_name: "order".into(),
        target: req.target.clone(),
        args_json: "\"buy AAPL 10%\"".into(),
        payload_hash: ctx.approval.clone().unwrap().payload_hash,
        status: ActionIntentStatus::Dispatched,
        backend_receipt_id: None,
        readback_verified: false,
    };
    let err = store
        .begin_dispatch(&ctx.task, Some(&approval_id), &intent)
        .await
        .unwrap_err();
    assert!(matches!(err, GateError::Conflict(_)), "{err:?}");
    assert!(store.actions(&task.task_id).await.unwrap().is_empty());
    assert_eq!(
        store.approval(&approval_id).await.unwrap().unwrap().status,
        ApprovalStatus::Rejected
    );
    // Through the gate, the re-read sees the rejection and nothing is sent.
    assert_eq!(
        run(&store, &tool, &req).await.unwrap_err(),
        PreDispatchError::ApprovalRejected(approval_id.to_string()).into()
    );
}

#[tokio::test]
async fn unsent_action_returns_its_approval_and_unknown_one_freezes_the_task() {
    let store = store().await;
    let task = running(&store, "T1").await;
    let tool = Order::new(vec![
        Err(ExecError::NotSent("connection refused".into())),
        Err(ExecError::Unknown("timeout after send".into())),
    ]);
    let req = approved_order(&store, &task, &tool).await;
    let approval_id = req.approval_id.clone().unwrap();

    assert!(matches!(
        run(&store, &tool, &req).await,
        Err(GateError::NotSent(_))
    ));
    assert_eq!(
        store.approval(&approval_id).await.unwrap().unwrap().status,
        ApprovalStatus::Approved
    );

    // The retry uses the returned approval; this time the outcome is unknown.
    assert!(matches!(
        run(&store, &tool, &req).await,
        Err(GateError::PostDispatchUnknown { .. })
    ));
    let mut frozen = reload(&store, &task).await;
    assert_eq!(frozen.state().code(), "OUTCOME_UNKNOWN");
    assert_eq!(frozen.retries_used, 1);
    assert_eq!(store.unsettled_actions().await.unwrap().len(), 1);

    // Reconciliation finds the order went through.
    frozen.transition(TaskState::Running, store.now()).unwrap();
    let verified = DispatchResult::Verified {
        receipt_id: "RCPT-K1".into(),
    };
    store
        .reconcile(&req.idempotency_key, &verified, &mut frozen, "recovery")
        .await
        .unwrap();
    assert!(store.unsettled_actions().await.unwrap().is_empty());
    assert_eq!(reload(&store, &task).await.state(), &TaskState::Running);
}

#[tokio::test]
async fn task_with_an_action_in_flight_cannot_be_cancelled() {
    let store = store().await;
    let task = running(&store, "T1").await;
    let tool = Order::new(vec![Err(ExecError::Unknown("timeout".into()))]);
    let req = approved_order(&store, &task, &tool).await;
    let _ = run(&store, &tool, &req).await;
    let mut frozen = reload(&store, &task).await;
    // Diagram 4a has no edge from OUTCOME_UNKNOWN to CANCELLED, and the store refuses a
    // cancel that sneaks past while the action is unsettled.
    frozen.transition(TaskState::Running, store.now()).unwrap();
    frozen
        .transition(
            TaskState::Cancelled {
                reason: "user".into(),
            },
            store.now(),
        )
        .unwrap();
    assert_eq!(
        store
            .save(&mut frozen, "cancel", "alice")
            .await
            .unwrap_err(),
        PreDispatchError::UnreconciledIntentExists("K1".into()).into()
    );
}

#[tokio::test]
async fn spend_limit_sees_every_cost_and_unit_economics_counts_ended_tasks_once() {
    let store = store().await;
    let cost = |cents, minutes| CostEntry {
        stage: "ACT".into(),
        category: "model".into(),
        amount: UsdCents::from_cents(cents),
        human_minutes: minutes,
        note: String::new(),
    };
    let now = store.now();
    let wait = || WaitMetadata::new("alice", now + 60, TimeoutAction::Complete).unwrap();

    // Completed, reopened, and completed again: one completed task.
    let mut done = running(&store, "T-DONE").await;
    store
        .record_cost(&done.task_id, &cost(300, 30.0))
        .await
        .unwrap();
    done = reload(&store, &done).await;
    assert_eq!(done.spend, UsdCents::from_cents(300));
    for state in [
        TaskState::Verifying,
        TaskState::Settling(wait()),
        TaskState::Completed,
        TaskState::Running,
        TaskState::Verifying,
        TaskState::Settling(wait()),
        TaskState::Completed,
    ] {
        done.transition(state, now).unwrap();
    }
    store.save(&mut done, "settled", "test").await.unwrap();

    let mut failed = running(&store, "T-FAIL").await;
    store
        .record_cost(&failed.task_id, &cost(100, 0.0))
        .await
        .unwrap();
    failed = reload(&store, &failed).await;
    failed
        .transition(TaskState::Failed { reason: "x".into() }, now)
        .unwrap();
    store.save(&mut failed, "failed", "test").await.unwrap();

    // Still running: counts in neither part.
    let open = running(&store, "T-OPEN").await;
    store
        .record_cost(&open.task_id, &cost(10_000, 600.0))
        .await
        .unwrap();

    let economics = store.unit_economics(6_000).await.unwrap();
    assert_eq!((economics.ended_tasks, economics.completed_tasks), (2, 1));
    assert_eq!(economics.api_and_tool_usd, 4.0);
    // $4 of spend plus 30 minutes at $60 an hour.
    assert_eq!(economics.cost_per_completed_task_usd, Some(34.0));
}

#[tokio::test]
async fn expired_waits_are_found_by_deadline() {
    let store = store().await;
    let mut task = running(&store, "T1").await;
    let deadline = store.now() + 3_600;
    let wait = WaitMetadata::new("alice", deadline, TimeoutAction::HandOff).unwrap();
    task.transition(TaskState::AwaitingInput(wait), store.now())
        .unwrap();
    store.save(&mut task, "asked", "test").await.unwrap();

    assert!(store.due_waits(store.now()).await.unwrap().is_empty());
    store.advance_clock(3_601);
    let due = store.due_waits(store.now()).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(
        due[0].state().wait_meta().unwrap().deadline_epoch_s,
        deadline
    );
}

#[tokio::test]
async fn results_deliveries_and_handoffs_round_trip() {
    let store = store().await;
    let mut task = running(&store, "T1").await;
    let as_of = AsOfDate::new("2026-02-10").unwrap();
    let decision = trade_core::CommitteeDecision {
        ticker: "AAPL".into(),
        as_of_date: as_of.clone(),
        quant_base_score: 0.25,
        consensus_score: 0.3,
        dispersion: 0.1,
        hard_veto: false,
        veto_reasons: vec![],
        escalate_review: false,
        worker_outputs: vec![],
    };
    store
        .save_evaluations(&task.task_id, &[decision])
        .await
        .unwrap();
    assert_eq!(
        store.evaluations(&task.task_id).await.unwrap()[0].consensus_score,
        0.3
    );

    let manifest = DeliveryManifest {
        task_id: task.task_id.clone(),
        final_state: TaskState::Verifying,
        completeness: Completeness::Partial,
        criteria_results: BTreeMap::from([("weights_delivered".to_string(), true)]),
        assumptions: vec![],
        gaps: vec!["orders_not_placed".into()],
        total_spend: UsdCents::ZERO,
        next_owner: "alice".into(),
        draft_id: None,
        weights: BTreeMap::new(),
    };
    let now = store.now();
    task.transition(TaskState::Verifying, now).unwrap();
    task.transition(
        TaskState::Settling(WaitMetadata::new("alice", now + 60, TimeoutAction::Complete).unwrap()),
        now,
    )
    .unwrap();
    store.deliver(&manifest, &mut task).await.unwrap();
    let stored = store.delivery(&task.task_id).await.unwrap().unwrap();
    assert_eq!(stored.gaps, manifest.gaps);

    let package = HandoffPackage {
        task_id: task.task_id.clone(),
        requester_id: "alice".into(),
        original_request: "rebalance".into(),
        trigger_reason: "settlement_disputed".into(),
        completed_actions: vec![],
        pending_actions: vec![],
        partial_weights: BTreeMap::new(),
        open_questions: vec!["why".into()],
        assigned_human_owner: Some("carol".into()),
        accepted_by_human: false,
    };
    task.transition(
        TaskState::HandedOff(
            WaitMetadata::new("carol", now + 60, TimeoutAction::Escalate).unwrap(),
        ),
        now,
    )
    .unwrap();
    store.hand_off(&package, &mut task).await.unwrap();
    assert_eq!(
        store
            .handoff(&task.task_id)
            .await
            .unwrap()
            .unwrap()
            .open_questions,
        ["why"]
    );
    assert_eq!(reload(&store, &task).await.state().code(), "HANDED_OFF");
}
