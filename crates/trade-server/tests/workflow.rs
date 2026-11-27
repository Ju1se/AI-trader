//! End-to-end controls: each test drives the real use cases against a fresh database and
//! the paper broker, and checks one behaviour of Diagrams 0, 3a, 3b, 4a, 4b and 6.

mod common;

use trade_adapters::{BrokerFault, PaperBroker};
use trade_core::{
    ActionIntentStatus, ApprovalStatus, Completeness, GateError, IdempotencyKey,
    PortfolioActionPayload, PreDispatchError, WeightBps,
};
use trade_gate::{GateRequest, run};
use trade_server::tools::SubmitOrders;
use trade_server::{DRAFT_TOOL, Framing, ORDER_TOOL, PAPER_BROKER_KEY, Step};

fn state(task: &trade_core::TaskRecord) -> &'static str {
    task.state().code()
}

#[tokio::test]
async fn a_repeated_trigger_returns_the_same_task() {
    let h = common::harness().await;
    let (first, created) = h.admit(&common::intake("cron-1")).await.unwrap();
    let (again, created_again) = h.admit(&common::intake("cron-1")).await.unwrap();
    assert!(created && !created_again);
    assert_eq!(first.task_id, again.task_id);
}

/// FRAME asks instead of guessing, with a real deadline; unanswered, a person takes over,
/// and an unclaimed hand-off passes to the backup owner (E1).
#[tokio::test]
async fn ambiguous_request_waits_for_an_answer_then_goes_to_a_person() {
    let h = common::harness().await;
    let waits = h.task_type.waits.clone();
    let (task, _) = h.admit(&common::intake("ask")).await.unwrap();
    let mut vague = common::request("2026-02-10", true);
    vague.as_of_date = None;
    vague.portfolio_capital = None;
    let framing = h.frame(&task.task_id, &vague).await.unwrap();
    assert_eq!(
        framing,
        Framing::NeedsInput(vec![
            "missing_as_of_date".into(),
            "missing_or_non_positive_capital".into()
        ])
    );
    let waiting = h.task(&task.task_id).await.unwrap();
    let wait = waiting.state().wait_meta().unwrap();
    assert_eq!(state(&waiting), "AWAITING_INPUT");
    assert_eq!(wait.owner_id, "pm_alice");
    assert!(wait.deadline_epoch_s >= h.store.now() + waits.input_ttl_s - 5);

    // The answer arrives in time on one task...
    let answered = h
        .frame(&task.task_id, &common::request("2026-02-10", true))
        .await
        .unwrap();
    assert!(matches!(answered, Framing::Ready(_)));

    // ...and never arrives on another.
    let (silent, _) = h.admit(&common::intake("silent")).await.unwrap();
    h.frame(&silent.task_id, &vague).await.unwrap();
    h.store.advance_clock(waits.input_ttl_s + 1);
    assert_eq!(h.sweep().await.unwrap().len(), 1);
    let handed = h.task(&silent.task_id).await.unwrap();
    assert_eq!(
        handed.state().wait_meta().unwrap().owner_id,
        waits.escalation_owner_id
    );
    assert!(h.store.handoff(&silent.task_id).await.unwrap().is_some());

    h.store.advance_clock(waits.handoff_ack_ttl_s + 1);
    h.sweep().await.unwrap();
    let escalated = h.task(&silent.task_id).await.unwrap();
    assert_eq!(state(&escalated), "HANDED_OFF");
    assert_eq!(
        escalated.state().wait_meta().unwrap().owner_id,
        waits.backup_owner_id
    );
}

/// The whole of Diagram 0 once, then a reopen: the task counts once as completed (E5).
#[tokio::test]
async fn full_run_completes_and_a_reopened_task_counts_once() {
    let h = common::harness().await;
    let (task, step) = common::delivered(&h).await;
    let Step::Delivered(manifest) = step else {
        unreachable!()
    };
    assert_eq!(manifest.completeness, Completeness::Complete);
    assert!(
        manifest.criteria_results.values().all(|ok| *ok),
        "{manifest:?}"
    );
    assert_eq!(h.broker.count_orders_for_task(&task), 1);

    h.confirm_settlement(&task, "pm_alice", 3.0).await.unwrap();
    let first = h.store.unit_economics(12_000).await.unwrap();
    assert_eq!((first.ended_tasks, first.completed_tasks), (1, 1));

    let reopened = h
        .reopen(&task, "late_corporate_action", "pm_alice")
        .await
        .unwrap();
    assert_eq!((reopened.reopened_count, reopened.reworks_used), (1, 1));
    // Draft and orders are already done and verified; the task goes straight to CLOSE.
    let again = h.advance(&task).await.unwrap();
    assert!(matches!(again, Step::Delivered(_)), "{again:?}");
    assert_eq!(h.broker.count_orders_for_task(&task), 1);
    h.confirm_settlement(&task, "pm_alice", 1.0).await.unwrap();
    let after = h.store.unit_economics(12_000).await.unwrap();
    assert_eq!(after.completed_tasks, 1);
    assert!(after.all_in_cost_usd > first.all_in_cost_usd);
    assert_eq!(
        h.store
            .memories(Some("pending_review"))
            .await
            .unwrap()
            .len(),
        2
    );
}

