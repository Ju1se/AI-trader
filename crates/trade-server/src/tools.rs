//! Concrete `trade_gate::Tool` implementations for rebalance draft creation (`R2`)
//! and standalone paper broker order submission (`R3`).
//! Authoritative veto lists and draft records are ALWAYS fetched from `DbStore` — never from callers.

use std::collections::BTreeMap;

use trade_adapters::PaperBrokerAdapter;
use trade_core::{
    PortfolioActionPayload, PreDispatchError, RiskTier, TaskRecord,
};
use trade_gate::{Tool, VerifiedGateTicket};
use trade_store::DbStore;

/// Isolated secret store residing outside worker crates (`Agent 7` 2.3).
#[derive(Clone, Default)]
pub struct CredentialVault {
    secrets: BTreeMap<String, String>,
}

impl CredentialVault {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_secret(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.secrets.insert(name.into(), value.into());
    }

    pub fn get_secret(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(|s| s.as_str())
    }
}

fn check_common_portfolio_bounds(
    args: &PortfolioActionPayload,
    task: &TaskRecord,
) -> Result<(), PreDispatchError> {
    if args.draft_id.trim().is_empty() {
        return Err(PreDispatchError::InvalidArguments("empty_draft_id".into()));
    }
    if args.markets.is_empty() {
        return Err(PreDispatchError::InvalidArguments("empty_markets".into()));
    }
    for mkt in &args.markets {
        if !task.authorized_markets.contains(mkt) {
            return Err(PreDispatchError::UnauthorizedMarket(mkt.clone()));
        }
    }
    let total_bps: u32 = args.weights_bps.values().map(|w| u32::from(w.bps())).sum();
    if total_bps > 10_000 {
        return Err(PreDispatchError::BusinessRuleViolation(
            "total_weight_exceeds_100pct".into(),
        ));
    }
    if args.weights_bps.values().any(|w| w.bps() > 4_500) {
        return Err(PreDispatchError::BusinessRuleViolation(
            "single_position_exceeds_45pct_cap".into(),
        ));
    }
    Ok(())
}

/// R2 reversible write tool: persists a rebalance draft after verifying against DB veto list.
pub struct CreateRebalanceDraftTool<'a> {
    pub store: &'a DbStore,
}

impl<'a> Tool for CreateRebalanceDraftTool<'a> {
    type Args = PortfolioActionPayload;
    type Receipt = String;

    fn name(&self) -> &str {
        "create_rebalance_draft"
    }

    fn risk_tier(&self) -> RiskTier {
        RiskTier::R2
    }

    fn canonical_args_json(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
        args.to_canonical_json()
            .map_err(|e| PreDispatchError::InvalidArguments(e.0))
    }

    fn validate_args(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
    ) -> Result<(), PreDispatchError> {
        check_common_portfolio_bounds(args, task)
    }

    async fn check_business_rules(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
    ) -> Result<(), PreDispatchError> {
        // Requirement #2: Read authoritative evaluations & hard-veto list directly from DB!
        let evals = self
            .store
            .get_stored_evaluations(&task.task_id)
            .await
            .map_err(|e| PreDispatchError::CheckFailed(e.to_string()))?;
        if evals.is_empty() {
            return Err(PreDispatchError::BusinessRuleViolation(
                "missing_db_security_evaluations".into(),
            ));
        }
        for ticker in args.weights_bps.keys() {
            let Some(ev) = evals.get(ticker) else {
                return Err(PreDispatchError::BusinessRuleViolation(format!(
                    "unevaluated_ticker:{ticker}"
                )));
            };
            if ev.hard_veto {
                return Err(PreDispatchError::BusinessRuleViolation(format!(
                    "vetoed_ticker_in_portfolio:{ticker}"
                )));
            }
        }
        Ok(())
    }

    async fn execute(
        &self,
        ticket: &VerifiedGateTicket,
        task_id: &str,
        _idempotency_key: &str,
        args: &Self::Args,
    ) -> Result<Self::Receipt, String> {
        self.store
            .write_rebalance_draft(ticket, task_id, args)
            .await
            .map_err(|e| e.to_string())
    }

    async fn readback_verify(
        &self,
        args: &Self::Args,
        receipt: &Self::Receipt,
    ) -> Result<bool, String> {
        let rb = self
            .store
            .readback_rebalance_draft(receipt)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rb.is_some_and(|d| d.weights == args.weights_bps))
    }

    async fn rehydrate_verified_receipt(
        &self,
        receipt_id: &str,
    ) -> Result<Option<Self::Receipt>, String> {
        let rb = self
            .store
            .readback_rebalance_draft(receipt_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rb.map(|d| d.draft_id))
    }
}

