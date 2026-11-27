//! The governed tools: the point-in-time reads the model asks for (R0, R1), the rebalance
//! draft (R2) and paper orders (R3).
//!
//! Business rules are checked against facts the store and the market data hold (the stored
//! veto list, the contract, the stored draft, the as-of date's market data), never against
//! what the proposer sends. Each tool also knows how to read its target system back by
//! idempotency key, which is how recovery settles an action whose outcome was unknown.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use trade_adapters::{BrokerOrderReceipt, FixtureMarket, PaperBroker};
use trade_core::{
    ActionIntentRecord, ExecError, GateError, IdempotencyKey, PitMarketSource, PitSecuritySnapshot,
    PortfolioActionPayload, PreDispatchError, RiskTier, StoredDraft, TaskContract, TaskId,
    TaskRecord, UsdCents, WeightBps,
};
use trade_gate::{DispatchResult, Tool, VerifiedGateTicket};
use trade_research::InvestmentCommittee;
use trade_store::Store;

pub const DRAFT_TOOL: &str = "create_rebalance_draft";
pub const ORDER_TOOL: &str = "submit_paper_orders";
pub const READ_MARKET: &str = "read_pit_market";
pub const READ_FILINGS: &str = "read_pit_filings";

pub fn is_read(tool: &str) -> bool {
    tool == READ_MARKET || tool == READ_FILINGS
}

/// A backstop independent of the strategy settings: no single position above 45%.
const MAX_POSITION_BPS: u16 = 4_500;

/// What the portfolio rules are checked against.
#[derive(Debug, Clone)]
pub struct PortfolioFacts {
    pub evaluated: BTreeSet<String>,
    pub vetoed: BTreeSet<String>,
    pub contract: TaskContract,
    /// The market data of the contract's date, for the liquidity and industry limits.
    pub universe: Vec<PitSecuritySnapshot>,
    /// The stored draft the orders must match. Read only for orders.
    pub draft: Option<StoredDraft>,
}

/// Each way `weights` breaks the strategy's portfolio limits, given the market data of the
/// as-of date: number of positions, total, single position, industry, and the share of a
/// security's daily trading. Empty when it complies. The gate checks this before a draft
/// or order is written, and the verifier checks it again afterwards.
pub fn limit_breaches(
    weights: &BTreeMap<String, WeightBps>,
    universe: &[PitSecuritySnapshot],
    committee: &InvestmentCommittee,
    contract: &TaskContract,
) -> Vec<String> {
    let mut breaches = Vec::new();
    if weights.len() > contract.max_positions {
        breaches.push(format!(
            "too_many_positions:{}>{}",
            weights.len(),
            contract.max_positions
        ));
    }
    let total: u32 = weights.values().map(|w| u32::from(w.bps())).sum();
    if total > 10_000 {
        breaches.push("total_weight_exceeds_10000_bps".into());
    }
    let row: BTreeMap<&str, &PitSecuritySnapshot> =
        universe.iter().map(|s| (s.ticker.as_str(), s)).collect();
    let capital = contract.portfolio_capital.to_dollars().max(1.0);
    let mut by_industry: BTreeMap<&str, u32> = BTreeMap::new();
    for (ticker, weight) in weights {
        let Some(s) = row.get(ticker.as_str()) else {
            breaches.push(format!("not_in_universe:{ticker}"));
            continue;
        };
        if weight.bps() > committee.max_single_weight.bps() {
            breaches.push(format!("single_weight_exceeded:{ticker}"));
        }
        let adv_cap =
            WeightBps::from_ratio_clamped(committee.max_adv_participation * s.adv_usd / capital);
        // One basis point of slack for rounding.
        if weight.bps() > adv_cap.bps().saturating_add(1) {
            breaches.push(format!("adv_cap_exceeded:{ticker}"));
        }
        *by_industry.entry(s.industry.as_str()).or_default() += u32::from(weight.bps());
    }
    for (industry, bps) in by_industry {
        if bps > u32::from(committee.max_industry_weight.bps()) {
            breaches.push(format!("industry_weight_exceeded:{industry}"));
        }
    }
    breaches
}

