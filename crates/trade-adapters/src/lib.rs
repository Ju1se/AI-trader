#![forbid(unsafe_code)]
//! External systems behind the core's ports: point-in-time market data and a paper broker.
//! Nothing here touches the database.

mod broker;
mod market;

pub use broker::{BrokerFault, BrokerOrderLine, BrokerOrderReceipt, PaperBroker};
pub use market::{FixtureMarket, ScopedView};
