//! Research results, drafts, costs and unit economics, calibration and curated memory.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use trade_core::{
    AsOfDate, CommitteeDecision, CostEntry, GateError, PortfolioActionPayload, StoredDraft, TaskId,
    UsdCents,
};
use trade_gate::VerifiedGateTicket;

use sqlx::AnyConnection;

use crate::{Change, Fact, Store, db, from_json, new_id, to_json};

#[derive(FromRow)]
struct DraftRow {
    draft_id: String,
    task_id: String,
    as_of_date: String,
    weights_json: String,
}

impl TryFrom<DraftRow> for StoredDraft {
    type Error = GateError;

    fn try_from(r: DraftRow) -> Result<Self, GateError> {
        Ok(Self {
            draft_id: r.draft_id,
            task_id: TaskId::new(r.task_id),
            as_of_date: AsOfDate::new(r.as_of_date)?,
            weights: from_json(&r.weights_json)?,
        })
    }
}

/// Diagram 6 CALCULATE: what one successful task costs, all in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitEconomics {
    pub ended_tasks: u64,
    pub completed_tasks: u64,
    pub api_and_tool_usd: f64,
    pub human_minutes: f64,
    pub all_in_cost_usd: f64,
    /// All-in cost of the ended tasks divided by the completed ones; `None` until one
    /// task completes.
    pub cost_per_completed_task_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct MemoryItem {
    pub memory_id: String,
    pub source_task_id: String,
    pub summary: String,
    /// `pending_review` until a person approves or rejects it.
    pub status: String,
    pub reviewed_by: Option<String>,
}

pub(crate) async fn insert_cost_in(
    conn: &mut AnyConnection,
    task_id: &TaskId,
    cost_id: &str,
    cost: &CostEntry,
    now: i64,
) -> Result<(), GateError> {
    sqlx::query(
        "INSERT INTO costs (cost_id, task_id, stage, category, amount_cents, human_minutes,
             note, created_epoch_s)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(cost_id)
    .bind(task_id.as_str())
    .bind(&cost.stage)
    .bind(&cost.category)
    .bind(cost.amount.cents())
    .bind(cost.human_minutes)
    .bind(&cost.note)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    Ok(())
}

/// States in which the agent's work on a task has ended. Costs count from here on.
const ENDED: &str = "('COMPLETED', 'FAILED', 'DECLINED', 'CANCELLED', 'HANDED_OFF')";

