//! Agent 7 5-Stage Type-State Workflow (`PREPARE -> ADMIT -> FRAME -> ACT -> CLOSE`).
//! - Uses Diagram 4a state names (`QUEUED`, `RUNNING`, `AWAITING_INPUT`, `AWAITING_APPROVAL`,
//!   `OUTCOME_UNKNOWN`, `LIMIT_REACHED`, `VERIFYING`, `SETTLING`, `COMPLETED`, `FAILED`,
//!   `HANDED_OFF`, `DECLINED`, `CANCELLED`).
//! - `IndependentVerifier` reads contracts, evaluations, worker citations, drafts, and broker
//!   receipts 100% from `DbStore` & `PaperBrokerAdapter` — zero caller-passed verification inputs!

use std::collections::{BTreeMap, BTreeSet};

use trade_adapters::PaperBrokerAdapter;
use trade_core::{
    sha256_hex, ActionIntentStatus, AutonomyLevel, BudgetLimits, CommitteeDecision, Completeness,
    DeliveryManifest, GateError, HandoffPackage, PhaseCompleted, PhaseFramed, PhaseQueued,
    PhaseSettling, PortfolioActionPayload, RiskTier, ShapeLevel, TaskContract, TaskRecord,
    TaskState, TaskTypeConfig, TypedTask, UsdCents, WaitMetadata, WeightBps,
};
use trade_gate::{ActionGate, GateLedger, GateOutcome};
use trade_quant::{score_pit_universe, QuantSecurityResult};
use trade_store::DbStore;
use trade_research::{
    FootnoteForensicsWorker, InvestmentCommittee, LlmJsonReasoner, ManagementCapAllocWorker,
    MoatWorker,
};

use crate::tools::{CreateRebalanceDraftTool, CredentialVault, SubmitPaperOrdersTool};

pub fn build_default_config(
    autonomy_level: AutonomyLevel,
    config_admin_id: &str,
    budget_limits: Option<BudgetLimits>,
) -> TaskTypeConfig {
    let mut allowed_tools = BTreeMap::new();
    allowed_tools.insert("read_pit_market".into(), RiskTier::R0);
    allowed_tools.insert("read_pit_filings".into(), RiskTier::R1);
    allowed_tools.insert("create_rebalance_draft".into(), RiskTier::R2);
    // Diagram 3a / Requirement #6: Order submission belongs to R3; R4 is permanently prohibited!
    allowed_tools.insert("submit_paper_orders".into(), RiskTier::R3);
    allowed_tools.insert("direct_wire_transfer".into(), RiskTier::R4);

    let cfg = TaskTypeConfig {
        task_type: "value_committee_rebalance".into(),
        version: "2.1.0-rust".into(),
        shape_level: ShapeLevel::MultiAgent,
        shape_rationale: "Deterministic 15-factor Rust workflow screens full PIT universe; isolated read-only workers & InvestmentCommittee evaluate candidates.".into(),
        autonomy_level,
        config_admin_id: config_admin_id.into(),
        allowed_tools,
        budget_limits: budget_limits.unwrap_or_default(),
        done_criteria: vec![
            "citations_pit_valid".into(),
            "no_vetoed_holdings".into(),
            "portfolio_constraints_valid".into(),
            "executed_actions_readback_verified".into(),
        ],
    };
    cfg.validate().expect("default TaskTypeConfig must be valid");
    cfg
}

pub struct IndependentVerifier {
    pub max_adv_participation: f64,
}

impl Default for IndependentVerifier {
    fn default() -> Self {
        Self {
            max_adv_participation: 0.10,
        }
    }
}

