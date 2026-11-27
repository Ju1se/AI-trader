//! The gate's ledger (Diagrams 3a and 3b), approval decisions and reconciliation (4b).

use sqlx::AnyConnection;
use sqlx::FromRow;
use trade_core::{
    ActionIntentRecord, ActionIntentStatus, ApprovalId, ApprovalRecord, ApprovalStatus, GateError,
    IdempotencyKey, PreDispatchError, TaskId, TaskRecord,
};
use trade_gate::{DispatchResult, GateContext, GateLedger, NewApproval};

use crate::tasks::{UNSETTLED, save_in, task_in, task_type_in, unsettled_action_in};
use crate::{Change, Fact, Store, db, new_id, not_found};

#[derive(FromRow)]
struct ApprovalRow {
    approval_id: String,
    task_id: String,
    requester_id: String,
    tool_name: String,
    target: String,
    payload_hash: String,
    args_json: String,
    status: String,
    approver_id: Option<String>,
    decision_note: Option<String>,
    expires_at_epoch_s: i64,
}

impl TryFrom<ApprovalRow> for ApprovalRecord {
    type Error = GateError;

    fn try_from(r: ApprovalRow) -> Result<Self, GateError> {
        Ok(Self {
            approval_id: ApprovalId::new(r.approval_id),
            task_id: TaskId::new(r.task_id),
            requester_id: r.requester_id,
            tool_name: r.tool_name,
            target: r.target,
            payload_hash: r.payload_hash,
            args_json: r.args_json,
            status: ApprovalStatus::from_db_str(&r.status)?,
            approver_id: r.approver_id,
            decision_note: r.decision_note,
            expires_at_epoch_s: r.expires_at_epoch_s,
        })
    }
}

#[derive(FromRow)]
struct ActionRow {
    idempotency_key: String,
    task_id: String,
    tool_name: String,
    target: String,
    args_json: String,
    payload_hash: String,
    status: String,
    backend_receipt_id: Option<String>,
    readback_verified: i64,
}

impl TryFrom<ActionRow> for ActionIntentRecord {
    type Error = GateError;

    fn try_from(r: ActionRow) -> Result<Self, GateError> {
        Ok(Self {
            idempotency_key: IdempotencyKey::new(r.idempotency_key),
            task_id: TaskId::new(r.task_id),
            tool_name: r.tool_name,
            target: r.target,
            args_json: r.args_json,
            payload_hash: r.payload_hash,
            status: ActionIntentStatus::from_db_str(&r.status)?,
            backend_receipt_id: r.backend_receipt_id,
            readback_verified: r.readback_verified != 0,
        })
    }
}

