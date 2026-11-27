#![forbid(unsafe_code)]
//! The only crate that talks to the database.
//!
//! One `AnyPool` serves SQLite and PostgreSQL with the same SQL. Rows map to structs with
//! `FromRow`. A write that touches more than one table is one transaction, and every task
//! update is guarded by the task's version: if someone else wrote first, the write fails
//! with `GateError::Conflict` and the caller reads again.

mod ledger;
mod results;
mod tasks;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::AnyConnection;
use sqlx::any::AnyPoolOptions;
use sqlx::{Any, AnyPool, Transaction};
use trade_core::{GateError, IdempotencyKey, TaskId, TaskRecord, TaskState, UsdCents, sha256_hex};
use trade_gate::DispatchResult;

pub use results::{CostEntry, MemoryItem, StoredDraft, UnitEconomics};
pub use tasks::TaskEvent;

/// How often a write that only records facts is retried after a version conflict.
const MAX_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct Store {
    pool: AnyPool,
    backend: &'static str,
    /// Added to the system clock. Only tests and the dev endpoints move it, to let
    /// deadlines pass without waiting.
    clock_offset_s: Arc<AtomicI64>,
}

impl Store {
    /// Connect and bring the schema up to date. Takes a `sqlite:` or `postgres://` URL,
    /// `:memory:`, or the path of a SQLite file.
    pub async fn connect(url_or_path: &str) -> Result<Self, GateError> {
        static DRIVERS: Once = Once::new();
        DRIVERS.call_once(sqlx::any::install_default_drivers);
        let url = match url_or_path {
            ":memory:" => "sqlite::memory:".to_string(),
            u if ["sqlite:", "postgres://", "postgresql://"]
                .iter()
                .any(|p| u.starts_with(p)) =>
            {
                u.to_string()
            }
            path => format!("sqlite://{path}?mode=rwc"),
        };
        let backend = if url.starts_with("sqlite:") {
            "sqlite"
        } else {
            "postgresql"
        };
        // An in-memory SQLite database exists only inside its one connection.
        let max_connections = if url.contains(":memory:") { 1 } else { 5 };
        let pool = AnyPoolOptions::new()
            .max_connections(max_connections)
            .connect(&url)
            .await
            .map_err(db)?;
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .map_err(|e| GateError::Storage(format!("migration: {e}")))?;
        Ok(Self {
            pool,
            backend,
            clock_offset_s: Arc::default(),
        })
    }

    pub fn backend(&self) -> &'static str {
        self.backend
    }

    pub fn now(&self) -> i64 {
        let system = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        system + self.clock_offset_s.load(Ordering::Relaxed)
    }

    /// Move this store's clock forward, to demonstrate or test what happens at a deadline.
    pub fn advance_clock(&self, seconds: i64) {
        self.clock_offset_s.fetch_add(seconds, Ordering::Relaxed);
    }

    async fn begin(&self) -> Result<Transaction<'static, Any>, GateError> {
        self.pool.begin().await.map_err(db)
    }

    /// Read the task, apply `fact` to it and to the rows it touches, and save it, in one
    /// transaction. Retried after a version conflict, which is safe because a fact holds
    /// whatever else happened; never use this to make a decision from a stale read.
    async fn record(
        &self,
        task_id: &TaskId,
        change: Change<'_>,
        fact: Fact<'_>,
    ) -> Result<TaskRecord, GateError> {
        for _ in 0..MAX_ATTEMPTS {
            let now = self.now();
            let mut tx = self.begin().await?;
            let mut task = tasks::task_in(&mut tx, task_id)
                .await?
                .ok_or_else(|| not_found("task", task_id))?;
            fact.apply(&mut tx, &mut task, now).await?;
            match tasks::save_in(&mut tx, &task, &change, now).await {
                Err(GateError::Conflict(_)) => continue,
                Err(e) => return Err(e),
                Ok(version) => {
                    tx.commit().await.map_err(db)?;
                    task.set_persisted_version(version);
                    return Ok(task);
                }
            }
        }
        Err(GateError::Conflict(format!("task {task_id} kept changing")))
    }
}

/// Something that happened to a task and must be recorded.
#[derive(Debug, Clone, Copy)]
enum Fact<'a> {
    /// The gate refused a proposal: one turn and one consecutive denial.
    Denied,
    /// A cost, added to the task's spend.
    Cost {
        cost_id: &'a str,
        cost: &'a CostEntry,
    },
    /// A line of the session log, such as the agent's transcript. `turn` counts one turn.
    Noted { turn: bool },
    /// How a dispatched action ended.
    Finished {
        key: &'a IdempotencyKey,
        result: &'a DispatchResult,
    },
}

impl Fact<'_> {
    async fn apply(
        self,
        conn: &mut AnyConnection,
        task: &mut TaskRecord,
        now: i64,
    ) -> Result<(), GateError> {
        match self {
            Fact::Denied => {
                task.turns_used += 1;
                task.consecutive_denials += 1;
            }
            Fact::Noted { turn } => {
                if turn {
                    task.turns_used += 1;
                }
            }
            Fact::Cost { cost_id, cost } => {
                results::insert_cost_in(conn, &task.task_id, cost_id, cost, now).await?;
                task.spend = UsdCents::from_cents(task.spend.cents() + cost.amount.cents());
            }
            Fact::Finished { key, result } => {
                ledger::settle_action_in(conn, key, result, "('DISPATCHED')", now).await?;
                match result {
                    DispatchResult::Verified { .. } => task.consecutive_denials = 0,
                    DispatchResult::NotSent { .. } => {}
                    DispatchResult::Unknown { .. } => {
                        let frozen = TaskState::OutcomeUnknown {
                            frozen_action_key: key.clone(),
                        };
                        // Normally RUNNING. If a person took the task over meanwhile, it
                        // stays with them; the unsettled action still blocks other actions.
                        if task.state().can_transition(&frozen) {
                            task.transition(frozen, now)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Why a task row changed, for its event log.
#[derive(Debug, Clone, Copy)]
struct Change<'a> {
    kind: &'a str,
    actor: &'a str,
    ref_id: Option<&'a str>,
    detail: &'a str,
}

impl<'a> Change<'a> {
    fn new(kind: &'a str, actor: &'a str, detail: &'a str) -> Self {
        Self {
            kind,
            actor,
            ref_id: None,
            detail,
        }
    }

    fn about(mut self, ref_id: &'a str) -> Self {
        self.ref_id = Some(ref_id);
        self
    }
}

/// A short random ID with a readable prefix, such as `APR-3f9c0a1b2d4e`.
pub fn new_id(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = format!(
        "{nanos}:{}:{}",
        SEQ.fetch_add(1, Ordering::Relaxed),
        std::process::id()
    );
    format!("{prefix}-{}", &sha256_hex(seed.as_bytes())[..12])
}

fn db(e: sqlx::Error) -> GateError {
    GateError::Storage(e.to_string())
}

fn not_found(what: &str, id: &str) -> GateError {
    GateError::Storage(format!("{what}_not_found:{id}"))
}

fn to_json<T: Serialize>(value: &T) -> Result<String, GateError> {
    serde_json::to_string(value).map_err(|e| GateError::Storage(e.to_string()))
}

fn from_json<T: DeserializeOwned>(raw: &str) -> Result<T, GateError> {
    serde_json::from_str(raw).map_err(|e| GateError::Storage(format!("corrupt_json:{e}")))
}

fn count(value: i64) -> Result<u32, GateError> {
    u32::try_from(value).map_err(|_| GateError::Storage(format!("corrupt_counter:{value}")))
}
