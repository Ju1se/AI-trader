//! Investment Committee aggregation, dispersion escalation, and constrained portfolio construction.
//! Lives inside `trade-research` (zero database or execution coupling).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use trade_core::{AgentEvidenceOutput, CommitteeDecision, UsdCents, WeightBps, round4};
use trade_quant::QuantSecurityResult;

use trade_core::ReadOnlyPitView;

/// Committee weights, escalation rule and portfolio limits. Loaded from
/// `config/strategy.toml`, so trying other values needs no recompilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InvestmentCommittee {
    pub weights: BTreeMap<String, f64>,
    /// Escalate a security to a person when the agents disagree more than this.
    pub dispersion_threshold: f64,
    /// ...but only if the consensus is above this score.
    pub escalation_consensus_floor: f64,
    pub max_single_weight: WeightBps,
    pub max_industry_weight: WeightBps,
    /// Largest share of a security's average daily volume one position may take.
    pub max_adv_participation: f64,
}

impl Default for InvestmentCommittee {
    fn default() -> Self {
        let weights = [
            ("quant", 0.45),
            ("moat", 0.20),
            ("management_capalloc", 0.15),
            ("footnote_forensics", 0.20),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        Self {
            weights,
            dispersion_threshold: 0.45,
            escalation_consensus_floor: -0.10,
            max_single_weight: WeightBps::new(3_500).expect("valid bps"),
            max_industry_weight: WeightBps::new(6_500).expect("valid bps"),
            max_adv_participation: 0.10,
        }
    }
}

/// The committee's portfolio: target weights, the vetoed securities with their reasons, and
/// the selected securities whose weight was halved pending a person's review.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetPortfolio {
    pub weights: BTreeMap<String, WeightBps>,
    pub vetoed: BTreeMap<String, Vec<String>>,
    pub escalated: Vec<String>,
}

impl InvestmentCommittee {
    pub fn evaluate_security(
        &self,
        quant: &QuantSecurityResult,
        worker_outputs: Vec<AgentEvidenceOutput>,
        view: &dyn ReadOnlyPitView,
    ) -> CommitteeDecision {
        let mut veto_reasons = quant.veto_reasons.clone();
        for out in &worker_outputs {
            if out.hard_veto {
                veto_reasons.push(format!(
                    "{}_hard_veto:{}",
                    out.agent,
                    out.evidence.join(";")
                ));
            }
        }
        let hard_veto = quant.hard_veto || worker_outputs.iter().any(|o| o.hard_veto);

        let mut components: Vec<(f64, f64, f64)> = vec![(
            self.weights.get("quant").copied().unwrap_or(0.45),
            view.get_agent_calibration("quant"),
            quant.quant_score,
        )];
        for out in &worker_outputs {
            let w = self.weights.get(&out.agent).copied().unwrap_or(0.15);
            components.push((w, out.confidence, out.score));
        }

        let den: f64 = components.iter().map(|(w, c, _)| w * c).sum();
        let (consensus, dispersion) = if den <= 1e-9 {
            (quant.quant_score, 0.0)
        } else {
            let mean_s: f64 = components.iter().map(|(w, c, s)| w * c * s).sum::<f64>() / den;
            let var: f64 = components
                .iter()
                .map(|(w, c, s)| w * c * (s - mean_s).powi(2))
                .sum::<f64>()
                / den;
            (mean_s, var.max(0.0).sqrt())
        };

        let escalate_review = !hard_veto
            && dispersion > self.dispersion_threshold
            && consensus > self.escalation_consensus_floor;

        CommitteeDecision {
            ticker: quant.ticker.clone(),
            as_of_date: quant.as_of_date.clone(),
            quant_base_score: round4(quant.quant_score),
            consensus_score: round4(consensus),
            dispersion: round4(dispersion),
            hard_veto,
            veto_reasons,
            escalate_review,
            worker_outputs,
        }
    }

    pub fn build_target_weights(
        &self,
        decisions: &[CommitteeDecision],
        quant_by_ticker: &BTreeMap<String, QuantSecurityResult>,
        portfolio_capital: UsdCents,
        max_positions: usize,
    ) -> TargetPortfolio {
        let mut vetoed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for d in decisions {
            if d.hard_veto {
                vetoed.insert(d.ticker.clone(), d.veto_reasons.clone());
            }
        }

        let mut eligible: Vec<&CommitteeDecision> = decisions
            .iter()
            .filter(|d| !d.hard_veto && d.consensus_score > 0.0)
            .collect();
        eligible.sort_by(|a, b| {
            b.consensus_score
                .total_cmp(&a.consensus_score)
                .then_with(|| a.ticker.cmp(&b.ticker))
        });
        let chosen: Vec<&CommitteeDecision> = eligible.into_iter().take(max_positions).collect();

        let mut weights: BTreeMap<String, WeightBps> = BTreeMap::new();
        let mut escalated_selected: Vec<String> = Vec::new();
        if chosen.is_empty() {
            return TargetPortfolio {
                weights,
                vetoed,
                escalated: escalated_selected,
            };
        }

        let raw_scores: Vec<f64> = chosen
            .iter()
            .map(|d| (d.consensus_score + 0.5).max(0.05))
            .collect();
        let total_raw: f64 = raw_scores.iter().sum();
        let cap_usd = portfolio_capital.to_dollars().max(1.0);
        let mut industry_used_bps: BTreeMap<String, u16> = BTreeMap::new();

        for (d, raw_s) in chosen.iter().zip(raw_scores.iter()) {
            let q = &quant_by_ticker[&d.ticker];
            let mut w_ratio = raw_s / total_raw;
            if d.escalate_review {
                w_ratio *= 0.5;
                escalated_selected.push(d.ticker.clone());
            }
            let raw_bps = WeightBps::from_ratio_clamped(w_ratio).bps();
            let adv_bps =
                WeightBps::from_ratio_clamped((self.max_adv_participation * q.adv_usd) / cap_usd)
                    .bps();
            let used_ind = industry_used_bps.get(&q.industry).copied().unwrap_or(0);
            let rem_ind = self.max_industry_weight.bps().saturating_sub(used_ind);

            let final_bps = raw_bps
                .min(self.max_single_weight.bps())
                .min(adv_bps)
                .min(rem_ind);
            if final_bps > 0 {
                weights.insert(d.ticker.clone(), WeightBps::new(final_bps).unwrap());
                industry_used_bps.insert(q.industry.clone(), used_ind.saturating_add(final_bps));
            }
        }

        let sum_bps: u32 = weights.values().map(|w| u32::from(w.bps())).sum();
        if sum_bps > 10_000 {
            for val in weights.values_mut() {
                let scaled = ((u32::from(val.bps()) * 10_000) / sum_bps) as u16;
                *val = WeightBps::new(scaled).unwrap();
            }
        }

        TargetPortfolio {
            weights,
            vetoed,
            escalated: escalated_selected,
        }
    }
}
