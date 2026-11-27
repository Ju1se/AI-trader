#![forbid(unsafe_code)]
//! Deterministic 15-factor value/quality/health engine and pitfall-safe PIT backtester.

pub mod backtest;
pub mod factors;

pub use backtest::{
    audit_survivorship_bias_exercise, run_pit_backtest, BacktestConfig, BacktestResult,
    PeriodBacktestRecord, PitMarketSource, SurvivorshipAudit, WindowMetrics,
};
pub use factors::{score_pit_universe, QuantSecurityResult, FACTOR_NAMES};
