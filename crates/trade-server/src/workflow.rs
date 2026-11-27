//! The use cases of Diagram 0, one function per step.
//!
//! Every use case reads what it needs from the store, decides, and writes back guarded by
//! the task's version. Nothing keeps a task in memory between steps, so any step can be
//! retried after a crash or a conflict. `advance` drives a task as far as it can go without
//! a person: research, the gated draft, the gated orders, then verification and delivery.

use std::collections::{BTreeMap, BTreeSet};

use trade_adapters::{FixtureMarket, PaperBroker};
use trade_core::{
    ActionIntentRecord, ActionIntentStatus, AgentEvidenceOutput, ApprovalId, ApprovalRecord,
    ApprovalStatus, AsOfDate, Completeness, DeliveryManifest, GateError, HandoffPackage,
    IdempotencyKey, PitMarketSource, PitSecuritySnapshot, PortfolioActionPayload, PreDispatchError,
    RiskTier, TaskContract, TaskId, TaskRecord, TaskState, TaskTypeConfig, TimeoutAction,
    TransitionError, UsdCents, WaitMetadata, sha256_hex,
};
use trade_gate::{DispatchResult, GateOutcome, GateRequest, run};
use trade_quant::{QuantSecurityResult, score_pit_universe};
use trade_research::{
    FootnoteForensicsWorker, ManagementCapAllocWorker, MoatWorker, TargetPortfolio,
};
use trade_store::{CostEntry, MemoryItem, Store};

use crate::agent::AgentModel;
use crate::config::Strategy;
use crate::tools::{
    CreateDraft, DRAFT_TOOL, ORDER_TOOL, READ_FILINGS, READ_MARKET, ReadPit, SubmitOrders,
    limit_breaches,
};

/// The task type this harness runs.
pub const REBALANCE: &str = "value_committee_rebalance";

/// The done criteria the verifier knows how to check. Any other name fails closed.
const CITATIONS: &str = "citations_pit_valid";
const HOLDINGS: &str = "no_vetoed_holdings";
const CONSTRAINTS: &str = "portfolio_constraints_valid";
const ACTIONS: &str = "executed_actions_readback_verified";

/// `advance` stops after this many rounds; every round either changes the task or returns.
const MAX_ROUNDS: usize = 64;

/// A trigger arriving at ADMIT.
#[derive(Debug, Clone)]
pub struct Intake {
    /// Deduplicates triggers: the same key always yields the same task.
    pub trigger_key: String,
    pub requester_id: String,
    pub acting_principal: String,
    pub trigger_source: String,
    pub markets: Vec<String>,
}

/// What the requester asked for, before FRAME checks it.
#[derive(Debug, Clone)]
pub struct FrameRequest {
    pub goal: String,
    pub as_of_date: Option<String>,
    pub markets: Vec<String>,
    pub portfolio_capital: Option<UsdCents>,
    pub max_positions: usize,
    /// May the task finish without placing orders if their approval is refused?
    pub orders_optional: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Framing {
    Ready(TaskContract),
    /// The request was ambiguous; the requester was asked these questions.
    NeedsInput(Vec<String>),
}

/// Where `advance` stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    NeedsApproval {
        approval_id: ApprovalId,
        tool: String,
    },
    /// The gate refused the proposed action. The reason is data for the next proposal.
    Denied(String),
    /// Verified and delivered; SETTLING until confirmed or the settlement window closes.
    Delivered(DeliveryManifest),
    /// A person owns the task now.
    HandedOff(String),
    /// Nothing to do until someone acts; the task is in this state.
    Waiting(&'static str),
}

/// Result of checking the done criteria against stored facts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub criteria: BTreeMap<String, bool>,
    pub gaps: Vec<String>,
}

impl Verification {
    pub fn passed(&self) -> bool {
        self.criteria.values().all(|ok| *ok)
    }

    fn fail(&mut self, criterion: &str, gap: impl Into<String>) {
        self.criteria.insert(criterion.into(), false);
        self.gaps.push(gap.into());
    }
}

pub struct Harness {
    pub store: Store,
    pub market: FixtureMarket,
    pub broker: PaperBroker,
    pub strategy: Strategy,
    /// The task type version new tasks start with.
    pub task_type: TaskTypeConfig,
    /// The broker credential, held here and never given to workers or models.
    pub broker_api_key: Option<String>,
    /// The model that proposes each action. Without one, ACT follows fixed rules.
    pub agent: Option<AgentModel>,
}

fn not_found(what: &str, id: &str) -> GateError {
    GateError::Storage(format!("{what}_not_found:{id}"))
}

fn not_now(task: &TaskRecord, to: &'static str) -> GateError {
    TransitionError {
        from: task.state().code(),
        to,
    }
    .into()
}

/// The paper broker's credential in demos and tests. Paper money only.
pub const PAPER_BROKER_KEY: &str = "paper-broker-demo-key";

