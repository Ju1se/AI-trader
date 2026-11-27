"""Deterministic quantitative factor engine and pitfall-safe PIT backtester."""

from trade_ai.quant.backtest import BacktestResult, audit_survivorship_bias_exercise, run_pit_backtest
from trade_ai.quant.factors import FACTOR_NAMES, QuantSecurityResult, score_pit_universe, standardize_cross_section

__all__ = [
    "FACTOR_NAMES",
    "QuantSecurityResult",
    "BacktestResult",
    "score_pit_universe",
    "standardize_cross_section",
    "run_pit_backtest",
    "audit_survivorship_bias_exercise",
]
