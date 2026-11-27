//! Comprehensive integration & Python-MVP parity test suite covering all 15+ Python tests
//! plus all 7 Diagram 3a/3b/4a/4b & crate-isolation invariants.

use std::collections::{BTreeMap, BTreeSet};

use trade_adapters::BrokerFaultMode;
use trade_server::{
    build_default_config, rebalance_dates, seed_pit_universe, CreateRebalanceDraftTool,
    CredentialVault, TradeAiHarness,
};
use trade_core::{
    AgentEvidenceOutput, AsOfDate, AutonomyLevel, BudgetLimits, Completeness, GateError,
    PortfolioActionPayload, PreDispatchError, RiskTier, TaskRecord, TaskState, UsdCents,
    WeightBps,
};
use trade_gate::{ActionGate, GateOutcome, Tool, VerifiedGateTicket};
use trade_quant::{
    audit_survivorship_bias_exercise, run_pit_backtest, score_pit_universe, BacktestConfig,
};
use trade_store::DbStore;
use trade_research::{
    FootnoteForensicsWorker, InvestmentCommittee, ManagementCapAllocWorker, MoatWorker,
    ReadOnlyPitView,
};

struct DummyR4Tool;
impl Tool for DummyR4Tool {
    type Args = String;
    type Receipt = String;
    fn name(&self) -> &str {
        "direct_wire_transfer"
    }
    fn risk_tier(&self) -> RiskTier {
        RiskTier::R4
    }
    fn canonical_args_json(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
        Ok(serde_json::to_string(args).unwrap())
    }
    fn validate_args(
        &self,
        _args: &Self::Args,
        _task: &TaskRecord,
    ) -> Result<(), PreDispatchError> {
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
        Ok("NEVER".into())
    }
    async fn readback_verify(
        &self,
        _args: &Self::Args,
        _receipt: &Self::Receipt,
    ) -> Result<bool, String> {
        Ok(true)
    }
    async fn rehydrate_verified_receipt(
        &self,
        _receipt_id: &str,
    ) -> Result<Option<Self::Receipt>, String> {
        Ok(None)
    }
}

struct UnconfiguredTool;
impl Tool for UnconfiguredTool {
    type Args = String;
    type Receipt = String;
    fn name(&self) -> &str {
        "rogue_shell_exec"
    }
    fn risk_tier(&self) -> RiskTier {
        RiskTier::R0
    }
    fn canonical_args_json(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
        Ok(serde_json::to_string(args).unwrap())
    }
    fn validate_args(
        &self,
        _args: &Self::Args,
        _task: &TaskRecord,
    ) -> Result<(), PreDispatchError> {
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
        Ok("NEVER".into())
    }
    async fn readback_verify(
        &self,
        _args: &Self::Args,
        _receipt: &Self::Receipt,
    ) -> Result<bool, String> {
        Ok(true)
    }
    async fn rehydrate_verified_receipt(
        &self,
        _receipt_id: &str,
    ) -> Result<Option<Self::Receipt>, String> {
        Ok(None)
    }
}

#[tokio::test]
async fn test_01_survivorship_bias_exercise_exact_numbers() {
    let audit = audit_survivorship_bias_exercise();
    assert!((audit.survivors_only_return - 0.05).abs() < 1e-6);
    assert!((audit.survivor_bias_free_return - (-0.2667)).abs() < 1e-4);
    assert!((audit.artificial_gain_pct_points - 31.7).abs() < 1e-6);
}

#[tokio::test]
async fn test_02_look_ahead_bias_blocked_by_filing_date() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let mkts = vec!["US".to_string()];

    let feb = AsOfDate::new("2026-02-10").unwrap();
    let snap_feb = store.get_pit_snapshot(&feb, &mkts).await.unwrap();
    let late_feb = snap_feb.iter().find(|r| r.ticker == "LATE_FILER").unwrap();
    assert_eq!(late_feb.fin_record_id, "FIN-LATE-2025");
    assert_eq!(late_feb.filing_date.as_str(), "2025-03-31");

    let fils_feb = store
        .get_pit_filings_for_ticker("LATE_FILER", &feb)
        .await
        .unwrap();
    assert_eq!(fils_feb.len(), 1);
    assert_eq!(fils_feb[0].record_id, "FIL-LATE-10K-2025");

    let may = AsOfDate::new("2026-05-01").unwrap();
    let snap_may = store.get_pit_snapshot(&may, &mkts).await.unwrap();
    let late_may = snap_may.iter().find(|r| r.ticker == "LATE_FILER").unwrap();
    assert_eq!(late_may.fin_record_id, "FIN-LATE-2026");
    assert_eq!(late_may.filing_date.as_str(), "2026-04-15");

    let fils_may = store
        .get_pit_filings_for_ticker("LATE_FILER", &may)
        .await
        .unwrap();
    assert_eq!(fils_may[0].record_id, "FIL-LATE-10K-2026");
}