impl Store {
    /// Replace a task's committee results. The gate checks business rules against these.
    pub async fn save_evaluations(
        &self,
        task_id: &TaskId,
        decisions: &[CommitteeDecision],
    ) -> Result<(), GateError> {
        let mut tx = self.begin().await?;
        sqlx::query("DELETE FROM security_evaluations WHERE task_id = $1")
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        for decision in decisions {
            sqlx::query(
                "INSERT INTO security_evaluations (task_id, ticker, as_of_date, hard_veto,
                     decision_json)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(task_id.as_str())
            .bind(&decision.ticker)
            .bind(decision.as_of_date.as_str())
            .bind(i64::from(decision.hard_veto))
            .bind(to_json(decision)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)
    }

    /// A task's committee results, by ticker.
    pub async fn evaluations(&self, task_id: &TaskId) -> Result<Vec<CommitteeDecision>, GateError> {
        sqlx::query_scalar::<_, String>(
            "SELECT decision_json FROM security_evaluations WHERE task_id = $1 ORDER BY ticker",
        )
        .bind(task_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(|raw| from_json(raw))
        .collect()
    }

    /// Only the gate can write a draft: writing takes the gate's ticket.
    pub async fn write_draft(
        &self,
        _ticket: &VerifiedGateTicket,
        task_id: &TaskId,
        payload: &PortfolioActionPayload,
    ) -> Result<String, GateError> {
        sqlx::query(
            "INSERT INTO drafts (draft_id, task_id, as_of_date, weights_json, created_epoch_s)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&payload.draft_id)
        .bind(task_id.as_str())
        .bind(payload.as_of_date.as_str())
        .bind(to_json(&payload.weights_bps)?)
        .bind(self.now())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(payload.draft_id.clone())
    }

    pub async fn draft(&self, draft_id: &str) -> Result<Option<StoredDraft>, GateError> {
        sqlx::query_as::<_, DraftRow>("SELECT * FROM drafts WHERE draft_id = $1")
            .bind(draft_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .map(StoredDraft::try_from)
            .transpose()
    }

    pub async fn latest_draft(&self, task_id: &TaskId) -> Result<Option<StoredDraft>, GateError> {
        sqlx::query_as::<_, DraftRow>(
            "SELECT * FROM drafts WHERE task_id = $1
             ORDER BY created_epoch_s DESC, draft_id DESC LIMIT 1",
        )
        .bind(task_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .map(StoredDraft::try_from)
        .transpose()
    }

    /// Record a cost. API and tool spend is added to the task's spend in the same
    /// transaction, so the spend limit always sees it.
    pub async fn record_cost(&self, task_id: &TaskId, cost: &CostEntry) -> Result<(), GateError> {
        let cost_id = new_id("COST");
        let change = Change::new("cost", &cost.category, &cost.stage).about(&cost_id);
        let fact = Fact::Cost {
            cost_id: &cost_id,
            cost,
        };
        self.record(task_id, change, fact).await.map(drop)
    }

    /// Diagram 6 CALCULATE. Only ended tasks count: their whole cost goes in the
    /// numerator, and each completed task counts once in the denominator, however often it
    /// was reopened before. Tasks still in progress count in neither.
    pub async fn unit_economics(
        &self,
        human_hourly_rate_cents: i64,
    ) -> Result<UnitEconomics, GateError> {
        let sql = format!(
            "SELECT CAST(COUNT(*) AS BIGINT),
                 CAST(COALESCE(SUM(CASE WHEN t.state_code = 'COMPLETED' THEN 1 ELSE 0 END), 0)
                     AS BIGINT),
                 CAST(COALESCE(SUM(c.cents), 0) AS BIGINT),
                 CAST(COALESCE(SUM(c.minutes), 0) AS DOUBLE PRECISION)
             FROM tasks t
             LEFT JOIN (SELECT task_id, SUM(amount_cents) AS cents,
                            SUM(human_minutes) AS minutes
                        FROM costs GROUP BY task_id) c ON c.task_id = t.task_id
             WHERE t.state_code IN {ENDED}"
        );
        let (ended, completed, api_cents, minutes) =
            sqlx::query_as::<_, (i64, i64, i64, f64)>(&sql)
                .fetch_one(&self.pool)
                .await
                .map_err(db)?;
        let human_cents = (minutes / 60.0 * human_hourly_rate_cents as f64).round() as i64;
        let all_in = UsdCents::from_cents(api_cents + human_cents);
        Ok(UnitEconomics {
            ended_tasks: ended as u64,
            completed_tasks: completed as u64,
            api_and_tool_usd: UsdCents::from_cents(api_cents).to_dollars(),
            human_minutes: minutes,
            all_in_cost_usd: all_in.to_dollars(),
            cost_per_completed_task_usd: (completed > 0)
                .then(|| UsdCents::from_cents(all_in.cents() / completed).to_dollars()),
        })
    }

    pub async fn set_calibration(
        &self,
        agent_name: &str,
        confidence: f64,
        historical_ic: f64,
    ) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO agent_calibration (agent_name, calibrated_confidence, historical_ic)
             VALUES ($1, $2, $3)
             ON CONFLICT (agent_name) DO UPDATE SET
                 calibrated_confidence = EXCLUDED.calibrated_confidence,
                 historical_ic = EXCLUDED.historical_ic",
        )
        .bind(agent_name)
        .bind(confidence)
        .bind(historical_ic)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Calibrated confidence per agent.
    pub async fn calibrations(&self) -> Result<BTreeMap<String, f64>, GateError> {
        Ok(sqlx::query_as::<_, (String, f64)>(
            "SELECT agent_name, calibrated_confidence FROM agent_calibration",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .into_iter()
        .collect())
    }

    pub async fn add_memory(&self, item: &MemoryItem) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO curated_memory (memory_id, source_task_id, summary, status, reviewed_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (memory_id) DO UPDATE SET summary = EXCLUDED.summary,
                 status = EXCLUDED.status, reviewed_by = EXCLUDED.reviewed_by",
        )
        .bind(&item.memory_id)
        .bind(&item.source_task_id)
        .bind(&item.summary)
        .bind(&item.status)
        .bind(&item.reviewed_by)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    pub async fn memories(&self, status: Option<&str>) -> Result<Vec<MemoryItem>, GateError> {
        sqlx::query_as::<_, MemoryItem>(
            "SELECT * FROM curated_memory WHERE $1 IS NULL OR status = $1 ORDER BY memory_id",
        )
        .bind(status)
        .fetch_all(&self.pool)
        .await
        .map_err(db)
    }
}
