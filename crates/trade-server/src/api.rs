//! The HTTP interface under `/v1`, for the frontend (`docs/api.md`).
//!
//! Thin on purpose: each handler checks who is calling, runs one use case of `Harness`,
//! turns errors into `{"error": {"code", "message"}}`, and announces the change on the
//! event stream. Identity is the `X-User-Id` header until real sign-in exists.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use trade_adapters::BrokerFault;
use trade_core::{
    ActionIntentRecord, ApprovalId, ApprovalRecord, ApprovalStatus, GateError, PreDispatchError,
    RiskTier, TaskId, TaskRecord, TaskState, TaskTypeConfig, UsdCents,
};

use crate::workflow::{Framing, Harness, Step, TaskRequest};

/// Human time in unit economics, per hour.
const HUMAN_RATE_CENTS: i64 = 12_000;

// ---- Errors and extractors ---------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

impl From<GateError> for ApiError {
    fn from(e: GateError) -> Self {
        use PreDispatchError as P;
        use StatusCode as S;
        let message = e.to_string();
        let (status, code) = match &e {
            GateError::PreDispatch(P::SeparationOfDutiesViolation(_)) => {
                (S::FORBIDDEN, "self_approval_forbidden".to_string())
            }
            GateError::PreDispatch(P::ApprovalNotFound(_)) => {
                (S::NOT_FOUND, "approval_not_found".into())
            }
            GateError::PreDispatch(P::ApprovalNotApproved(_)) => {
                (S::CONFLICT, "approval_not_pending".into())
            }
            GateError::PreDispatch(P::UnreconciledIntentExists(_)) => {
                (S::CONFLICT, "unresolved_actions".into())
            }
            GateError::PreDispatch(p) => {
                let code = p.code();
                (
                    S::CONFLICT,
                    code.split(':').next().unwrap_or("denied").to_string(),
                )
            }
            GateError::StateTransition(_) => (S::CONFLICT, "invalid_transition".into()),
            GateError::Conflict(_) => (S::CONFLICT, "state_conflict".into()),
            GateError::Schema(_) => (S::UNPROCESSABLE_ENTITY, "invalid_request".into()),
            GateError::NotFound { what, .. } => (S::NOT_FOUND, format!("{what}_not_found")),
            // The model or the market data: its own code when it gives one, such as
            // `model_unreachable`.
            GateError::External(s) => (S::BAD_GATEWAY, external_code(s)),
            _ => (S::INTERNAL_SERVER_ERROR, "internal".into()),
        };
        Self {
            status,
            code,
            message,
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn external_code(detail: &str) -> String {
    match detail.split_once(':') {
        Some((code, _)) if code.chars().all(|c| c.is_ascii_lowercase() || c == '_') => code.into(),
        _ => "external_error".into(),
    }
}

fn forbidden(code: &str, message: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, code, message)
}

fn conflict(code: &str, message: &str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", message)
}

/// The caller, from `X-User-Id`.
pub struct User(pub String);

impl<S: Send + Sync> FromRequestParts<S> for User {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts
            .headers
            .get("x-user-id")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| User(id.to_string()))
            .ok_or_else(|| {
                let message = "send the caller's ID in the X-User-Id header";
                ApiError::new(StatusCode::UNAUTHORIZED, "missing_identity", message)
            })
    }
}

/// A JSON body whose errors have the same shape as every other error.
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(value)| Body(value))
            .map_err(|e| invalid(e.body_text()))
    }
}

// ---- Application ---------------------------------------------------------------------------

/// What the event stream announces. Clients fetch the task again for details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub task_id: String,
    pub state: &'static str,
    pub state_version: i64,
}

#[derive(Clone)]
pub struct App(Arc<Inner>);

struct Inner {
    harness: Harness,
    changes: broadcast::Sender<Change>,
    dev: bool,
}

impl App {
    /// `dev` adds the `/v1/dev` endpoints for frontend work; never in production.
    pub fn new(harness: Harness, dev: bool) -> Self {
        let (changes, _) = broadcast::channel(256);
        Self(Arc::new(Inner {
            harness,
            changes,
            dev,
        }))
    }

