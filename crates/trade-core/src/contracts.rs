//! Strongly-validated contracts for Agent 1-7 governance (Diagrams 3a, 3b, 4a, 4b).
//! - Unconfigured tools are rejected (`lookup_tool_risk -> None`).
//! - `R4` means permanently prohibited (`永远拒绝`); order submission is `R3`.
//! - Nothing outside the gate can make an approval; requests name it by `approval_id`.
//! - Caller action payloads NEVER carry veto lists (`vetoed_tickers`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, IdempotencyKey, TaskId};
use crate::plan::{Plan, PlanStep};
use crate::state::{TaskState, TimeoutAction, WaitMetadata};
use crate::types::{
    AsOfDate, FilingDate, PeriodEndDate, SchemaError, UsdCents, WeightBps, require_non_empty,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShapeLevel {
    SingleCall,
    FixedWorkflow,
    SingleAgent,
    MultiAgent,
}

impl ShapeLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SingleCall => "SINGLE_CALL",
            Self::FixedWorkflow => "FIXED_WORKFLOW",
            Self::SingleAgent => "SINGLE_AGENT",
            Self::MultiAgent => "MULTI_AGENT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutonomyLevel {
    Shadow,
    DraftOnly,
    PerActionApproval,
    BoundedAuto,
}

impl AutonomyLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "SHADOW",
            Self::DraftOnly => "DRAFT_ONLY",
            Self::PerActionApproval => "PER_ACTION_APPROVAL",
            Self::BoundedAuto => "BOUNDED_AUTO",
        }
    }
}

/// Diagram 3a Risk Tiers:
/// - `R0`: Public/low-risk read
/// - `R1`: Scoped read
/// - `R2`: Reversible write (e.g. `create_rebalance_draft`)
/// - `R3`: Consequential / high-stakes order submission (e.g. `submit_paper_orders`)
/// - `R4`: Permanently prohibited (`永远拒绝`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskTier {
    R0,
    R1,
    R2,
    R3,
    R4,
}

impl RiskTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::R0 => "R0",
            Self::R1 => "R1",
            Self::R2 => "R2",
            Self::R3 => "R3",
            Self::R4 => "R4",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetLimits {
    pub max_turns: u32,
    pub max_spend: UsdCents,
    pub reserved_close_spend: UsdCents,
    pub max_wall_ms: u64,
    pub max_retries: u32,
    pub max_consecutive_denials: u32,
    pub max_reworks: u32,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_spend: UsdCents::from_cents(500),
            reserved_close_spend: UsdCents::from_cents(25),
            max_wall_ms: 120_000,
            max_retries: 3,
            max_consecutive_denials: 6,
            max_reworks: 2,
        }
    }
}

impl BudgetLimits {
    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.max_turns < 1 {
            return Err(SchemaError("max_turns must be >= 1".into()));
        }
        if self.max_spend.cents() <= 0 {
            return Err(SchemaError("max_spend must be > 0".into()));
        }
        if self.reserved_close_spend.cents() < 0
            || self.reserved_close_spend.cents() >= self.max_spend.cents()
        {
            return Err(SchemaError(
                "reserved_close_spend must be in [0, max_spend)".into(),
            ));
        }
        if self.max_wall_ms == 0 || self.max_consecutive_denials == 0 {
            return Err(SchemaError(
                "max_wall_ms and max_consecutive_denials must be >= 1".into(),
            ));
        }
        Ok(())
    }

    pub fn usable_execution_spend(&self) -> UsdCents {
        UsdCents::from_cents(self.max_spend.cents() - self.reserved_close_spend.cents())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTypeConfig {
    pub task_type: String,
    pub version: String,
    pub shape_level: ShapeLevel,
    pub shape_rationale: String,
    pub autonomy_level: AutonomyLevel,
    pub config_admin_id: String,
    pub allowed_tools: BTreeMap<String, RiskTier>,
    pub budget_limits: BudgetLimits,
    pub done_criteria: Vec<String>,
    #[serde(default)]
    pub waits: WaitPolicy,
    /// Diagram 2: the steps of a task of this type and what each waits for. Every new task's
    /// plan starts from it. Checked at start-up: a cycle, an unknown step or a tool that is
    /// not allowed stops the server.
    #[serde(default)]
    pub plan: Vec<PlanStep>,
}

/// How long each kind of wait may last, and who takes over when nobody acts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaitPolicy {
    pub input_ttl_s: i64,
    pub approval_ttl_s: i64,
    pub settle_window_s: i64,
    pub handoff_ack_ttl_s: i64,
    /// How long a task out of budget waits for its owner before what is done is delivered.
    pub limit_ttl_s: i64,
    pub approver_role: String,
    pub escalation_owner_id: String,
    pub backup_owner_id: String,
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self {
            input_ttl_s: 48 * 3_600,
            approval_ttl_s: 24 * 3_600,
            settle_window_s: 72 * 3_600,
            handoff_ack_ttl_s: 4 * 3_600,
            limit_ttl_s: 4 * 3_600,
            approver_role: "portfolio_approver".into(),
            escalation_owner_id: "risk_committee_lead".into(),
            backup_owner_id: "operations_on_call".into(),
        }
    }
}

