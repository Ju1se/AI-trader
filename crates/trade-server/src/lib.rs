#![forbid(unsafe_code)]
//! The application: settings from files, the two governed tools, and the use cases that
//! drive a task through Diagram 0. The HTTP interface and the command line sit on top.

pub mod agent;
pub mod api;
pub mod config;
pub mod tools;
pub mod workflow;

pub use agent::AgentModel;
pub use api::{App, Change};
pub use config::{ResearchParams, Strategy, load_task_types};
pub use tools::{
    CreateDraft, DRAFT_TOOL, ORDER_TOOL, PortfolioFacts, SubmitOrders, limit_breaches,
};
pub use workflow::{
    FrameRequest, Framing, Harness, Intake, PAPER_BROKER_KEY, REBALANCE, Step, Verification,
};
