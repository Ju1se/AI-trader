//! Deterministic 15-factor cross-sectional engine + Hard Veto risk overlay.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use trade_core::{AsOfDate, FilingDate, PeriodEndDate, PitSecuritySnapshot, round4};

/// Weights and thresholds of the factor model. The defaults reproduce the Python
/// baseline; `config/strategy.toml` can override any of them without recompiling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FactorParams {
    /// Securities trading less than this per day are vetoed as illiquid.
    pub min_adv_usd: f64,
    pub weights: BucketWeights,
    /// Weight of momentum, used only to break near-ties.
    pub momentum_tiebreak: f64,
    pub penalty: PenaltyParams,
    pub veto: VetoParams,
}

/// How much each factor bucket contributes to the alpha score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BucketWeights {
    pub valuation: f64,
    pub quality: f64,
    pub financial_health: f64,
    pub earnings_quality: f64,
    pub cash_flow: f64,
    pub growth: f64,
    pub capital_allocation: f64,
}

/// Soft risk penalties: half a point per breached threshold, then weighted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PenaltyParams {
    pub altman_z_below: f64,
    pub interest_cov_below: f64,
    pub beneish_m_above: f64,
    pub accruals_above: f64,
    pub distress_weight: f64,
    pub manipulation_weight: f64,
    pub illiquidity_weight: f64,
}

/// Hard vetoes: any one of them excludes the security.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VetoParams {
    pub altman_z_below: f64,
    pub interest_cov_below: f64,
    pub beneish_m_above: f64,
    /// ...together with accruals at or above this quantile of the universe.
    pub accruals_quantile: f64,
}

impl Default for FactorParams {
    fn default() -> Self {
        Self {
            min_adv_usd: 1_000_000.0,
            weights: BucketWeights::default(),
            momentum_tiebreak: 0.05,
            penalty: PenaltyParams::default(),
            veto: VetoParams::default(),
        }
    }
}

impl Default for BucketWeights {
    fn default() -> Self {
        Self {
            valuation: 0.30,
            quality: 0.20,
            financial_health: 0.15,
            earnings_quality: 0.10,
            cash_flow: 0.10,
            growth: 0.10,
            capital_allocation: 0.05,
        }
    }
}

impl Default for PenaltyParams {
    fn default() -> Self {
        Self {
            altman_z_below: 2.99,
            interest_cov_below: 4.0,
            beneish_m_above: -2.20,
            accruals_above: 0.05,
            distress_weight: 0.15,
            manipulation_weight: 0.15,
            illiquidity_weight: 0.10,
        }
    }
}

impl Default for VetoParams {
    fn default() -> Self {
        Self {
            altman_z_below: 1.8,
            interest_cov_below: 1.5,
            beneish_m_above: -1.78,
            accruals_quantile: 0.80,
        }
    }
}

pub const FACTOR_NAMES: [&str; 15] = [
    "bm",
    "ep",
    "ebit_ev",
    "fcf_ev",
    "gp_assets",
    "roic",
    "net_debt_ebitda_inv",
    "interest_cov",
    "altman_z",
    "accruals_inv",
    "cfo_ni",
    "beneish_m_inv",
    "sales_cagr_3y",
    "asset_growth_inv",
    "net_share_issuance_inv",
];