async fn approval_in(
    conn: &mut AnyConnection,
    id: &ApprovalId,
) -> Result<Option<ApprovalRecord>, GateError> {
    sqlx::query_as::<_, ApprovalRow>("SELECT * FROM approvals WHERE approval_id = $1")
        .bind(id.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?
        .map(ApprovalRecord::try_from)
        .transpose()
}

async fn action_in(
    conn: &mut AnyConnection,
    key: &IdempotencyKey,
) -> Result<Option<ActionIntentRecord>, GateError> {
    sqlx::query_as::<_, ActionRow>("SELECT * FROM actions WHERE idempotency_key = $1")
        .bind(key.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?
        .map(ActionIntentRecord::try_from)
        .transpose()
}

fn actions_from(rows: Vec<ActionRow>) -> Result<Vec<ActionIntentRecord>, GateError> {
    rows.into_iter().map(ActionIntentRecord::try_from).collect()
}

/// Record how a sent action ended, if it is still in one of the `from` statuses. An action
/// that never left gives back the approval it used, since that approval was not spent.
pub(crate) async fn settle_action_in(
    conn: &mut AnyConnection,
    key: &IdempotencyKey,
    result: &DispatchResult,
    from: &str,
    now: i64,
) -> Result<(), GateError> {
    let (status, receipt, detail) = match result {
        DispatchResult::Verified { receipt_id } => ("VERIFIED", Some(receipt_id.as_str()), None),
        DispatchResult::NotSent { detail } => ("FAILED", None, Some(detail.as_str())),
        DispatchResult::Unknown { receipt_id, detail } => {
            ("UNKNOWN", receipt_id.as_deref(), Some(detail.as_str()))
        }
    };
    let sql = format!(
        "UPDATE actions SET status = $1, backend_receipt_id = $2, readback_verified = $3,
             detail = $4, updated_epoch_s = $5
         WHERE idempotency_key = $6 AND status IN {from}"
    );
    let updated = sqlx::query(&sql)
        .bind(status)
        .bind(receipt)
        .bind(i64::from(status == "VERIFIED"))
        .bind(detail)
        .bind(now)
        .bind(key.as_str())
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    if updated.rows_affected() != 1 {
        return Err(GateError::Storage(format!(
            "action {key} is not awaiting a result"
        )));
    }
    if matches!(result, DispatchResult::NotSent { .. }) {
        sqlx::query(
            "UPDATE approvals SET status = 'APPROVED', consumed_epoch_s = NULL
             WHERE status = 'CONSUMED'
               AND approval_id = (SELECT approval_id FROM actions WHERE idempotency_key = $1)",
        )
        .bind(key.as_str())
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    }
    Ok(())
}

fn result_name(result: &DispatchResult) -> &'static str {
    match result {
        DispatchResult::Verified { .. } => "verified",
        DispatchResult::NotSent { .. } => "not_sent",
        DispatchResult::Unknown { .. } => "unknown",
    }
}

impl GateLedger for Store {
    async fn load_context(
        &self,
        task_id: &TaskId,
        key: &IdempotencyKey,
        approval_id: Option<&ApprovalId>,
    ) -> Result<GateContext, GateError> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        let task = task_in(&mut conn, task_id)
            .await?
            .ok_or_else(|| not_found("task", task_id))?;
        let config = task_type_in(&mut conn, &task.task_type, &task.config_version)
            .await?
            .ok_or_else(|| not_found("task_type", &task.config_version))?;
        let approval = match approval_id {
            Some(id) => approval_in(&mut conn, id).await?,
            None => None,
        };
        Ok(GateContext {
            existing_intent: action_in(&mut conn, key).await?,
            unresolved_intent: unsettled_action_in(&mut conn, task_id).await?,
            task,
            config,
            approval,
            now_epoch_s: self.now(),
        })
    }

    async fn save_task(&self, task: &TaskRecord, reason: &str) -> Result<(), GateError> {
        let mut task = task.clone();
        self.save(&mut task, reason, "gate").await
    }

    async fn record_denial(
        &self,
        task_id: &TaskId,
        reason: &PreDispatchError,
    ) -> Result<(), GateError> {
        let code = reason.code();
        let change = Change::new("denied", "gate", &code);
        self.record(task_id, change, Fact::Denied).await.map(drop)
    }

    async fn request_approval(
        &self,
        task: &TaskRecord,
        approval: &NewApproval,
    ) -> Result<ApprovalId, GateError> {
        let id = ApprovalId::new(new_id("APR"));
        let now = self.now();
        let mut tx = self.begin().await?;
        sqlx::query(
            "INSERT INTO approvals (approval_id, task_id, requester_id, tool_name, target,
                 payload_hash, args_json, status, expires_at_epoch_s, created_epoch_s)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'PENDING', $8, $9)",
        )
        .bind(id.as_str())
        .bind(task.task_id.as_str())
        .bind(&task.requester_id)
        .bind(&approval.tool_name)
        .bind(&approval.target)
        .bind(&approval.payload_hash)
        .bind(&approval.args_json)
        .bind(approval.expires_at_epoch_s)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let change = Change::new("approval_requested", "gate", &approval.tool_name).about(&id);
        save_in(&mut tx, task, &change, now).await?;
        tx.commit().await.map_err(db)?;
        Ok(id)
    }

    async fn begin_dispatch(
        &self,
        task: &TaskRecord,
        approval_id: Option<&ApprovalId>,
        intent: &ActionIntentRecord,
    ) -> Result<(), GateError> {
        let now = self.now();
        let key = intent.idempotency_key.as_str();
        let mut tx = self.begin().await?;
        // Checked again here, in the transaction that uses it up: still approved, for this
        // task and this exact payload, and not expired.
        if let Some(id) = approval_id {
            let used = sqlx::query(
                "UPDATE approvals SET status = 'CONSUMED', consumed_epoch_s = $1
                 WHERE approval_id = $2 AND task_id = $3 AND status = 'APPROVED'
                   AND payload_hash = $4 AND expires_at_epoch_s > $1",
            )
            .bind(now)
            .bind(id.as_str())
            .bind(task.task_id.as_str())
            .bind(&intent.payload_hash)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            if used.rows_affected() != 1 {
                return Err(GateError::Conflict(format!(
                    "approval {id} is no longer usable"
                )));
            }
        }
        // One row per key; only an action that never left may be sent again under it.
        let registered = sqlx::query(
            "INSERT INTO actions (idempotency_key, task_id, tool_name, target, args_json,
                 payload_hash, approval_id, status, readback_verified, created_epoch_s,
                 updated_epoch_s)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'DISPATCHED', 0, $8, $8)
             ON CONFLICT (idempotency_key) DO UPDATE SET status = 'DISPATCHED',
                 approval_id = EXCLUDED.approval_id, detail = NULL,
                 updated_epoch_s = EXCLUDED.updated_epoch_s
             WHERE actions.status IN ('FAILED', 'PLANNED')
               AND actions.payload_hash = EXCLUDED.payload_hash",
        )
        .bind(key)
        .bind(task.task_id.as_str())
        .bind(&intent.tool_name)
        .bind(&intent.target)
        .bind(&intent.args_json)
        .bind(&intent.payload_hash)
        .bind(approval_id.map(|id| id.as_str()))
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        if registered.rows_affected() != 1 {
            return Err(GateError::Conflict(format!(
                "action {key} is already registered"
            )));
        }
        let change = Change::new("dispatched", "gate", &intent.tool_name).about(key);
        save_in(&mut tx, task, &change, now).await?;
        tx.commit().await.map_err(db)
    }

    async fn finish_dispatch(
        &self,
        task_id: &TaskId,
        key: &IdempotencyKey,
        result: &DispatchResult,
    ) -> Result<(), GateError> {
        let change = Change::new("action_finished", "gate", result_name(result)).about(key);
        let fact = Fact::Finished { key, result };
        self.record(task_id, change, fact).await.map(drop)
    }
}