    pub fn harness(&self) -> &Harness {
        &self.0.harness
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.0.changes.subscribe()
    }

    pub fn router(&self, cors_origins: &[String]) -> Router {
        let mut v1 = Router::new()
            .route("/meta/states", get(states))
            .route("/task-types", get(task_types))
            .route("/tasks", get(list_tasks).post(create_task))
            .route("/tasks/{id}", get(task_detail))
            .route("/tasks/{id}/events", get(task_events))
            .route("/tasks/{id}/actions", get(task_actions))
            .route("/tasks/{id}/evaluations", get(task_evaluations))
            .route("/tasks/{id}/advance", post(advance))
            .route("/tasks/{id}/answer", post(answer))
            .route("/tasks/{id}/settle", post(settle))
            .route("/tasks/{id}/cancel", post(cancel))
            .route("/tasks/{id}/reopen", post(reopen))
            .route("/tasks/{id}/hand-back", post(hand_back))
            .route("/tasks/{id}/limit", post(limit))
            .route("/approvals", get(list_approvals))
            .route("/approvals/{id}", get(approval_detail))
            .route("/approvals/{id}/decision", post(decide))
            .route("/metrics/scorecard", get(scorecard))
            .route("/stream", get(stream));
        if self.0.dev {
            v1 = v1
                .route("/dev/seed", post(dev_seed))
                .route("/dev/clock", post(dev_clock))
                .route("/dev/broker-fault", post(dev_broker_fault));
        }
        let origins: Vec<HeaderValue> =
            cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        let cors = CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(Any)
            .allow_headers(Any);
        Router::new()
            .route("/health", get(health))
            .nest("/v1", v1)
            .layer(cors)
            .with_state(self.clone())
    }

    /// Handle passed deadlines and announce what changed. The server runs this
    /// periodically.
    pub async fn sweep(&self) -> Result<usize, GateError> {
        let handled = self.0.harness.sweep().await?;
        for task_id in &handled {
            self.announce("task.updated", task_id).await;
        }
        Ok(handled.len())
    }

    async fn announce(&self, kind: &'static str, task_id: &TaskId) {
        if let Ok(Some(task)) = self.0.harness.store.task(task_id).await {
            // Sending fails only when nobody is listening, which is fine.
            let _ = self.0.changes.send(Change {
                kind,
                task_id: task_id.to_string(),
                state: task.state().code(),
                state_version: task.version(),
            });
        }
    }
}

// ---- Views -----------------------------------------------------------------------------------

/// Label and who should act next, for each state.
fn label(code: &str) -> (&'static str, &'static str) {
    match code {
        "QUEUED" => ("排队", "operator"),
        "RUNNING" => ("运行中", "operator"),
        "AWAITING_INPUT" => ("等回答", "requester"),
        "AWAITING_APPROVAL" => ("等批准", "approver"),
        "OUTCOME_UNKNOWN" => ("结果未知", "reconciler"),
        "LIMIT_REACHED" => ("触上限", "owner"),
        "VERIFYING" => ("验收中", "verifier"),
        "SETTLING" => ("待确认", "requester"),
        "COMPLETED" => ("已完成", "none"),
        "FAILED" => ("已失败", "owner"),
        "HANDED_OFF" => ("转人工", "assignee"),
        "DECLINED" => ("被否决", "requester"),
        _ => ("已取消", "none"),
    }
}