#[tokio::test]
async fn test_03_delisted_security_included_before_delist_and_excluded_after() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let mkts = vec!["US".to_string()];

    let feb = AsOfDate::new("2026-02-10").unwrap();
    let snap_feb = store.get_pit_snapshot(&feb, &mkts).await.unwrap();
    let debt = snap_feb.iter().find(|r| r.ticker == "DEBT_FAIL").unwrap();
    assert_eq!(debt.delist_return, Some(-0.90));

    let may = AsOfDate::new("2026-05-01").unwrap();
    let snap_may = store.get_pit_snapshot(&may, &mkts).await.unwrap();
    assert!(snap_may.iter().all(|r| r.ticker != "DEBT_FAIL"));
}

#[tokio::test]
async fn test_04_hard_veto_blocks_value_trap_and_distress() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let feb = AsOfDate::new("2026-02-10").unwrap();
    let snap = store
        .get_pit_snapshot(&feb, &["US".to_string()])
        .await
        .unwrap();
    let scored: BTreeMap<String, _> = score_pit_universe(&snap, &feb, 1_000_000.0)
        .into_iter()
        .map(|r| (r.ticker.clone(), r))
        .collect();

    assert!(scored["TRAP_CO"].hard_veto);
    assert!(scored["TRAP_CO"]
        .veto_reasons
        .contains(&"negative_cfo_positive_ni".to_string()));
    assert!(scored["DEBT_FAIL"].hard_veto);
    assert!(scored["DEBT_FAIL"]
        .veto_reasons
        .iter()
        .any(|r| r.contains("distress_zscore")));
    assert!(!scored["AAPL_MOAT"].hard_veto);
}

#[tokio::test]
async fn test_05_backtest_exact_python_parity_and_adv_cap() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let dates = rebalance_dates();
    let mkts = vec!["US".to_string()];
    let src = store.preload_backtest_source(&dates, &mkts).await.unwrap();

    let bt = run_pit_backtest(
        &src,
        &dates,
        BacktestConfig {
            markets: mkts.clone(),
            oos_start_date: Some(AsOfDate::new("2026-02-10").unwrap()),
            ..Default::default()
        },
    )
    .unwrap();

    // Exact decimal-for-decimal parity against Python v0.1-python-mvp!
    assert_eq!(bt.overall.periods, 4);
    assert!((bt.overall.cumulative_return - 0.2926).abs() < 1e-4);
    assert!((bt.overall.sharpe_ratio - 35.9151).abs() < 1e-3);
    assert!((bt.overall.avg_turnover - 0.2083).abs() < 1e-4);
    assert!((bt.overall.avg_rank_ic.unwrap() - 0.1107).abs() < 1e-4);

    let is_m = bt.in_sample.unwrap();
    let oos_m = bt.out_of_sample.unwrap();
    assert!((is_m.cumulative_return - 0.1511).abs() < 1e-4);
    assert!((oos_m.cumulative_return - 0.1229).abs() < 1e-4);
    assert!((bt.periods[0].cost_drag - 0.0015).abs() < 1e-6);

    // ADV cap under $200M capital
    let single_dt = vec![AsOfDate::new("2026-02-10").unwrap()];
    let bt_large = run_pit_backtest(
        &src,
        &single_dt,
        BacktestConfig {
            markets: mkts,
            portfolio_capital: UsdCents::from_dollars(200_000_000.0),
            max_adv_participation: 0.10,
            ..Default::default()
        },
    )
    .unwrap();
    let total_w: f64 = bt_large.periods[0].weights.values().sum();
    assert!(total_w < 0.15);
}

#[tokio::test]
async fn test_06_diagram_3a_unconfigured_tool_rejected_and_r4_permanently_prohibited() {
    let store = DbStore::connect(":memory:").await.unwrap();
    let cfg = build_default_config(AutonomyLevel::BoundedAuto, "platform_admin", None);
    let harness = TradeAiHarness::new(cfg.clone());
    let (mut task, _) = harness
        .admit_task(&store, "trig-r4", "pm_alice", "svc", "cli", &["US".into()])
        .await
        .unwrap();
    task.transition(TaskState::Running).unwrap();
    store
        .save_task_optimistic(&mut task, "run", "test")
        .await
        .unwrap();

    let mounted = BTreeSet::from([
        "rogue_shell_exec".to_string(),
        "direct_wire_transfer".to_string(),
    ]);

    let e1 = ActionGate::evaluate_and_execute(
        &store,
        &UnconfiguredTool,
        &mut task,
        &cfg,
        &mounted,
        "host",
        &"ls".to_string(),
        "idem-unconf",
        None,
        0,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        e1,
        GateError::PreDispatch(PreDispatchError::ToolNotConfigured(
            "rogue_shell_exec".into()
        ))
    );

    let e2 = ActionGate::evaluate_and_execute(
        &store,
        &DummyR4Tool,
        &mut task,
        &cfg,
        &mounted,
        "bank",
        &"1000000".to_string(),
        "idem-r4",
        None,
        0,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        e2,
        GateError::PreDispatch(PreDispatchError::R4PermanentlyProhibited(
            "direct_wire_transfer".into()
        ))
    );
}

