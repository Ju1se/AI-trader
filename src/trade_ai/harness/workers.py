"""Read-only specialist Workers (Moat, Management/CapAlloc, Footnote Forensics)."""

from __future__ import annotations

import json
import os
import urllib.request
from typing import Any

from trade_ai.quant.factors import QuantSecurityResult
from trade_ai.schemas import AgentEvidenceOutput, UntrustedContextItem
from trade_ai.store import ReadOnlyDataView


class LLMJsonReasoner:
    """
    Optional external LLM adapter (routed outside worker sandbox using CredentialVault).
    Falls back cleanly to deterministic structured evaluation when offline or unconfigured.
    """

    def __init__(self, api_key: str | None = None, endpoint: str | None = None, model: str = "gpt-4o-mini") -> None:
        self.api_key = api_key or os.environ.get("TRADE_AI_LLM_API_KEY")
        self.endpoint = endpoint or os.environ.get("TRADE_AI_LLM_ENDPOINT")
        self.model = model

    @property
    def is_configured(self) -> bool:
        return bool(self.api_key and self.endpoint)

    def summarize_json(self, system_prompt: str, user_payload: str) -> dict[str, Any] | None:
        if not self.is_configured:
            return None
        req = urllib.request.Request(
            self.endpoint,  # type: ignore[arg-type]
            data=json.dumps(
                {
                    "model": self.model,
                    "messages": [
                        {"role": "system", "content": system_prompt},
                        {"role": "user", "content": user_payload},
                    ],
                    "response_format": {"type": "json_object"},
                }
            ).encode("utf-8"),
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {self.api_key}",
            },
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=15) as resp:
            body = json.loads(resp.read().decode("utf-8"))
        content = body["choices"][0]["message"]["content"]
        return json.loads(content)


def _clamp(val: float, lo: float = -1.0, hi: float = 1.0) -> float:
    return max(lo, min(hi, val))


def _scan_keywords(
    items: list[UntrustedContextItem],
    positive_terms: tuple[str, ...],
    negative_terms: tuple[str, ...],
) -> tuple[list[str], list[str], list[str], bool]:
    pos_hits: list[str] = []
    neg_hits: list[str] = []
    cited_ids: list[str] = []
    injection_seen = False

    for item in items:
        # Wrap explicitly in <untrusted_data> envelope so downstream models treat text purely as data
        _ = item.to_prompt_envelope()
        if item.injection_flagged:
            injection_seen = True
        low = item.content.lower()
        matched = False
        for term in positive_terms:
            if term in low:
                pos_hits.append(f"{term}({item.record_id})")
                matched = True
        for term in negative_terms:
            if term in low:
                neg_hits.append(f"{term}({item.record_id})")
                matched = True
        if matched or item.injection_flagged:
            cited_ids.append(item.record_id)

    return pos_hits, neg_hits, cited_ids, injection_seen


