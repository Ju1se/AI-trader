//! Read-only data access that research and quant code needs. The market adapter implements
//! these traits, so neither research nor quant reads files or a database itself.

use std::collections::BTreeMap;

use crate::contracts::PitSecuritySnapshot;
use crate::types::{AsOfDate, UntrustedFiling};

/// What a specialist worker may read: filings in its authorised markets and its calibration.
pub trait ReadOnlyPitView {
    fn get_pit_filings(
        &self,
        ticker: &str,
        as_of: &AsOfDate,
    ) -> Result<Vec<UntrustedFiling>, String>;
    fn get_agent_calibration(&self, agent_name: &str) -> f64;
}

/// Point-in-time market data for scoring and backtests.
pub trait PitMarketSource {
    /// Data that was public on `as_of`. Contains no future information.
    fn get_pit_snapshot(
        &self,
        as_of: &AsOfDate,
        markets: &[String],
    ) -> Result<Vec<PitSecuritySnapshot>, String>;

    /// Realised one-month returns after `as_of`. Only the backtest calls this; snapshots
    /// handed to research never carry them, so look-ahead cannot reach the workers.
    fn get_forward_returns(
        &self,
        as_of: &AsOfDate,
        markets: &[String],
    ) -> Result<BTreeMap<String, f64>, String>;
}