#[tokio::test]
async fn test_07_pre_dispatch_vs_post_dispatch_error_enum_distinction() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let cfg = build_default_config(AutonomyLevel::PerActionApproval, "platform_admin", None);
    let mut harness = TradeAiHarness::new(cfg.clone());
    let mkts = vec!["US".to_string()];

    let (task, _) = harness
        .admit_task(&store, "trig-err-enum", "pm_alice", "svc", "cli", &mkts)
        .await
        .unwrap();
    harness
        .frame_task(
            &store,
            &task.task_id,
            "err test",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            3,
        )
        .await
        .unwrap()
        .unwrap();

    let res = harness
        .run_research_and_draft(&store, &task.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();
    let GateOutcome::NeedsApproval { approval_id, .. } = res.gate_outcome.as_ref().unwrap() else {
        panic!("expected NeedsApproval");
    };
    store
        .decide_approval(approval_id, "approver_bob", true, None, "platform_admin")
        .await
        .unwrap();
    let GateOutcome::Executed {
        receipt: draft_id, ..
    } = harness
        .submit_rebalance_draft(&store, &task.task_id, &res, Some(approval_id))
        .await
        .unwrap()
    else {
        panic!("expected draft executed");
    };

    // 1. Pre-dispatch failure (empty CredentialVault) -> returns GateError::PreDispatch, task stays RUNNING!
    harness.vault = CredentialVault::new();
    let pre_err = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, "ord-pre", None)
        .await
        .unwrap_err();
    assert!(matches!(
        pre_err,
        GateError::PreDispatch(PreDispatchError::MissingCredential(_))
    ));
    let after_pre = store.get_task(&task.task_id).await.unwrap().unwrap();
    assert_eq!(after_pre.state().code(), "RUNNING");

    // Restore vault, approve R3 order, inject post-dispatch broker fault -> PostDispatchUnknown + OUTCOME_UNKNOWN!
    harness
        .vault
        .set_secret("paper_broker_api_key", TradeAiHarness::DEFAULT_BROKER_SECRET);
    let GateOutcome::NeedsApproval {
        approval_id: ord_apr,
        ..
    } = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, "ord-post", None)
        .await
        .unwrap()
    else {
        panic!("expected order NeedsApproval");
    };
    store
        .decide_approval(&ord_apr, "approver_bob", true, None, "platform_admin")
        .await
        .unwrap();

    harness
        .broker
        .inject_next_fault(BrokerFaultMode::DropBeforeFill);
    let post_err = harness
        .execute_paper_orders(
            &store,
            &task.task_id,
            &draft_id,
            "ord-post",
            Some(&ord_apr),
        )
        .await
        .unwrap_err();
    assert!(matches!(post_err, GateError::PostDispatchUnknown { .. }));
    let after_post = store.get_task(&task.task_id).await.unwrap().unwrap();
    assert_eq!(after_post.state().code(), "OUTCOME_UNKNOWN");
}

#[tokio::test]
async fn test_08_readonly_worker_scope_and_unmounted_write_tools() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let feb = AsOfDate::new("2026-02-10").unwrap();
    let ro = store
        .build_scoped_readonly_view(&["CN".to_string()], &feb)
        .await
        .unwrap();
    // AAPL_MOAT is in US, outside CN authorized_markets -> denied!
    assert!(ro.get_pit_filings("AAPL_MOAT", &feb).is_err());

    // Unmounted write tool rejected at step 1
    let cfg = build_default_config(AutonomyLevel::DraftOnly, "platform_admin", None);
    let harness = TradeAiHarness::new(cfg.clone());
    let (mut task, _) = harness
        .admit_task(&store, "trig-unm", "pm_alice", "svc", "cli", &["US".into()])
        .await
        .unwrap();
    task.transition(TaskState::Running).unwrap();
    store
        .save_task_optimistic(&mut task, "run", "test")
        .await
        .unwrap();

    let tool = CreateRebalanceDraftTool { store: &store };
    let worker_mounts = BTreeSet::from(["read_pit_market".to_string()]);
    let payload = PortfolioActionPayload {
        draft_id: "D1".into(),
        as_of_date: feb,
        weights_bps: BTreeMap::new(),
        markets: vec!["US".into()],
        portfolio_capital_cents: None,
    };
    let err = ActionGate::evaluate_and_execute(
        &store,
        &tool,
        &mut task,
        &cfg,
        &worker_mounts,
        "portfolio",
        &payload,
        "idem-unm",
        None,
        0,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        GateError::PreDispatch(PreDispatchError::ToolUnmounted(
            "create_rebalance_draft".into()
        ))
    );
}