impl TaskTypeConfig {
    pub fn validate(&self) -> Result<(), SchemaError> {
        require_non_empty(&self.task_type, "task_type")?;
        require_non_empty(&self.version, "version")?;
        require_non_empty(&self.shape_rationale, "shape_rationale")?;
        require_non_empty(&self.config_admin_id, "config_admin_id")?;
        if self.done_criteria.is_empty() {
            return Err(SchemaError("done_criteria must not be empty".into()));
        }
        if let Some(step) = self
            .plan
            .iter()
            .find(|s| !self.allowed_tools.contains_key(&s.tool))
        {
            return Err(SchemaError(format!(
                "plan_step_{}_tool_not_allowed",
                step.id
            )));
        }
        Plan::new(self.plan.clone())?;
        self.budget_limits.validate()
    }

    /// Diagram 4a: a task out of budget waits for the escalation owner, who grants more or
    /// has what is done delivered; at the deadline, what is done is delivered.
    pub fn limit_reached(&self, reason: &str, now: i64) -> Result<TaskState, SchemaError> {
        let waits = &self.waits;
        let wait = WaitMetadata::new(
            &waits.escalation_owner_id,
            now + waits.limit_ttl_s,
            TimeoutAction::Deliver,
        )?;
        Ok(TaskState::LimitReached {
            reason: reason.into(),
            wait,
        })
    }