#[derive(Debug, Clone)]
pub struct QuantSecurityResult {
    pub ticker: String,
    pub name: String,
    pub market: String,
    pub industry: String,
    pub as_of_date: AsOfDate,
    pub fin_record_id: String,
    pub filing_date: FilingDate,
    pub period_end: PeriodEndDate,
    pub raw_factors: BTreeMap<String, f64>,
    pub std_factors: BTreeMap<String, f64>,
    pub bucket_scores: BTreeMap<String, f64>,
    pub raw_accruals: f64,
    pub alpha_score: f64,
    pub risk_penalty: f64,
    pub quant_score: f64,
    pub hard_veto: bool,
    pub veto_reasons: Vec<String>,
    pub adv_usd: f64,
    pub delist_date: Option<String>,
    pub delist_return: Option<f64>,
    pub assumptions: Vec<String>,
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q * ((sorted.len() - 1) as f64);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = pos - (lo as f64);
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

fn winsorize_and_rank_to_signed(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    if n <= 1 {
        return vec![0.0; n];
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    if (sorted[n - 1] - sorted[0]).abs() <= 1e-12 {
        return vec![0.0; n];
    }
    let lo = quantile(&sorted, 0.05);
    let hi = quantile(&sorted, 0.95).max(lo);
    let clipped: Vec<f64> = values.iter().map(|v| v.clamp(lo, hi)).collect();

    let min_v = clipped.iter().copied().fold(f64::INFINITY, f64::min);
    let max_v = clipped.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if (max_v - min_v).abs() <= 1e-12 {
        return vec![0.0; n];
    }

    let mut indexed: Vec<(usize, f64)> = clipped.into_iter().enumerate().collect();
    indexed.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut ranks = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && (indexed[j + 1].1 - indexed[i].1).abs() <= 1e-12 {
            j += 1;
        }
        let avg_rank = 0.5 * ((i + j) as f64);
        let signed = 2.0 * (avg_rank / ((n - 1) as f64)) - 1.0;
        for item in indexed.iter().take(j + 1).skip(i) {
            ranks[item.0] = signed;
        }
        i = j + 1;
    }
    ranks
}

pub fn compute_raw_factors(row: &PitSecuritySnapshot) -> (BTreeMap<String, f64>, f64) {
    let market_cap = row.market_cap.max(1e-6);
    let ev = row.enterprise_value.max(1e-6);
    let total_assets = row.total_assets.max(1e-6);
    let prev_total_assets = row.prev_total_assets.max(1e-6);
    let avg_ta = 0.5 * (total_assets + prev_total_assets);
    let total_liabilities = row.total_liabilities.max(1e-6);
    let shares_out = row.shares_out.max(1e-6);
    let prev_shares_out = row.prev_shares_out.max(1e-6);
    let revenue_3y_ago = row.revenue_3y_ago.max(1e-6);
    let revenue = row.revenue.max(0.0);

    let fcf = row.cfo - row.capex;
    let net_debt = row.total_debt - row.cash;
    let wc = row.current_assets - row.current_liabilities;

    let altman_z = 1.2 * (wc / total_assets)
        + 1.4 * (row.retained_earnings / total_assets)
        + 3.3 * (row.ebit / total_assets)
        + 0.6 * (market_cap / total_liabilities)
        + 1.0 * (revenue / total_assets);

    let raw_accruals = (row.net_income - row.cfo) / avg_ta;
    let cfo_ni = if row.net_income.abs() > 1e-9 {
        row.cfo / row.net_income.abs()
    } else {
        0.0
    };
    let sales_cagr = if revenue > 0.0 {
        (revenue / revenue_3y_ago).powf(1.0 / 3.0) - 1.0
    } else {
        -1.0
    };

    let mut f = BTreeMap::new();
    f.insert("bm".into(), row.book_value / market_cap);
    f.insert("ep".into(), row.net_income / market_cap);
    f.insert("ebit_ev".into(), row.ebit / ev);
    f.insert("fcf_ev".into(), fcf / ev);
    f.insert("gp_assets".into(), row.gross_profit / total_assets);
    f.insert("roic".into(), row.roic);
    f.insert(
        "net_debt_ebitda_inv".into(),
        -(net_debt / row.ebit.max(1e-6)),
    );
    f.insert(
        "interest_cov".into(),
        row.ebit / row.interest_expense.max(1e-6),
    );
    f.insert("altman_z".into(), altman_z);
    f.insert("accruals_inv".into(), -raw_accruals);
    f.insert("cfo_ni".into(), cfo_ni);
    f.insert("beneish_m_inv".into(), -row.beneish_m);
    f.insert("sales_cagr_3y".into(), sales_cagr);
    f.insert(
        "asset_growth_inv".into(),
        -((total_assets - prev_total_assets) / prev_total_assets),
    );
    f.insert(
        "net_share_issuance_inv".into(),
        -((shares_out - prev_shares_out) / prev_shares_out),
    );
    (f, raw_accruals)
}

pub fn score_pit_universe(
    snapshots: &[PitSecuritySnapshot],
    as_of_date: &AsOfDate,
    params: &FactorParams,
) -> Vec<QuantSecurityResult> {
    let (w, pen, veto) = (&params.weights, &params.penalty, &params.veto);
    if snapshots.is_empty() {
        return Vec::new();
    }

    let n = snapshots.len();
    let mut raw_list = Vec::with_capacity(n);
    let mut accruals_vec = Vec::with_capacity(n);

    for row in snapshots {
        // Compile-time & runtime PIT guard: FilingDate MUST be visible at as_of_date!
        debug_assert!(row.filing_date.is_visible_at(as_of_date));
        let (mut rf, raw_acc) = compute_raw_factors(row);
        rf.insert("momentum_12_1".into(), row.momentum_12_1);
        rf.insert("amihud_illiq".into(), row.amihud_illiq);
        raw_list.push(rf);
        accruals_vec.push(raw_acc);
    }

    let mut sorted_acc = accruals_vec.clone();
    sorted_acc.sort_by(|a, b| a.total_cmp(b));
    let accruals_veto_level = quantile(&sorted_acc, veto.accruals_quantile);

    // Group by (market, industry), falling back to (market, "__fallback__") if group < 3
    let mut initial_groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (idx, row) in snapshots.iter().enumerate() {
        initial_groups
            .entry((row.market.clone(), row.industry.clone()))
            .or_default()
            .push(idx);
    }

    let mut effective_groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    let mut assumptions = vec![Vec::<String>::new(); n];
    for ((mkt, ind), idxs) in initial_groups {
        if idxs.len() >= 3 {
            effective_groups.insert((mkt, ind), idxs);
        } else {
            for &i in &idxs {
                assumptions[i].push("standardized_by_market_fallback".into());
            }
            effective_groups
                .entry((mkt, "__fallback__".into()))
                .or_default()
                .extend(idxs);
        }
    }

    let mut all_keys: Vec<&str> = FACTOR_NAMES.to_vec();
    all_keys.push("momentum_12_1");
    all_keys.push("amihud_illiq");

    let mut std_rows: Vec<BTreeMap<String, f64>> = vec![BTreeMap::new(); n];
    for idxs in effective_groups.values() {
        for &fk in &all_keys {
            let vals: Vec<f64> = idxs.iter().map(|&i| raw_list[i][fk]).collect();
            let ranked = winsorize_and_rank_to_signed(&vals);
            for (pos, &i) in idxs.iter().enumerate() {
                std_rows[i].insert(fk.to_string(), ranked[pos]);
            }
        }
    }

    let mut results = Vec::with_capacity(n);
    for (idx, row) in snapshots.iter().enumerate() {
        let mut rf = raw_list[idx].clone();
        rf.remove("momentum_12_1");
        rf.remove("amihud_illiq");
        let sf = &std_rows[idx];
        let raw_acc = accruals_vec[idx];

        let val_b = (sf["bm"] + sf["ep"] + sf["ebit_ev"] + sf["fcf_ev"]) / 4.0;
        let qual_b = (sf["gp_assets"] + sf["roic"]) / 2.0;
        let health_b = (sf["altman_z"] + sf["interest_cov"] + sf["net_debt_ebitda_inv"]) / 3.0;
        let eq_b = (sf["accruals_inv"] + sf["cfo_ni"] + sf["beneish_m_inv"]) / 3.0;
        let cf_b = (sf["fcf_ev"] + sf["cfo_ni"]) / 2.0;
        let growth_b = (sf["sales_cagr_3y"] + sf["asset_growth_inv"]) / 2.0;
        let cap_b = sf["net_share_issuance_inv"];
        let mom_b = sf["momentum_12_1"];

        let alpha_score = w.valuation * val_b
            + w.quality * qual_b
            + w.financial_health * health_b
            + w.earnings_quality * eq_b
            + w.cash_flow * cf_b
            + w.growth * growth_b
            + w.capital_allocation * cap_b;

        let mut bucket_scores = BTreeMap::new();
        bucket_scores.insert("valuation".into(), round4(val_b));
        bucket_scores.insert("quality_profitability".into(), round4(qual_b));
        bucket_scores.insert("financial_health".into(), round4(health_b));
        bucket_scores.insert("earnings_quality".into(), round4(eq_b));
        bucket_scores.insert("cash_flow".into(), round4(cf_b));
        bucket_scores.insert("growth_conservative".into(), round4(growth_b));
        bucket_scores.insert("capital_allocation".into(), round4(cap_b));
        bucket_scores.insert("momentum_tiebreak".into(), round4(mom_b));

        let mut d_pen = 0.0;
        if rf["altman_z"] < pen.altman_z_below {
            d_pen += 0.5;
        }
        if rf["interest_cov"] < pen.interest_cov_below {
            d_pen += 0.5;
        }
        let mut m_pen = 0.0;
        if row.beneish_m > pen.beneish_m_above {
            m_pen += 0.5;
        }
        if raw_acc > pen.accruals_above {
            m_pen += 0.5;
        }
        let l_pen = sf["amihud_illiq"].max(0.0);
        let risk_penalty = pen.distress_weight * d_pen
            + pen.manipulation_weight * m_pen
            + pen.illiquidity_weight * l_pen;
        let quant_score =
            (alpha_score + params.momentum_tiebreak * mom_b - risk_penalty).clamp(-1.0, 1.0);

        let mut veto_reasons = Vec::new();
        if rf["altman_z"] < veto.altman_z_below {
            veto_reasons.push(format!(
                "distress_zscore(Z={:.2}<{:.2})",
                rf["altman_z"], veto.altman_z_below
            ));
        }
        if rf["interest_cov"] < veto.interest_cov_below {
            veto_reasons.push(format!(
                "low_interest_coverage(IC={:.2}<{:.2})",
                rf["interest_cov"], veto.interest_cov_below
            ));
        }
        if row.beneish_m > veto.beneish_m_above && raw_acc >= accruals_veto_level {
            veto_reasons.push(format!(
                "earnings_manipulation_risk(M={:.2}>{:.2},Acc={:.3})",
                row.beneish_m, veto.beneish_m_above, raw_acc
            ));
        }
        if row.cfo < 0.0 && row.net_income > 0.0 {
            veto_reasons.push("negative_cfo_positive_ni".into());
        }
        if row.adv_usd < params.min_adv_usd || row.is_halted || row.is_st {
            veto_reasons.push("illiquidity_or_restricted".into());
        }

        results.push(QuantSecurityResult {
            ticker: row.ticker.clone(),
            name: row.name.clone(),
            market: row.market.clone(),
            industry: row.industry.clone(),
            as_of_date: as_of_date.clone(),
            fin_record_id: row.fin_record_id.clone(),
            filing_date: row.filing_date.clone(),
            period_end: row.period_end.clone(),
            raw_factors: rf,
            std_factors: sf.clone(),
            bucket_scores,
            raw_accruals: raw_acc,
            alpha_score: round4(alpha_score),
            risk_penalty: round4(risk_penalty),
            quant_score: round4(quant_score),
            hard_veto: !veto_reasons.is_empty(),
            veto_reasons,
            adv_usd: row.adv_usd,
            delist_date: row.delist_date.clone(),
            delist_return: row.delist_return,
            assumptions: assumptions[idx].clone(),
        });
    }

    results.sort_by(|a, b| {
        a.hard_veto
            .cmp(&b.hard_veto)
            .then_with(|| b.quant_score.total_cmp(&a.quant_score))
            .then_with(|| a.ticker.cmp(&b.ticker))
    });
    results
}