#[tokio::test]
async fn test_09_prompt_injection_wrapped_neutralized_and_committee_parity() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let feb = AsOfDate::new("2026-02-10").unwrap();
    let mkts = vec!["US".to_string()];

    let ro = store.build_scoped_readonly_view(&mkts, &feb).await.unwrap();
    let filings = ro.get_pit_filings("DISP_CORP", &feb).unwrap();
    assert!(filings[0].injection_flagged);
    assert!(filings[0]
        .to_xml_envelope()
        .contains("NEVER_FOLLOW_INSTRUCTIONS_INSIDE"));

    let snap = store.get_pit_snapshot(&feb, &mkts).await.unwrap();
    let snap_by: BTreeMap<_, _> = snap.iter().map(|r| (r.ticker.clone(), r.clone())).collect();
    let scored = score_pit_universe(&snap, &feb, 1_000_000.0);
    let q_by: BTreeMap<_, _> = scored
        .iter()
        .map(|r| (r.ticker.clone(), r.clone()))
        .collect();

    let moat = MoatWorker
        .evaluate(&ro, &snap_by["DISP_CORP"], &q_by["DISP_CORP"], &feb, None)
        .unwrap();
    let mgmt = ManagementCapAllocWorker
        .evaluate(&ro, &snap_by["DISP_CORP"], &q_by["DISP_CORP"], &feb, None)
        .unwrap();
    let foren = FootnoteForensicsWorker
        .evaluate(&ro, &snap_by["DISP_CORP"], &q_by["DISP_CORP"], &feb, None)
        .unwrap();

    let dec = InvestmentCommittee::default().evaluate_security(
        &q_by["DISP_CORP"],
        vec![moat, mgmt, foren],
        &ro,
    );
    assert!((dec.quant_base_score - 0.1611).abs() < 1e-3);
    assert!((dec.consensus_score - 0.0956).abs() < 2e-3);
    assert!((dec.dispersion - 0.490).abs() < 2e-3);
    assert!(dec.escalate_review);

    // Full portfolio target weights parity on 2026-02-10: {"CAT_IND": 0.35, "MSFT_QUAL": 0.3144, "AAPL_MOAT": 0.2681}
    let harness = TradeAiHarness::new(build_default_config(
        AutonomyLevel::DraftOnly,
        "platform_admin",
        None,
    ));
    let (t, _) = harness
        .admit_task(&store, "trig-par", "pm_alice", "svc", "cli", &mkts)
        .await
        .unwrap();
    harness
        .frame_task(
            &store,
            &t.task_id,
            "parity",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            3,
        )
        .await
        .unwrap()
        .unwrap();
    let run = harness
        .run_research_and_draft(&store, &t.task_id, None, &["TRAP_CO".into()], 8, 1_000_000.0, None)
        .await
        .unwrap();
    assert_eq!(run.payload.weights_bps["CAT_IND"].bps(), 3_500);
    assert_eq!(run.payload.weights_bps["MSFT_QUAL"].bps(), 3_144);
    assert_eq!(run.payload.weights_bps["AAPL_MOAT"].bps(), 2_681);
}

#[tokio::test]
async fn test_10_db_backed_approval_lifecycle_tamper_expiry_single_use_and_rejection() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let cfg = build_default_config(AutonomyLevel::PerActionApproval, "platform_admin", None);
    let harness = TradeAiHarness::new(cfg);
    let mkts = vec!["US".to_string()];

    let (task, _) = harness
        .admit_task(&store, "trig-apr-db", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    harness
        .frame_task(
            &store,
            &task.task_id,
            "db approvals",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            2,
        )
        .await
        .unwrap()
        .unwrap();

    let res = harness
        .run_research_and_draft(&store, &task.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();
    let GateOutcome::NeedsApproval { approval_id, .. } = res.gate_outcome.as_ref().unwrap() else {
        panic!("expected NeedsApproval");
    };

    // Trying to submit while approval is still PENDING fails!
    let pending_err = harness
        .submit_rebalance_draft(&store, &task.task_id, &res, Some(approval_id))
        .await
        .unwrap_err();
    assert!(matches!(
        pending_err,
        GateError::PreDispatch(PreDispatchError::ApprovalNotApproved(_))
    ));

    // Requester & Admin cannot approve in DB
    assert!(matches!(
        store
            .decide_approval(approval_id, "pm_alice", true, None, "platform_admin")
            .await
            .unwrap_err(),
        GateError::PreDispatch(PreDispatchError::SeparationOfDutiesViolation(_))
    ));
    assert!(matches!(
        store
            .decide_approval(approval_id, "platform_admin", true, None, "platform_admin")
            .await
            .unwrap_err(),
        GateError::PreDispatch(PreDispatchError::SeparationOfDutiesViolation(_))
    ));

    // Independent approver approves in DB
    store
        .decide_approval(approval_id, "approver_bob", true, None, "platform_admin")
        .await
        .unwrap();

    // Mutating payload by 1 bps causes payload_hash mismatch against DB approval!
    let mut tampered = res.clone();
    tampered
        .payload
        .weights_bps
        .insert("AAPL_MOAT".into(), WeightBps::new(1_111).unwrap());
    let mismatch_err = harness
        .submit_rebalance_draft(&store, &task.task_id, &tampered, Some(approval_id))
        .await
        .unwrap_err();
    assert_eq!(
        mismatch_err,
        GateError::PreDispatch(PreDispatchError::ApprovalPayloadMismatch)
    );

    // Original payload succeeds once and marks approval CONSUMED
    let ok = harness
        .submit_rebalance_draft(&store, &task.task_id, &res, Some(approval_id))
        .await
        .unwrap();
    assert!(matches!(ok, GateOutcome::Executed { replayed: false, .. }));
    assert_eq!(
        store.get_approval(approval_id).await.unwrap().unwrap().status,
        trade_core::ApprovalStatus::Consumed
    );

    // Reusing consumed approval_id on a different idempotency key fails with ApprovalAlreadyUsed!
    let mut loaded = store.get_task(&task.task_id).await.unwrap().unwrap();
    let reuse_err = ActionGate::evaluate_and_execute(
        &store,
        &CreateRebalanceDraftTool { store: &store },
        &mut loaded,
        &harness.config,
        &TradeAiHarness::mounted_harness_tools(),
        "portfolio:2026-02-10",
        &res.payload,
        "different-idem-key",
        Some(approval_id),
        0,
        false,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        reuse_err,
        GateError::PreDispatch(PreDispatchError::ApprovalAlreadyUsed(_))
    ));

    // Rejecting an R3 order approval transitions task to DECLINED!
    let GateOutcome::NeedsApproval {
        approval_id: ord_apr,
        ..
    } = harness
        .execute_paper_orders(
            &store,
            &task.task_id,
            &res.payload.draft_id,
            "ord-dec-1",
            None,
        )
        .await
        .unwrap()
    else {
        panic!("expected order NeedsApproval");
    };
    store
        .decide_approval(
            &ord_apr,
            "approver_bob",
            false,
            Some("risk veto"),
            "platform_admin",
        )
        .await
        .unwrap();
    let rej_err = harness
        .execute_paper_orders(
            &store,
            &task.task_id,
            &res.payload.draft_id,
            "ord-dec-1",
            Some(&ord_apr),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        rej_err,
        GateError::PreDispatch(PreDispatchError::ApprovalRejected(_))
    ));
    assert_eq!(
        store.get_task(&task.task_id).await.unwrap().unwrap().state().code(),
        "DECLINED"
    );
}