impl Store {
    pub async fn approval(&self, id: &ApprovalId) -> Result<Option<ApprovalRecord>, GateError> {
        let mut conn = self.pool.acquire().await.map_err(db)?;
        approval_in(&mut conn, id).await
    }

    /// Approvals, optionally only those in one status, newest first.
    pub async fn approvals(
        &self,
        status: Option<ApprovalStatus>,
    ) -> Result<Vec<ApprovalRecord>, GateError> {
        sqlx::query_as::<_, ApprovalRow>(
            "SELECT * FROM approvals WHERE $1 IS NULL OR status = $1
             ORDER BY created_epoch_s DESC, approval_id",
        )
        .bind(status.map(ApprovalStatus::as_str))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .into_iter()
        .map(ApprovalRecord::try_from)
        .collect()
    }

    /// A task's approvals, newest first.
    pub async fn task_approvals(&self, task_id: &TaskId) -> Result<Vec<ApprovalRecord>, GateError> {
        sqlx::query_as::<_, ApprovalRow>(
            "SELECT * FROM approvals WHERE task_id = $1 ORDER BY created_epoch_s DESC, approval_id",
        )
        .bind(task_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .into_iter()
        .map(ApprovalRecord::try_from)
        .collect()
    }

    /// Record a decision and save `task` with its next state, in one transaction. A
    /// pending approval can be approved, rejected or expired; an approved one that was not
    /// used yet can still be revoked (rejected) or expire. Nothing past its deadline can be
    /// approved.
    pub async fn decide_approval(
        &self,
        id: &ApprovalId,
        status: ApprovalStatus,
        approver_id: Option<&str>,
        note: Option<&str>,
        task: &mut TaskRecord,
    ) -> Result<(), GateError> {
        let now = self.now();
        let mut tx = self.begin().await?;
        let decided = sqlx::query(
            "UPDATE approvals SET status = $1, approver_id = $2, decision_note = $3,
                 decided_epoch_s = $4
             WHERE approval_id = $5 AND task_id = $6
               AND (status = 'PENDING' OR (status = 'APPROVED' AND $1 <> 'APPROVED'))
               AND ($1 <> 'APPROVED' OR expires_at_epoch_s > $4)",
        )
        .bind(status.as_str())
        .bind(approver_id)
        .bind(note)
        .bind(now)
        .bind(id.as_str())
        .bind(task.task_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        if decided.rows_affected() != 1 {
            return Err(GateError::Conflict(format!(
                "approval {id} cannot take this decision"
            )));
        }
        let actor = approver_id.unwrap_or("sweeper");
        let change = Change::new("approval_decided", actor, status.as_str()).about(id);
        let version = save_in(&mut tx, task, &change, now).await?;
        tx.commit().await.map_err(db)?;
        task.set_persisted_version(version);
        Ok(())
    }

    pub async fn actions(&self, task_id: &TaskId) -> Result<Vec<ActionIntentRecord>, GateError> {
        let rows = sqlx::query_as::<_, ActionRow>(
            "SELECT * FROM actions WHERE task_id = $1 ORDER BY created_epoch_s, idempotency_key",
        )
        .bind(task_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        actions_from(rows)
    }

    /// Actions sent but not settled, across all tasks: what recovery must reconcile.
    pub async fn unsettled_actions(&self) -> Result<Vec<ActionIntentRecord>, GateError> {
        let sql = format!(
            "SELECT * FROM actions WHERE status IN {UNSETTLED} ORDER BY created_epoch_s, idempotency_key"
        );
        let rows = sqlx::query_as::<_, ActionRow>(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        actions_from(rows)
    }

    /// Settle an action after checking the target system, and save `task` with its next
    /// state in the same transaction.
    pub async fn reconcile(
        &self,
        key: &IdempotencyKey,
        result: &DispatchResult,
        task: &mut TaskRecord,
        actor: &str,
    ) -> Result<(), GateError> {
        let now = self.now();
        let mut tx = self.begin().await?;
        settle_action_in(&mut tx, key, result, UNSETTLED, now).await?;
        let change = Change::new("reconciled", actor, result_name(result)).about(key);
        let version = save_in(&mut tx, task, &change, now).await?;
        tx.commit().await.map_err(db)?;
        task.set_persisted_version(version);
        Ok(())
    }
}