/// C2: a refused irreversible step that the contract lets a person do instead is skipped,
/// and the rest is delivered as partial.
#[tokio::test]
async fn refused_orders_are_skipped_and_the_draft_delivered_as_partial() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    let step = common::approve(&h, step).await;
    let step = common::decide(&h, step, false).await;
    let Step::Delivered(manifest) = step else {
        panic!("{step:?}")
    };
    assert_eq!(manifest.completeness, Completeness::Partial);
    assert!(
        manifest
            .gaps
            .contains(&"orders_not_placed:approval_refused".to_string())
    );
    assert_eq!(h.broker.count_orders_for_task(&task), 0);
    // REPLAN: version 2 drops the refused step; version 1 had it.
    let plan = h.store.plan(&task).await.unwrap().unwrap();
    assert_eq!(plan.version, 2);
    assert!(plan.step("orders").unwrap().skipped);
}

/// C2: refusing a step that cannot be skipped ends the task as DECLINED.
#[tokio::test]
async fn refusing_a_required_step_declines_the_task() {
    let h = common::harness().await;
    let draft_refused = common::start(&h, "2026-02-10").await;
    let step = h.advance(&draft_refused).await.unwrap();
    assert_eq!(
        common::decide(&h, step, false).await,
        Step::Waiting("DECLINED")
    );

    let orders_required = common::start_with(&h, "2026-05-01", false).await;
    let step = h.advance(&orders_required).await.unwrap();
    let step = common::approve(&h, step).await;
    assert_eq!(
        common::decide(&h, step, false).await,
        Step::Waiting("DECLINED")
    );
}

/// E1: an approval nobody gives in time counts as refused, never as given.
#[tokio::test]
async fn an_approval_that_times_out_counts_as_refused() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let Step::NeedsApproval { approval_id, .. } = h.advance(&task).await.unwrap() else {
        panic!("expected an approval request");
    };
    h.store.advance_clock(h.task_type.waits.approval_ttl_s + 1);
    assert_eq!(h.sweep().await.unwrap().len(), 1);
    let approval = h.store.approval(&approval_id).await.unwrap().unwrap();
    assert_eq!(approval.status, ApprovalStatus::Expired);
    assert_eq!(state(&h.task(&task).await.unwrap()), "DECLINED");
}

/// E1: a delivery nobody disputes completes when the settlement window closes.
#[tokio::test]
async fn an_undisputed_delivery_completes_when_the_window_closes() {
    let h = common::harness().await;
    let (task, _) = common::delivered(&h).await;
    h.store.advance_clock(h.task_type.waits.settle_window_s + 1);
    assert_eq!(h.sweep().await.unwrap().len(), 1);
    assert_eq!(state(&h.task(&task).await.unwrap()), "COMPLETED");
}

/// Separation of duties: neither the requester nor the configuration admin may approve.
#[tokio::test]
async fn requester_and_admin_cannot_approve() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let Step::NeedsApproval { approval_id, .. } = h.advance(&task).await.unwrap() else {
        panic!("expected an approval request");
    };
    for who in ["pm_alice", "platform_admin"] {
        let err = h
            .decide_approval(&approval_id, who, true, None)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            PreDispatchError::SeparationOfDutiesViolation(who.into()).into()
        );
    }
}

/// The gate checks the proposal against the stored veto list, not against the proposer.
#[tokio::test]
async fn a_proposal_holding_a_vetoed_security_is_refused() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let Step::NeedsApproval { approval_id, .. } = h.advance(&task).await.unwrap() else {
        panic!("expected an approval request");
    };
    h.decide_approval(&approval_id, "approver_bob", true, None)
        .await
        .unwrap();
    let (mut payload, _): (PortfolioActionPayload, _) = h.proposal(&task).await.unwrap();
    payload
        .weights_bps
        .insert("TRAP_CO".into(), WeightBps::new(500).unwrap());
    let request = GateRequest {
        task_id: task.clone(),
        target: "portfolio:2026-02-10".into(),
        args: payload,
        idempotency_key: IdempotencyKey::new("tampered"),
        approval_id: None,
    };
    let err = run(&h.store, &h.draft_tool(), &request).await.unwrap_err();
    assert_eq!(
        err,
        PreDispatchError::BusinessRuleViolation("vetoed_ticker_in_portfolio:TRAP_CO".into()).into()
    );
}