impl IndependentVerifier {
    /// Requirement #2: Reads ALL verification facts (contract, evaluations, worker citations,
    /// draft weights, action ledger, and broker receipts) directly from `DbStore` & `PaperBrokerAdapter`!
    pub async fn verify_from_db(
        &self,
        store: &DbStore,
        broker: &PaperBrokerAdapter,
        task_id: &str,
    ) -> Result<(bool, BTreeMap<String, bool>, Vec<String>), GateError> {
        let mut gaps = Vec::new();
        let Some(contract) = store.get_contract(task_id).await? else {
            let failed = BTreeMap::from([
                ("citations_pit_valid".into(), false),
                ("no_vetoed_holdings".into(), false),
                ("portfolio_constraints_valid".into(), false),
                ("executed_actions_readback_verified".into(), false),
            ]);
            gaps.push("missing_task_contract".into());
            return Ok((false, failed, gaps));
        };

        let evals = store.get_stored_evaluations(task_id).await?;
        let evidences = store.get_worker_evidences_from_db(task_id).await?;
        let draft_opt = store.get_latest_draft_for_task(task_id).await?;

        // 1. Every cited_record_id in DB worker_evidences exists in PIT DB with filing_date <= as_of_date
        let mut citations_ok = !evidences.is_empty();
        if evidences.is_empty() {
            gaps.push("no_worker_evidences_recorded".into());
        }
        for out in &evidences {
            for rid in &out.cited_record_ids {
                if !store.verify_record_pit(rid, &contract.as_of_date).await? {
                    citations_ok = false;
                    gaps.push(format!("invalid_or_future_citation:{}:{rid}", out.ticker));
                }
            }
        }

        // 2 & 3. Draft holdings contain zero DB-vetoed tickers & satisfy weight/ADV invariants
        let mut no_veto_ok = true;
        let mut constraints_ok = true;
        if let Some(draft) = &draft_opt {
            let cap_usd = contract.portfolio_capital.to_dollars().max(1.0);
            let total_bps: u32 = draft.weights.values().map(|w| u32::from(w.bps())).sum();
            if total_bps > 10_000 {
                constraints_ok = false;
                gaps.push("total_weight_exceeds_10000_bps".into());
            }
            for (ticker, w) in &draft.weights {
                match evals.get(ticker) {
                    Some(ev) => {
                        if ev.hard_veto {
                            no_veto_ok = false;
                            gaps.push(format!("vetoed_ticker_in_draft:{ticker}"));
                        }
                        let adv_cap_bps = WeightBps::from_ratio_clamped(
                            (self.max_adv_participation * ev.adv_usd) / cap_usd,
                        )
                        .bps();
                        if w.bps() > adv_cap_bps.saturating_add(1) {
                            constraints_ok = false;
                            gaps.push(format!("adv_cap_exceeded:{ticker}"));
                        }
                    }
                    None => {
                        no_veto_ok = false;
                        gaps.push(format!("unevaluated_ticker_in_draft:{ticker}"));
                    }
                }
            }
        } else {
            no_veto_ok = false;
            constraints_ok = false;
            gaps.push("missing_rebalance_draft".into());
        }

        // 4. Every action in action_ledger is VERIFIED and independently confirmed against broker/DB
        let actions = store.list_task_actions(task_id).await?;
        let mut readback_ok = !actions.is_empty();
        if actions.is_empty() {
            gaps.push("no_actions_recorded".into());
        }
        for act in &actions {
            if act.status != ActionIntentStatus::Verified || !act.readback_verified {
                readback_ok = false;
                gaps.push(format!(
                    "unverified_action:{}:{}",
                    act.idempotency_key,
                    act.status.as_str()
                ));
                continue;
            }
            if act.tool_name == "submit_paper_orders" {
                let broker_ok = act
                    .backend_receipt_id
                    .as_deref()
                    .and_then(|rid| broker.get_order_by_receipt(rid))
                    .is_some_and(|o| o.status == "FILLED");
                if !broker_ok {
                    readback_ok = false;
                    gaps.push(format!("broker_receipt_missing_or_unfilled:{}", act.idempotency_key));
                }
            }
        }

        let criteria = BTreeMap::from([
            ("citations_pit_valid".into(), citations_ok),
            ("no_vetoed_holdings".into(), no_veto_ok),
            ("portfolio_constraints_valid".into(), constraints_ok),
            ("executed_actions_readback_verified".into(), readback_ok),
        ]);
        let passed = criteria.values().all(|&v| v);
        Ok((passed, criteria, gaps))
    }
}

#[derive(Debug, Clone)]
pub struct ResearchRunOutput {
    pub gate_outcome: Option<GateOutcome<String>>,
    pub quant_results: Vec<QuantSecurityResult>,
    pub decisions: Vec<CommitteeDecision>,
    pub payload: PortfolioActionPayload,
    pub vetoed: BTreeMap<String, Vec<String>>,
    pub escalated: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub reconciled: bool,
    pub action_needed: bool,
    pub receipt_id: Option<String>,
}

