#![forbid(unsafe_code)]
//! End-to-end governed research & paper-execution orchestration, concrete `Tool` adapters,
//! DB-only `IndependentVerifier`, and multi-period PIT fixture dataset.

pub mod fixtures;
pub mod tools;
pub mod workflow;

pub use fixtures::{rebalance_dates, seed_pit_universe, REBALANCE_DATE_STRS};
pub use tools::{CreateRebalanceDraftTool, CredentialVault, SubmitPaperOrdersTool};
pub use workflow::{
    build_default_config, IndependentVerifier, ReconcileOutcome, ResearchRunOutput, TradeAiHarness,
    VerifyOutcome,
};