impl Harness {
    /// Everything from the repository layout: `config/strategy.toml`,
    /// `config/task_types/*.toml`, `fixtures/market.json`, and the database at `db_url`.
    /// Then settle whatever a previous run left in flight.
    pub async fn open(root: &std::path::Path, db_url: &str) -> Result<Self, String> {
        let strategy = Strategy::load(root.join("config/strategy.toml"))?;
        let task_types = crate::config::load_task_types(root.join("config/task_types"))?;
        let market = FixtureMarket::load(root.join("fixtures/market.json"))?;
        let store = Store::connect(db_url).await.map_err(|e| e.to_string())?;
        let broker = PaperBroker::new(PAPER_BROKER_KEY);
        let key = Some(PAPER_BROKER_KEY.to_string());
        let harness = Self::new(store, market, broker, key, strategy, task_types)
            .await
            .map_err(|e| e.to_string())?;
        harness.recover().await.map_err(|e| e.to_string())?;
        Ok(harness)
    }

    /// Register the task types and seed the agents' starting calibration if the store has
    /// none yet.
    pub async fn new(
        store: Store,
        market: FixtureMarket,
        broker: PaperBroker,
        broker_api_key: Option<String>,
        strategy: Strategy,
        task_types: Vec<TaskTypeConfig>,
    ) -> Result<Self, GateError> {
        for config in &task_types {
            store.put_task_type(config).await?;
        }
        let task_type = task_types
            .into_iter()
            .find(|t| t.task_type == REBALANCE)
            .ok_or_else(|| not_found("task_type", REBALANCE))?;
        if store.calibrations().await?.is_empty() {
            for (agent, confidence) in market.seed_calibrations() {
                store.set_calibration(agent, *confidence, 0.0).await?;
            }
        }
        Ok(Self {
            store,
            market,
            broker,
            strategy,
            task_type,
            broker_api_key,
            agent: None,
        })
    }

    pub fn draft_tool(&self) -> CreateDraft {
        CreateDraft {
            store: self.store.clone(),
            market: self.market.clone(),
            committee: self.strategy.committee.clone(),
        }
    }

    pub fn read_tool(&self, name: &'static str) -> ReadPit {
        ReadPit {
            name,
            store: self.store.clone(),
            market: self.market.clone(),
        }
    }

    pub fn order_tool(&self) -> SubmitOrders {
        SubmitOrders {
            store: self.store.clone(),
            market: self.market.clone(),
            committee: self.strategy.committee.clone(),
            broker: self.broker.clone(),
            api_key: self.broker_api_key.clone(),
        }
    }

    pub async fn task(&self, task_id: &TaskId) -> Result<TaskRecord, GateError> {
        self.store
            .task(task_id)
            .await?
            .ok_or_else(|| not_found("task", task_id))
    }

    /// Let `agent` propose every action from now on.
    pub fn with_agent(mut self, agent: AgentModel) -> Self {
        self.agent = Some(agent);
        self
    }

    pub(crate) async fn contract(&self, task_id: &TaskId) -> Result<TaskContract, GateError> {
        self.store
            .contract(task_id)
            .await?
            .ok_or_else(|| not_found("contract", task_id))
    }

    pub(crate) async fn config_of(&self, task: &TaskRecord) -> Result<TaskTypeConfig, GateError> {
        self.store
            .task_type(&task.task_type, &task.config_version)
            .await?
            .ok_or_else(|| not_found("task_type", &task.config_version))
    }

    // ---- ADMIT and FRAME -------------------------------------------------------------

    /// ADMIT: create the task, or return the one this trigger already created.
    pub async fn admit(&self, intake: &Intake) -> Result<(TaskRecord, bool), GateError> {
        let id = format!("TASK-{}", &sha256_hex(intake.trigger_key.as_bytes())[..10]);
        let task = TaskRecord::new_queued(
            TaskId::new(id),
            &intake.trigger_key,
            &self.task_type.task_type,
            &self.task_type.version,
            &intake.requester_id,
            &intake.acting_principal,
            &intake.trigger_source,
            intake.markets.clone(),
        )?;
        self.store.create_task(&task, &intake.trigger_source).await
    }