/// UTC, ISO 8601, with `Z`.
pub fn iso(epoch_s: i64) -> String {
    let (days, secs) = (epoch_s.div_euclid(86_400), epoch_s.rem_euclid(86_400));
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (secs / 3_600, secs % 3_600 / 60, secs % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn task_view(t: &TaskRecord) -> Value {
    let code = t.state().code();
    let (state_label, owner_role) = label(code);
    json!({
        "id": t.task_id,
        "task_type": t.task_type,
        "config_version": t.config_version,
        "state": code,
        "state_label": state_label,
        "owner_role": owner_role,
        "state_detail": t.state().detail(),
        "state_version": t.version(),
        "requester": t.requester_id,
        "acting_principal": t.acting_principal,
        "markets": t.authorized_markets,
        "wait": t.state().wait_meta().map(|w| json!({
            "owner": w.owner_id,
            "deadline": iso(w.deadline_epoch_s),
            "on_timeout": w.timeout_action.as_str(),
        })),
        "usage": {
            "turns": t.turns_used,
            "spend_usd": t.spend.to_dollars(),
            "retries": t.retries_used,
            "consecutive_denials": t.consecutive_denials,
            "reworks": t.reworks_used,
            "reopened": t.reopened_count,
            "granted_turns": t.granted_turns,
            "granted_spend_usd": t.granted_spend.to_dollars(),
        },
        "allowed_next_states": t.state().next_codes(),
    })
}

fn rule(tier: Option<RiskTier>) -> &'static str {
    match tier {
        Some(RiskTier::R3) => {
            "R3: consequential and not reversible; a person approves this exact payload, once"
        }
        Some(RiskTier::R2) => "R2: reversible write; this task type asks a person for every action",
        _ => "not in the task type's configuration",
    }
}

fn approval_view(h: &Harness, a: &ApprovalRecord) -> Value {
    let tier = h.task_type.lookup_tool_risk(&a.tool_name);
    json!({
        "id": a.approval_id,
        "task_id": a.task_id,
        "tool": a.tool_name,
        "target": a.target,
        "tier": tier.map(RiskTier::as_str),
        "rule": rule(tier),
        // The exact content approved: show this first.
        "payload": serde_json::from_str::<Value>(&a.args_json).unwrap_or(Value::Null),
        "payload_hash": a.payload_hash,
        "status": a.status.as_str(),
        "requester": a.requester_id,
        "approver": a.approver_id,
        "note": a.decision_note,
        "expires_at": iso(a.expires_at_epoch_s),
    })
}

/// Where `advance` stopped, as the API and the command line report it.
pub fn step_view(step: &Step) -> Value {
    match step {
        Step::NeedsApproval { approval_id, tool } => {
            json!({"kind": "needs_approval", "approval_id": approval_id, "tool": tool})
        }
        Step::Denied(reason) => json!({"kind": "denied", "reason": reason}),
        Step::Delivered(m) => json!({
            "kind": "delivered",
            "completeness": m.completeness.as_str(),
            "gaps": m.gaps,
        }),
        Step::HandedOff(owner) => json!({"kind": "handed_off", "owner": owner}),
        Step::Waiting(state) => json!({"kind": "waiting", "state": state}),
    }
}

fn action_view(a: &ActionIntentRecord) -> Value {
    json!({
        "idempotency_key": a.idempotency_key,
        "task_id": a.task_id,
        "tool_name": a.tool_name,
        "target": a.target,
        "args": serde_json::from_str::<Value>(&a.args_json).unwrap_or(Value::Null),
        "payload_hash": a.payload_hash,
        "status": a.status.as_str(),
        "backend_receipt_id": a.backend_receipt_id,
        "readback_verified": a.readback_verified,
    })
}

fn task_type_view(t: &TaskTypeConfig) -> Value {
    let tools: BTreeMap<&str, &str> = t
        .allowed_tools
        .iter()
        .map(|(tool, tier)| (tool.as_str(), tier.as_str()))
        .collect();
    json!({
        "task_type": t.task_type,
        "version": t.version,
        "shape_level": t.shape_level.as_str(),
        "shape_rationale": t.shape_rationale,
        "autonomy_level": t.autonomy_level.as_str(),
        "allowed_tools": tools,
        "budget_limits": t.budget_limits,
        "done_criteria": t.done_criteria,
        "waits": t.waits,
    })
}

async fn detail(h: &Harness, id: &TaskId) -> ApiResult<Value> {
    let task = h.task(id).await?;
    let approvals: Vec<Value> = h
        .store
        .task_approvals(id)
        .await?
        .iter()
        .map(|a| approval_view(h, a))
        .collect();
    // What FRAME asked, from the event that put the task in AWAITING_INPUT.
    let mut questions: Vec<String> = Vec::new();
    if matches!(task.state(), TaskState::AwaitingInput(_)) {
        let events = h.store.events(id).await?;
        if let Some(asked) = events
            .iter()
            .rev()
            .find_map(|e| e.detail.strip_prefix("needs_input:"))
        {
            questions = asked.split(',').map(str::to_string).collect();
        }
    }
    let mut view = task_view(&task);
    view["open_questions"] = json!(questions);
    view["contract"] = json!(h.store.contract(id).await?);
    view["plan"] = match h.store.plan(id).await? {
        Some(plan) => {
            let done = h.progress(id, &plan).await?;
            let steps: Vec<Value> = plan
                .steps
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "tool": s.tool,
                        "depends_on": s.depends_on,
                        "skippable": s.skippable,
                        "status": plan.status_of(s, &done).as_str(),
                    })
                })
                .collect();
            json!({"version": plan.version, "steps": steps})
        }
        None => Value::Null,
    };
    view["approvals"] = json!(approvals);
    view["draft"] = json!(h.store.latest_draft(id).await?);
    view["delivery"] = json!(h.store.delivery(id).await?);
    view["handoff"] = json!(h.store.handoff(id).await?);
    Ok(view)
}