/// R3 consequential order submission tool (`Diagram 3a`: 下单归 R3) backed by standalone `PaperBrokerAdapter`.
pub struct SubmitPaperOrdersTool<'a> {
    pub store: &'a DbStore,
    pub broker: &'a PaperBrokerAdapter,
    pub vault: &'a CredentialVault,
}

impl<'a> Tool for SubmitPaperOrdersTool<'a> {
    type Args = PortfolioActionPayload;
    type Receipt = String;

    fn name(&self) -> &str {
        "submit_paper_orders"
    }

    fn risk_tier(&self) -> RiskTier {
        RiskTier::R3
    }

    fn canonical_args_json(&self, args: &Self::Args) -> Result<String, PreDispatchError> {
        args.to_canonical_json()
            .map_err(|e| PreDispatchError::InvalidArguments(e.0))
    }

    fn validate_args(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
    ) -> Result<(), PreDispatchError> {
        check_common_portfolio_bounds(args, task)?;
        if args.portfolio_capital_cents.is_none_or(|c| c.cents() <= 0) {
            return Err(PreDispatchError::InvalidArguments(
                "missing_or_non_positive_portfolio_capital".into(),
            ));
        }
        Ok(())
    }

    async fn check_business_rules(
        &self,
        args: &Self::Args,
        task: &TaskRecord,
    ) -> Result<(), PreDispatchError> {
        // Requirement #2: Re-verify both DB veto list AND stored draft weights from DB!
        let evals = self
            .store
            .get_stored_evaluations(&task.task_id)
            .await
            .map_err(|e| PreDispatchError::CheckFailed(e.to_string()))?;
        if evals.is_empty() {
            return Err(PreDispatchError::BusinessRuleViolation(
                "missing_db_security_evaluations".into(),
            ));
        }
        for ticker in args.weights_bps.keys() {
            let Some(ev) = evals.get(ticker) else {
                return Err(PreDispatchError::BusinessRuleViolation(format!(
                    "unevaluated_ticker:{ticker}"
                )));
            };
            if ev.hard_veto {
                return Err(PreDispatchError::BusinessRuleViolation(format!(
                    "vetoed_ticker_in_portfolio:{ticker}"
                )));
            }
        }
        let draft = self
            .store
            .readback_rebalance_draft(&args.draft_id)
            .await
            .map_err(|e| PreDispatchError::CheckFailed(e.to_string()))?
            .ok_or_else(|| {
                PreDispatchError::BusinessRuleViolation(format!(
                    "draft_not_found:{}",
                    args.draft_id
                ))
            })?;
        if draft.task_id != task.task_id || draft.weights != args.weights_bps {
            return Err(PreDispatchError::BusinessRuleViolation(
                "order_weights_diverge_from_stored_draft".into(),
            ));
        }
        Ok(())
    }

    fn check_pre_dispatch_ready(&self) -> Result<(), PreDispatchError> {
        if self.vault.get_secret("paper_broker_api_key").is_none() {
            return Err(PreDispatchError::MissingCredential(
                "paper_broker_api_key".into(),
            ));
        }
        Ok(())
    }

    async fn execute(
        &self,
        _ticket: &VerifiedGateTicket,
        task_id: &str,
        idempotency_key: &str,
        args: &Self::Args,
    ) -> Result<Self::Receipt, String> {
        let api_key = self
            .vault
            .get_secret("paper_broker_api_key")
            .ok_or_else(|| "missing_paper_broker_api_key".to_string())?;
        let cap = args
            .portfolio_capital_cents
            .ok_or_else(|| "missing_capital".to_string())?;
        let receipt = self.broker.submit_orders(
            api_key,
            idempotency_key,
            task_id,
            &args.as_of_date,
            cap,
            &args.weights_bps,
        )?;
        Ok(receipt.receipt_id)
    }

    async fn readback_verify(
        &self,
        args: &Self::Args,
        receipt: &Self::Receipt,
    ) -> Result<bool, String> {
        let Some(order) = self.broker.get_order_by_receipt(receipt) else {
            return Ok(false);
        };
        Ok(order.status == "FILLED" && order.orders.len() == args.weights_bps.len())
    }

    async fn rehydrate_verified_receipt(
        &self,
        receipt_id: &str,
    ) -> Result<Option<Self::Receipt>, String> {
        Ok(self
            .broker
            .get_order_by_receipt(receipt_id)
            .map(|o| o.receipt_id))
    }
}
