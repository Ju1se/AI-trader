#![forbid(unsafe_code)]
//! Pure domain core: typed IDs, the Diagram 4a state machine, Diagram 3a risk tiers,
//! contracts, errors and the read-only data ports. No IO and no async.

pub mod contracts;
pub mod errors;
pub mod ids;
pub mod ports;
pub mod state;
pub mod types;

pub use contracts::*;
pub use errors::*;
pub use ids::*;
pub use ports::*;
pub use state::*;
pub use types::*;
