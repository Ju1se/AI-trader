//! Typed IDs. Records refer to each other by these instead of by references, so ownership
//! stays simple and a task ID can never be passed where an approval ID is expected.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        /// Lets an ID be read wherever a `&str` is expected. The reverse is not possible:
        /// a plain string never silently becomes an ID.
        impl std::ops::Deref for $name {
            type Target = str;

            fn deref(&self) -> &str {
                &self.0
            }
        }
    )*};
}

id_type!(
    /// A task, stable from intake to settlement.
    TaskId,
    /// One approval request and its decision.
    ApprovalId,
    /// One consequential action; retries reuse it so the action happens at most once.
    IdempotencyKey,
);