    /// Diagram 3a: Tools not explicitly listed in `allowed_tools` return `None` and MUST be denied.
    pub fn lookup_tool_risk(&self, tool_name: &str) -> Option<RiskTier> {
        self.allowed_tools.get(tool_name).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskContract {
    pub task_id: TaskId,
    pub goal: String,
    pub as_of_date: AsOfDate,
    pub universe_markets: Vec<String>,
    pub portfolio_capital: UsdCents,
    pub max_positions: usize,
    pub expected_outputs: Vec<String>,
    pub done_checks: Vec<String>,
    pub assumptions: Vec<String>,
    /// Whether the task may finish without its irreversible (R3) step. If so, a refused
    /// approval for that step sends the task back to work to deliver what it has; if not,
    /// the refusal ends the task as DECLINED (Diagram 3b).
    pub irreversible_steps_deferred: bool,
}

impl TaskContract {
    pub fn validate(&self) -> Result<(), SchemaError> {
        require_non_empty(&self.task_id, "task_id")?;
        require_non_empty(&self.goal, "goal")?;
        if self.universe_markets.is_empty() {
            return Err(SchemaError("universe_markets must not be empty".into()));
        }
        if self.portfolio_capital.cents() <= 0 {
            return Err(SchemaError("portfolio_capital must be > 0".into()));
        }
        if self.max_positions < 1 {
            return Err(SchemaError("max_positions must be >= 1".into()));
        }
        if self.expected_outputs.is_empty() || self.done_checks.is_empty() {
            return Err(SchemaError(
                "expected_outputs and done_checks must not be empty".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvidenceOutput {
    pub ticker: String,
    pub as_of_date: AsOfDate,
    pub agent: String,
    pub score: f64,
    pub confidence: f64,
    pub hard_veto: bool,
    pub evidence: Vec<String>,
    pub cited_record_ids: Vec<String>,
    pub missingness: f64,
    pub stale_data: bool,
    pub assumptions: Vec<String>,
}

impl AgentEvidenceOutput {
    pub fn validate(&self) -> Result<(), SchemaError> {
        require_non_empty(&self.ticker, "ticker")?;
        require_non_empty(&self.agent, "agent")?;
        if !self.score.is_finite() || !(-1.0..=1.0).contains(&self.score) {
            return Err(SchemaError(format!(
                "score {} outside [-1.0, 1.0]",
                self.score
            )));
        }
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(SchemaError(format!(
                "confidence {} outside [0.0, 1.0]",
                self.confidence
            )));
        }
        if !self.missingness.is_finite() || !(0.0..=1.0).contains(&self.missingness) {
            return Err(SchemaError(format!(
                "missingness {} outside [0.0, 1.0]",
                self.missingness
            )));
        }
        if self.evidence.is_empty() {
            return Err(SchemaError(
                "evidence must contain at least one item".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitteeDecision {
    pub ticker: String,
    pub as_of_date: AsOfDate,
    pub quant_base_score: f64,
    pub consensus_score: f64,
    pub dispersion: f64,
    pub hard_veto: bool,
    pub veto_reasons: Vec<String>,
    pub escalate_review: bool,
    pub worker_outputs: Vec<AgentEvidenceOutput>,
}

/// Caller action payload. Notice: NEVER contains `vetoed_tickers` — the backend reads
/// the authoritative veto set directly from the database (`security_evaluations`)!
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioActionPayload {
    pub draft_id: String,
    pub as_of_date: AsOfDate,
    pub weights_bps: BTreeMap<String, WeightBps>,
    pub markets: Vec<String>,
    pub portfolio_capital_cents: Option<UsdCents>,
}

impl PortfolioActionPayload {
    pub fn to_canonical_json(&self) -> Result<String, SchemaError> {
        serde_json::to_string(self).map_err(|e| SchemaError(e.to_string()))
    }
}

/// DB-managed approval lifecycle status (`PENDING -> APPROVED | REJECTED -> CONSUMED | EXPIRED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Consumed,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Approved => "APPROVED",
            Self::Rejected => "REJECTED",
            Self::Consumed => "CONSUMED",
            Self::Expired => "EXPIRED",
        }
    }

    pub fn from_db_str(s: &str) -> Result<Self, SchemaError> {
        match s {
            "PENDING" => Ok(Self::Pending),
            "APPROVED" => Ok(Self::Approved),
            "REJECTED" => Ok(Self::Rejected),
            "CONSUMED" => Ok(Self::Consumed),
            "EXPIRED" => Ok(Self::Expired),
            other => Err(SchemaError(format!("unknown_approval_status:{other}"))),
        }
    }
}

/// One approval request and its decision. Only the store creates or changes it; the gate
/// reads it from the store, never from the proposer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub approval_id: ApprovalId,
    pub task_id: TaskId,
    pub requester_id: String,
    pub tool_name: String,
    pub target: String,
    /// Hash of tool, target and canonical arguments: the approval covers exactly this.
    pub payload_hash: String,
    pub args_json: String,
    pub status: ApprovalStatus,
    pub approver_id: Option<String>,
    pub decision_note: Option<String>,
    pub expires_at_epoch_s: i64,
}

/// Diagram 3b / Requirement #3: Action status includes `Dispatched`, and rejects unknown strings!
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionIntentStatus {
    Dispatched,
    Verified,
    Unknown,
    Failed,
}

impl ActionIntentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dispatched => "DISPATCHED",
            Self::Verified => "VERIFIED",
            Self::Unknown => "UNKNOWN",
            Self::Failed => "FAILED",
        }
    }

    pub fn from_db_str(s: &str) -> Result<Self, SchemaError> {
        match s {
            "DISPATCHED" => Ok(Self::Dispatched),
            "VERIFIED" => Ok(Self::Verified),
            "UNKNOWN" => Ok(Self::Unknown),
            "FAILED" => Ok(Self::Failed),
            other => Err(SchemaError(format!("unknown_action_intent_status:{other}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionIntentRecord {
    pub idempotency_key: IdempotencyKey,
    pub task_id: TaskId,
    pub tool_name: String,
    pub target: String,
    pub args_json: String,
    pub payload_hash: String,
    pub status: ActionIntentStatus,
    pub backend_receipt_id: Option<String>,
    pub readback_verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Completeness {
    Complete,
    Partial,
}

impl Completeness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "COMPLETE",
            Self::Partial => "PARTIAL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryManifest {
    pub task_id: TaskId,
    pub completeness: Completeness,
    pub criteria_results: BTreeMap<String, bool>,
    pub assumptions: Vec<String>,
    pub gaps: Vec<String>,
    pub total_spend: UsdCents,
    pub next_owner: String,
    pub draft_id: Option<String>,
    pub weights: BTreeMap<String, WeightBps>,
}

/// The rebalance draft as written, which orders must match exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDraft {
    pub draft_id: String,
    pub task_id: TaskId,
    pub as_of_date: AsOfDate,
    pub weights: BTreeMap<String, WeightBps>,
}

/// One cost entry (Diagram 8 COST_ENTRY).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostEntry {
    /// PREPARE, ADMIT, FRAME, ACT or CLOSE.
    pub stage: String,
    /// For example `model`, `broker` or `human_review`.
    pub category: String,
    /// API and tool spend. It counts against the task's spend limit.
    pub amount: UsdCents,
    /// Time people spent. Priced in unit economics, not in the task's spend limit.
    pub human_minutes: f64,
    pub note: String,
}

impl CostEntry {
    pub fn new(stage: &str, category: &str, amount: UsdCents, note: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            category: category.into(),
            amount,
            human_minutes: 0.0,
            note: note.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffPackage {
    pub task_id: TaskId,
    pub requester_id: String,
    pub original_request: String,
    pub trigger_reason: String,
    pub completed_actions: Vec<String>,
    pub pending_actions: Vec<String>,
    pub partial_weights: BTreeMap<String, WeightBps>,
}

/// Everything public about one security on a given date. Forward returns are deliberately
/// absent: the backtest gets them through `PitMarketSource::get_forward_returns`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitSecuritySnapshot {
    pub ticker: String,
    pub name: String,
    pub market: String,
    pub industry: String,
    pub list_date: String,
    pub delist_date: Option<String>,
    pub delist_return: Option<f64>,
    pub fin_record_id: String,
    pub period_end: PeriodEndDate,
    pub filing_date: FilingDate,
    pub book_value: f64,
    pub market_cap: f64,
    pub enterprise_value: f64,
    pub revenue: f64,
    pub revenue_3y_ago: f64,
    pub gross_profit: f64,
    pub ebit: f64,
    pub net_income: f64,
    pub interest_expense: f64,
    pub cfo: f64,
    pub capex: f64,
    pub total_assets: f64,
    pub prev_total_assets: f64,
    pub current_assets: f64,
    pub current_liabilities: f64,
    pub total_debt: f64,
    pub cash: f64,
    pub retained_earnings: f64,
    pub total_liabilities: f64,
    pub shares_out: f64,
    pub prev_shares_out: f64,
    pub goodwill: f64,
    pub prev_goodwill: f64,
    pub sbc_expense: f64,
    pub insider_net_buy_usd: f64,
    pub beneish_m: f64,
    pub roic: f64,
    pub wacc: f64,
    pub trade_date: String,
    pub close_price: f64,
    pub adv_usd: f64,
    pub amihud_illiq: f64,
    pub momentum_12_1: f64,
    pub is_halted: bool,
    pub is_st: bool,
}