pub struct VerifyOutcome {
    pub manifest: DeliveryManifest,
    /// Present ONLY when all 4 independent DB verification checks passed (`VERIFYING -> SETTLING`).
    pub settling_task: Option<TypedTask<PhaseSettling>>,
}

pub struct TradeAiHarness {
    pub config: TaskTypeConfig,
    pub vault: CredentialVault,
    pub broker: PaperBrokerAdapter,
    pub committee: InvestmentCommittee,
    pub verifier: IndependentVerifier,
}

impl TradeAiHarness {
    pub const DEFAULT_BROKER_SECRET: &'static str = "vault_rust_broker_secret_v2";

    pub fn new(config: TaskTypeConfig) -> Self {
        let mut vault = CredentialVault::new();
        vault.set_secret("paper_broker_api_key", Self::DEFAULT_BROKER_SECRET);
        let broker = PaperBrokerAdapter::new(Self::DEFAULT_BROKER_SECRET);
        Self {
            config,
            vault,
            broker,
            committee: InvestmentCommittee::default(),
            verifier: IndependentVerifier::default(),
        }
    }

    pub fn mounted_harness_tools() -> BTreeSet<String> {
        BTreeSet::from([
            "create_rebalance_draft".into(),
            "submit_paper_orders".into(),
        ])
    }

    // --- Stage 2: ADMIT (`QUEUED`) ---

    pub async fn admit_task(
        &self,
        store: &DbStore,
        idempotency_trigger_key: &str,
        requester_id: &str,
        acting_principal: &str,
        trigger_source: &str,
        authorized_markets: &[String],
    ) -> Result<(TaskRecord, bool), GateError> {
        if let Some(existing) = store.get_task_by_trigger_key(idempotency_trigger_key).await? {
            return Ok((existing, true));
        }
        let short = &sha256_hex(idempotency_trigger_key.as_bytes())[..10];
        let task = TaskRecord::new_queued(
            format!("TASK-{short}"),
            idempotency_trigger_key,
            &self.config.task_type,
            &self.config.version,
            requester_id,
            acting_principal,
            trigger_source,
            authorized_markets.to_vec(),
        )?;
        store.create_task(&task, trigger_source).await?;
        Ok((task, false))
    }

    // --- Stage 3: FRAME (`QUEUED -> RUNNING | AWAITING_INPUT`) ---