fn page<T>(items: Vec<T>, total: i64) -> Json<Value>
where
    T: Serialize,
{
    Json(json!({"items": items, "total": total}))
}

// ---- Requests --------------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
    requester: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl ListQuery {
    fn window(&self) -> (i64, i64) {
        (
            self.limit.unwrap_or(50).clamp(1, 200),
            self.offset.unwrap_or(0).max(0),
        )
    }
}

#[derive(Deserialize)]
struct Reason {
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct SettleBody {
    /// `accept` or `return`.
    decision: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    review_minutes: f64,
}

#[derive(Deserialize)]
struct LimitBody {
    /// `grant` or `deliver`.
    decision: String,
    #[serde(default)]
    extra_turns: u32,
    #[serde(default)]
    extra_spend_usd: f64,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct DecisionBody {
    /// `approve` or `reject`.
    decision: String,
    reason: Option<String>,
}

// ---- Handlers --------------------------------------------------------------------------------

async fn health(State(app): State<App>) -> Json<Value> {
    let h = app.harness();
    let model = h.agent.as_ref().map(|a| a.model.name());
    Json(json!({"status": "ok", "database": h.store.backend(), "model": model}))
}

async fn states() -> Json<Value> {
    let rows: Vec<Value> = TaskState::all_codes()
        .iter()
        .map(|code| {
            let (state_label, owner_role) = label(code);
            json!({"state": code, "state_label": state_label, "owner_role": owner_role})
        })
        .collect();
    Json(json!(rows))
}

async fn task_types(State(app): State<App>) -> Json<Value> {
    Json(json!([task_type_view(&app.harness().task_type)]))
}

async fn create_task(
    State(app): State<App>,
    User(user): User,
    Body(req): Body<TaskRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let h = app.harness();
    let (task, created) = h.start(&user, "api", &req).await?;
    if created {
        app.announce("task.created", &task.task_id).await;
    }
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let body = json!({"task": detail(h, &task.task_id).await?, "created": created});
    Ok((status, Json(body)))
}

async fn list_tasks(
    State(app): State<App>,
    _: User,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let (limit, offset) = q.window();
    let (tasks, total) = app
        .harness()
        .store
        .task_page(q.state.as_deref(), q.requester.as_deref(), limit, offset)
        .await?;
    Ok(page(tasks.iter().map(task_view).collect(), total))
}

async fn task_detail(
    State(app): State<App>,
    _: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(detail(app.harness(), &TaskId::new(id)).await?))
}

/// The task's ID, after checking the task exists.
async fn require_task(h: &Harness, id: String) -> ApiResult<TaskId> {
    let id = TaskId::new(id);
    h.task(&id).await?;
    Ok(id)
}

/// Check that the task waits in `state` and that `user` is the person it waits for.
async fn waiting_for(
    h: &Harness,
    id: &TaskId,
    user: &str,
    state: &str,
    code: &str,
) -> ApiResult<()> {
    let task = h.task(id).await?;
    if task.state().code() != state {
        return Err(conflict(code, &format!("the task is not in {state}")));
    }
    if task.state().wait_meta().map(|w| w.owner_id.as_str()) != Some(user) {
        let message = "only the person the task waits for decides";
        return Err(forbidden("not_allowed", message));
    }
    Ok(())
}

/// Announce the change on the stream and return the task as it is now.
async fn updated(app: &App, id: &TaskId) -> ApiResult<Json<Value>> {
    app.announce("task.updated", id).await;
    Ok(Json(detail(app.harness(), id).await?))
}

async fn task_events(
    State(app): State<App>,
    _: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = require_task(h, id).await?;
    let events: Vec<Value> = h
        .store
        .events(&id)
        .await?
        .into_iter()
        .map(|e| {
            let at = iso(e.created_epoch_s);
            let mut v = json!(e);
            v["at"] = json!(at);
            v
        })
        .collect();
    Ok(Json(json!(events)))
}

async fn task_actions(
    State(app): State<App>,
    _: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = require_task(h, id).await?;
    let actions: Vec<Value> = h
        .store
        .actions(&id)
        .await?
        .iter()
        .map(action_view)
        .collect();
    Ok(Json(json!(actions)))
}

async fn task_evaluations(
    State(app): State<App>,
    _: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = require_task(h, id).await?;
    Ok(Json(json!(h.store.evaluations(&id).await?)))
}

/// Run the agent's loop until it needs a person.
async fn advance(
    State(app): State<App>,
    _: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = TaskId::new(id);
    let step = h.advance(&id).await?;
    app.announce("task.updated", &id).await;
    Ok(Json(
        json!({"step": step_view(&step), "task": detail(h, &id).await?}),
    ))
}

async fn answer(
    State(app): State<App>,
    User(user): User,
    Path(id): Path<String>,
    Body(req): Body<TaskRequest>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = TaskId::new(id);
    let task = h.task(&id).await?;
    if user != task.requester_id {
        return Err(forbidden(
            "not_requester",
            "only the requester answers its questions",
        ));
    }
    if !matches!(task.state(), TaskState::AwaitingInput(_)) {
        return Err(conflict(
            "not_awaiting_input",
            "the task has no open question",
        ));
    }
    let questions = match h.frame(&id, &req).await? {
        Framing::Ready(_) => vec![],
        Framing::NeedsInput(questions) => questions,
    };
    app.announce("task.updated", &id).await;
    Ok(Json(
        json!({"open_questions": questions, "task": detail(h, &id).await?}),
    ))
}

async fn settle(
    State(app): State<App>,
    User(user): User,
    Path(id): Path<String>,
    Body(body): Body<SettleBody>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = TaskId::new(id);
    waiting_for(h, &id, &user, "SETTLING", "not_settling").await?;
    match body.decision.as_str() {
        "accept" => {
            h.confirm_settlement(&id, &user, body.review_minutes)
                .await?
        }
        "return" => h.dispute(&id, &body.comment, &user).await?,
        other => {
            return Err(invalid(format!(
                "decision must be accept or return, not {other:?}"
            )));
        }
    };
    updated(&app, &id).await
}

/// Diagram 4a: the owner a task out of budget waits for grants more, or has what is done
/// delivered.
async fn limit(
    State(app): State<App>,
    User(user): User,
    Path(id): Path<String>,
    Body(body): Body<LimitBody>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = TaskId::new(id);
    waiting_for(h, &id, &user, "LIMIT_REACHED", "not_limit_reached").await?;
    match body.decision.as_str() {
        "grant" if body.extra_turns > 0 || body.extra_spend_usd > 0.0 => {
            let spend = UsdCents::from_dollars(body.extra_spend_usd.max(0.0));
            h.grant_budget(&id, &user, body.extra_turns, spend, &body.reason)
                .await?;
        }
        "grant" => return Err(invalid("a grant needs extra_turns or extra_spend_usd")),
        "deliver" => {
            h.deliver_what_is_done(&id).await?;
        }
        other => {
            return Err(invalid(format!(
                "decision must be grant or deliver, not {other:?}"
            )));
        }
    }
    updated(&app, &id).await
}

/// What a person may do to a task outside the flow.
enum Op {
    Cancel,
    Reopen,
    HandBack,
}

/// Cancel and reopen are the requester's; handing back is the current owner's.
async fn by_requester_or_owner(
    app: &App,
    user: &str,
    id: TaskId,
    op: Op,
    reason: &str,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let task = h.task(&id).await?;
    let owner = task.state().wait_meta().map(|w| w.owner_id.as_str());
    let allowed = match op {
        Op::HandBack => owner == Some(user),
        Op::Cancel | Op::Reopen => user == task.requester_id,
    };
    if !allowed {
        return Err(forbidden(
            "not_allowed",
            "this caller may not do that to this task",
        ));
    }
    match op {
        Op::Cancel => h.cancel(&id, reason, user).await?,
        Op::Reopen => h.reopen(&id, reason, user).await?,
        Op::HandBack => h.hand_back(&id, user).await?,
    };
    updated(app, &id).await
}

async fn cancel(
    State(app): State<App>,
    User(user): User,
    Path(id): Path<String>,
    Body(body): Body<Reason>,
) -> ApiResult<Json<Value>> {
    by_requester_or_owner(&app, &user, TaskId::new(id), Op::Cancel, &body.reason).await
}

async fn reopen(
    State(app): State<App>,
    User(user): User,
    Path(id): Path<String>,
    Body(body): Body<Reason>,
) -> ApiResult<Json<Value>> {
    by_requester_or_owner(&app, &user, TaskId::new(id), Op::Reopen, &body.reason).await
}

async fn hand_back(
    State(app): State<App>,
    User(user): User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    by_requester_or_owner(&app, &user, TaskId::new(id), Op::HandBack, "").await
}

async fn list_approvals(
    State(app): State<App>,
    _: User,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let status = q
        .status
        .as_deref()
        .map(ApprovalStatus::from_db_str)
        .transpose()
        .map_err(|e| invalid(e.0))?;
    let all = h.store.approvals(status).await?;
    let (limit, offset) = q.window();
    let items = all
        .iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|a| approval_view(h, a))
        .collect();
    Ok(page::<Value>(items, all.len() as i64))
}

