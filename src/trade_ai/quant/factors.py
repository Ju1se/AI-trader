"""Deterministic 15-factor value/quality/health engine and Hard Veto overlay."""

from __future__ import annotations

from dataclasses import dataclass, field
import math
from statistics import mean
from typing import Any, Sequence

FACTOR_NAMES: tuple[str, ...] = (
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
)


@dataclass
class QuantSecurityResult:
    """Deterministic quantitative feature packet and score for one security."""

    ticker: str
    name: str
    market: str
    industry: str
    as_of_date: str
    fin_record_id: str
    filing_date: str
    period_end: str
    raw_factors: dict[str, float]
    std_factors: dict[str, float]
    bucket_scores: dict[str, float]
    alpha_score: float
    risk_penalty: float
    quant_score: float
    hard_veto: bool
    veto_reasons: list[str]
    adv_usd: float
    forward_1m_return: float
    delist_date: str | None
    delist_return: float | None
    assumptions: list[str] = field(default_factory=list)


def _quantile(sorted_vals: list[float], q: float) -> float:
    if not sorted_vals:
        return 0.0
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    pos = q * (len(sorted_vals) - 1)
    lo = int(math.floor(pos))
    hi = int(math.ceil(pos))
    if lo == hi:
        return sorted_vals[lo]
    frac = pos - lo
    return sorted_vals[lo] * (1.0 - frac) + sorted_vals[hi] * frac


def _winsorize_and_rank_to_signed(values: list[float]) -> list[float]:
    n = len(values)
    if n <= 1:
        return [0.0] * n
    sorted_vals = sorted(values)
    lo = _quantile(sorted_vals, 0.05)
    hi = _quantile(sorted_vals, 0.95)
    clipped = [min(max(v, lo), hi) for v in values]

    if max(clipped) - min(clipped) <= 1e-12:
        return [0.0] * n

    # Average-tie rank mapping into [-1.0, 1.0]
    indexed = sorted(enumerate(clipped), key=lambda item: item[1])
    ranks = [0.0] * n
    i = 0
    while i < n:
        j = i
        while j + 1 < n and abs(indexed[j + 1][1] - indexed[i][1]) <= 1e-12:
            j += 1
        avg_rank = 0.5 * (i + j)
        for k in range(i, j + 1):
            ranks[indexed[k][0]] = 2.0 * (avg_rank / (n - 1)) - 1.0
        i = j + 1
    return ranks


def compute_raw_factors(row: dict[str, Any]) -> tuple[dict[str, float], float]:
    """Compute the 15 baseline factors + raw accruals from a PIT fundamental/price row."""
    market_cap = max(float(row["market_cap"]), 1e-6)
    ev = max(float(row["enterprise_value"]), 1e-6)
    total_assets = max(float(row["total_assets"]), 1e-6)
    prev_total_assets = max(float(row["prev_total_assets"]), 1e-6)
    avg_ta = 0.5 * (total_assets + prev_total_assets)
    total_liabilities = max(float(row["total_liabilities"]), 1e-6)
    shares_out = max(float(row["shares_out"]), 1e-6)
    prev_shares_out = max(float(row["prev_shares_out"]), 1e-6)
    revenue_3y_ago = max(float(row["revenue_3y_ago"]), 1e-6)
    revenue = max(float(row["revenue"]), 0.0)

    ebit = float(row["ebit"])
    net_income = float(row["net_income"])
    cfo = float(row["cfo"])
    capex = float(row["capex"])
    fcf = cfo - capex
    net_debt = float(row["total_debt"]) - float(row["cash"])
    wc = float(row["current_assets"]) - float(row["current_liabilities"])

    altman_z = (
        1.2 * (wc / total_assets)
        + 1.4 * (float(row["retained_earnings"]) / total_assets)
        + 3.3 * (ebit / total_assets)
        + 0.6 * (market_cap / total_liabilities)
        + 1.0 * (revenue / total_assets)
    )
    raw_accruals = (net_income - cfo) / avg_ta
    cfo_ni = (cfo / abs(net_income)) if abs(net_income) > 1e-9 else 0.0
    sales_cagr = (revenue / revenue_3y_ago) ** (1.0 / 3.0) - 1.0 if revenue > 0 else -1.0

    factors = {
        "bm": float(row["book_value"]) / market_cap,
        "ep": net_income / market_cap,
        "ebit_ev": ebit / ev,
        "fcf_ev": fcf / ev,
        "gp_assets": float(row["gross_profit"]) / total_assets,
        "roic": float(row["roic"]),
        "net_debt_ebitda_inv": -(net_debt / max(ebit, 1e-6)),
        "interest_cov": ebit / max(float(row["interest_expense"]), 1e-6),
        "altman_z": altman_z,
        "accruals_inv": -raw_accruals,
        "cfo_ni": cfo_ni,
        "beneish_m_inv": -float(row["beneish_m"]),
        "sales_cagr_3y": sales_cagr,
        "asset_growth_inv": -((total_assets - prev_total_assets) / prev_total_assets),
        "net_share_issuance_inv": -((shares_out - prev_shares_out) / prev_shares_out),
    }
    return factors, raw_accruals