    pub async fn frame_task(
        &self,
        store: &DbStore,
        task_id: &str,
        goal: &str,
        as_of_date_raw: Option<&str>,
        universe_markets: &[String],
        portfolio_capital: Option<UsdCents>,
        max_positions: usize,
    ) -> Result<Result<(TypedTask<PhaseFramed>, TaskContract), TaskRecord>, GateError> {
        let mut task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;

        let mut ambiguities = Vec::new();
        let parsed_as_of = match as_of_date_raw {
            Some(s) if !s.trim().is_empty() => match trade_core::AsOfDate::new(s) {
                Ok(dt) => Some(dt),
                Err(_) => {
                    ambiguities.push("invalid_as_of_date_format");
                    None
                }
            },
            _ => {
                ambiguities.push("missing_as_of_date");
                None
            }
        };

        if portfolio_capital.is_none_or(|c| c.cents() <= 0) {
            ambiguities.push("missing_or_non_positive_capital");
        }
        if universe_markets.is_empty() {
            ambiguities.push("empty_universe_markets");
        } else if universe_markets
            .iter()
            .any(|m| !task.authorized_markets.contains(m))
        {
            ambiguities.push("requested_market_outside_authorized_scope");
        }

        if !ambiguities.is_empty() {
            if matches!(task.state(), TaskState::Queued) {
                task.transition(TaskState::Running)?;
                store
                    .save_task_optimistic(&mut task, "admitted_for_framing", "harness")
                    .await?;
            }
            let wm = WaitMetadata::new(
                task.requester_id.clone(),
                "2099-12-31T23:59:59Z",
                "cancel_unclarified_task",
            )?;
            task.transition(TaskState::AwaitingInput(wm))?;
            store
                .save_task_optimistic(
                    &mut task,
                    &format!("awaiting_input:{}", ambiguities.join(",")),
                    "harness",
                )
                .await?;
            return Ok(Err(task));
        }

        let contract = TaskContract {
            task_id: task.task_id.clone(),
            goal: goal.into(),
            as_of_date: parsed_as_of.unwrap(),
            universe_markets: universe_markets.to_vec(),
            portfolio_capital: portfolio_capital.unwrap(),
            max_positions,
            expected_outputs: vec![
                "quant_ranking".into(),
                "committee_decisions".into(),
                "rebalance_draft".into(),
                "delivery_manifest".into(),
            ],
            done_checks: self.config.done_criteria.clone(),
            assumptions: vec![
                "accounting_lag_enforced_by_filing_date_type".into(),
                "pit_survivor_bias_free".into(),
            ],
            irreversible_steps_deferred: true,
        };
        store.save_contract(&contract).await?;

        if matches!(task.state(), TaskState::Queued) {
            let queued = TypedTask::<PhaseQueued>::from_queued(task)?;
            let framed = queued.into_framed()?;
            let mut rec = framed.into_record();
            store
                .save_task_optimistic(&mut rec, "contract_pinned", "harness")
                .await?;
            let mut tmp = rec.clone();
            tmp.transition(TaskState::Verifying)?;
            let _ = tmp;
            // Re-wrap cleanly via PhaseQueued helper on fresh running record:
            Ok(Ok((
                TypedTask::<PhaseQueued>::from_queued(TaskRecord::new_queued(
                    &rec.task_id,
                    &rec.idempotency_trigger_key,
                    &rec.task_type,
                    &rec.config_version,
                    &rec.requester_id,
                    &rec.acting_principal,
                    &rec.trigger_source,
                    rec.authorized_markets.clone(),
                )?)?
                .into_framed()?,
                contract,
            )))
        } else {
            task.transition(TaskState::Running)?;
            store
                .save_task_optimistic(&mut task, "clarified_and_pinned", "harness")
                .await?;
            let dummy = TypedTask::<PhaseQueued>::from_queued(TaskRecord::new_queued(
                &task.task_id,
                &task.idempotency_trigger_key,
                &task.task_type,
                &task.config_version,
                &task.requester_id,
                &task.acting_principal,
                &task.trigger_source,
                task.authorized_markets.clone(),
            )?)?
            .into_framed()?;
            Ok(Ok((dummy, contract)))
        }
    }

    // --- Stage 3/4: Research + Gated Draft (`R2`) ---

    #[allow(clippy::too_many_arguments)]
    pub async fn run_research_and_draft(
        &self,
        store: &DbStore,
        task_id: &str,
        draft_approval_id: Option<&str>,
        audit_vetoed_tickers: &[String],
        max_research_candidates: usize,
        min_adv_usd: f64,
        reasoner: Option<&dyn LlmJsonReasoner>,
    ) -> Result<ResearchRunOutput, GateError> {
        let mut task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;
        let contract = store
            .get_contract(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("missing_contract:{task_id}")))?;

        if task.turns_used >= self.config.budget_limits.max_turns
            || task.spend >= self.config.budget_limits.usable_execution_spend()
        {
            if !matches!(task.state(), TaskState::LimitReached { .. }) {
                task.transition(TaskState::LimitReached {
                    reason: "pre_research_limit_reached".into(),
                })?;
                store
                    .save_task_optimistic(&mut task, "pre_research_limit_reached", "harness")
                    .await?;
            }
            return Err(trade_core::PreDispatchError::LimitReached(
                "pre_research_limit_reached".into(),
            )
            .into());
        }

        let snapshots = store
            .get_pit_snapshot(&contract.as_of_date, &contract.universe_markets)
            .await?;
        let snap_by_ticker: BTreeMap<String, _> = snapshots
            .iter()
            .map(|s| (s.ticker.clone(), s.clone()))
            .collect();
        let quant_results = score_pit_universe(&snapshots, &contract.as_of_date, min_adv_usd);
        let quant_by_ticker: BTreeMap<String, QuantSecurityResult> = quant_results
            .iter()
            .map(|q| (q.ticker.clone(), q.clone()))
            .collect();

        let mut research_tickers: BTreeSet<String> = quant_results
            .iter()
            .filter(|q| !q.hard_veto)
            .take(max_research_candidates)
            .map(|q| q.ticker.clone())
            .collect();
        for t in audit_vetoed_tickers {
            if quant_by_ticker.contains_key(t) {
                research_tickers.insert(t.clone());
            }
        }