#[tokio::test]
async fn test_11_backend_reads_veto_list_from_db_ignoring_caller() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let harness = TradeAiHarness::new(build_default_config(
        AutonomyLevel::DraftOnly,
        "platform_admin",
        None,
    ));
    let mkts = vec!["US".to_string()];
    let (task, _) = harness
        .admit_task(&store, "trig-db-veto", "pm_alice", "svc", "cli", &mkts)
        .await
        .unwrap();
    harness
        .frame_task(
            &store,
            &task.task_id,
            "veto test",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            2,
        )
        .await
        .unwrap()
        .unwrap();
    harness
        .run_research_and_draft(&store, &task.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();

    // Caller tries to sneak TRAP_CO (hard-vetoed in DB!) into weights_bps
    let mut loaded = store.get_task(&task.task_id).await.unwrap().unwrap();
    let sneaky_payload = PortfolioActionPayload {
        draft_id: "DRAFT-SNEAKY".into(),
        as_of_date: AsOfDate::new("2026-02-10").unwrap(),
        weights_bps: BTreeMap::from([("TRAP_CO".into(), WeightBps::new(2_500).unwrap())]),
        markets: mkts,
        portfolio_capital_cents: None,
    };
    let err = ActionGate::evaluate_and_execute(
        &store,
        &CreateRebalanceDraftTool { store: &store },
        &mut loaded,
        &harness.config,
        &TradeAiHarness::mounted_harness_tools(),
        "portfolio:2026-02-10",
        &sneaky_payload,
        "idem-sneaky",
        None,
        0,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        GateError::PreDispatch(PreDispatchError::BusinessRuleViolation(
            "vetoed_ticker_in_portfolio:TRAP_CO".into()
        ))
    );
}