async fn load_portfolio_facts(
    store: &Store,
    market: &FixtureMarket,
    task: &TaskRecord,
    draft_id: Option<&str>,
) -> Result<PortfolioFacts, GateError> {
    let decisions = store.evaluations(&task.task_id).await?;
    let contract = store
        .contract(&task.task_id)
        .await?
        .ok_or_else(|| GateError::not_found("contract", &task.task_id))?;
    let universe = market
        .get_pit_snapshot(&contract.as_of_date, &contract.universe_markets)
        .map_err(GateError::External)?;
    let draft = match draft_id {
        Some(id) => store.draft(id).await?,
        None => None,
    };
    Ok(PortfolioFacts {
        evaluated: decisions.iter().map(|d| d.ticker.clone()).collect(),
        vetoed: decisions
            .iter()
            .filter(|d| d.hard_veto)
            .map(|d| d.ticker.clone())
            .collect(),
        contract,
        universe,
        draft,
    })
}

fn check_portfolio(
    args: &PortfolioActionPayload,
    task: &TaskRecord,
    facts: &PortfolioFacts,
    committee: &InvestmentCommittee,
) -> Result<(), PreDispatchError> {
    use PreDispatchError::{BusinessRuleViolation as Rule, InvalidArguments, UnauthorizedMarket};
    if args.draft_id.trim().is_empty() {
        return Err(InvalidArguments("empty_draft_id".into()));
    }
    if args.markets.is_empty() {
        return Err(InvalidArguments("empty_markets".into()));
    }
    if let Some(market) = args
        .markets
        .iter()
        .find(|m| !task.authorized_markets.contains(m))
    {
        return Err(UnauthorizedMarket(market.clone()));
    }
    let total_bps: u32 = args.weights_bps.values().map(|w| u32::from(w.bps())).sum();
    if total_bps > 10_000 {
        return Err(Rule("total_weight_exceeds_100pct".into()));
    }
    if args
        .weights_bps
        .values()
        .any(|w| w.bps() > MAX_POSITION_BPS)
    {
        return Err(Rule("single_position_exceeds_45pct_cap".into()));
    }
    if facts.evaluated.is_empty() {
        return Err(Rule("missing_db_security_evaluations".into()));
    }
    for ticker in args.weights_bps.keys() {
        if !facts.evaluated.contains(ticker) {
            return Err(Rule(format!("unevaluated_ticker:{ticker}")));
        }
        if facts.vetoed.contains(ticker) {
            return Err(Rule(format!("vetoed_ticker_in_portfolio:{ticker}")));
        }
    }
    let breaches = limit_breaches(
        &args.weights_bps,
        &facts.universe,
        committee,
        &facts.contract,
    );
    match breaches.into_iter().next() {
        Some(breach) => Err(Rule(breach)),
        None => Ok(()),
    }
}

fn canonical(args: &PortfolioActionPayload) -> Result<String, PreDispatchError> {
    args.to_canonical_json()
        .map_err(|e| PreDispatchError::InvalidArguments(e.0))
}

fn payload_of(action: &ActionIntentRecord) -> Result<PortfolioActionPayload, GateError> {
    serde_json::from_str(&action.args_json)
        .map_err(|e| GateError::Storage(format!("corrupt_action_args:{e}")))
}

/// A read the model asks for. The date comes from the contract, not from the model.
#[derive(Debug, Clone, Serialize)]
pub struct ReadRequest {
    pub as_of_date: String,
    /// The security whose filings to read; `None` reads the market data of the date.
    pub ticker: Option<String>,
}

/// R0 and R1: what the model reads passes the same gate as a write (Diagram 3a) and is
/// registered like one. A read changes nothing, so `execute` only issues a receipt; the
/// harness reads the point-in-time data once the gate has let the call through.
pub struct ReadPit {
    pub name: &'static str,
    pub store: Store,
    pub market: FixtureMarket,
}

impl Tool for ReadPit {
    type Args = ReadRequest;
    type Facts = PortfolioFacts;

    fn name(&self) -> &str {
        self.name
    }

    fn min_tier(&self) -> RiskTier {
        RiskTier::R0
    }

    fn canonical_args(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
        if self.name == READ_FILINGS && args.ticker.is_none() {
            return Err(PreDispatchError::InvalidArguments("missing_ticker".into()));
        }
        serde_json::to_string(args).map_err(|e| PreDispatchError::InvalidArguments(e.to_string()))
    }

