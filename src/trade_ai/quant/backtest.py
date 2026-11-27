"""Pitfall-safe Point-in-Time backtester addressing all 5 pitfalls in the exercise."""

from __future__ import annotations

from dataclasses import dataclass
import math
from statistics import mean, stdev
from typing import Any, Sequence

from trade_ai.quant.factors import QuantSecurityResult, score_pit_universe
from trade_ai.store import StateAndPITStore


@dataclass(frozen=True)
class PeriodBacktestRecord:
    rebalance_date: str
    selected_tickers: list[str]
    vetoed_tickers: list[str]
    weights: dict[str, float]
    turnover: float
    cost_drag: float
    gross_return: float
    net_return: float
    rank_ic: float | None


@dataclass(frozen=True)
class WindowMetrics:
    periods: int
    cumulative_return: float
    annualized_return: float
    annualized_volatility: float
    sharpe_ratio: float
    max_drawdown: float
    avg_turnover: float
    avg_rank_ic: float | None


@dataclass(frozen=True)
class BacktestResult:
    markets: tuple[str, ...]
    one_way_cost_bps: float
    max_adv_participation: float
    buffer_band_pct: float
    periods: list[PeriodBacktestRecord]
    overall: WindowMetrics
    in_sample: WindowMetrics | None
    out_of_sample: WindowMetrics | None


def audit_survivorship_bias_exercise() -> dict[str, float]:
    """
    Reproduce the exact textbook Survivorship Bias exercise from
    '资料/8 Common Backtesting Pitfalls Exercise.pdf':
      Stock A: +30% (still trading)
      Stock B: -20% (still trading)
      Stock C: -90% (delisted liquidation)
    """
    survivors = [0.30, -0.20]
    all_stocks = [0.30, -0.20, -0.90]
    survivors_avg = mean(survivors)
    all_avg = mean(all_stocks)
    bias = survivors_avg - all_avg
    return {
        "survivors_only_return": round(survivors_avg, 4),
        "survivor_bias_free_return": round(all_avg, 4),
        "artificial_gain_pct_points": round(bias * 100.0, 1),
    }


def _rankdata(xs: list[float]) -> list[float]:
    n = len(xs)
    indexed = sorted(enumerate(xs), key=lambda item: item[1])
    ranks = [0.0] * n
    i = 0
    while i < n:
        j = i
        while j + 1 < n and abs(indexed[j + 1][1] - indexed[i][1]) <= 1e-12:
            j += 1
        avg_rank = 0.5 * (i + j) + 1.0
        for k in range(i, j + 1):
            ranks[indexed[k][0]] = avg_rank
        i = j + 1
    return ranks


def _spearman_ic(scores: list[float], returns: list[float]) -> float | None:
    if len(scores) < 3:
        return None
    rx = _rankdata(scores)
    ry = _rankdata(returns)
    mx = mean(rx)
    my = mean(ry)
    num = sum((a - mx) * (b - my) for a, b in zip(rx, ry))
    den_x = math.sqrt(sum((a - mx) ** 2 for a in rx))
    den_y = math.sqrt(sum((b - my) ** 2 for b in ry))
    if den_x <= 1e-12 or den_y <= 1e-12:
        return 0.0
    return round(num / (den_x * den_y), 4)


def _compute_window_metrics(records: list[PeriodBacktestRecord]) -> WindowMetrics:
    if not records:
        return WindowMetrics(0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, None)
    rets = [r.net_return for r in records]
    nav = 1.0
    peak = 1.0
    max_dd = 0.0
    for r in rets:
        nav *= 1.0 + r
        peak = max(peak, nav)
        dd = (nav / peak) - 1.0
        max_dd = min(max_dd, dd)

    n = len(rets)
    cum_ret = nav - 1.0
    ann_ret = (nav ** (12.0 / n) - 1.0) if nav > 0 else -1.0
    ann_vol = (stdev(rets) * math.sqrt(12.0)) if n >= 2 else 0.0
    sharpe = (ann_ret / ann_vol) if ann_vol > 1e-9 else 0.0
    ics = [r.rank_ic for r in records if r.rank_ic is not None]
    return WindowMetrics(
        periods=n,
        cumulative_return=round(cum_ret, 4),
        annualized_return=round(ann_ret, 4),
        annualized_volatility=round(ann_vol, 4),
        sharpe_ratio=round(sharpe, 4),
        max_drawdown=round(max_dd, 4),
        avg_turnover=round(mean(r.turnover for r in records), 4),
        avg_rank_ic=round(mean(ics), 4) if ics else None,
    )