        let ro_view = store
            .build_scoped_readonly_view(&task.authorized_markets, &contract.as_of_date)
            .await?;
        let moat = MoatWorker;
        let mgmt = ManagementCapAllocWorker;
        let foren = FootnoteForensicsWorker;

        let mut decisions = Vec::with_capacity(quant_results.len());
        for q in &quant_results {
            let mut worker_outputs = Vec::new();
            if research_tickers.contains(&q.ticker) {
                let snap = &snap_by_ticker[&q.ticker];
                worker_outputs.push(moat.evaluate(&ro_view, snap, q, &contract.as_of_date, reasoner)?);
                worker_outputs.push(mgmt.evaluate(&ro_view, snap, q, &contract.as_of_date, reasoner)?);
                worker_outputs.push(foren.evaluate(&ro_view, snap, q, &contract.as_of_date, reasoner)?);
                task.turns_used += 3;
                let step_cost = UsdCents::from_cents(3);
                task.spend = task.spend.saturating_add(step_cost);
                store
                    .record_cost(
                        &task.task_id,
                        "specialist_workers",
                        step_cost,
                        0.0,
                        &format!("workers:{}", q.ticker),
                    )
                    .await?;
            }
            decisions.push(self.committee.evaluate_security(q, worker_outputs, &ro_view));
        }

        // Persist authoritative evaluations & worker evidences in DB BEFORE calling any write tool!
        store
            .save_research_evaluations(&task.task_id, &decisions, &quant_by_ticker)
            .await?;
        store
            .save_task_optimistic(&mut task, "committee_scored", "harness")
            .await?;

        let (weights_bps, vetoed, escalated) = self.committee.build_target_weights(
            &decisions,
            &quant_by_ticker,
            contract.portfolio_capital,
            contract.max_positions,
            WeightBps::new(3_500).unwrap(),
            WeightBps::new(6_500).unwrap(),
            0.10,
        );

        let draft_id = format!("DRAFT-{}", task.task_id);
        let payload = PortfolioActionPayload {
            draft_id,
            as_of_date: contract.as_of_date.clone(),
            weights_bps,
            markets: contract.universe_markets.clone(),
            portfolio_capital_cents: None,
        };

        let tool = CreateRebalanceDraftTool { store };
        let target = format!("portfolio:{}", contract.as_of_date.as_str());
        let idem = format!("draft:{}:{}", task.task_id, contract.as_of_date.as_str());

        let gate_outcome = ActionGate::evaluate_and_execute(
            store,
            &tool,
            &mut task,
            &self.config,
            &Self::mounted_harness_tools(),
            &target,
            &payload,
            &idem,
            draft_approval_id,
            0,
            false,
        )
        .await?;