/// Diagram 4b: the request was lost before the broker saw it. The task freezes, the broker
/// is asked, nothing is found, and the order is sent again once, reusing its approval.
#[tokio::test]
async fn a_request_lost_before_the_broker_is_reconciled_and_sent_once() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    let step = common::approve(&h, step).await;
    h.broker.inject_next_fault(BrokerFault::DropBeforeFill);
    let step = common::approve(&h, step).await;
    assert!(matches!(step, Step::Delivered(_)), "{step:?}");
    assert_eq!(h.broker.count_orders_for_task(&task), 1);
    let done = h.task(&task).await.unwrap();
    assert_eq!(done.retries_used, 1);
    let kinds: Vec<String> = h
        .store
        .events(&task)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"reconciled".to_string()), "{kinds:?}");
}

/// Diagram 4b: the broker filled the order but the reply was lost. Reconciliation finds
/// the fill and no second order is placed.
#[tokio::test]
async fn a_lost_reply_after_a_fill_is_reconciled_without_a_second_order() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    let step = common::approve(&h, step).await;
    h.broker.inject_next_fault(BrokerFault::TimeoutAfterFill);
    let step = common::approve(&h, step).await;
    let Step::Delivered(manifest) = step else {
        panic!("{step:?}")
    };
    assert_eq!(manifest.completeness, Completeness::Complete);
    assert_eq!(h.broker.count_orders_for_task(&task), 1);
    assert_eq!(h.task(&task).await.unwrap().retries_used, 0);
}

/// Diagram 4b at start-up: an action left in flight by a crash is settled by the next
/// process from what the broker shows. The file database also survives the restart.
#[tokio::test]
async fn a_restart_settles_what_a_crash_left_in_flight() {
    let path = std::env::temp_dir().join(format!("trade-restart-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = path.to_string_lossy().to_string();
    let broker = PaperBroker::new(PAPER_BROKER_KEY);
    let key = Some(PAPER_BROKER_KEY);

    let before = common::harness_on(&db, broker.clone(), key, |_| {}).await;
    let task = common::start(&before, "2026-02-10").await;
    let step = before.advance(&task).await.unwrap();
    let Step::NeedsApproval { approval_id, .. } = common::approve(&before, step).await else {
        panic!("expected the order approval");
    };
    before
        .decide_approval(&approval_id, "approver_bob", true, None)
        .await
        .unwrap();
    // The process sends the order and dies before it hears back.
    broker.inject_next_fault(BrokerFault::TimeoutAfterFill);
    let contract = before.store.contract(&task).await.unwrap().unwrap();
    let draft = before.store.latest_draft(&task).await.unwrap().unwrap();
    let request = GateRequest {
        approval_id: Some(approval_id),
        ..SubmitOrders::request(&contract, draft)
    };
    let err = run(&before.store, &before.order_tool(), &request)
        .await
        .unwrap_err();
    assert!(
        matches!(err, GateError::PostDispatchUnknown { .. }),
        "{err:?}"
    );
    drop(before);

    let after = common::harness_on(&db, broker.clone(), key, |_| {}).await;
    assert_eq!(after.recover().await.unwrap(), vec![task.clone()]);
    let resumed = after.task(&task).await.unwrap();
    assert_eq!(state(&resumed), "RUNNING");
    let actions = after.store.actions(&task).await.unwrap();
    assert!(
        actions
            .iter()
            .all(|a| a.status == ActionIntentStatus::Verified)
    );
    assert!(matches!(
        after.advance(&task).await.unwrap(),
        Step::Delivered(_)
    ));
    assert_eq!(broker.count_orders_for_task(&task), 1);
    let _ = std::fs::remove_file(&path);
}

/// The verifier reads stored facts only: a citation of a record that did not exist by the
/// as-of date fails the check, every rework fails it again, and then a person takes over.
#[tokio::test]
async fn a_tampered_citation_fails_verification_and_goes_to_a_person() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    let mut decisions = h.store.evaluations(&task).await.unwrap();
    let examined = decisions
        .iter_mut()
        .find(|d| !d.worker_outputs.is_empty())
        .unwrap();
    examined.worker_outputs[0]
        .cited_record_ids
        .push("FIL-FROM-THE-FUTURE".into());
    h.store.save_evaluations(&task, &decisions).await.unwrap();

    let step = common::approve(&h, step).await;
    let step = common::approve(&h, step).await;
    assert_eq!(
        step,
        Step::HandedOff(h.task_type.waits.escalation_owner_id.clone())
    );
    let handed = h.task(&task).await.unwrap();
    assert_eq!(handed.reworks_used, h.task_type.budget_limits.max_reworks);
    let package = h.store.handoff(&task).await.unwrap().unwrap();
    assert!(
        package
            .trigger_reason
            .contains("invalid_or_future_citation"),
        "{package:?}"
    );
}

/// An empty conclusion cannot pass: no specialist evidence fails the citation check.
#[tokio::test]
async fn verification_fails_without_any_evidence() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    h.advance(&task).await.unwrap();
    let mut decisions = h.store.evaluations(&task).await.unwrap();
    for d in &mut decisions {
        d.worker_outputs.clear();
    }
    h.store.save_evaluations(&task, &decisions).await.unwrap();
    let record = h.task(&task).await.unwrap();
    let contract = h.store.contract(&task).await.unwrap().unwrap();
    let check = h.verify(&record, &contract, &h.task_type).await.unwrap();
    assert!(!check.passed());
    assert!(check.gaps.contains(&"no_specialist_evidence".to_string()));
}