#[tokio::test]
async fn test_12_shadow_and_draft_only_autonomy_modes() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let mkts = vec!["US".to_string()];

    // SHADOW blocks R2 draft
    let h_sh = TradeAiHarness::new(build_default_config(
        AutonomyLevel::Shadow,
        "platform_admin",
        None,
    ));
    let (t1, _) = h_sh
        .admit_task(&store, "trig-sh", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    h_sh.frame_task(
        &store,
        &t1.task_id,
        "sh",
        Some("2026-02-10"),
        &mkts,
        Some(UsdCents::from_dollars(1_000_000.0)),
        2,
    )
    .await
    .unwrap()
    .unwrap();
    let sh_err = h_sh
        .run_research_and_draft(&store, &t1.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap_err();
    assert!(matches!(
        sh_err,
        GateError::PreDispatch(PreDispatchError::AutonomyModeBlocked(_))
    ));

    // DRAFT_ONLY allows R2 draft without approval, blocks R3 order submission
    let h_dr = TradeAiHarness::new(build_default_config(
        AutonomyLevel::DraftOnly,
        "platform_admin",
        None,
    ));
    let (t2, _) = h_dr
        .admit_task(&store, "trig-dr", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    h_dr.frame_task(
        &store,
        &t2.task_id,
        "dr",
        Some("2026-02-10"),
        &mkts,
        Some(UsdCents::from_dollars(1_000_000.0)),
        2,
    )
    .await
    .unwrap()
    .unwrap();
    let dr_ok = h_dr
        .run_research_and_draft(&store, &t2.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();
    assert!(matches!(dr_ok.gate_outcome, Some(GateOutcome::Executed { .. })));

    let ord_err = h_dr
        .execute_paper_orders(&store, &t2.task_id, &dr_ok.payload.draft_id, "ord-1", None)
        .await
        .unwrap_err();
    assert!(matches!(
        ord_err,
        GateError::PreDispatch(PreDispatchError::AutonomyModeBlocked(_))
    ));
}

#[tokio::test]
async fn test_13_trigger_deduplication_and_awaiting_input_clarification() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let harness = TradeAiHarness::new(build_default_config(
        AutonomyLevel::DraftOnly,
        "platform_admin",
        None,
    ));
    let mkts = vec!["US".to_string()];

    let (t1, dup1) = harness
        .admit_task(&store, "cron-dedup-1", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    let (t2, dup2) = harness
        .admit_task(&store, "cron-dedup-1", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    assert!(!dup1);
    assert!(dup2);
    assert_eq!(t1.task_id, t2.task_id);
    assert_eq!(t1.state().code(), "QUEUED");

    // Missing as_of_date -> pauses in AWAITING_INPUT with WaitMetadata
    let paused = harness
        .frame_task(
            &store,
            &t1.task_id,
            "ambiguous",
            None,
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            3,
        )
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(paused.state().code(), "AWAITING_INPUT");
    assert_eq!(paused.state().wait_meta().unwrap().owner_id, "pm_alice");

    // Clarify -> transitions AWAITING_INPUT -> RUNNING
    let (_framed, contract) = harness
        .frame_task(
            &store,
            &t1.task_id,
            "clarified",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            3,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(contract.as_of_date.as_str(), "2026-02-10");
    assert_eq!(
        store.get_task(&t1.task_id).await.unwrap().unwrap().state().code(),
        "RUNNING"
    );
}

#[tokio::test]
async fn test_14_broker_drop_before_fill_freeze_reconcile_and_idempotent_retry() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let harness = TradeAiHarness::new(build_default_config(
        AutonomyLevel::PerActionApproval,
        "platform_admin",
        None,
    ));
    let mkts = vec!["US".to_string()];

    let (task, _) = harness
        .admit_task(&store, "trig-rec-1", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    harness
        .frame_task(
            &store,
            &task.task_id,
            "rec 1",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            2,
        )
        .await
        .unwrap()
        .unwrap();

    let res = harness
        .run_research_and_draft(&store, &task.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();
    let GateOutcome::NeedsApproval {
        approval_id: d_apr, ..
    } = res.gate_outcome.as_ref().unwrap()
    else {
        panic!("expected draft NeedsApproval");
    };
    store
        .decide_approval(d_apr, "approver_bob", true, None, "platform_admin")
        .await
        .unwrap();
    let GateOutcome::Executed {
        receipt: draft_id, ..
    } = harness
        .submit_rebalance_draft(&store, &task.task_id, &res, Some(d_apr))
        .await
        .unwrap()
    else {
        panic!("expected draft executed");
    };

    let ord_idem = format!("ord:{}:1", task.task_id);
    let GateOutcome::NeedsApproval {
        approval_id: o_apr, ..
    } = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, &ord_idem, None)
        .await
        .unwrap()
    else {
        panic!("expected order NeedsApproval");
    };
    store
        .decide_approval(&o_apr, "approver_bob", true, None, "platform_admin")
        .await
        .unwrap();

    // Fault 1: DropBeforeFill -> PostDispatchUnknown & OUTCOME_UNKNOWN
    harness
        .broker
        .inject_next_fault(BrokerFaultMode::DropBeforeFill);
    let drop_err = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, &ord_idem, Some(&o_apr))
        .await
        .unwrap_err();
    assert!(matches!(drop_err, GateError::PostDispatchUnknown { .. }));

    // Any new action while OUTCOME_UNKNOWN is frozen!
    let frozen = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, "ord:other", None)
        .await
        .unwrap_err();
    assert!(matches!(
        frozen,
        GateError::PreDispatch(PreDispatchError::TaskFrozenOutcomeUnknown(_))
    ));

    // Reconcile -> finds no order at broker, resets approval & intent safely for retry
    let rec = harness
        .reconcile_before_retry(&store, &task.task_id, &ord_idem)
        .await
        .unwrap();
    assert!(rec.reconciled);
    assert!(rec.action_needed);

    // Retry with SAME idempotency_key and SAME approval_id succeeds!
    let exec_ok = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, &ord_idem, Some(&o_apr))
        .await
        .unwrap();
    assert!(matches!(
        exec_ok,
        GateOutcome::Executed {
            replayed: false,
            ..
        }
    ));

    // Replay with SAME idempotency_key returns replayed=true and creates zero duplicate broker orders!
    let replay = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, &ord_idem, Some(&o_apr))
        .await
        .unwrap();
    assert!(matches!(
        replay,
        GateOutcome::Executed { replayed: true, .. }
    ));
    assert_eq!(harness.broker.count_orders_for_task(&task.task_id), 1);
}

#[tokio::test]
async fn test_15_broker_timeout_after_fill_reconciles_without_duplicate_order() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let harness = TradeAiHarness::new(build_default_config(
        AutonomyLevel::PerActionApproval,
        "platform_admin",
        None,
    ));
    let mkts = vec!["US".to_string()];

    let (task, _) = harness
        .admit_task(&store, "trig-rec-2", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    harness
        .frame_task(
            &store,
            &task.task_id,
            "rec 2",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            2,
        )
        .await
        .unwrap()
        .unwrap();

    let res = harness
        .run_research_and_draft(&store, &task.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();
    let GateOutcome::NeedsApproval {
        approval_id: d_apr, ..
    } = res.gate_outcome.as_ref().unwrap()
    else {
        panic!("expected draft NeedsApproval");
    };
    store
        .decide_approval(d_apr, "approver_bob", true, None, "platform_admin")
        .await
        .unwrap();
    let GateOutcome::Executed {
        receipt: draft_id, ..
    } = harness
        .submit_rebalance_draft(&store, &task.task_id, &res, Some(d_apr))
        .await
        .unwrap()
    else {
        panic!("expected draft executed");
    };

    let ord_idem = format!("ord:{}:afterfill", task.task_id);
    let GateOutcome::NeedsApproval {
        approval_id: o_apr, ..
    } = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, &ord_idem, None)
        .await
        .unwrap()
    else {
        panic!("expected order NeedsApproval");
    };
    store
        .decide_approval(&o_apr, "approver_bob", true, None, "platform_admin")
        .await
        .unwrap();

    // Broker fills the order, THEN drops response -> OUTCOME_UNKNOWN!
    harness
        .broker
        .inject_next_fault(BrokerFaultMode::TimeoutAfterFill);
    assert!(matches!(
        harness
            .execute_paper_orders(&store, &task.task_id, &draft_id, &ord_idem, Some(&o_apr))
            .await
            .unwrap_err(),
        GateError::PostDispatchUnknown { .. }
    ));

    // Reconcile discovers the already-filled order at broker -> marks VERIFIED, action_needed=false!
    let rec = harness
        .reconcile_before_retry(&store, &task.task_id, &ord_idem)
        .await
        .unwrap();
    assert!(rec.reconciled);
    assert!(!rec.action_needed);
    assert!(rec.receipt_id.is_some());
    assert_eq!(harness.broker.count_orders_for_task(&task.task_id), 1);

    // Independent verifier passes cleanly after reconciliation!
    let verified = harness.verify_and_close(&store, &task.task_id).await.unwrap();
    assert_eq!(verified.manifest.final_state.code(), "SETTLING");
    assert!(verified.settling_task.is_some());
}

#[tokio::test]
async fn test_16_db_only_independent_verifier_catches_tampered_db_citation() {
    let store = DbStore::connect(":memory:").await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let harness = TradeAiHarness::new(build_default_config(
        AutonomyLevel::DraftOnly,
        "platform_admin",
        None,
    ));
    let mkts = vec!["US".to_string()];

    let (task, _) = harness
        .admit_task(&store, "trig-ver-db", "pm_alice", "svc", "cli", &mkts)
        .await
        .unwrap();
    harness
        .frame_task(
            &store,
            &task.task_id,
            "verify db",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            2,
        )
        .await
        .unwrap()
        .unwrap();
    let res = harness
        .run_research_and_draft(&store, &task.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();

    // Corrupt a worker evidence row in DB with future citation FIN-LATE-2026 (filed 2026-04-15 > 2026-02-10)
    let mut tampered_decisions = res.decisions.clone();
    tampered_decisions[0].worker_outputs = vec![AgentEvidenceOutput {
        ticker: tampered_decisions[0].ticker.clone(),
        as_of_date: AsOfDate::new("2026-02-10").unwrap(),
        agent: "moat".into(),
        score: 0.5,
        confidence: 0.8,
        hard_veto: false,
        evidence: vec!["future leak".into()],
        cited_record_ids: vec!["FIN-LATE-2026".into()],
        missingness: 0.0,
        stale_data: false,
        assumptions: vec![],
    }];
    let q_by: BTreeMap<_, _> = res
        .quant_results
        .iter()
        .map(|q| (q.ticker.clone(), q.clone()))
        .collect();
    store
        .save_research_evaluations(&task.task_id, &tampered_decisions, &q_by)
        .await
        .unwrap();

    // Notice: verify_and_close takes ONLY (&store, &task.task_id) and catches the future citation from DB!
    let outcome = harness.verify_and_close(&store, &task.task_id).await.unwrap();
    assert_eq!(outcome.manifest.final_state.code(), "HANDED_OFF");
    assert!(!outcome.manifest.criteria_results["citations_pit_valid"]);
    assert!(outcome.settling_task.is_none());
}

#[tokio::test]
async fn test_17_limit_reached_manifest_handoff_settling_completed_reopen_and_file_db() {
    let tmp_db = format!("/tmp/trade_ai_rust_test_{}.sqlite3", std::process::id());
    let _ = std::fs::remove_file(&tmp_db);
    let store = DbStore::connect(&tmp_db).await.unwrap();
    seed_pit_universe(&store).await.unwrap();
    let mkts = vec!["US".to_string()];

    // 1. Tight budget -> LIMIT_REACHED + blocked DeliveryManifest + HandoffPackage -> HANDED_OFF
    let tight_cfg = build_default_config(
        AutonomyLevel::DraftOnly,
        "platform_admin",
        Some(BudgetLimits {
            max_turns: 2,
            ..Default::default()
        }),
    );
    let h_lim = TradeAiHarness::new(tight_cfg);
    let (mut t_lim, _) = h_lim
        .admit_task(&store, "trig-lim", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    h_lim
        .frame_task(
            &store,
            &t_lim.task_id,
            "tight",
            Some("2026-02-10"),
            &mkts,
            Some(UsdCents::from_dollars(1_000_000.0)),
            2,
        )
        .await
        .unwrap()
        .unwrap();
    t_lim = store.get_task(&t_lim.task_id).await.unwrap().unwrap();
    t_lim.turns_used = 2;
    store
        .save_task_optimistic(&mut t_lim, "exhaust_turns", "test")
        .await
        .unwrap();

    let lim_err = h_lim
        .run_research_and_draft(&store, &t_lim.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap_err();
    assert!(matches!(
        lim_err,
        GateError::PreDispatch(PreDispatchError::LimitReached(_))
    ));
    let lim_out = h_lim.verify_and_close(&store, &t_lim.task_id).await.unwrap();
    assert_eq!(lim_out.manifest.completeness, Completeness::Blocked);
    assert_eq!(lim_out.manifest.final_state.code(), "LIMIT_REACHED");

    let pkg = h_lim
        .create_handoff_package(
            &store,
            &t_lim.task_id,
            "Run tight rebalance",
            "limit_reached",
            BTreeMap::new(),
            vec!["Increase max_turns?".into()],
        )
        .await
        .unwrap();
    let accepted = h_lim
        .confirm_handoff(&store, pkg, "human_ops_lead")
        .await
        .unwrap();
    assert!(accepted.accepted_by_human);
    assert_eq!(
        store.get_task(&t_lim.task_id).await.unwrap().unwrap().state().code(),
        "HANDED_OFF"
    );

    // 2. Full completion -> SETTLING -> COMPLETED -> curated_memory -> economics -> reopen
    let h_ok = TradeAiHarness::new(build_default_config(
        AutonomyLevel::DraftOnly,
        "platform_admin",
        None,
    ));
    let (t_ok, _) = h_ok
        .admit_task(&store, "trig-ok", "pm_alice", "svc", "cron", &mkts)
        .await
        .unwrap();
    h_ok.frame_task(
        &store,
        &t_ok.task_id,
        "normal close",
        Some("2026-02-10"),
        &mkts,
        Some(UsdCents::from_dollars(1_000_000.0)),
        2,
    )
    .await
    .unwrap()
    .unwrap();
    h_ok.run_research_and_draft(&store, &t_ok.task_id, None, &[], 8, 1_000_000.0, None)
        .await
        .unwrap();
    let ver_ok = h_ok.verify_and_close(&store, &t_ok.task_id).await.unwrap();
    assert_eq!(ver_ok.manifest.final_state.code(), "SETTLING");

    let completed = h_ok
        .confirm_settlement(&store, ver_ok.settling_task.unwrap(), "pm_alice", 3.0)
        .await
        .unwrap();
    assert_eq!(completed.record().state().code(), "COMPLETED");

    let mems = store.list_curated_memory(Some("pending_review")).await.unwrap();
    assert_eq!(mems.len(), 1);

    let econ_before = store.calculate_unit_economics(12_000).await.unwrap();
    assert_eq!(econ_before.verified_completed_non_reopened, 1);
    assert!(econ_before.cost_per_verified_task_usd.is_some());

    // Reopen COMPLETED -> RUNNING deducts from verified_completed_non_reopened!
    let reopened = h_ok
        .reopen_task(&store, &t_ok.task_id, "audit_recheck", "pm_alice")
        .await
        .unwrap();
    assert_eq!(reopened.state().code(), "RUNNING");
    assert_eq!(reopened.reopened_count, 1);

    let econ_after = store.calculate_unit_economics(12_000).await.unwrap();
    assert_eq!(econ_after.verified_completed_non_reopened, 0);
    assert!(econ_after.cost_per_verified_task_usd.is_none());

    drop(store);
    let _ = std::fs::remove_file(&tmp_db);
}