    /// FRAME: pin the task contract, or ask the requester when the request is ambiguous.
    /// Also takes the answer to an earlier question.
    pub async fn frame(&self, task_id: &TaskId, req: &FrameRequest) -> Result<Framing, GateError> {
        let mut task = self.task(task_id).await?;
        if !matches!(
            task.state(),
            TaskState::Queued | TaskState::AwaitingInput(_)
        ) {
            return Err(not_now(&task, "RUNNING"));
        }
        let config = self.config_of(&task).await?;
        let now = self.store.now();
        let mut questions = Vec::new();
        let as_of = match req.as_of_date.as_deref().map(str::trim) {
            None | Some("") => {
                questions.push("missing_as_of_date");
                None
            }
            Some(raw) => AsOfDate::new(raw)
                .inspect_err(|_| questions.push("invalid_as_of_date"))
                .ok(),
        };
        if req.portfolio_capital.is_none_or(|c| c.cents() <= 0) {
            questions.push("missing_or_non_positive_capital");
        }
        if req.markets.is_empty() {
            questions.push("empty_universe_markets");
        } else if req
            .markets
            .iter()
            .any(|m| !task.authorized_markets.contains(m))
        {
            questions.push("market_outside_authorized_scope");
        }
        if req.max_positions == 0 {
            questions.push("max_positions_must_be_positive");
        }

        task.transition(TaskState::Running, now)?;
        let (Some(as_of_date), Some(portfolio_capital), true) =
            (as_of, req.portfolio_capital, questions.is_empty())
        else {
            let deadline = now + config.waits.input_ttl_s;
            let wait = WaitMetadata::new(&task.requester_id, deadline, TimeoutAction::HandOff)?;
            task.transition(TaskState::AwaitingInput(wait), now)?;
            let reason = format!("needs_input:{}", questions.join(","));
            self.store.save(&mut task, &reason, "frame").await?;
            return Ok(Framing::NeedsInput(
                questions.iter().map(|q| q.to_string()).collect(),
            ));
        };
        let contract = TaskContract {
            task_id: task.task_id.clone(),
            goal: req.goal.clone(),
            as_of_date,
            universe_markets: req.markets.clone(),
            portfolio_capital,
            max_positions: req.max_positions,
            expected_outputs: vec![
                "quant_ranking".into(),
                "committee_decisions".into(),
                "rebalance_draft".into(),
                "delivery_manifest".into(),
            ],
            done_checks: config.done_criteria.clone(),
            assumptions: vec![
                "accounting_lag_enforced_by_filing_date".into(),
                "point_in_time_universe_without_survivor_bias".into(),
            ],
            irreversible_steps_deferred: req.orders_optional,
        };
        self.store.save_contract(&contract).await?;
        self.store
            .save(&mut task, "contract_pinned", "frame")
            .await?;
        Ok(Framing::Ready(contract))
    }

    // ---- The execution loop ------------------------------------------------------------

    /// Drive the task until it needs a person or is delivered. Each round reads the task
    /// again, so this is safe to call at any time, any number of times.
    pub async fn advance(&self, task_id: &TaskId) -> Result<Step, GateError> {
        for _ in 0..MAX_ROUNDS {
            let task = self.task(task_id).await?;
            let step = match task.state() {
                TaskState::Running => match &self.agent {
                    Some(agent) => self.act_with_model(task, agent).await?,
                    None => self.act(task).await?,
                },
                TaskState::LimitReached { .. } => self.close(task).await?,
                TaskState::OutcomeUnknown { .. } => {
                    if self.reconcile(task_id).await? {
                        None
                    } else {
                        Some(
                            self.hand_off(task_id, "outcome_unknown_after_reconciliation")
                                .await?,
                        )
                    }
                }
                other => Some(Step::Waiting(other.code())),
            };
            if let Some(step) = step {
                return Ok(step);
            }
        }
        Err(GateError::Conflict(format!(
            "task {task_id} did not settle"
        )))
    }

    /// One round of ACT: research once, then the draft, then the orders, then CLOSE.
    /// `None` means something changed and the loop should look again.
    async fn act(&self, mut task: TaskRecord) -> Result<Option<Step>, GateError> {
        let contract = self.contract(&task.task_id).await?;
        if self.store.evaluations(&task.task_id).await?.is_empty() {
            self.research(&mut task, &contract).await?;
            return Ok(None);
        }
        let actions = self.store.actions(&task.task_id).await?;
        let done = |tool: &str| {
            actions
                .iter()
                .any(|a| a.tool_name == tool && a.status == ActionIntentStatus::Verified)
        };
        let approvals = self.store.task_approvals(&task.task_id).await?;
        let approved = |tool: &str| {
            approvals
                .iter()
                .find(|a| a.tool_name == tool && a.status == ApprovalStatus::Approved)
                .map(|a| a.approval_id.clone())
        };
        let as_of = contract.as_of_date.as_str();

        if !done(DRAFT_TOOL) {
            let request = GateRequest {
                task_id: task.task_id.clone(),
                target: format!("portfolio:{as_of}"),
                args: self.proposal(&task.task_id).await?.0,
                idempotency_key: IdempotencyKey::new(format!("draft:{}:{as_of}", task.task_id)),
                approval_id: approved(DRAFT_TOOL),
            };
            return gated(
                run(&self.store, &self.draft_tool(), &request).await,
                DRAFT_TOOL,
            );
        }
        if !done(ORDER_TOOL) && !refused(&approvals, ORDER_TOOL) {
            let draft = self
                .store
                .latest_draft(&task.task_id)
                .await?
                .ok_or_else(|| not_found("draft", &task.task_id))?;
            let request = GateRequest {
                task_id: task.task_id.clone(),
                target: format!("paper_broker:{as_of}"),
                args: PortfolioActionPayload {
                    draft_id: draft.draft_id,
                    as_of_date: contract.as_of_date.clone(),
                    weights_bps: draft.weights,
                    markets: contract.universe_markets.clone(),
                    portfolio_capital_cents: Some(contract.portfolio_capital),
                },
                idempotency_key: IdempotencyKey::new(format!("order:{}:{as_of}", task.task_id)),
                approval_id: approved(ORDER_TOOL),
            };
            let result = run(&self.store, &self.order_tool(), &request).await;
            if let Ok(GateOutcome::Executed {
                receipt_id,
                replayed: false,
                ..
            }) = &result
            {
                let cost = CostEntry {
                    stage: "ACT".into(),
                    category: "broker".into(),
                    amount: UsdCents::from_cents(1),
                    human_minutes: 0.0,
                    note: receipt_id.clone(),
                };
                self.store.record_cost(&task.task_id, &cost).await?;
            }
            return gated(result, ORDER_TOOL);
        }
        self.close(task).await
    }