    async fn load_facts(
        &self,
        task: &TaskRecord,
        _: &Self::Args,
    ) -> Result<Self::Facts, GateError> {
        load_portfolio_facts(&self.store, &self.market, task, None).await
    }

    /// Only the contract's date, and only securities of the markets the task may see.
    fn check(
        &self,
        args: &Self::Args,
        _: &TaskRecord,
        facts: &Self::Facts,
    ) -> Result<(), PreDispatchError> {
        use PreDispatchError::BusinessRuleViolation as Rule;
        if args.as_of_date != facts.contract.as_of_date.as_str() {
            return Err(Rule("read_outside_contract_date".into()));
        }
        match &args.ticker {
            Some(t) if !facts.universe.iter().any(|s| &s.ticker == t) => {
                Err(Rule(format!("ticker_outside_universe:{t}")))
            }
            _ => Ok(()),
        }
    }

    async fn execute(
        &self,
        _: &VerifiedGateTicket,
        _: &TaskId,
        key: &IdempotencyKey,
        _: &Self::Args,
    ) -> Result<String, ExecError> {
        Ok(key.to_string())
    }

    async fn readback(&self, _: &Self::Args, _: &str) -> Result<(), String> {
        Ok(())
    }
}

/// R2: write the rebalance draft. Reversible, so it needs approval only when the task
/// type's autonomy level asks for it.
pub struct CreateDraft {
    pub store: Store,
    pub market: FixtureMarket,
    pub committee: InvestmentCommittee,
}

impl CreateDraft {
    fn matches(draft: &StoredDraft, task_id: &TaskId, args: &PortfolioActionPayload) -> bool {
        &draft.task_id == task_id
            && draft.as_of_date == args.as_of_date
            && draft.weights == args.weights_bps
    }

    /// Read the store back for an action whose outcome is unknown.
    pub async fn reconcile(
        &self,
        action: &ActionIntentRecord,
    ) -> Result<DispatchResult, GateError> {
        let args = payload_of(action)?;
        Ok(match self.store.draft(&args.draft_id).await? {
            Some(d) if Self::matches(&d, &action.task_id, &args) => DispatchResult::Verified {
                receipt_id: d.draft_id,
            },
            Some(_) => DispatchResult::Unknown {
                receipt_id: Some(args.draft_id),
                detail: "stored_draft_differs".into(),
            },
            None => DispatchResult::NotSent {
                detail: "no_draft_stored".into(),
            },
        })
    }
}

impl Tool for CreateDraft {
    type Args = PortfolioActionPayload;
    type Facts = PortfolioFacts;

    fn name(&self) -> &str {
        DRAFT_TOOL
    }

    fn min_tier(&self) -> RiskTier {
        RiskTier::R2
    }

    fn canonical_args(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
        canonical(args)
    }

    async fn load_facts(
        &self,
        task: &TaskRecord,
        _: &Self::Args,
    ) -> Result<Self::Facts, GateError> {
        load_portfolio_facts(&self.store, &self.market, task, None).await
    }

    fn check(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
        facts: &Self::Facts,
    ) -> Result<(), PreDispatchError> {
        check_portfolio(args, task, facts, &self.committee)
    }

    async fn execute(
        &self,
        ticket: &VerifiedGateTicket,
        task_id: &TaskId,
        _: &IdempotencyKey,
        args: &Self::Args,
    ) -> Result<String, ExecError> {
        // A local insert either happened or it did not.
        self.store
            .write_draft(ticket, task_id, args)
            .await
            .map_err(|e| ExecError::NotSent(e.to_string()))
    }