/// A used-up limit stops the work; what was finished is verified and delivered as partial.
#[tokio::test]
async fn a_used_up_limit_waits_for_the_owner_then_delivers_what_was_finished() {
    // Research takes 21 turns, asking for the draft's approval one, writing it one more.
    let h = common::harness_with(|t| t.budget_limits.max_turns = 23).await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    assert_eq!(
        common::approve(&h, step).await,
        Step::Waiting("LIMIT_REACHED")
    );
    let waiting = h.task(&task).await.unwrap();
    let wait = waiting.state().wait_meta().unwrap();
    assert_eq!(wait.owner_id, h.task_type.waits.escalation_owner_id);
    let step = h.deliver_what_is_done(&task).await.unwrap();
    let Step::Delivered(manifest) = step else {
        panic!("{step:?}")
    };
    assert_eq!(manifest.completeness, Completeness::Partial);
    assert!(
        manifest
            .gaps
            .contains(&"stopped_by_limit:max_turns".to_string()),
        "{manifest:?}"
    );
    assert!(manifest.draft_id.is_some());
    assert_eq!(h.broker.count_orders_for_task(&task), 0);
    let actions = h.store.actions(&task).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].tool_name, DRAFT_TOOL);
}

/// Diagram 4a: the owner grants more turns and the task runs to the end, orders included.
#[tokio::test]
async fn the_owner_grants_more_budget_and_the_task_finishes() {
    let h = common::harness_with(|t| t.budget_limits.max_turns = 23).await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    assert_eq!(
        common::approve(&h, step).await,
        Step::Waiting("LIMIT_REACHED")
    );
    let owner = h.task_type.waits.escalation_owner_id.clone();
    let more = trade_core::UsdCents::from_cents(0);
    let requester = h.task(&task).await.unwrap().requester_id;
    let own = h.grant_budget(&task, &requester, 10, more, "mine").await;
    assert!(
        own.unwrap_err()
            .to_string()
            .contains("separation_of_duties"),
        "the requester cannot raise their own budget"
    );
    h.grant_budget(&task, &owner, 10, more, "finish the orders")
        .await
        .unwrap();
    let step = h.advance(&task).await.unwrap();
    let Step::Delivered(manifest) = common::approve(&h, step).await else {
        panic!("expected a delivery")
    };
    assert_eq!(manifest.completeness, Completeness::Complete);
    assert_eq!(h.task(&task).await.unwrap().granted_turns, 10);
    assert_eq!(h.broker.count_orders_for_task(&task), 1);
}

/// Reworks and retries cannot be bought: a task stopped by them can only deliver.
#[tokio::test]
async fn a_grant_cannot_buy_more_reworks() {
    let h = common::harness().await;
    let task = common::start(&h, "2026-02-10").await;
    let mut record = h.task(&task).await.unwrap();
    let now = h.store.now();
    let stopped = h.task_type.limit_reached("max_reworks", now).unwrap();
    record.transition(stopped, now).unwrap();
    h.store
        .save(&mut record, "limit_reached:max_reworks", "test")
        .await
        .unwrap();
    let owner = h.task_type.waits.escalation_owner_id.clone();
    let more = trade_core::UsdCents::from_cents(0);
    let refused = h.grant_budget(&task, &owner, 10, more, "more").await;
    assert!(
        matches!(refused, Err(trade_core::GateError::StateTransition(_))),
        "{refused:?}"
    );
    assert_eq!(state(&h.task(&task).await.unwrap()), "LIMIT_REACHED");
}

/// The broker credential is checked before anything is sent.
#[tokio::test]
async fn a_missing_credential_is_refused_before_dispatch() {
    let broker = PaperBroker::new(PAPER_BROKER_KEY);
    let h = common::harness_on(":memory:", broker, None, |_| {}).await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    let step = common::approve(&h, step).await;
    assert_eq!(
        step,
        Step::Denied("missing_credential:paper_broker_api_key".into())
    );
    let actions = h.store.actions(&task).await.unwrap();
    assert!(actions.iter().all(|a| a.tool_name != ORDER_TOOL));
    assert_eq!(state(&h.task(&task).await.unwrap()), "RUNNING");
}