    /// The quant screen on the contract's date: what was public then, scored. Pure and
    /// free, so it is recomputed rather than stored.
    pub(crate) fn screen(
        &self,
        contract: &TaskContract,
    ) -> Result<(Vec<PitSecuritySnapshot>, Vec<QuantSecurityResult>), GateError> {
        let snapshots = self
            .market
            .get_pit_snapshot(&contract.as_of_date, &contract.universe_markets)
            .map_err(GateError::Storage)?;
        let quant = score_pit_universe(&snapshots, &contract.as_of_date, &self.strategy.factors);
        Ok((snapshots, quant))
    }

    /// RETRIEVE, DELEGATE and the committee: the specialists examine the best candidates
    /// (plus the audited ones) through a read-only view, and the committee's results are
    /// stored before any write tool may run.
    pub(crate) async fn research(
        &self,
        task: &mut TaskRecord,
        contract: &TaskContract,
    ) -> Result<(), GateError> {
        let params = &self.strategy.research;
        let (snapshots, quant) = self.screen(contract)?;
        let mut examine: BTreeSet<&str> = quant
            .iter()
            .filter(|q| !q.hard_veto)
            .take(params.max_candidates)
            .map(|q| q.ticker.as_str())
            .collect();
        examine.extend(
            params
                .audit_tickers
                .iter()
                .map(String::as_str)
                .filter(|t| quant.iter().any(|q| q.ticker == *t)),
        );
        let snapshot_of: BTreeMap<&str, &PitSecuritySnapshot> =
            snapshots.iter().map(|s| (s.ticker.as_str(), s)).collect();
        let view = self.market.view(
            task.authorized_markets.clone(),
            self.store.calibrations().await?,
        );
        let as_of = &contract.as_of_date;

        let mut decisions = Vec::with_capacity(quant.len());
        for q in &quant {
            let mut outputs: Vec<AgentEvidenceOutput> = Vec::new();
            if examine.contains(q.ticker.as_str()) {
                let snap = snapshot_of[q.ticker.as_str()];
                outputs.push(MoatWorker.evaluate(&view, snap, q, as_of, None)?);
                outputs.push(ManagementCapAllocWorker.evaluate(&view, snap, q, as_of, None)?);
                outputs.push(FootnoteForensicsWorker.evaluate(&view, snap, q, as_of, None)?);
                let cost = CostEntry {
                    stage: "FRAME".into(),
                    category: "specialists".into(),
                    amount: params.worker_cost,
                    human_minutes: 0.0,
                    note: q.ticker.clone(),
                };
                self.store.record_cost(&task.task_id, &cost).await?;
            }
            decisions.push(self.strategy.committee.evaluate_security(q, outputs, &view));
        }
        self.store
            .save_evaluations(&task.task_id, &decisions)
            .await?;
        // Recording costs updated the task; read it again before counting the turns.
        *task = self.task(&task.task_id).await?;
        task.turns_used += 3 * examine.len() as u32;
        self.store.save(task, "committee_scored", "research").await
    }

    /// The committee's portfolio from the stored evaluations, and the draft payload for it.
    pub async fn proposal(
        &self,
        task_id: &TaskId,
    ) -> Result<(PortfolioActionPayload, TargetPortfolio), GateError> {
        let contract = self.contract(task_id).await?;
        let (_, quant) = self.screen(&contract)?;
        let quant_by_ticker: BTreeMap<String, QuantSecurityResult> =
            quant.into_iter().map(|q| (q.ticker.clone(), q)).collect();
        let decisions = self.store.evaluations(task_id).await?;
        let portfolio = self.strategy.committee.build_target_weights(
            &decisions,
            &quant_by_ticker,
            contract.portfolio_capital,
            contract.max_positions,
        );
        let payload = PortfolioActionPayload {
            draft_id: format!("DRAFT-{task_id}"),
            as_of_date: contract.as_of_date,
            weights_bps: portfolio.weights.clone(),
            markets: contract.universe_markets,
            portfolio_capital_cents: None,
        };
        Ok((payload, portfolio))
    }

    // ---- CLOSE -------------------------------------------------------------------------