def standardize_cross_section(
    rows: list[dict[str, Any]],
    factor_keys: Sequence[str],
    by: tuple[str, ...] = ("market", "industry"),
) -> tuple[list[dict[str, float]], list[list[str]]]:
    """
    Group-wise winsorize (5%/95%) + cross-sectional rank mapping to [-1, 1].
    Falls back to market-level grouping if an industry group has fewer than 3 securities.
    """
    n = len(rows)
    std_out: list[dict[str, float]] = [{} for _ in range(n)]
    assumptions: list[list[str]] = [[] for _ in range(n)]
    if n == 0:
        return std_out, assumptions

    groups: dict[tuple[Any, ...], list[int]] = {}
    for idx, row in enumerate(rows):
        key = tuple(row[col] for col in by)
        groups.setdefault(key, []).append(idx)

    effective_groups: dict[tuple[Any, ...], list[int]] = {}
    for key, idxs in groups.items():
        if len(idxs) >= 3 or by == ("market",):
            effective_groups[key] = idxs
        else:
            fallback_key = (rows[idxs[0]]["market"], "__fallback__")
            effective_groups.setdefault(fallback_key, []).extend(idxs)
            for idx in idxs:
                assumptions[idx].append("standardized_by_market_fallback")

    for idxs in effective_groups.values():
        for fk in factor_keys:
            vals = [float(rows[idx]["_raw_for_std"][fk]) for idx in idxs]
            ranked = _winsorize_and_rank_to_signed(vals)
            for pos, idx in enumerate(idxs):
                std_out[idx][fk] = ranked[pos]

    return std_out, assumptions