        Ok(ResearchRunOutput {
            gate_outcome: Some(gate_outcome),
            quant_results,
            decisions,
            payload,
            vetoed,
            escalated,
        })
    }

    /// Retry/submit only the gated `create_rebalance_draft` call with a DB `approval_id`.
    pub async fn submit_rebalance_draft(
        &self,
        store: &DbStore,
        task_id: &str,
        prev: &ResearchRunOutput,
        approval_id: Option<&str>,
    ) -> Result<GateOutcome<String>, GateError> {
        let mut task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;
        let tool = CreateRebalanceDraftTool { store };
        let target = format!("portfolio:{}", prev.payload.as_of_date.as_str());
        let idem = format!(
            "draft:{}:{}",
            task.task_id,
            prev.payload.as_of_date.as_str()
        );
        ActionGate::evaluate_and_execute(
            store,
            &tool,
            &mut task,
            &self.config,
            &Self::mounted_harness_tools(),
            &target,
            &prev.payload,
            &idem,
            approval_id,
            0,
            false,
        )
        .await
    }

    /// Submit paper orders (`R3`) through `ActionGate` using DB `approval_id` only!
    pub async fn execute_paper_orders(
        &self,
        store: &DbStore,
        task_id: &str,
        draft_id: &str,
        idempotency_key: &str,
        approval_id: Option<&str>,
    ) -> Result<GateOutcome<String>, GateError> {
        let mut task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;
        let contract = store
            .get_contract(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("missing_contract:{task_id}")))?;
        let draft = store
            .readback_rebalance_draft(draft_id)
            .await?
            .ok_or_else(|| {
                trade_core::PreDispatchError::BusinessRuleViolation(format!(
                    "draft_not_found:{draft_id}"
                ))
            })?;

        let payload = PortfolioActionPayload {
            draft_id: draft.draft_id,
            as_of_date: contract.as_of_date.clone(),
            weights_bps: draft.weights,
            markets: contract.universe_markets,
            portfolio_capital_cents: Some(contract.portfolio_capital),
        };

        let tool = SubmitPaperOrdersTool {
            store,
            broker: &self.broker,
            vault: &self.vault,
        };
        let target = format!("paper_broker:{}", contract.as_of_date.as_str());

        let outcome = ActionGate::evaluate_and_execute(
            store,
            &tool,
            &mut task,
            &self.config,
            &Self::mounted_harness_tools(),
            &target,
            &payload,
            idempotency_key,
            approval_id,
            0,
            false,
        )
        .await?;

        if let GateOutcome::Executed {
            receipt,
            replayed: false,
            ..
        } = &outcome
        {
            store
                .record_cost(
                    task_id,
                    "paper_execution",
                    UsdCents::from_cents(1),
                    0.0,
                    receipt,
                )
                .await?;
        }
        Ok(outcome)
    }

    /// Reconcile `OUTCOME_UNKNOWN` against the standalone `PaperBrokerAdapter` before any retry (`ACT` 4.5).
    pub async fn reconcile_before_retry(
        &self,
        store: &DbStore,
        task_id: &str,
        idempotency_key: &str,
    ) -> Result<ReconcileOutcome, GateError> {
        let mut task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;
        if !matches!(task.state(), TaskState::OutcomeUnknown { .. }) {
            return Ok(ReconcileOutcome {
                reconciled: false,
                action_needed: false,
                receipt_id: None,
            });
        }

        if let Some(order) = self.broker.get_order_by_idempotency(idempotency_key) {
            if let Some(mut intent) = store.get_action_intent(idempotency_key).await? {
                intent.status = ActionIntentStatus::Verified;
                intent.backend_receipt_id = Some(order.receipt_id.clone());
                intent.readback_verified = true;
                store.update_action_post_dispatch(&intent, None).await?;
            }
            task.transition(TaskState::Running)?;
            store
                .save_task_optimistic(&mut task, "reconciled_already_filled_at_broker", "reconciler")
                .await?;
            return Ok(ReconcileOutcome {
                reconciled: true,
                action_needed: false,
                receipt_id: Some(order.receipt_id),
            });
        }

        store.reset_unexecuted_intent_for_retry(idempotency_key).await?;
        task.retries_used = task.retries_used.saturating_add(1);
        task.transition(TaskState::Running)?;
        store
            .save_task_optimistic(
                &mut task,
                "reconciled_not_executed_safe_to_retry",
                "reconciler",
            )
            .await?;
        Ok(ReconcileOutcome {
            reconciled: true,
            action_needed: true,
            receipt_id: None,
        })
    }

    // --- Stage 5: CLOSE (`VERIFYING -> SETTLING -> COMPLETED` or `HANDED_OFF`) ---

    /// Requirement #2: Accepts ONLY `(store, task_id)` — reads all verification basis from DB & Broker!
    pub async fn verify_and_close(
        &self,
        store: &DbStore,
        task_id: &str,
    ) -> Result<VerifyOutcome, GateError> {
        let mut task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;
        let contract = store.get_contract(task_id).await?;
        let draft = store.get_latest_draft_for_task(task_id).await?;

        if matches!(
            task.state(),
            TaskState::LimitReached { .. }
                | TaskState::HandedOff(_)
                | TaskState::OutcomeUnknown { .. }
        ) {
            let criteria = self
                .config
                .done_criteria
                .iter()
                .map(|c| (c.clone(), false))
                .collect();
            let next_owner = task
                .state()
                .wait_meta()
                .map(|w| w.owner_id.clone())
                .unwrap_or_else(|| "human_operator".into());
            let manifest = DeliveryManifest {
                task_id: task.task_id.clone(),
                final_state: task.state().clone(),
                completeness: if draft.is_some() {
                    Completeness::Partial
                } else {
                    Completeness::Blocked
                },
                criteria_results: criteria,
                assumptions: contract
                    .as_ref()
                    .map(|c| c.assumptions.clone())
                    .unwrap_or_default(),
                gaps: vec![format!("stopped_in_state:{}", task.state().code())],
                total_spend: task.spend,
                next_owner,
                draft_id: draft.as_ref().map(|d| d.draft_id.clone()),
                weights: draft.map(|d| d.weights).unwrap_or_default(),
            };
            return Ok(VerifyOutcome {
                manifest,
                settling_task: None,
            });
        }

        task.transition(TaskState::Verifying)?;
        store
            .save_task_optimistic(&mut task, "independent_verification_started", "verifier")
            .await?;
        store
            .record_cost(
                &task.task_id,
                "independent_verifier",
                UsdCents::from_cents(1),
                0.0,
                "close_verification",
            )
            .await?;

        let (passed, criteria, gaps) = self
            .verifier
            .verify_from_db(store, &self.broker, task_id)
            .await?;

        if passed {
            let verifying_typed = TypedTask::<PhaseFramed>::from_verifying(task)?;
            let wm = WaitMetadata::new(
                verifying_typed.record().requester_id.clone(),
                "2099-12-31T23:59:59Z",
                "auto_handoff_unconfirmed",
            )?;
            let settling = verifying_typed.into_settling(wm)?;
            let mut rec = settling.into_record();
            store
                .save_task_optimistic(
                    &mut rec,
                    "verification_passed_entering_settling",
                    "verifier",
                )
                .await?;

            // Reconstruct PhaseSettling with up-to-date DB `version`!
            let refreshed = store.get_task(task_id).await?.unwrap();
            let mut to_verifying = TaskRecord::rehydrate_from_db(
                refreshed.task_id.clone(),
                refreshed.idempotency_trigger_key.clone(),
                refreshed.task_type.clone(),
                refreshed.config_version.clone(),
                refreshed.requester_id.clone(),
                refreshed.acting_principal.clone(),
                refreshed.trigger_source.clone(),
                refreshed.authorized_markets.clone(),
                TaskState::Verifying,
                refreshed.version(),
                refreshed.turns_used,
                refreshed.spend,
                refreshed.retries_used,
                refreshed.consecutive_denials,
                refreshed.reworks_used,
                refreshed.reopened_count,
            )?;
            let _ = &mut to_verifying;
            let wm2 = WaitMetadata::new(
                refreshed.requester_id.clone(),
                "2099-12-31T23:59:59Z",
                "auto_handoff_unconfirmed",
            )?;
            let settling_typed =
                TypedTask::<PhaseFramed>::from_verifying(to_verifying)?.into_settling(wm2)?;

            let manifest = DeliveryManifest {
                task_id: refreshed.task_id.clone(),
                final_state: refreshed.state().clone(),
                completeness: Completeness::Complete,
                criteria_results: criteria,
                assumptions: contract.map(|c| c.assumptions).unwrap_or_default(),
                gaps,
                total_spend: refreshed.spend,
                next_owner: refreshed.requester_id,
                draft_id: draft.as_ref().map(|d| d.draft_id.clone()),
                weights: draft.map(|d| d.weights).unwrap_or_default(),
            };
            Ok(VerifyOutcome {
                manifest,
                settling_task: Some(settling_typed),
            })
        } else {
            let wm = WaitMetadata::new(
                "risk_committee_lead",
                "2099-12-31T23:59:59Z",
                "manual_review_required",
            )?;
            task.transition(TaskState::HandedOff(wm))?;
            store
                .save_task_optimistic(
                    &mut task,
                    &format!("verification_failed:{}", gaps.join(",")),
                    "verifier",
                )
                .await?;
            let manifest = DeliveryManifest {
                task_id: task.task_id.clone(),
                final_state: task.state().clone(),
                completeness: if draft.is_some() {
                    Completeness::Partial
                } else {
                    Completeness::Blocked
                },
                criteria_results: criteria,
                assumptions: contract.map(|c| c.assumptions).unwrap_or_default(),
                gaps,
                total_spend: task.spend,
                next_owner: "risk_committee_lead".into(),
                draft_id: draft.as_ref().map(|d| d.draft_id.clone()),
                weights: draft.map(|d| d.weights).unwrap_or_default(),
            };
            Ok(VerifyOutcome {
                manifest,
                settling_task: None,
            })
        }
    }

    pub async fn confirm_settlement(
        &self,
        store: &DbStore,
        settling_task: TypedTask<PhaseSettling>,
        confirmer_id: &str,
        human_review_minutes: f64,
    ) -> Result<TypedTask<PhaseCompleted>, GateError> {
        let completed = settling_task.into_completed()?;
        let mut rec = completed.into_record();
        store
            .save_task_optimistic(&mut rec, &format!("settled_by:{confirmer_id}"), confirmer_id)
            .await?;
        store
            .record_cost(
                &rec.task_id,
                "human_settlement_review",
                UsdCents::ZERO,
                human_review_minutes,
                confirmer_id,
            )
            .await?;
        store
            .add_curated_memory(
                &format!("MEM-{}", rec.task_id),
                &rec.task_id,
                &format!(
                    "Settled {} v{} in {} turns",
                    rec.task_type, rec.config_version, rec.turns_used
                ),
            )
            .await?;

        // Reconstruct completed handle with updated DB version
        let settling_tmp = TypedTask::<PhaseFramed>::from_verifying(TaskRecord::rehydrate_from_db(
            rec.task_id.clone(),
            rec.idempotency_trigger_key.clone(),
            rec.task_type.clone(),
            rec.config_version.clone(),
            rec.requester_id.clone(),
            rec.acting_principal.clone(),
            rec.trigger_source.clone(),
            rec.authorized_markets.clone(),
            TaskState::Verifying,
            rec.version(),
            rec.turns_used,
            rec.spend,
            rec.retries_used,
            rec.consecutive_denials,
            rec.reworks_used,
            rec.reopened_count,
        )?)?
        .into_settling(WaitMetadata::new("done", "2099-12-31T23:59:59Z", "none")?)?;
        Ok(settling_tmp.into_completed()?)
    }

    pub async fn reopen_task(
        &self,
        store: &DbStore,
        task_id: &str,
        reason: &str,
        actor: &str,
    ) -> Result<TaskRecord, GateError> {
        let mut task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;
        task.transition(TaskState::Running)?;
        store
            .save_task_optimistic(&mut task, &format!("reopened:{reason}"), actor)
            .await?;
        Ok(task)
    }

    pub async fn create_handoff_package(
        &self,
        store: &DbStore,
        task_id: &str,
        original_request: &str,
        trigger_reason: &str,
        partial_weights: BTreeMap<String, WeightBps>,
        open_questions: Vec<String>,
    ) -> Result<HandoffPackage, GateError> {
        let task = store
            .get_task(task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{task_id}")))?;
        let actions = store.list_task_actions(task_id).await?;
        let completed_actions = actions
            .iter()
            .filter(|a| a.readback_verified)
            .map(|a| format!("{}:{}:{}", a.tool_name, a.idempotency_key, a.status.as_str()))
            .collect();
        let pending_actions = actions
            .iter()
            .filter(|a| !a.readback_verified)
            .map(|a| format!("{}:{}:{}", a.tool_name, a.idempotency_key, a.status.as_str()))
            .collect();

        Ok(HandoffPackage {
            task_id: task.task_id,
            requester_id: task.requester_id,
            original_request: original_request.into(),
            trigger_reason: trigger_reason.into(),
            completed_actions,
            pending_actions,
            partial_weights,
            open_questions,
            assigned_human_owner: None,
            accepted_by_human: false,
        })
    }

    pub async fn confirm_handoff(
        &self,
        store: &DbStore,
        mut pkg: HandoffPackage,
        human_owner_id: &str,
    ) -> Result<HandoffPackage, GateError> {
        let mut task = store
            .get_task(&pkg.task_id)
            .await?
            .ok_or_else(|| GateError::Storage(format!("unknown_task:{}", pkg.task_id)))?;
        pkg.assigned_human_owner = Some(human_owner_id.into());
        pkg.accepted_by_human = true;
        let wm = WaitMetadata::new(
            human_owner_id,
            "2099-12-31T23:59:59Z",
            "human_owner_follow_up",
        )?;
        if !matches!(task.state(), TaskState::HandedOff(_)) {
            task.transition(TaskState::HandedOff(wm))?;
        }
        store
            .save_task_optimistic(
                &mut task,
                &format!("handoff_accepted:{human_owner_id}"),
                human_owner_id,
            )
            .await?;
        Ok(pkg)
    }
}