    /// VERIFY from stored facts only, then DELIVER, rework, or hand off. From
    /// LIMIT_REACHED, what was finished is verified and delivered as partial. `None` means
    /// rework: the loop goes round again.
    pub(crate) async fn close(&self, mut task: TaskRecord) -> Result<Option<Step>, GateError> {
        let from_limit = match task.state() {
            TaskState::LimitReached { reason } => Some(reason.clone()),
            _ => None,
        };
        task.transition(TaskState::Verifying, self.store.now())?;
        self.store.save(&mut task, "verifying", "close").await?;
        let cost = CostEntry {
            stage: "CLOSE".into(),
            category: "verifier".into(),
            amount: UsdCents::from_cents(1),
            human_minutes: 0.0,
            note: "independent_verification".into(),
        };
        self.store.record_cost(&task.task_id, &cost).await?;

        let mut task = self.task(&task.task_id).await?;
        let contract = self.contract(&task.task_id).await?;
        let config = self.config_of(&task).await?;
        let mut check = self.verify(&task, &contract, &config).await?;
        let now = self.store.now();

        if !check.passed() {
            // Rework while there is budget for it; otherwise a person takes over.
            if from_limit.is_none() && task.reworks_used < config.budget_limits.max_reworks {
                task.transition(TaskState::Running, now)?;
                let reason = format!("rework:{}", check.gaps.join(","));
                self.store.save(&mut task, &reason, "close").await?;
                return Ok(None);
            }
            let reason = format!("verification_failed:{}", check.gaps.join(","));
            return self.hand_off(&task.task_id, &reason).await.map(Some);
        }

        let actions = self.store.actions(&task.task_id).await?;
        let orders_placed = actions
            .iter()
            .any(|a| a.tool_name == ORDER_TOOL && a.status == ActionIntentStatus::Verified);
        if !orders_placed {
            let approvals = self.store.task_approvals(&task.task_id).await?;
            check.gaps.push(if refused(&approvals, ORDER_TOOL) {
                "orders_not_placed:approval_refused".into()
            } else {
                "orders_not_placed".into()
            });
        }
        if let Some(limit) = &from_limit {
            check.gaps.push(format!("stopped_by_limit:{limit}"));
        }
        let completeness = if orders_placed && from_limit.is_none() {
            Completeness::Complete
        } else {
            Completeness::Partial
        };
        let draft = self.store.latest_draft(&task.task_id).await?;
        let wait = WaitMetadata::new(
            &task.requester_id,
            now + config.waits.settle_window_s,
            TimeoutAction::Complete,
        )?;
        task.transition(TaskState::Settling(wait), now)?;
        let manifest = DeliveryManifest {
            task_id: task.task_id.clone(),
            final_state: task.state().clone(),
            completeness,
            criteria_results: check.criteria,
            assumptions: contract.assumptions,
            gaps: check.gaps,
            total_spend: task.spend,
            next_owner: task.requester_id.clone(),
            draft_id: draft.as_ref().map(|d| d.draft_id.clone()),
            weights: draft.map(|d| d.weights).unwrap_or_default(),
        };
        self.store.deliver(&manifest, &mut task).await?;
        Ok(Some(Step::Delivered(manifest)))
    }

    /// The done criteria, checked against the store, the market data and the broker only;
    /// nothing comes from the workers' or the proposer's say-so. An empty result fails.
    pub async fn verify(
        &self,
        task: &TaskRecord,
        contract: &TaskContract,
        config: &TaskTypeConfig,
    ) -> Result<Verification, GateError> {
        let mut check = Verification {
            criteria: BTreeMap::new(),
            gaps: Vec::new(),
        };
        for criterion in &config.done_criteria {
            check.criteria.insert(criterion.clone(), true);
            if ![CITATIONS, HOLDINGS, CONSTRAINTS, ACTIONS].contains(&criterion.as_str()) {
                check.fail(criterion, format!("unknown_criterion:{criterion}"));
            }
        }
        let as_of = &contract.as_of_date;
        let decisions = self.store.evaluations(&task.task_id).await?;

        // Every record a specialist cites existed and was public on the as-of date.
        let outputs: Vec<&AgentEvidenceOutput> =
            decisions.iter().flat_map(|d| &d.worker_outputs).collect();
        if outputs.is_empty() {
            check.fail(CITATIONS, "no_specialist_evidence");
        }
        for out in &outputs {
            for record in &out.cited_record_ids {
                if !self.market.record_public(record, as_of) {
                    check.fail(
                        CITATIONS,
                        format!("invalid_or_future_citation:{}:{record}", out.ticker),
                    );
                }
            }
        }

        // The draft holds only evaluated, unvetoed securities, within the portfolio limits.
        match self.store.latest_draft(&task.task_id).await? {
            None => {
                check.fail(HOLDINGS, "missing_rebalance_draft");
                check.fail(CONSTRAINTS, "missing_rebalance_draft");
            }
            Some(draft) => {
                let vetoed: BTreeMap<&str, bool> = decisions
                    .iter()
                    .map(|d| (d.ticker.as_str(), d.hard_veto))
                    .collect();
                for ticker in draft.weights.keys() {
                    match vetoed.get(ticker.as_str()) {
                        None => {
                            check.fail(HOLDINGS, format!("unevaluated_ticker_in_draft:{ticker}"))
                        }
                        Some(true) => {
                            check.fail(HOLDINGS, format!("vetoed_ticker_in_draft:{ticker}"))
                        }
                        Some(false) => {}
                    }
                }
                let (universe, _) = self.screen(contract)?;
                let committee = &self.strategy.committee;
                for breach in limit_breaches(&draft.weights, &universe, committee, contract) {
                    check.fail(CONSTRAINTS, breach);
                }
            }
        }

        // Every action was verified, and the broker still shows exactly those orders.
        let actions = self.store.actions(&task.task_id).await?;
        if actions.is_empty() {
            check.fail(ACTIONS, "no_actions_recorded");
        }
        for action in &actions {
            let key = &action.idempotency_key;
            if action.status != ActionIntentStatus::Verified || !action.readback_verified {
                check.fail(
                    ACTIONS,
                    format!("unverified_action:{key}:{}", action.status.as_str()),
                );
            } else if action.tool_name == ORDER_TOOL {
                let seen = self.order_tool().reconcile(action).await?;
                let expected = action.backend_receipt_id.clone().unwrap_or_default();
                if seen
                    != (DispatchResult::Verified {
                        receipt_id: expected,
                    })
                {
                    check.fail(ACTIONS, format!("broker_readback_mismatch:{key}"));
                }
            }
        }
        Ok(check)
    }

