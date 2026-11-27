//! Survivor-bias-free, look-ahead-free PIT backtester enforcing all 5 pitfalls,
//! with exact numerical parity against the v0.1-python-mvp baseline.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use trade_core::{AsOfDate, Bps, PitMarketSource, UsdCents, round4};

use crate::factors::{FactorParams, QuantSecurityResult, score_pit_universe};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurvivorshipAudit {
    pub survivors_only_return: f64,
    pub survivor_bias_free_return: f64,
    pub artificial_gain_pct_points: f64,
}

/// Exact verification of the textbook Survivorship Bias exercise (`8 Common Backtesting Pitfalls Exercise.pdf`).
pub fn audit_survivorship_bias_exercise() -> SurvivorshipAudit {
    let survivors = [0.30_f64, -0.20_f64];
    let all_stocks = [0.30_f64, -0.20_f64, -0.90_f64];
    let surv_avg = survivors.iter().sum::<f64>() / (survivors.len() as f64);
    let all_avg = all_stocks.iter().sum::<f64>() / (all_stocks.len() as f64);
    let bias_pts = ((surv_avg - all_avg) * 1000.0).round() / 10.0;
    SurvivorshipAudit {
        survivors_only_return: (surv_avg * 10_000.0).round() / 10_000.0,
        survivor_bias_free_return: (all_avg * 10_000.0).round() / 10_000.0,
        artificial_gain_pct_points: bias_pts,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeriodBacktestRecord {
    pub rebalance_date: AsOfDate,
    pub selected_tickers: Vec<String>,
    pub vetoed_tickers: Vec<String>,
    pub weights: BTreeMap<String, f64>,
    pub turnover: f64,
    pub cost_drag: f64,
    pub gross_return: f64,
    pub net_return: f64,
    pub rank_ic: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowMetrics {
    pub periods: usize,
    pub cumulative_return: f64,
    pub annualized_return: f64,
    pub annualized_volatility: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown: f64,
    pub avg_turnover: f64,
    pub avg_rank_ic: Option<f64>,
}

/// Backtest settings. Loaded from `config/strategy.toml`, so they change without recompiling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BacktestConfig {
    pub markets: Vec<String>,
    pub top_n: usize,
    pub portfolio_capital: UsdCents,
    pub one_way_cost: Bps,
    pub max_adv_participation: f64,
    pub max_single_weight: f64,
    pub buffer_band_pct: f64,
    pub oos_start_date: Option<AsOfDate>,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            markets: vec!["US".into()],
            top_n: 3,
            portfolio_capital: UsdCents::from_dollars(1_000_000.0),
            one_way_cost: Bps(15),
            max_adv_participation: 0.10,
            max_single_weight: 0.40,
            buffer_band_pct: 0.20,
            oos_start_date: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BacktestResult {
    pub config: BacktestConfig,
    pub periods: Vec<PeriodBacktestRecord>,
    pub overall: WindowMetrics,
    pub in_sample: Option<WindowMetrics>,
    pub out_of_sample: Option<WindowMetrics>,
}

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

fn rankdata(xs: &[f64]) -> Vec<f64> {
    let n = xs.len();
    let mut indexed: Vec<(usize, f64)> = xs.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut ranks = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && (indexed[j + 1].1 - indexed[i].1).abs() <= 1e-12 {
            j += 1;
        }
        let avg = 0.5 * ((i + j) as f64) + 1.0;
        for item in indexed.iter().take(j + 1).skip(i) {
            ranks[item.0] = avg;
        }
        i = j + 1;
    }
    ranks
}

fn spearman_ic(scores: &[f64], returns: &[f64]) -> Option<f64> {
    if scores.len() < 3 {
        return None;
    }
    let rx = rankdata(scores);
    let ry = rankdata(returns);
    let n = rx.len() as f64;
    let mx = rx.iter().sum::<f64>() / n;
    let my = ry.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut dx = 0.0;
    let mut dy = 0.0;
    for (a, b) in rx.iter().zip(ry.iter()) {
        num += (a - mx) * (b - my);
        dx += (a - mx).powi(2);
        dy += (b - my).powi(2);
    }
    if dx <= 1e-12 || dy <= 1e-12 {
        return Some(0.0);
    }
    Some(round4(num / (dx.sqrt() * dy.sqrt())))
}

fn compute_window_metrics(records: &[PeriodBacktestRecord]) -> WindowMetrics {
    if records.is_empty() {
        return WindowMetrics {
            periods: 0,
            cumulative_return: 0.0,
            annualized_return: 0.0,
            annualized_volatility: 0.0,
            sharpe_ratio: 0.0,
            max_drawdown: 0.0,
            avg_turnover: 0.0,
            avg_rank_ic: None,
        };
    }
    let n = records.len();
    let mut nav = 1.0_f64;
    let mut peak = 1.0_f64;
    let mut max_dd = 0.0_f64;
    for r in records {
        nav *= 1.0 + r.net_return;
        peak = peak.max(nav);
        let dd = (nav / peak) - 1.0;
        max_dd = max_dd.min(dd);
    }
    let cum_ret = nav - 1.0;
    let ann_ret = if nav > 0.0 {
        nav.powf(12.0 / (n as f64)) - 1.0
    } else {
        -1.0
    };
    let mean_r = records.iter().map(|r| r.net_return).sum::<f64>() / (n as f64);
    let ann_vol = if n >= 2 {
        let var = records
            .iter()
            .map(|r| (r.net_return - mean_r).powi(2))
            .sum::<f64>()
            / ((n - 1) as f64);
        var.sqrt() * 12.0_f64.sqrt()
    } else {
        0.0
    };
    let sharpe = if ann_vol > 1e-9 {
        ann_ret / ann_vol
    } else {
        0.0
    };
    let avg_to = records.iter().map(|r| r.turnover).sum::<f64>() / (n as f64);
    let ics: Vec<f64> = records.iter().filter_map(|r| r.rank_ic).collect();
    let avg_ic = if ics.is_empty() {
        None
    } else {
        Some(round4(ics.iter().sum::<f64>() / (ics.len() as f64)))
    };
    WindowMetrics {
        periods: n,
        cumulative_return: round4(cum_ret),
        annualized_return: round4(ann_ret),
        annualized_volatility: round4(ann_vol),
        sharpe_ratio: round4(sharpe),
        max_drawdown: round4(max_dd),
        avg_turnover: round4(avg_to),
        avg_rank_ic: avg_ic,
    }
}

/// The return actually earned until the next rebalance: the delisting return if the
/// security was delisted in between, otherwise its forward return from the data source.
fn realized_holding_return(
    res: &QuantSecurityResult,
    next_dt: Option<&AsOfDate>,
    forward: &BTreeMap<String, f64>,
) -> Result<f64, String> {
    if let (Some(delist_dt), Some(delist_ret)) = (&res.delist_date, res.delist_return)
        && next_dt.is_none_or(|nxt| delist_dt.as_str() <= nxt.as_str())
    {
        return Ok(delist_ret);
    }
    forward
        .get(&res.ticker)
        .copied()
        .ok_or_else(|| format!("missing_forward_return:{}", res.ticker))
}

pub fn run_pit_backtest(
    source: &dyn PitMarketSource,
    rebalance_dates: &[AsOfDate],
    config: &BacktestConfig,
    factors: &FactorParams,
) -> Result<BacktestResult, String> {
    let mut prev_weights: BTreeMap<String, f64> = BTreeMap::new();
    let mut periods = Vec::with_capacity(rebalance_dates.len());
    let cap_usd = config.portfolio_capital.to_dollars().max(1.0);

    for (idx, dt) in rebalance_dates.iter().enumerate() {
        let next_dt = rebalance_dates.get(idx + 1);
        let snapshot = source.get_pit_snapshot(dt, &config.markets)?;
        let forward = source.get_forward_returns(dt, &config.markets)?;
        let scored = score_pit_universe(&snapshot, dt, factors);

        let eligible: Vec<&QuantSecurityResult> = scored.iter().filter(|r| !r.hard_veto).collect();
        let vetoed: Vec<String> = scored
            .iter()
            .filter(|r| r.hard_veto)
            .map(|r| r.ticker.clone())
            .collect();

        // Buffer band retention to suppress noisy turnover
        let cutoff = ((config.top_n as f64) * (1.0 + config.buffer_band_pct)).ceil() as usize;
        let cutoff = cutoff.max(config.top_n).min(eligible.len());
        let mut selected: Vec<String> = Vec::with_capacity(config.top_n);
        for r in eligible.iter().take(cutoff) {
            if prev_weights.contains_key(&r.ticker) && selected.len() < config.top_n {
                selected.push(r.ticker.clone());
            }
        }
        for r in &eligible {
            if selected.len() >= config.top_n {
                break;
            }
            if !selected.contains(&r.ticker) {
                selected.push(r.ticker.clone());
            }
        }

        let by_ticker: BTreeMap<&str, &QuantSecurityResult> =
            scored.iter().map(|r| (r.ticker.as_str(), r)).collect();

        let mut new_weights: BTreeMap<String, f64> = BTreeMap::new();
        if !selected.is_empty() {
            let raw_w = 1.0 / (selected.len() as f64);
            for t in &selected {
                let q = by_ticker[t.as_str()];
                let adv_cap_w = (config.max_adv_participation * q.adv_usd) / cap_usd;
                let w = raw_w.min(config.max_single_weight).min(adv_cap_w);
                if w > 0.0 {
                    new_weights.insert(t.clone(), round6(w));
                }
            }
        }

        let mut all_tickers: BTreeSet<&String> = BTreeSet::new();
        all_tickers.extend(prev_weights.keys());
        all_tickers.extend(new_weights.keys());

        let traded_frac: f64 = all_tickers
            .into_iter()
            .map(|t| {
                let w_new = new_weights.get(t).copied().unwrap_or(0.0);
                let w_old = prev_weights.get(t).copied().unwrap_or(0.0);
                (w_new - w_old).abs()
            })
            .sum();

        let turnover = 0.5 * traded_frac;
        let cost_drag = traded_frac * config.one_way_cost.as_ratio();
        let mut gross_ret = 0.0;
        for (t, &w) in &new_weights {
            gross_ret += w * realized_holding_return(by_ticker[t.as_str()], next_dt, &forward)?;
        }
        let net_ret = gross_ret - cost_drag;

        let ic_scores: Vec<f64> = eligible.iter().map(|r| r.quant_score).collect();
        let ic_rets = eligible
            .iter()
            .map(|r| realized_holding_return(r, next_dt, &forward))
            .collect::<Result<Vec<f64>, String>>()?;

        periods.push(PeriodBacktestRecord {
            rebalance_date: dt.clone(),
            selected_tickers: selected,
            vetoed_tickers: vetoed,
            weights: new_weights.clone(),
            turnover: round4(turnover),
            cost_drag: round6(cost_drag),
            gross_return: round6(gross_ret),
            net_return: round6(net_ret),
            rank_ic: spearman_ic(&ic_scores, &ic_rets),
        });
        prev_weights = new_weights;
    }

    let overall = compute_window_metrics(&periods);
    let (in_sample, out_of_sample) = if let Some(oos_dt) = &config.oos_start_date {
        let is_vec: Vec<PeriodBacktestRecord> = periods
            .iter()
            .filter(|p| &p.rebalance_date < oos_dt)
            .cloned()
            .collect();
        let oos_vec: Vec<PeriodBacktestRecord> = periods
            .iter()
            .filter(|p| &p.rebalance_date >= oos_dt)
            .cloned()
            .collect();
        (
            Some(compute_window_metrics(&is_vec)),
            Some(compute_window_metrics(&oos_vec)),
        )
    } else {
        let mid = periods.len() / 2;
        if mid > 0 {
            (
                Some(compute_window_metrics(&periods[..mid])),
                Some(compute_window_metrics(&periods[mid..])),
            )
        } else {
            (None, None)
        }
    };

    Ok(BacktestResult {
        config: config.clone(),
        periods,
        overall,
        in_sample,
        out_of_sample,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_core::{FilingDate, PeriodEndDate, PitSecuritySnapshot};

    fn sample_snap(
        ticker: &str,
        bm_bv: f64,
        z_ebit: f64,
        cfo: f64,
        beneish: f64,
    ) -> PitSecuritySnapshot {
        PitSecuritySnapshot {
            ticker: ticker.into(),
            name: ticker.into(),
            market: "US".into(),
            industry: "Tech".into(),
            list_date: "2015-01-01".into(),
            delist_date: None,
            delist_return: None,
            fin_record_id: format!("FIN-{ticker}"),
            period_end: PeriodEndDate::new("2025-12-31").unwrap(),
            filing_date: FilingDate::new("2026-02-01").unwrap(),
            book_value: bm_bv,
            market_cap: 1000.0,
            enterprise_value: 1000.0,
            revenue: 900.0,
            revenue_3y_ago: 700.0,
            gross_profit: 450.0,
            ebit: z_ebit,
            net_income: 150.0,
            interest_expense: 10.0,
            cfo,
            capex: 30.0,
            total_assets: 1000.0,
            prev_total_assets: 950.0,
            current_assets: 400.0,
            current_liabilities: 180.0,
            total_debt: 150.0,
            cash: 200.0,
            retained_earnings: 500.0,
            total_liabilities: 350.0,
            shares_out: 98.0,
            prev_shares_out: 100.0,
            goodwill: 40.0,
            prev_goodwill: 40.0,
            sbc_expense: 10.0,
            insider_net_buy_usd: 200_000.0,
            beneish_m: beneish,
            roic: 0.24,
            wacc: 0.08,
            trade_date: "2026-02-10".into(),
            close_price: 100.0,
            adv_usd: 20_000_000.0,
            amihud_illiq: 0.01,
            momentum_12_1: 0.10,
            is_halted: false,
            is_st: false,
        }
    }

    #[test]
    fn survivorship_exercise_matches_pdf() {
        let ex = audit_survivorship_bias_exercise();
        assert!((ex.survivors_only_return - 0.05).abs() < 1e-6);
        assert!((ex.survivor_bias_free_return - (-0.2667)).abs() < 1e-4);
        assert!((ex.artificial_gain_pct_points - 31.7).abs() < 1e-6);
    }

    #[test]
    fn factor_scoring_flags_negative_cfo_value_trap() {
        let dt = AsOfDate::new("2026-02-10").unwrap();
        let rows = vec![
            sample_snap("GOOD_A", 900.0, 260.0, 220.0, -2.7),
            sample_snap("GOOD_B", 800.0, 240.0, 200.0, -2.6),
            sample_snap("TRAP_X", 1400.0, 180.0, -50.0, -1.1),
        ];
        let scored = score_pit_universe(&rows, &dt, &FactorParams::default());
        let trap = scored.iter().find(|r| r.ticker == "TRAP_X").unwrap();
        assert!(trap.hard_veto);
        assert!(
            trap.veto_reasons
                .iter()
                .any(|s| s == "negative_cfo_positive_ni")
        );
    }
}
