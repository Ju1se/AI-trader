//! Task types, tasks and their event log, contracts, waits, deliveries and hand-offs.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::AnyConnection;
use sqlx::FromRow;
use trade_core::{
    DeliveryManifest, GateError, HandoffPackage, IdempotencyKey, PreDispatchError, TaskContract,
    TaskId, TaskRecord, TaskState, TaskTypeConfig, TimeoutAction, UsdCents, WaitMetadata,
};

use crate::{Change, Store, count, db, from_json, not_found, to_json};

/// Statuses of an action that was sent and not yet settled.
pub(crate) const UNSETTLED: &str = "('DISPATCHED', 'EXECUTED', 'UNKNOWN')";

/// One line of a task's audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct TaskEvent {
    pub task_id: String,
    pub seq: i64,
    pub kind: String,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub version: i64,
    pub actor: String,
    pub ref_id: Option<String>,
    pub detail: String,
    pub created_epoch_s: i64,
}

#[derive(FromRow)]
pub(crate) struct TaskRow {
    task_id: String,
    idempotency_trigger_key: String,
    task_type: String,
    config_version: String,
    requester_id: String,
    acting_principal: String,
    trigger_source: String,
    authorized_markets_json: String,
    state_code: String,
    state_detail: Option<String>,
    wait_owner: Option<String>,
    wait_deadline_epoch_s: Option<i64>,
    wait_timeout_action: Option<String>,
    version: i64,
    running_since_epoch_s: Option<i64>,
    turns_used: i64,
    spend_cents: i64,
    retries_used: i64,
    consecutive_denials: i64,
    reworks_used: i64,
    reopened_count: i64,
}

impl TryFrom<TaskRow> for TaskRecord {
    type Error = GateError;

    fn try_from(r: TaskRow) -> Result<Self, GateError> {
        let wait = match (r.wait_owner, r.wait_deadline_epoch_s, r.wait_timeout_action) {
            (Some(owner), Some(deadline), Some(action)) => Some(WaitMetadata::new(
                owner,
                deadline,
                TimeoutAction::from_db_str(&action)?,
            )?),
            _ => None,
        };
        let state = TaskState::from_db_parts(&r.state_code, r.state_detail, wait)?;
        let mut task = TaskRecord::new_queued(
            TaskId::new(r.task_id),
            r.idempotency_trigger_key,
            r.task_type,
            r.config_version,
            r.requester_id,
            r.acting_principal,
            r.trigger_source,
            from_json(&r.authorized_markets_json)?,
        )?
        .restored(state, r.version)?;
        task.running_since_epoch_s = r.running_since_epoch_s;
        task.turns_used = count(r.turns_used)?;
        task.spend = UsdCents::from_cents(r.spend_cents);
        task.retries_used = count(r.retries_used)?;
        task.consecutive_denials = count(r.consecutive_denials)?;
        task.reworks_used = count(r.reworks_used)?;
        task.reopened_count = count(r.reopened_count)?;
        Ok(task)
    }
}

fn tasks_from(rows: Vec<TaskRow>) -> Result<Vec<TaskRecord>, GateError> {
    rows.into_iter().map(TaskRecord::try_from).collect()
}

pub(crate) async fn task_in(
    conn: &mut AnyConnection,
    task_id: &TaskId,
) -> Result<Option<TaskRecord>, GateError> {
    sqlx::query_as::<_, TaskRow>("SELECT * FROM tasks WHERE task_id = $1")
        .bind(task_id.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?
        .map(TaskRecord::try_from)
        .transpose()
}

pub(crate) async fn task_type_in(
    conn: &mut AnyConnection,
    task_type: &str,
    version: &str,
) -> Result<Option<TaskTypeConfig>, GateError> {
    sqlx::query_scalar::<_, String>(
        "SELECT config_json FROM task_types WHERE task_type = $1 AND version = $2",
    )
    .bind(task_type)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db)?
    .map(|raw| from_json(&raw))
    .transpose()
}