    // ---- People: approvals, settlement, hand-offs ----------------------------------------

    /// APPROVE (Diagram 3b). Approval sends the task back to work, where the gate checks
    /// and uses it. A rejection does the same if the step may be skipped (an irreversible
    /// step the contract lets a person do instead); otherwise the task ends as DECLINED.
    pub async fn decide_approval(
        &self,
        approval_id: &ApprovalId,
        approver_id: &str,
        approve: bool,
        note: Option<&str>,
    ) -> Result<TaskRecord, GateError> {
        let approval = self
            .store
            .approval(approval_id)
            .await?
            .ok_or_else(|| PreDispatchError::ApprovalNotFound(approval_id.to_string()))?;
        let mut task = self.task(&approval.task_id).await?;
        let config = self.config_of(&task).await?;
        if approver_id == approval.requester_id || approver_id == config.config_admin_id {
            return Err(PreDispatchError::SeparationOfDutiesViolation(approver_id.into()).into());
        }
        if approval.status != ApprovalStatus::Pending {
            return Err(
                PreDispatchError::ApprovalNotApproved(approval.status.as_str().into()).into(),
            );
        }
        let expired = self.store.now() >= approval.expires_at_epoch_s;
        let status = match (approve, expired) {
            (_, true) => ApprovalStatus::Expired,
            (true, false) => ApprovalStatus::Approved,
            (false, false) => ApprovalStatus::Rejected,
        };
        self.settle_approval(
            &mut task,
            &approval,
            status,
            Some(approver_id),
            note,
            &config,
        )
        .await?;
        if expired {
            return Err(PreDispatchError::ApprovalExpired(approval_id.to_string()).into());
        }
        Ok(task)
    }

    async fn settle_approval(
        &self,
        task: &mut TaskRecord,
        approval: &ApprovalRecord,
        status: ApprovalStatus,
        approver_id: Option<&str>,
        note: Option<&str>,
        config: &TaskTypeConfig,
    ) -> Result<(), GateError> {
        let skippable = config.lookup_tool_risk(&approval.tool_name) == Some(RiskTier::R3)
            && self
                .contract(&task.task_id)
                .await?
                .irreversible_steps_deferred;
        let next = if status == ApprovalStatus::Approved || skippable {
            TaskState::Running
        } else {
            TaskState::Declined {
                reason: format!(
                    "approval_{}:{}",
                    status.as_str().to_lowercase(),
                    approval.approval_id
                ),
            }
        };
        task.transition(next, self.store.now())?;
        self.store
            .decide_approval(&approval.approval_id, status, approver_id, note, task)
            .await
    }

    /// SETTLE: the requester accepts the delivery.
    pub async fn confirm_settlement(
        &self,
        task_id: &TaskId,
        confirmer_id: &str,
        review_minutes: f64,
    ) -> Result<TaskRecord, GateError> {
        let mut task = self.task(task_id).await?;
        task.transition(TaskState::Completed, self.store.now())?;
        self.store
            .save(
                &mut task,
                &format!("settled_by:{confirmer_id}"),
                confirmer_id,
            )
            .await?;
        if review_minutes > 0.0 {
            let cost = CostEntry {
                stage: "CLOSE".into(),
                category: "human_review".into(),
                amount: UsdCents::ZERO,
                human_minutes: review_minutes,
                note: confirmer_id.into(),
            };
            self.store.record_cost(task_id, &cost).await?;
        }
        // ORGANIZE: a lesson for a person to review before it is reused.
        let lesson = MemoryItem {
            memory_id: format!("MEM-{task_id}-{}", task.reopened_count),
            source_task_id: task_id.to_string(),
            summary: format!(
                "Settled {} v{} in {} turns, {} reworks, ${:.2} spent",
                task.task_type,
                task.config_version,
                task.turns_used,
                task.reworks_used,
                task.spend.to_dollars()
            ),
            status: "pending_review".into(),
            reviewed_by: None,
        };
        self.store.add_memory(&lesson).await?;
        self.task(task_id).await
    }