def score_pit_universe(
    snapshot_rows: list[dict[str, Any]],
    as_of_date: str,
    min_adv_usd: float = 1_000_000.0,
) -> list[QuantSecurityResult]:
    """Score a Point-in-Time universe using the 15 baseline factors + Hard Veto rules."""
    if not snapshot_rows:
        return []

    enriched: list[dict[str, Any]] = []
    accruals_list: list[float] = []
    for row in snapshot_rows:
        raw_factors, raw_accruals = compute_raw_factors(row)
        accruals_list.append(raw_accruals)
        copy_row = dict(row)
        copy_row["_raw_factors"] = raw_factors
        copy_row["_raw_accruals"] = raw_accruals
        copy_row["_raw_for_std"] = {
            **raw_factors,
            "momentum_12_1": float(row["momentum_12_1"]),
            "amihud_illiq": float(row["amihud_illiq"]),
        }
        enriched.append(copy_row)

    p80_accruals = _quantile(sorted(accruals_list), 0.80)
    std_keys = FACTOR_NAMES + ("momentum_12_1", "amihud_illiq")
    std_rows, std_assumptions = standardize_cross_section(enriched, std_keys, by=("market", "industry"))

    results: list[QuantSecurityResult] = []
    for idx, row in enumerate(enriched):
        rf = row["_raw_factors"]
        sf = std_rows[idx]

        # Main Alpha Buckets (deep-research-report.md weights)
        val_b = mean([sf["bm"], sf["ep"], sf["ebit_ev"], sf["fcf_ev"]])
        qual_b = mean([sf["gp_assets"], sf["roic"]])
        health_b = mean([sf["altman_z"], sf["interest_cov"], sf["net_debt_ebitda_inv"]])
        eq_b = mean([sf["accruals_inv"], sf["cfo_ni"], sf["beneish_m_inv"]])
        cf_b = mean([sf["fcf_ev"], sf["cfo_ni"]])
        growth_b = mean([sf["sales_cagr_3y"], sf["asset_growth_inv"]])
        cap_b = sf["net_share_issuance_inv"]
        mom_b = sf["momentum_12_1"]

        alpha_score = (
            0.30 * val_b
            + 0.20 * qual_b
            + 0.15 * health_b
            + 0.10 * eq_b
            + 0.10 * cf_b
            + 0.10 * growth_b
            + 0.05 * cap_b
        )
        bucket_scores = {
            "valuation": round(val_b, 4),
            "quality_profitability": round(qual_b, 4),
            "financial_health": round(health_b, 4),
            "earnings_quality": round(eq_b, 4),
            "cash_flow": round(cf_b, 4),
            "growth_conservative": round(growth_b, 4),
            "capital_allocation": round(cap_b, 4),
            "momentum_tiebreak": round(mom_b, 4),
        }

        # Soft Risk Penalties (D_i, M_i, L_i)
        d_pen = 0.0
        if rf["altman_z"] < 2.99:
            d_pen += 0.5
        if rf["interest_cov"] < 4.0:
            d_pen += 0.5

        m_pen = 0.0
        if float(row["beneish_m"]) > -2.20:
            m_pen += 0.5
        if row["_raw_accruals"] > 0.05:
            m_pen += 0.5

        l_pen = max(0.0, sf["amihud_illiq"])
        risk_penalty = 0.15 * d_pen + 0.15 * m_pen + 0.10 * l_pen

        quant_score = max(-1.0, min(1.0, alpha_score + 0.05 * mom_b - risk_penalty))

        # Hard Veto Rules
        veto_reasons: list[str] = []
        if rf["altman_z"] < 1.8:
            veto_reasons.append(f"distress_zscore(Z={rf['altman_z']:.2f}<1.80)")
        if rf["interest_cov"] < 1.5:
            veto_reasons.append(f"low_interest_coverage(IC={rf['interest_cov']:.2f}<1.50)")
        if float(row["beneish_m"]) > -1.78 and row["_raw_accruals"] >= p80_accruals:
            veto_reasons.append(
                f"earnings_manipulation_risk(M={float(row['beneish_m']):.2f}>-1.78,Acc={row['_raw_accruals']:.3f})"
            )
        if float(row["cfo"]) < 0 and float(row["net_income"]) > 0:
            veto_reasons.append("negative_cfo_positive_ni")
        if float(row["adv_usd"]) < min_adv_usd or bool(row["is_halted"]) or bool(row["is_st"]):
            veto_reasons.append("illiquidity_or_restricted")

        results.append(
            QuantSecurityResult(
                ticker=row["ticker"],
                name=row["name"],
                market=row["market"],
                industry=row["industry"],
                as_of_date=as_of_date,
                fin_record_id=row["fin_record_id"],
                filing_date=row["filing_date"],
                period_end=row["period_end"],
                raw_factors=rf,
                std_factors=sf,
                bucket_scores=bucket_scores,
                alpha_score=round(alpha_score, 4),
                risk_penalty=round(risk_penalty, 4),
                quant_score=round(quant_score, 4),
                hard_veto=len(veto_reasons) > 0,
                veto_reasons=veto_reasons,
                adv_usd=float(row["adv_usd"]),
                forward_1m_return=float(row["forward_1m_return"]),
                delist_date=row.get("delist_date"),
                delist_return=row.get("delist_return"),
                assumptions=std_assumptions[idx],
            )
        )

    results.sort(key=lambda r: (r.hard_veto, -r.quant_score, r.ticker))
    return results