class MoatWorker:
    """Evaluates competitive advantage persistence: ROIC-WACC spread, gross profitability & moat filings."""

    agent_name = "moat"

    def evaluate(
        self,
        data_view: ReadOnlyDataView,
        snap_row: dict[str, Any],
        quant_res: QuantSecurityResult,
        as_of_date: str,
        reasoner: LLMJsonReasoner | None = None,
    ) -> AgentEvidenceOutput:
        ticker = snap_row["ticker"]
        filings = data_view.get_pit_filings(ticker, as_of_date)

        roic_spread = float(snap_row["roic"]) - float(snap_row["wacc"])
        gp_assets = quant_res.raw_factors["gp_assets"]
        sales_cagr = quant_res.raw_factors["sales_cagr_3y"]

        pos_terms = ("pricing power", "switching cost", "network effect", "high retention", "installed base", "scale advantage")
        neg_terms = ("margin compression", "commoditized", "price war", "customer churn", "lost market share")
        pos_hits, neg_hits, cited_ids, injection_seen = _scan_keywords(filings, pos_terms, neg_terms)

        # Base quantitative moat score
        score = 0.45 * _clamp(roic_spread / 0.12) + 0.30 * quant_res.std_factors["gp_assets"] + 0.15 * _clamp(sales_cagr / 0.10)
        score += 0.10 * min(len(pos_hits), 2) - 0.18 * min(len(neg_hits), 2)
        score = _clamp(score)

        if not cited_ids and filings:
            cited_ids = [filings[0].record_id]
        cited_ids = list(dict.fromkeys([quant_res.fin_record_id] + cited_ids))

        evidence = [
            f"ROIC-WACC={roic_spread:.2%}",
            f"GP/Assets={gp_assets:.2f}",
            f"SalesCAGR3Y={sales_cagr:.2%}",
        ]
        if pos_hits:
            evidence.append(f"MoatSignals={','.join(pos_hits[:3])}")
        if neg_hits:
            evidence.append(f"MoatRisks={','.join(neg_hits[:3])}")
        if injection_seen:
            evidence.append("UntrustedPromptInjectionIgnored=true")

        assumptions = list(quant_res.assumptions)
        if not filings:
            assumptions.append("no_text_filings_available_used_financials_only")
        if injection_seen:
            assumptions.append("untrusted_injection_detected_and_ignored")

        missingness = 0.0 if filings else 0.25
        calibrated_conf = data_view.get_agent_calibration(self.agent_name)
        confidence = round(max(0.1, min(1.0, calibrated_conf * (1.0 - missingness))), 4)

        out = AgentEvidenceOutput(
            ticker=ticker,
            as_of_date=as_of_date,
            agent=self.agent_name,
            score=round(score, 4),
            confidence=confidence,
            hard_veto=False,
            evidence=evidence,
            cited_record_ids=cited_ids,
            missingness=missingness,
            stale_data=False,
            assumptions=assumptions,
        )
        out.validate()
        return out


class ManagementCapAllocWorker:
    """Evaluates capital allocation, insider alignment, SBC dilution, and goodwill bloat."""

    agent_name = "management_capalloc"

    def evaluate(
        self,
        data_view: ReadOnlyDataView,
        snap_row: dict[str, Any],
        quant_res: QuantSecurityResult,
        as_of_date: str,
        reasoner: LLMJsonReasoner | None = None,
    ) -> AgentEvidenceOutput:
        ticker = snap_row["ticker"]
        filings = data_view.get_pit_filings(ticker, as_of_date)

        net_issuance = -quant_res.raw_factors["net_share_issuance_inv"]
        sbc_dilution = float(snap_row["sbc_expense"]) / max(float(snap_row["market_cap"]), 1e-6)
        insider_net = float(snap_row["insider_net_buy_usd"])
        goodwill_jump = (float(snap_row["goodwill"]) - float(snap_row["prev_goodwill"])) / max(
            float(snap_row["total_assets"]), 1e-6
        )

        pos_terms = ("roic-linked", "disciplined buyback", "insider purchase", "debt reduction", "dividend growth")
        neg_terms = ("overpaid acquisition", "goodwill impairment", "aggressive dilution", "insider selling", "related-party")
        pos_hits, neg_hits, cited_ids, injection_seen = _scan_keywords(filings, pos_terms, neg_terms)

        score = 0.35 * _clamp(-net_issuance / 0.04)
        score += 0.25 * _clamp(insider_net / 1_000_000.0)
        score -= 0.25 * _clamp(sbc_dilution / 0.03, 0.0, 1.0)
        score -= 0.35 * _clamp(goodwill_jump / 0.15, 0.0, 1.0)
        score += 0.10 * min(len(pos_hits), 2) - 0.15 * min(len(neg_hits), 2)
        score = _clamp(score)

        if not cited_ids and filings:
            cited_ids = [filings[0].record_id]
        cited_ids = list(dict.fromkeys([quant_res.fin_record_id] + cited_ids))

        evidence = [
            f"NetShareIssuance={net_issuance:.2%}",
            f"SBC/MCap={sbc_dilution:.2%}",
            f"InsiderNetBuyUSD={insider_net:,.0f}",
            f"GoodwillJump/TA={goodwill_jump:.2%}",
        ]
        if pos_hits:
            evidence.append(f"GovernancePositives={','.join(pos_hits[:3])}")
        if neg_hits:
            evidence.append(f"GovernanceFlags={','.join(neg_hits[:3])}")
        if injection_seen:
            evidence.append("UntrustedPromptInjectionIgnored=true")

        assumptions = []
        if injection_seen:
            assumptions.append("untrusted_injection_detected_and_ignored")

        missingness = 0.0 if filings else 0.20
        calibrated_conf = data_view.get_agent_calibration(self.agent_name)
        confidence = round(max(0.1, min(1.0, calibrated_conf * (1.0 - missingness))), 4)

        out = AgentEvidenceOutput(
            ticker=ticker,
            as_of_date=as_of_date,
            agent=self.agent_name,
            score=round(score, 4),
            confidence=confidence,
            hard_veto=False,
            evidence=evidence,
            cited_record_ids=cited_ids,
            missingness=missingness,
            stale_data=False,
            assumptions=assumptions,
        )
        out.validate()
        return out