    async fn readback(&self, args: &Self::Args, receipt_id: &str) -> Result<(), String> {
        match self.store.draft(receipt_id).await {
            Ok(Some(draft)) if draft.weights == args.weights_bps => Ok(()),
            Ok(Some(_)) => Err("stored_weights_differ".into()),
            Ok(None) => Err("draft_missing".into()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// R3: place the paper orders for the stored draft. Always needs a person's approval.
pub struct SubmitOrders {
    pub store: Store,
    pub market: FixtureMarket,
    pub committee: InvestmentCommittee,
    pub broker: PaperBroker,
    /// Held outside every worker and model; `None` means the credential is missing.
    pub api_key: Option<String>,
}

impl SubmitOrders {
    /// The broker shows exactly the requested orders: same tickers, weights and amounts.
    fn matches(receipt: &BrokerOrderReceipt, args: &PortfolioActionPayload) -> Result<(), String> {
        if receipt.status != "FILLED" {
            return Err(format!("order_status:{}", receipt.status));
        }
        let capital = args
            .portfolio_capital_cents
            .unwrap_or(UsdCents::ZERO)
            .cents() as i128;
        let expected: Vec<(String, u16, i64)> = args
            .weights_bps
            .iter()
            .map(|(t, w)| {
                (
                    t.clone(),
                    w.bps(),
                    (capital * i128::from(w.bps()) / 10_000) as i64,
                )
            })
            .collect();
        let filled: Vec<(String, u16, i64)> = receipt
            .orders
            .iter()
            .map(|o| {
                (
                    o.ticker.clone(),
                    o.weight_bps.bps(),
                    o.notional_cents.cents(),
                )
            })
            .collect();
        if filled != expected {
            return Err(format!(
                "orders_differ:expected={expected:?},filled={filled:?}"
            ));
        }
        Ok(())
    }

    /// Ask the broker, by idempotency key, what happened to an action whose outcome is
    /// unknown.
    pub async fn reconcile(
        &self,
        action: &ActionIntentRecord,
    ) -> Result<DispatchResult, GateError> {
        let args = payload_of(action)?;
        Ok(
            match self
                .broker
                .get_order_by_idempotency(&action.idempotency_key)
            {
                Some(receipt) => match Self::matches(&receipt, &args) {
                    Ok(()) => DispatchResult::Verified {
                        receipt_id: receipt.receipt_id,
                    },
                    Err(detail) => DispatchResult::Unknown {
                        receipt_id: Some(receipt.receipt_id),
                        detail,
                    },
                },
                None => DispatchResult::NotSent {
                    detail: "broker_has_no_order".into(),
                },
            },
        )
    }
}

impl Tool for SubmitOrders {
    type Args = PortfolioActionPayload;
    type Facts = PortfolioFacts;

    fn name(&self) -> &str {
        ORDER_TOOL
    }

    fn min_tier(&self) -> RiskTier {
        RiskTier::R3
    }

    fn canonical_args(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
        canonical(args)
    }

    async fn load_facts(
        &self,
        task: &TaskRecord,
        args: &Self::Args,
    ) -> Result<Self::Facts, GateError> {
        load_portfolio_facts(&self.store, &self.market, task, Some(&args.draft_id)).await
    }

    fn check(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
        facts: &Self::Facts,
    ) -> Result<(), PreDispatchError> {
        check_portfolio(args, task, facts, &self.committee)?;
        if args.portfolio_capital_cents.is_none_or(|c| c.cents() <= 0) {
            return Err(PreDispatchError::InvalidArguments(
                "missing_or_non_positive_portfolio_capital".into(),
            ));
        }
        match &facts.draft {
            None => Err(PreDispatchError::BusinessRuleViolation(format!(
                "draft_not_found:{}",
                args.draft_id
            ))),
            Some(d) if !CreateDraft::matches(d, &task.task_id, args) => Err(
                PreDispatchError::BusinessRuleViolation("orders_differ_from_stored_draft".into()),
            ),
            Some(_) if self.api_key.is_none() => Err(PreDispatchError::MissingCredential(
                "paper_broker_api_key".into(),
            )),
            Some(_) => Ok(()),
        }
    }

    async fn execute(
        &self,
        _: &VerifiedGateTicket,
        task_id: &TaskId,
        key: &IdempotencyKey,
        args: &Self::Args,
    ) -> Result<String, ExecError> {
        let api_key = self.api_key.as_deref().unwrap_or_default();
        let capital = args.portfolio_capital_cents.unwrap_or(UsdCents::ZERO);
        self.broker
            .submit_orders(
                api_key,
                key,
                task_id,
                &args.as_of_date,
                capital,
                &args.weights_bps,
            )
            .map(|receipt| receipt.receipt_id)
    }

    async fn readback(&self, args: &Self::Args, receipt_id: &str) -> Result<(), String> {
        let receipt = self
            .broker
            .get_order_by_receipt(receipt_id)
            .ok_or_else(|| "order_missing".to_string())?;
        Self::matches(&receipt, args)
    }
}