pub(crate) async fn unsettled_action_in(
    conn: &mut AnyConnection,
    task_id: &TaskId,
) -> Result<Option<IdempotencyKey>, GateError> {
    let sql = format!(
        "SELECT idempotency_key FROM actions WHERE task_id = $1 AND status IN {UNSETTLED} \
         ORDER BY created_epoch_s, idempotency_key LIMIT 1"
    );
    Ok(sqlx::query_scalar::<_, String>(&sql)
        .bind(task_id.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?
        .map(IdempotencyKey::new))
}

/// Save `task` if the stored version is still `task.version()`, and log the change, in
/// the caller's transaction. Returns the new version.
pub(crate) async fn save_in(
    conn: &mut AnyConnection,
    task: &TaskRecord,
    change: &Change<'_>,
    now: i64,
) -> Result<i64, GateError> {
    task.validate()?;
    let conflict = || {
        GateError::Conflict(format!(
            "task {} changed after version {}",
            task.task_id,
            task.version()
        ))
    };
    let (from_state, stored_version) = sqlx::query_as::<_, (String, i64)>(
        "SELECT state_code, version FROM tasks WHERE task_id = $1",
    )
    .bind(task.task_id.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db)?
    .ok_or_else(|| not_found("task", &task.task_id))?;
    if stored_version != task.version() {
        return Err(conflict());
    }
    // A task with an action in flight can be neither failed nor cancelled: that action
    // must be reconciled first.
    if matches!(
        task.state(),
        TaskState::Failed { .. } | TaskState::Cancelled { .. }
    ) && let Some(key) = unsettled_action_in(conn, &task.task_id).await?
    {
        return Err(PreDispatchError::UnreconciledIntentExists(key.to_string()).into());
    }
    let wait = task.state().wait_meta();
    let updated = sqlx::query(
        "UPDATE tasks SET state_code = $1, state_detail = $2, wait_owner = $3,
             wait_deadline_epoch_s = $4, wait_timeout_action = $5, running_since_epoch_s = $6,
             turns_used = $7, spend_cents = $8, retries_used = $9, consecutive_denials = $10,
             reworks_used = $11, reopened_count = $12, updated_epoch_s = $13,
             version = version + 1
         WHERE task_id = $14 AND version = $15",
    )
    .bind(task.state().code())
    .bind(task.state().detail())
    .bind(wait.map(|w| w.owner_id.as_str()))
    .bind(wait.map(|w| w.deadline_epoch_s))
    .bind(wait.map(|w| w.timeout_action.as_str()))
    .bind(task.running_since_epoch_s)
    .bind(i64::from(task.turns_used))
    .bind(task.spend.cents())
    .bind(i64::from(task.retries_used))
    .bind(i64::from(task.consecutive_denials))
    .bind(i64::from(task.reworks_used))
    .bind(i64::from(task.reopened_count))
    .bind(now)
    .bind(task.task_id.as_str())
    .bind(task.version())
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    if updated.rows_affected() != 1 {
        return Err(conflict());
    }
    let version = task.version() + 1;
    let to_state = task.state().code();
    log_in(
        conn,
        task,
        Some(&from_state),
        to_state,
        version,
        change,
        now,
    )
    .await?;
    Ok(version)
}

/// Append to the task's event log. Callers update the task row first, which serialises
/// concurrent writers on that row, so the per-task sequence number cannot collide.
async fn log_in(
    conn: &mut AnyConnection,
    task: &TaskRecord,
    from_state: Option<&str>,
    to_state: &str,
    version: i64,
    change: &Change<'_>,
    now: i64,
) -> Result<(), GateError> {
    sqlx::query(
        "INSERT INTO task_events
             (task_id, seq, kind, from_state, to_state, version, actor, ref_id, detail,
              created_epoch_s)
         SELECT $1, COALESCE(MAX(seq), 0) + 1, $2, $3, $4, $5, $6, $7, $8, $9
         FROM task_events WHERE task_id = $1",
    )
    .bind(task.task_id.as_str())
    .bind(change.kind)
    .bind(from_state)
    .bind(to_state)
    .bind(version)
    .bind(change.actor)
    .bind(change.ref_id)
    .bind(change.detail)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    Ok(())
}

impl Store {
    /// Register a version of a task type. Versions never change: registering an existing
    /// version with different content is an error, so a changed rule gets a new version.
    pub async fn put_task_type(&self, config: &TaskTypeConfig) -> Result<(), GateError> {
        config.validate()?;
        let json = to_json(config)?;
        let mut tx = self.begin().await?;
        sqlx::query(
            "INSERT INTO task_types (task_type, version, config_json, created_epoch_s)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(&config.task_type)
        .bind(&config.version)
        .bind(&json)
        .bind(self.now())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let stored = task_type_in(&mut tx, &config.task_type, &config.version).await?;
        if stored.as_ref().map(to_json).transpose()? != Some(json) {
            return Err(GateError::Storage(format!(
                "task type {} version {} already exists with other content; bump the version",
                config.task_type, config.version
            )));
        }
        tx.commit().await.map_err(db)
    }

    pub async fn task_type(
        &self,
        task_type: &str,
        version: &str,
    ) -> Result<Option<TaskTypeConfig>, GateError> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        task_type_in(&mut conn, task_type, version).await
    }

    /// Create the task unless one with the same trigger key exists. Returns the stored
    /// task either way, and whether it was created now.
    pub async fn create_task(
        &self,
        task: &TaskRecord,
        actor: &str,
    ) -> Result<(TaskRecord, bool), GateError> {
        task.validate()?;
        let now = self.now();
        let mut tx = self.begin().await?;
        let wait = task.state().wait_meta();
        let inserted = sqlx::query(
            "INSERT INTO tasks (task_id, idempotency_trigger_key, task_type, config_version,
                 requester_id, acting_principal, trigger_source, authorized_markets_json,
                 state_code, state_detail, wait_owner, wait_deadline_epoch_s,
                 wait_timeout_action, version, running_since_epoch_s, turns_used, spend_cents,
                 retries_used, consecutive_denials, reworks_used, reopened_count,
                 created_epoch_s, updated_epoch_s)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                 $16, $17, $18, $19, $20, $21, $22, $22)
             ON CONFLICT DO NOTHING",
        )
        .bind(task.task_id.as_str())
        .bind(&task.idempotency_trigger_key)
        .bind(&task.task_type)
        .bind(&task.config_version)
        .bind(&task.requester_id)
        .bind(&task.acting_principal)
        .bind(&task.trigger_source)
        .bind(to_json(&task.authorized_markets)?)
        .bind(task.state().code())
        .bind(task.state().detail())
        .bind(wait.map(|w| w.owner_id.as_str()))
        .bind(wait.map(|w| w.deadline_epoch_s))
        .bind(wait.map(|w| w.timeout_action.as_str()))
        .bind(task.version())
        .bind(task.running_since_epoch_s)
        .bind(i64::from(task.turns_used))
        .bind(task.spend.cents())
        .bind(i64::from(task.retries_used))
        .bind(i64::from(task.consecutive_denials))
        .bind(i64::from(task.reworks_used))
        .bind(i64::from(task.reopened_count))
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let created = inserted.rows_affected() == 1;
        if created {
            let change = Change::new("created", actor, &task.trigger_source);
            log_in(
                &mut tx,
                task,
                None,
                task.state().code(),
                task.version(),
                &change,
                now,
            )
            .await?;
        }
        let stored =
            sqlx::query_as::<_, TaskRow>("SELECT * FROM tasks WHERE idempotency_trigger_key = $1")
                .bind(&task.idempotency_trigger_key)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok((stored.try_into()?, created))
    }

    pub async fn task(&self, task_id: &TaskId) -> Result<Option<TaskRecord>, GateError> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        task_in(&mut conn, task_id).await
    }

    /// One page of tasks, newest first, optionally only those in `state` or from
    /// `requester`, and how many match in all.
    pub async fn task_page(
        &self,
        state: Option<&str>,
        requester: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TaskRecord>, i64), GateError> {
        let filter = "($1 IS NULL OR state_code = $1) AND ($2 IS NULL OR requester_id = $2)";
        let rows = sqlx::query_as::<_, TaskRow>(&format!(
            "SELECT * FROM tasks WHERE {filter}
             ORDER BY created_epoch_s DESC, task_id LIMIT $3 OFFSET $4"
        ))
        .bind(state)
        .bind(requester)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let total = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM tasks WHERE {filter}"
        ))
        .bind(state)
        .bind(requester)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok((tasks_from(rows)?, total))
    }

    /// How many tasks are in each state, and approvals in each status.
    pub async fn status_counts(
        &self,
    ) -> Result<(BTreeMap<String, i64>, BTreeMap<String, i64>), GateError> {
        let mut counts = Vec::new();
        for (table, column) in [("tasks", "state_code"), ("approvals", "status")] {
            let sql =
                format!("SELECT {column}, CAST(COUNT(*) AS BIGINT) FROM {table} GROUP BY {column}");
            let rows = sqlx::query_as::<_, (String, i64)>(&sql)
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;
            counts.push(rows.into_iter().collect::<BTreeMap<_, _>>());
        }
        let approvals = counts.pop().unwrap_or_default();
        Ok((counts.pop().unwrap_or_default(), approvals))
    }

    /// Save `task` if nobody changed it since it was read. On success `task` carries the
    /// new version; on `GateError::Conflict` read it again and decide again.
    pub async fn save(
        &self,
        task: &mut TaskRecord,
        reason: &str,
        actor: &str,
    ) -> Result<(), GateError> {
        let now = self.now();
        let mut tx = self.begin().await?;
        let version = save_in(&mut tx, task, &Change::new("update", actor, reason), now).await?;
        tx.commit().await.map_err(db)?;
        task.set_persisted_version(version);
        Ok(())
    }

    pub async fn events(&self, task_id: &TaskId) -> Result<Vec<TaskEvent>, GateError> {
        sqlx::query_as::<_, TaskEvent>("SELECT * FROM task_events WHERE task_id = $1 ORDER BY seq")
            .bind(task_id.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(db)
    }

    /// Waiting tasks whose deadline has passed, oldest deadline first.
    pub async fn due_waits(&self, now: i64) -> Result<Vec<TaskRecord>, GateError> {
        let rows = sqlx::query_as::<_, TaskRow>(
            "SELECT * FROM tasks WHERE wait_deadline_epoch_s <= $1
             AND state_code IN ('AWAITING_INPUT', 'AWAITING_APPROVAL', 'SETTLING', 'HANDED_OFF')
             ORDER BY wait_deadline_epoch_s, task_id",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        tasks_from(rows)
    }

    pub async fn save_contract(&self, contract: &TaskContract) -> Result<(), GateError> {
        contract.validate()?;
        sqlx::query(
            "INSERT INTO contracts (task_id, contract_json, created_epoch_s) VALUES ($1, $2, $3)
             ON CONFLICT (task_id) DO UPDATE SET contract_json = EXCLUDED.contract_json",
        )
        .bind(contract.task_id.as_str())
        .bind(to_json(contract)?)
        .bind(self.now())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    pub async fn contract(&self, task_id: &TaskId) -> Result<Option<TaskContract>, GateError> {
        self.json_by_task(
            "SELECT contract_json FROM contracts WHERE task_id = $1",
            task_id,
        )
        .await
    }

    /// Store the delivery and save `task`, now SETTLING, in one transaction.
    pub async fn deliver(
        &self,
        manifest: &DeliveryManifest,
        task: &mut TaskRecord,
    ) -> Result<(), GateError> {
        let now = self.now();
        let mut tx = self.begin().await?;
        sqlx::query(
            "INSERT INTO deliveries (task_id, completeness, manifest_json, delivered_epoch_s)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (task_id) DO UPDATE SET completeness = EXCLUDED.completeness,
                 manifest_json = EXCLUDED.manifest_json,
                 delivered_epoch_s = EXCLUDED.delivered_epoch_s",
        )
        .bind(task.task_id.as_str())
        .bind(manifest.completeness.as_str())
        .bind(to_json(manifest)?)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let change = Change::new("delivered", "workflow", manifest.completeness.as_str());
        let version = save_in(&mut tx, task, &change, now).await?;
        tx.commit().await.map_err(db)?;
        task.set_persisted_version(version);
        Ok(())
    }

    pub async fn delivery(&self, task_id: &TaskId) -> Result<Option<DeliveryManifest>, GateError> {
        self.json_by_task(
            "SELECT manifest_json FROM deliveries WHERE task_id = $1",
            task_id,
        )
        .await
    }

    /// Store the hand-off package and save `task`, now HANDED_OFF, in one transaction.
    pub async fn hand_off(
        &self,
        package: &HandoffPackage,
        task: &mut TaskRecord,
    ) -> Result<(), GateError> {
        let now = self.now();
        let owner = task.state().wait_meta().map_or("", |w| w.owner_id.as_str());
        let mut tx = self.begin().await?;
        sqlx::query(
            "INSERT INTO handoffs (task_id, owner_id, package_json, accepted_by, created_epoch_s)
             VALUES ($1, $2, $3, NULL, $4)
             ON CONFLICT (task_id) DO UPDATE SET owner_id = EXCLUDED.owner_id,
                 package_json = EXCLUDED.package_json, accepted_by = NULL,
                 created_epoch_s = EXCLUDED.created_epoch_s, accepted_epoch_s = NULL",
        )
        .bind(task.task_id.as_str())
        .bind(owner)
        .bind(to_json(package)?)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let change = Change::new("handed_off", "workflow", &package.trigger_reason);
        let version = save_in(&mut tx, task, &change, now).await?;
        tx.commit().await.map_err(db)?;
        task.set_persisted_version(version);
        Ok(())
    }

    pub async fn handoff(&self, task_id: &TaskId) -> Result<Option<HandoffPackage>, GateError> {
        self.json_by_task(
            "SELECT package_json FROM handoffs WHERE task_id = $1",
            task_id,
        )
        .await
    }

    async fn json_by_task<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        task_id: &TaskId,
    ) -> Result<Option<T>, GateError> {
        sqlx::query_scalar::<_, String>(sql)
            .bind(task_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .map(|raw| from_json(&raw))
            .transpose()
    }
}
