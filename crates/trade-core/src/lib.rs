#![forbid(unsafe_code)]
//! Core domain types, Diagram 4a 13-state machine with private state & transition table,
//! Diagram 3a R0-R4 risk tiers, algebraic PreDispatch vs PostDispatchUnknown errors,
//! `sha2` payload hashing, and `serde` contracts.

pub mod contracts;
pub mod errors;
pub mod state;
pub mod types;

pub use contracts::*;
pub use errors::*;
pub use state::*;
pub use types::*;
