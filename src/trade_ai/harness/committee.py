"""Investment Committee aggregation, dispersion escalation, and constrained portfolio construction."""

from __future__ import annotations

import math
from typing import Sequence

from trade_ai.quant.factors import QuantSecurityResult
from trade_ai.schemas import AgentEvidenceOutput, CommitteeDecision
from trade_ai.store import StateAndPITStore


class InvestmentCommittee:
    """
    Combines deterministic quant scores with read-only specialist worker evidence.
    Computes calibrated confidence-weighted consensus score and dispersion Disp_i.
    """

    DEFAULT_WEIGHTS: dict[str, float] = {
        "quant": 0.45,
        "moat": 0.20,
        "management_capalloc": 0.15,
        "footnote_forensics": 0.20,
    }

    def __init__(
        self,
        weights: dict[str, float] | None = None,
        dispersion_threshold: float = 0.45,
    ) -> None:
        self.weights = weights or dict(self.DEFAULT_WEIGHTS)
        self.dispersion_threshold = dispersion_threshold

    def evaluate_security(
        self,
        quant_res: QuantSecurityResult,
        worker_outputs: list[AgentEvidenceOutput],
        store: StateAndPITStore,
    ) -> CommitteeDecision:
        veto_reasons = list(quant_res.veto_reasons)
        for out in worker_outputs:
            if out.hard_veto:
                veto_reasons.append(f"{out.agent}_hard_veto:{';'.join(out.evidence)}")

        hard_veto = quant_res.hard_veto or any(o.hard_veto for o in worker_outputs)

        components: list[tuple[float, float, float]] = [
            (self.weights.get("quant", 0.45), store.get_agent_calibration("quant"), quant_res.quant_score)
        ]
        for out in worker_outputs:
            w = self.weights.get(out.agent, 0.15)
            components.append((w, out.confidence, out.score))

        den = sum(w * c for w, c, _ in components)
        if den <= 1e-9:
            consensus = quant_res.quant_score
            dispersion = 0.0
        else:
            consensus = sum(w * c * s for w, c, s in components) / den
            var = sum(w * c * ((s - consensus) ** 2) for w, c, s in components) / den
            dispersion = math.sqrt(max(0.0, var))

        escalate = (not hard_veto) and (dispersion > self.dispersion_threshold) and (consensus > -0.10)

        decision = CommitteeDecision(
            ticker=quant_res.ticker,
            as_of_date=quant_res.as_of_date,
            quant_base_score=round(quant_res.quant_score, 4),
            consensus_score=round(consensus, 4),
            dispersion=round(dispersion, 4),
            hard_veto=hard_veto,
            veto_reasons=veto_reasons,
            escalate_review=escalate,
            worker_outputs=worker_outputs,
        )
        decision.validate()
        return decision

    def build_target_weights(
        self,
        decisions: Sequence[CommitteeDecision],
        quant_by_ticker: dict[str, QuantSecurityResult],
        portfolio_capital_usd: float,
        max_positions: int,
        max_single_weight: float = 0.35,
        max_industry_weight: float = 0.60,
        max_adv_participation: float = 0.10,
    ) -> tuple[dict[str, float], dict[str, list[str]], list[str]]:
        """Build constrained long-only target weights excluding hard-vetoed securities."""
        vetoed: dict[str, list[str]] = {
            d.ticker: list(d.veto_reasons) for d in decisions if d.hard_veto
        }
        eligible = [d for d in decisions if not d.hard_veto and d.consensus_score > 0.0]
        eligible.sort(key=lambda d: (-d.consensus_score, d.ticker))
        chosen = eligible[:max_positions]

        weights: dict[str, float] = {}
        escalated: list[str] = []
        if not chosen:
            return weights, vetoed, escalated

        raw_scores = [max(0.05, d.consensus_score + 0.5) for d in chosen]
        total_raw = sum(raw_scores)
        industry_used: dict[str, float] = {}

        for d, raw_s in zip(chosen, raw_scores):
            q = quant_by_ticker[d.ticker]
            w = raw_s / total_raw
            if d.escalate_review:
                w *= 0.5
                escalated.append(d.ticker)
            adv_cap = (max_adv_participation * q.adv_usd) / max(portfolio_capital_usd, 1.0)
            ind_rem = max(0.0, max_industry_weight - industry_used.get(q.industry, 0.0))
            w = min(w, max_single_weight, adv_cap, ind_rem)
            if w > 1e-4:
                w = round(w, 4)
                weights[d.ticker] = w
                industry_used[q.industry] = industry_used.get(q.industry, 0.0) + w

        # Safety cap: total weight never exceeds 1.0
        total_w = sum(weights.values())
        if total_w > 1.0:
            scale = 1.0 / total_w
            weights = {k: round(v * scale, 4) for k, v in weights.items()}

        return weights, vetoed, escalated