    /// The requester disputes the delivery during the settlement window: rework.
    pub async fn dispute(
        &self,
        task_id: &TaskId,
        reason: &str,
        by: &str,
    ) -> Result<TaskRecord, GateError> {
        self.move_to(
            task_id,
            TaskState::Running,
            &format!("disputed:{reason}"),
            by,
            |s| matches!(s, TaskState::Settling(_)),
        )
        .await
    }

    /// A later check finds a completed task's outcome wrong.
    pub async fn reopen(
        &self,
        task_id: &TaskId,
        reason: &str,
        by: &str,
    ) -> Result<TaskRecord, GateError> {
        self.move_to(
            task_id,
            TaskState::Running,
            &format!("reopened:{reason}"),
            by,
            |s| matches!(s, TaskState::Completed),
        )
        .await
    }

    /// A person answered and hands the task back to the agent.
    pub async fn hand_back(&self, task_id: &TaskId, by: &str) -> Result<TaskRecord, GateError> {
        self.move_to(task_id, TaskState::Running, "handed_back", by, |s| {
            matches!(s, TaskState::HandedOff(_))
        })
        .await
    }

    pub async fn cancel(
        &self,
        task_id: &TaskId,
        reason: &str,
        by: &str,
    ) -> Result<TaskRecord, GateError> {
        let state = TaskState::Cancelled {
            reason: reason.into(),
        };
        self.move_to(task_id, state, "cancelled", by, |_| true)
            .await
    }

    async fn move_to(
        &self,
        task_id: &TaskId,
        next: TaskState,
        reason: &str,
        by: &str,
        allowed_from: impl Fn(&TaskState) -> bool,
    ) -> Result<TaskRecord, GateError> {
        let mut task = self.task(task_id).await?;
        if !allowed_from(task.state()) {
            return Err(not_now(&task, next.code()));
        }
        task.transition(next, self.store.now())?;
        self.store.save(&mut task, reason, by).await?;
        Ok(task)
    }

    /// ESCALATE: give the task to a person, with what was done and what is open.
    pub async fn hand_off(&self, task_id: &TaskId, reason: &str) -> Result<Step, GateError> {
        let mut task = self.task(task_id).await?;
        let config = self.config_of(&task).await?;
        let owner = config.waits.escalation_owner_id.clone();
        let actions = self.store.actions(task_id).await?;
        let (done, open): (Vec<_>, Vec<_>) = actions
            .iter()
            .partition(|a| a.status == ActionIntentStatus::Verified);
        let describe = |a: &&ActionIntentRecord| {
            format!(
                "{}:{}:{}",
                a.tool_name,
                a.idempotency_key,
                a.status.as_str()
            )
        };
        let package = HandoffPackage {
            task_id: task.task_id.clone(),
            requester_id: task.requester_id.clone(),
            original_request: self
                .store
                .contract(task_id)
                .await?
                .map(|c| c.goal)
                .unwrap_or_default(),
            trigger_reason: reason.into(),
            completed_actions: done.iter().map(describe).collect(),
            pending_actions: open.iter().map(describe).collect(),
            partial_weights: self
                .store
                .latest_draft(task_id)
                .await?
                .map(|d| d.weights)
                .unwrap_or_default(),
            open_questions: vec![reason.into()],
            assigned_human_owner: Some(owner.clone()),
            accepted_by_human: false,
        };
        let now = self.store.now();
        let wait = WaitMetadata::new(
            &owner,
            now + config.waits.handoff_ack_ttl_s,
            TimeoutAction::Escalate,
        )?;
        task.transition(TaskState::HandedOff(wait), now)?;
        self.store.hand_off(&package, &mut task).await?;
        Ok(Step::HandedOff(owner))
    }

    // ---- Deadlines and recovery ----------------------------------------------------------