class FootnoteForensicsWorker:
    """Evaluates earnings quality, accruals, Beneish M, and accounting footnote red flags."""

    agent_name = "footnote_forensics"

    def evaluate(
        self,
        data_view: ReadOnlyDataView,
        snap_row: dict[str, Any],
        quant_res: QuantSecurityResult,
        as_of_date: str,
        reasoner: LLMJsonReasoner | None = None,
    ) -> AgentEvidenceOutput:
        ticker = snap_row["ticker"]
        filings = data_view.get_pit_filings(ticker, as_of_date)

        beneish_m = float(snap_row["beneish_m"])
        raw_accruals = -quant_res.raw_factors["accruals_inv"]
        cfo_ni = quant_res.raw_factors["cfo_ni"]

        pos_terms = ("unqualified audit", "conservative accounting", "cash conversion")
        severe_terms = ("restatement", "material weakness", "sec investigation", "bill-and-hold", "going concern")
        pos_hits, severe_hits, cited_ids, injection_seen = _scan_keywords(filings, pos_terms, severe_terms)

        hard_veto = False
        if severe_hits:
            hard_veto = True
        if beneish_m > -1.78 and float(snap_row["cfo"]) <= 0:
            hard_veto = True

        score = (
            0.35 * quant_res.std_factors["accruals_inv"]
            + 0.35 * quant_res.std_factors["cfo_ni"]
            + 0.30 * quant_res.std_factors["beneish_m_inv"]
        )
        if severe_hits:
            score = -1.0
        elif pos_hits:
            score = _clamp(score + 0.10)

        if not cited_ids and filings:
            cited_ids = [filings[0].record_id]
        cited_ids = list(dict.fromkeys([quant_res.fin_record_id] + cited_ids))

        evidence = [
            f"BeneishM={beneish_m:.2f}",
            f"Accruals={raw_accruals:.3f}",
            f"CFO/NI={cfo_ni:.2f}",
        ]
        if severe_hits:
            evidence.append(f"FootnoteRedFlags={','.join(severe_hits)}")
        if injection_seen:
            evidence.append("UntrustedPromptInjectionIgnored=true")

        assumptions = []
        if injection_seen:
            assumptions.append("untrusted_injection_detected_and_ignored")

        missingness = 0.0 if filings else 0.25
        calibrated_conf = data_view.get_agent_calibration(self.agent_name)
        confidence = round(max(0.1, min(1.0, calibrated_conf * (1.0 - missingness))), 4)

        out = AgentEvidenceOutput(
            ticker=ticker,
            as_of_date=as_of_date,
            agent=self.agent_name,
            score=round(_clamp(score), 4),
            confidence=confidence,
            hard_veto=hard_veto,
            evidence=evidence,
            cited_record_ids=cited_ids,
            missingness=missingness,
            stale_data=False,
            assumptions=assumptions,
        )
        out.validate()
        return out
