//! Settings read from files at start-up, so they change without recompiling: the strategy
//! (`config/strategy.toml`, D1) and the task-type profiles (`config/task_types/*.toml`, D2).

use std::path::Path;

use serde::Deserialize;
use trade_core::{AsOfDate, TaskTypeConfig, UsdCents, WeightBps};
use trade_quant::{BacktestConfig, FactorParams};
use trade_research::{InvestmentCommittee, Specialists};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strategy {
    pub markets: Vec<String>,
    pub rebalance_dates: Vec<AsOfDate>,
    #[serde(default)]
    pub research: ResearchParams,
    #[serde(default)]
    pub factors: FactorParams,
    #[serde(default)]
    pub committee: InvestmentCommittee,
    #[serde(default)]
    pub specialists: Specialists,
    #[serde(default)]
    pub backtest: BacktestConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResearchParams {
    /// The specialists examine this many of the best securities the quant screen kept...
    pub max_candidates: usize,
    /// ...plus these, even when vetoed, to audit the veto.
    pub audit_tickers: Vec<String>,
    pub max_positions: usize,
    pub portfolio_capital: UsdCents,
    /// Booked per security the specialists examine, until model calls are metered.
    pub worker_cost: UsdCents,
}

impl Default for ResearchParams {
    fn default() -> Self {
        Self {
            max_candidates: 8,
            audit_tickers: vec![],
            max_positions: 3,
            portfolio_capital: UsdCents::from_dollars(1_000_000.0),
            worker_cost: UsdCents::from_cents(3),
        }
    }
}

impl Strategy {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let mut strategy: Self = toml::from_str(&read(path.as_ref())?)
            .map_err(|e| format!("{}: {e}", path.as_ref().display()))?;
        // Dates and weights deserialize without their constructors' checks; run them here.
        for date in &strategy.rebalance_dates {
            AsOfDate::new(date.as_str()).map_err(|e| e.0)?;
        }
        let committee = &strategy.committee;
        for bps in [committee.max_single_weight, committee.max_industry_weight] {
            WeightBps::new(bps.bps()).map_err(|e| e.0)?;
        }
        strategy.backtest.markets = strategy.markets.clone();
        Ok(strategy)
    }
}

/// Every task-type profile in `dir`, validated.
pub fn load_task_types(dir: impl AsRef<Path>) -> Result<Vec<TaskTypeConfig>, String> {
    let dir = dir.as_ref();
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let config: TaskTypeConfig =
                toml::from_str(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))?;
            config
                .validate()
                .map_err(|e| format!("{}: {}", path.display(), e.0))?;
            Ok(config)
        })
        .collect()
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}