async fn load_approval(h: &Harness, id: &ApprovalId) -> ApiResult<ApprovalRecord> {
    h.store
        .approval(id)
        .await?
        .ok_or_else(|| GateError::from(PreDispatchError::ApprovalNotFound(id.to_string())).into())
}

async fn approval_detail(
    State(app): State<App>,
    _: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let approval = load_approval(h, &ApprovalId::new(id)).await?;
    Ok(Json(approval_view(h, &approval)))
}

async fn decide(
    State(app): State<App>,
    User(user): User,
    Path(id): Path<String>,
    Body(body): Body<DecisionBody>,
) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let id = ApprovalId::new(id);
    let approve = match body.decision.as_str() {
        "approve" => true,
        "reject" => false,
        other => {
            return Err(invalid(format!(
                "decision must be approve or reject, not {other:?}"
            )));
        }
    };
    let task_id = load_approval(h, &id).await?.task_id;
    let result = h
        .decide_approval(&id, &user, approve, body.reason.as_deref())
        .await;
    // An expired approval still changed the task.
    app.announce("task.updated", &task_id).await;
    result?;
    Ok(Json(approval_view(h, &load_approval(h, &id).await?)))
}

async fn scorecard(State(app): State<App>, _: User) -> ApiResult<Json<Value>> {
    let store = &app.harness().store;
    let (tasks_by_state, approvals_by_status) = store.status_counts().await?;
    Ok(Json(json!({
        "tasks_by_state": tasks_by_state,
        "approvals_by_status": approvals_by_status,
        "unit_economics": store.unit_economics(HUMAN_RATE_CENTS).await?,
        "human_hourly_rate_usd": HUMAN_RATE_CENTS as f64 / 100.0,
    })))
}