def _realized_holding_return(res: QuantSecurityResult, next_rebalance_date: str | None) -> float:
    """Include delisting liquidation return if the security delists during the holding window."""
    if res.delist_date and res.delist_return is not None:
        if next_rebalance_date is None or res.delist_date <= next_rebalance_date:
            return float(res.delist_return)
    return float(res.forward_1m_return)


def run_pit_backtest(
    store: StateAndPITStore,
    rebalance_dates: Sequence[str],
    markets: Sequence[str] = ("US",),
    top_n: int = 3,
    portfolio_capital_usd: float = 1_000_000.0,
    one_way_cost_bps: float = 15.0,
    max_adv_participation: float = 0.10,
    max_single_weight: float = 0.40,
    buffer_band_pct: float = 0.20,
    min_adv_usd: float = 1_000_000.0,
    oos_start_date: str | None = None,
) -> BacktestResult:
    """
    Execute a survivor-bias-free, look-ahead-free, cost- and ADV-constrained backtest.
    """
    prev_weights: dict[str, float] = {}
    records: list[PeriodBacktestRecord] = []
    sorted_dates = list(rebalance_dates)

    for idx, dt in enumerate(sorted_dates):
        next_dt = sorted_dates[idx + 1] if idx + 1 < len(sorted_dates) else None
        snapshot = store.get_pit_snapshot(as_of_date=dt, markets=markets)
        scored = score_pit_universe(snapshot, as_of_date=dt, min_adv_usd=min_adv_usd)

        eligible = [r for r in scored if not r.hard_veto]
        vetoed = [r.ticker for r in scored if r.hard_veto]

        # Buffer band retention: existing holdings inside top ceil(top_n * (1 + buffer_band_pct)) stay
        buffer_cutoff = max(top_n, int(math.ceil(top_n * (1.0 + buffer_band_pct))))
        buffer_tickers = {r.ticker for r in eligible[:buffer_cutoff]}
        retained = [r.ticker for r in eligible[:buffer_cutoff] if r.ticker in prev_weights]

        selected: list[str] = []
        for t in retained:
            if len(selected) < top_n:
                selected.append(t)
        for r in eligible:
            if len(selected) >= top_n:
                break
            if r.ticker not in selected:
                selected.append(r.ticker)

        # Equal weight capped by max_single_weight & ADV participation constraint
        by_ticker = {r.ticker: r for r in scored}
        new_weights: dict[str, float] = {}
        if selected:
            raw_w = 1.0 / len(selected)
            for t in selected:
                adv_cap_w = (max_adv_participation * by_ticker[t].adv_usd) / max(portfolio_capital_usd, 1.0)
                w = min(raw_w, max_single_weight, adv_cap_w)
                if w > 0:
                    new_weights[t] = round(w, 6)

        all_tickers = set(prev_weights) | set(new_weights)
        traded_frac = sum(abs(new_weights.get(t, 0.0) - prev_weights.get(t, 0.0)) for t in all_tickers)
        turnover = 0.5 * traded_frac
        cost_drag = traded_frac * (one_way_cost_bps / 10_000.0)

        gross_ret = sum(
            w * _realized_holding_return(by_ticker[t], next_dt)
            for t, w in new_weights.items()
            if t in by_ticker
        )
        net_ret = gross_ret - cost_drag

        ic_scores = [r.quant_score for r in eligible]
        ic_rets = [_realized_holding_return(r, next_dt) for r in eligible]
        rank_ic = _spearman_ic(ic_scores, ic_rets)

        records.append(
            PeriodBacktestRecord(
                rebalance_date=dt,
                selected_tickers=selected,
                vetoed_tickers=vetoed,
                weights=new_weights,
                turnover=round(turnover, 4),
                cost_drag=round(cost_drag, 6),
                gross_return=round(gross_ret, 6),
                net_return=round(net_ret, 6),
                rank_ic=rank_ic,
            )
        )
        prev_weights = new_weights

    overall = _compute_window_metrics(records)
    if oos_start_date:
        is_recs = [r for r in records if r.rebalance_date < oos_start_date]
        oos_recs = [r for r in records if r.rebalance_date >= oos_start_date]
        in_sample = _compute_window_metrics(is_recs)
        out_of_sample = _compute_window_metrics(oos_recs)
    else:
        mid = len(records) // 2
        in_sample = _compute_window_metrics(records[:mid]) if mid > 0 else None
        out_of_sample = _compute_window_metrics(records[mid:]) if records[mid:] else None

    return BacktestResult(
        markets=tuple(markets),
        one_way_cost_bps=one_way_cost_bps,
        max_adv_participation=max_adv_participation,
        buffer_band_pct=buffer_band_pct,
        periods=records,
        overall=overall,
        in_sample=in_sample,
        out_of_sample=out_of_sample,
    )
