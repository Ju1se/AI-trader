#![forbid(unsafe_code)]
//! External systems: point-in-time market data, a paper broker, and a chat model behind
//! the OpenAI-compatible API. Nothing here touches the database.

mod broker;
mod market;
mod model;

pub use broker::{BrokerFault, BrokerOrderLine, BrokerOrderReceipt, PaperBroker};
pub use market::{FixtureMarket, ScopedView};
pub use model::{ChatMessage, ChatModel, Reply};
