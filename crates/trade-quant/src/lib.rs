#![forbid(unsafe_code)]
//! Deterministic 15-factor value/quality/health engine and pitfall-safe PIT backtester.

pub mod backtest;
pub mod factors;

pub use backtest::{
    BacktestConfig, BacktestResult, PeriodBacktestRecord, SurvivorshipAudit, WindowMetrics,
    audit_survivorship_bias_exercise, run_pit_backtest,
};
pub use factors::{
    BucketWeights, FACTOR_NAMES, FactorParams, PenaltyParams, QuantSecurityResult, VetoParams,
    score_pit_universe,
};