    /// Act on every wait whose deadline has passed. A timeout is never a silent approval:
    /// an unanswered question goes to a person, an unanswered approval counts as refused,
    /// an undisputed delivery completes, and an unclaimed hand-off goes to the next owner.
    /// Returns the tasks it handled.
    pub async fn sweep(&self) -> Result<Vec<TaskId>, GateError> {
        let due = self.store.due_waits(self.store.now()).await?;
        for task in &due {
            match self.expire(task.clone()).await {
                // Someone acted between the query and now; their action stands.
                Ok(()) | Err(GateError::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(due.into_iter().map(|t| t.task_id).collect())
    }

    async fn expire(&self, mut task: TaskRecord) -> Result<(), GateError> {
        let config = self.config_of(&task).await?;
        let now = self.store.now();
        match task.state().clone() {
            TaskState::AwaitingInput(_) => {
                self.hand_off(&task.task_id, "no_answer_before_deadline")
                    .await?;
            }
            TaskState::AwaitingApproval(_) => {
                let pending = self
                    .store
                    .task_approvals(&task.task_id)
                    .await?
                    .into_iter()
                    .find(|a| a.status == ApprovalStatus::Pending);
                match pending {
                    Some(approval) => {
                        let note = Some("deadline_passed");
                        self.settle_approval(
                            &mut task,
                            &approval,
                            ApprovalStatus::Expired,
                            None,
                            note,
                            &config,
                        )
                        .await?;
                    }
                    None => {
                        self.hand_off(&task.task_id, "awaiting_unknown_approval")
                            .await?;
                    }
                }
            }
            TaskState::Settling(_) => {
                self.confirm_settlement(&task.task_id, "settlement_window", 0.0)
                    .await?;
            }
            TaskState::HandedOff(wait) => {
                let waits = &config.waits;
                // Unclaimed: first the backup owner, then keep reminding them.
                task.reassign_wait(&waits.backup_owner_id, now + waits.handoff_ack_ttl_s)?;
                let reason = format!("handoff_unclaimed_by:{}", wait.owner_id);
                self.store.save(&mut task, &reason, "sweeper").await?;
            }
            other => return Err(not_now(&task, other.code())),
        }
        Ok(())
    }

    /// Diagram 4b: settle the task's unsettled actions by reading the target systems.
    /// Returns `false` if one could not be settled, which needs a person.
    pub async fn reconcile(&self, task_id: &TaskId) -> Result<bool, GateError> {
        let unsettled: Vec<_> = self
            .store
            .unsettled_actions()
            .await?
            .into_iter()
            .filter(|a| &a.task_id == task_id)
            .collect();
        for action in &unsettled {
            let result = match action.tool_name.as_str() {
                DRAFT_TOOL => self.draft_tool().reconcile(action).await?,
                ORDER_TOOL => self.order_tool().reconcile(action).await?,
                // A read changed nothing outside; it only has to be marked as read.
                READ_MARKET | READ_FILINGS => DispatchResult::Verified {
                    receipt_id: action.idempotency_key.to_string(),
                },
                other => DispatchResult::Unknown {
                    receipt_id: None,
                    detail: format!("no_reconciler_for:{other}"),
                },
            };
            if matches!(result, DispatchResult::Unknown { .. }) {
                return Ok(false);
            }
            let mut task = self.task(task_id).await?;
            if matches!(task.state(), TaskState::OutcomeUnknown { .. }) {
                task.transition(TaskState::Running, self.store.now())?;
            }
            self.store
                .reconcile(&action.idempotency_key, &result, &mut task, "reconciler")
                .await?;
        }
        Ok(true)
    }

    /// At start-up: whatever was in flight when the process stopped is settled now, or
    /// handed to a person. Assumes this is the only process running tasks.
    pub async fn recover(&self) -> Result<Vec<TaskId>, GateError> {
        let tasks: BTreeSet<TaskId> = self
            .store
            .unsettled_actions()
            .await?
            .into_iter()
            .map(|a| a.task_id)
            .collect();
        for task_id in &tasks {
            if !self.reconcile(task_id).await? {
                self.hand_off(task_id, "outcome_unknown_at_restart").await?;
            }
        }
        Ok(tasks.into_iter().collect())
    }
}

fn refused(approvals: &[ApprovalRecord], tool: &str) -> bool {
    approvals.iter().any(|a| {
        a.tool_name == tool
            && matches!(a.status, ApprovalStatus::Rejected | ApprovalStatus::Expired)
    })
}

/// What a gate result means for the loop: carry on, stop for a person, or report a denial.
fn gated(result: Result<GateOutcome, GateError>, tool: &str) -> Result<Option<Step>, GateError> {
    match result {
        Ok(GateOutcome::Executed { .. }) => Ok(None),
        Ok(GateOutcome::NeedsApproval { approval_id, .. }) => Ok(Some(Step::NeedsApproval {
            approval_id,
            tool: tool.into(),
        })),
        // The gate moved the task to LIMIT_REACHED; the next round closes it.
        Err(GateError::PreDispatch(PreDispatchError::LimitReached(_))) => Ok(None),
        Err(GateError::PreDispatch(reason)) => Ok(Some(Step::Denied(reason.code()))),
        // Not sent: the next round retries, within the retry limit. Unknown outcome: the
        // next round reconciles. Conflict: the next round reads again.
        Err(
            GateError::NotSent(_) | GateError::PostDispatchUnknown { .. } | GateError::Conflict(_),
        ) => Ok(None),
        Err(other) => Err(other),
    }
}