/// Server-Sent Events: `task.created` and `task.updated`, with a comment every 15 s.
async fn stream(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = futures_util::stream::unfold(app.subscribe(), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(change) => {
                    let event = Event::default().event(change.kind).json_data(&change);
                    return Some((Ok(event.unwrap_or_default()), rx));
                }
                // A slow client missed some; it refetches on the next one.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ---- Development endpoints -------------------------------------------------------------------

/// Demo tasks, each brought to its state by the real use cases. Running it again leaves
/// existing demo tasks as they are.
async fn dev_seed(State(app): State<App>) -> ApiResult<Json<Value>> {
    let h = app.harness();
    let mut seeded = BTreeMap::new();
    for name in [
        "queued",
        "needs-input",
        "draft-approval",
        "order-approval",
        "settling",
        "completed",
        "declined",
        "handed-off",
    ] {
        let key = format!("demo:{name}");
        let (task, created) = if name == "queued" {
            h.admit(&key, "alice", "dev_seed").await?
        } else {
            let mut request = h.rebalance_request("2026-02-10");
            request.trigger_key = Some(key);
            if name == "needs-input" {
                request.as_of_date = None;
            }
            h.start("alice", "dev_seed", &request).await?
        };
        let id = task.task_id;
        if created && name != "queued" {
            if name == "handed-off" {
                h.hand_off(&id, "demo_hand_off").await?;
            } else if name != "needs-input" {
                let mut step = h.advance(&id).await?;
                let decisions: &[bool] = match name {
                    "order-approval" => &[true],
                    "settling" | "completed" => &[true, true],
                    "declined" => &[false],
                    _ => &[],
                };
                for &approve in decisions {
                    if let Step::NeedsApproval { approval_id, .. } = &step {
                        h.decide_approval(approval_id, "bob", approve, None).await?;
                        step = h.advance(&id).await?;
                    }
                }
                if name == "completed" {
                    h.confirm_settlement(&id, "alice", 2.0).await?;
                }
            }
        }
        app.announce(
            if created {
                "task.created"
            } else {
                "task.updated"
            },
            &id,
        )
        .await;
        let state = h.task(&id).await?.state().code();
        seeded.insert(name, json!({"task_id": id, "state": state}));
    }
    Ok(Json(json!({"tasks": seeded})))
}

#[derive(Deserialize)]
struct ClockBody {
    advance_s: i64,
}

/// Move the clock forward and handle the deadlines that passed.
async fn dev_clock(State(app): State<App>, Body(body): Body<ClockBody>) -> ApiResult<Json<Value>> {
    app.harness().store.advance_clock(body.advance_s.max(0));
    let handled = app.sweep().await?;
    Ok(Json(
        json!({"now": iso(app.harness().store.now()), "handled": handled}),
    ))
}

#[derive(Deserialize)]
struct FaultBody {
    /// `drop_before_fill` or `timeout_after_fill`, for the next order.
    fault: String,
}

async fn dev_broker_fault(
    State(app): State<App>,
    Body(body): Body<FaultBody>,
) -> ApiResult<Json<Value>> {
    let fault = match body.fault.as_str() {
        "drop_before_fill" => BrokerFault::DropBeforeFill,
        "timeout_after_fill" => BrokerFault::TimeoutAfterFill,
        other => return Err(invalid(format!("unknown fault {other:?}"))),
    };
    app.harness().broker.inject_next_fault(fault);
    Ok(Json(json!({"next_order": body.fault})))
}

#[cfg(test)]
mod tests {
    use super::iso;

    #[test]
    fn timestamps_are_utc_iso_8601() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso(1_795_651_200), "2026-11-26T00:00:00Z");
    }
}
