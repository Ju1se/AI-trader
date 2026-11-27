//! The HTTP interface, called in process. Each test is one behaviour the frontend relies
//! on (`docs/api.md`): identity, the error shape, deduplication, both people's steps, the
//! timeouts, the event stream and the development endpoints.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use trade_server::App;

async fn app(dev: bool) -> (App, Router) {
    let app = App::new(common::harness().await, dev);
    let router = app.router(&["http://localhost:3000".to_string()]);
    (app, router)
}

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    user: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(user) = user {
        request = request.header("x-user-id", user);
    }
    let request = match body {
        Some(b) => request
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn new_task(as_of: Option<&str>) -> Value {
    json!({
        "goal": "rebalance",
        "as_of_date": as_of,
        "portfolio_capital_usd": 1_000_000.0,
        "trigger_key": format!("api-test-{}", as_of.unwrap_or("none")),
    })
}

/// Create a task as alice and return its ID.
async fn create(router: &Router, as_of: Option<&str>) -> String {
    let (status, body) = call(
        router,
        "POST",
        "/v1/tasks",
        Some("alice"),
        Some(new_task(as_of)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["task"]["id"].as_str().unwrap().to_string()
}

async fn advance(router: &Router, id: &str) -> Value {
    let (status, body) = call(
        router,
        "POST",
        &format!("/v1/tasks/{id}/advance"),
        Some("ops"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn decide(router: &Router, approval: &str, who: &str, decision: &str) -> (StatusCode, Value) {
    let body = json!({"decision": decision, "reason": "test"});
    call(
        router,
        "POST",
        &format!("/v1/approvals/{approval}/decision"),
        Some(who),
        Some(body),
    )
    .await
}

fn state(body: &Value) -> &str {
    body["state"]
        .as_str()
        .unwrap_or_else(|| body["task"]["state"].as_str().unwrap())
}

#[tokio::test]
async fn health_needs_no_identity_but_tasks_do() {
    let (_, router) = app(false).await;
    let (status, body) = call(&router, "GET", "/health", None, None).await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::OK, Some("ok"))
    );
    let (status, body) = call(&router, "GET", "/v1/tasks", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "missing_identity");
}

#[tokio::test]
async fn the_state_table_lists_all_thirteen_and_their_moves() {
    let (_, router) = app(false).await;
    let (_, states) = call(&router, "GET", "/v1/meta/states", None, None).await;
    assert_eq!(states.as_array().unwrap().len(), 13);
    let id = create(&router, Some("2026-02-10")).await;
    let (_, task) = call(
        &router,
        "GET",
        &format!("/v1/tasks/{id}"),
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(state(&task), "RUNNING");
    assert_eq!(
        task["allowed_next_states"],
        json!([
            "AWAITING_INPUT",
            "AWAITING_APPROVAL",
            "OUTCOME_UNKNOWN",
            "LIMIT_REACHED",
            "VERIFYING",
            "FAILED",
            "HANDED_OFF",
            "CANCELLED"
        ])
    );
}

#[tokio::test]
async fn creating_the_same_task_twice_returns_the_first() {
    let (_, router) = app(false).await;
    let body = new_task(Some("2026-02-10"));
    let (first, a) = call(
        &router,
        "POST",
        "/v1/tasks",
        Some("alice"),
        Some(body.clone()),
    )
    .await;
    let (second, b) = call(&router, "POST", "/v1/tasks", Some("alice"), Some(body)).await;
    assert_eq!((first, second), (StatusCode::CREATED, StatusCode::OK));
    assert_eq!(b["created"], false);
    assert_eq!(a["task"]["id"], b["task"]["id"]);
    let (_, list) = call(
        &router,
        "GET",
        "/v1/tasks?state=RUNNING",
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(list["total"], 1);
}

#[tokio::test]
async fn unknown_objects_and_bad_bodies_use_the_error_shape() {
    let (_, router) = app(false).await;
    let (status, body) = call(&router, "GET", "/v1/tasks/TASK-nope", Some("alice"), None).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("task_not_found"))
    );
    let (status, body) = call(
        &router,
        "GET",
        "/v1/approvals/APR-nope",
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("approval_not_found"))
    );
    let (status, body) = call(
        &router,
        "POST",
        "/v1/tasks",
        Some("alice"),
        Some(json!({"as_of_date": 3})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_request"))
    );
}

/// Diagram 0 through the API: two approvals by a second person, delivery, settlement.
#[tokio::test]
async fn full_path_with_both_approvals_delivery_and_settlement() {
    let (_, router) = app(false).await;
    let id = create(&router, Some("2026-02-10")).await;
    let step = advance(&router, &id).await;
    assert_eq!(step["step"]["kind"], "needs_approval");
    let (_, inbox) = call(
        &router,
        "GET",
        "/v1/approvals?status=PENDING",
        Some("bob"),
        None,
    )
    .await;
    assert_eq!(inbox["total"], 1);
    let draft = inbox["items"][0].clone();
    assert_eq!(draft["tier"], "R2");
    assert!(draft["payload"]["weights_bps"].is_object(), "{draft}");
    assert!(draft["expires_at"].as_str().unwrap().ends_with('Z'));

    let approval = draft["id"].as_str().unwrap();
    let (status, body) = decide(&router, approval, "alice", "approve").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("self_approval_forbidden"))
    );
    let (status, body) = decide(&router, approval, "bob", "approve").await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::OK, Some("APPROVED"))
    );

    let step = advance(&router, &id).await;
    assert_eq!(step["step"]["tool"], "submit_paper_orders");
    let orders = step["step"]["approval_id"].as_str().unwrap().to_string();
    decide(&router, &orders, "bob", "approve").await;
    let step = advance(&router, &id).await;
    assert_eq!(step["step"]["kind"], "delivered");
    assert_eq!(step["step"]["completeness"], "COMPLETE");
    assert_eq!(state(&step), "SETTLING");

    let settle = json!({"decision": "accept", "review_minutes": 3.0});
    let (status, body) = call(
        &router,
        "POST",
        &format!("/v1/tasks/{id}/settle"),
        Some("alice"),
        Some(settle),
    )
    .await;
    assert_eq!((status, state(&body)), (StatusCode::OK, "COMPLETED"));
    let (_, events) = call(
        &router,
        "GET",
        &format!("/v1/tasks/{id}/events"),
        Some("alice"),
        None,
    )
    .await;
    assert!(events.as_array().unwrap().len() > 10);
    assert!(events[0]["at"].as_str().unwrap().ends_with('Z'));
    let (_, card) = call(&router, "GET", "/v1/metrics/scorecard", Some("alice"), None).await;
    assert_eq!(card["tasks_by_state"]["COMPLETED"], 1);
    assert_eq!(card["unit_economics"]["completed_tasks"], 1);
}

/// C2 through the API: refusing the draft declines; refusing optional orders is skipped.
#[tokio::test]
async fn refusals_decline_or_skip_by_the_contract() {
    let (_, router) = app(false).await;
    let declined = create(&router, Some("2026-02-10")).await;
    let step = advance(&router, &declined).await;
    decide(
        &router,
        step["step"]["approval_id"].as_str().unwrap(),
        "bob",
        "reject",
    )
    .await;
    let (_, task) = call(
        &router,
        "GET",
        &format!("/v1/tasks/{declined}"),
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(state(&task), "DECLINED");

    let skipped = create(&router, Some("2026-05-01")).await;
    let step = advance(&router, &skipped).await;
    decide(
        &router,
        step["step"]["approval_id"].as_str().unwrap(),
        "bob",
        "approve",
    )
    .await;
    let step = advance(&router, &skipped).await;
    decide(
        &router,
        step["step"]["approval_id"].as_str().unwrap(),
        "bob",
        "reject",
    )
    .await;
    let step = advance(&router, &skipped).await;
    assert_eq!(step["step"]["completeness"], "PARTIAL");
}

/// E1: an approval past its deadline cannot be given, and counts as refused.
#[tokio::test]
async fn an_expired_approval_is_refused() {
    let (app, router) = app(false).await;
    let id = create(&router, Some("2026-02-10")).await;
    let step = advance(&router, &id).await;
    let approval = step["step"]["approval_id"].as_str().unwrap();
    app.harness()
        .store
        .advance_clock(app.harness().task_type.waits.approval_ttl_s + 1);
    let (status, body) = decide(&router, approval, "bob", "approve").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("approval_expired"))
    );
    let (_, task) = call(
        &router,
        "GET",
        &format!("/v1/tasks/{id}"),
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(state(&task), "DECLINED");
    let (status, body) = decide(&router, approval, "bob", "approve").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("approval_not_pending"))
    );
}

#[tokio::test]
async fn only_the_requester_answers_its_questions() {
    let (_, router) = app(false).await;
    let id = create(&router, None).await;
    let (_, task) = call(
        &router,
        "GET",
        &format!("/v1/tasks/{id}"),
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(state(&task), "AWAITING_INPUT");
    assert_eq!(task["open_questions"], json!(["missing_as_of_date"]));
    let path = format!("/v1/tasks/{id}/answer");
    let answer = new_task(Some("2026-02-10"));
    let (status, body) = call(&router, "POST", &path, Some("bob"), Some(answer.clone())).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("not_requester"))
    );
    let (status, body) = call(&router, "POST", &path, Some("alice"), Some(answer.clone())).await;
    assert_eq!((status, state(&body)), (StatusCode::OK, "RUNNING"));
    let (status, body) = call(&router, "POST", &path, Some("alice"), Some(answer)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_awaiting_input"))
    );
}

#[tokio::test]
async fn moves_the_state_does_not_allow_are_refused() {
    let (_, router) = app(false).await;
    let id = create(&router, Some("2026-02-10")).await;
    let settle = json!({"decision": "accept"});
    let (status, body) = call(
        &router,
        "POST",
        &format!("/v1/tasks/{id}/settle"),
        Some("alice"),
        Some(settle),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("not_settling"))
    );
    let (status, body) = call(
        &router,
        "POST",
        &format!("/v1/tasks/{id}/reopen"),
        Some("alice"),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("invalid_transition"))
    );
}

#[tokio::test]
async fn a_returned_delivery_is_reworked_and_delivered_again() {
    let (_, router) = app(false).await;
    let id = create(&router, Some("2026-02-10")).await;
    let step = advance(&router, &id).await;
    decide(
        &router,
        step["step"]["approval_id"].as_str().unwrap(),
        "bob",
        "approve",
    )
    .await;
    let step = advance(&router, &id).await;
    decide(
        &router,
        step["step"]["approval_id"].as_str().unwrap(),
        "bob",
        "reject",
    )
    .await;
    assert_eq!(
        advance(&router, &id).await["step"]["completeness"],
        "PARTIAL"
    );
    let back = json!({"decision": "return", "comment": "place the orders after all"});
    let (status, body) = call(
        &router,
        "POST",
        &format!("/v1/tasks/{id}/settle"),
        Some("alice"),
        Some(back),
    )
    .await;
    assert_eq!((status, state(&body)), (StatusCode::OK, "RUNNING"));
    assert_eq!(body["usage"]["reworks"], 1);
    assert_eq!(advance(&router, &id).await["step"]["kind"], "delivered");
}

#[tokio::test]
async fn only_the_requester_cancels() {
    let (_, router) = app(false).await;
    let id = create(&router, Some("2026-02-10")).await;
    let path = format!("/v1/tasks/{id}/cancel");
    let (status, body) = call(
        &router,
        "POST",
        &path,
        Some("bob"),
        Some(json!({"reason": "x"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("not_allowed"))
    );
    let (status, body) = call(
        &router,
        "POST",
        &path,
        Some("alice"),
        Some(json!({"reason": "x"})),
    )
    .await;
    assert_eq!((status, state(&body)), (StatusCode::OK, "CANCELLED"));
}

/// The stream announces every change; the endpoint speaks Server-Sent Events.
#[tokio::test]
async fn changes_are_announced_on_the_stream() {
    let (app, router) = app(false).await;
    let mut changes = app.subscribe();
    let id = create(&router, Some("2026-02-10")).await;
    let created = changes.recv().await.unwrap();
    assert_eq!(
        (created.kind, created.task_id.as_str(), created.state),
        ("task.created", id.as_str(), "RUNNING")
    );
    advance(&router, &id).await;
    let updated = changes.recv().await.unwrap();
    assert_eq!(
        (updated.kind, updated.state),
        ("task.updated", "AWAITING_APPROVAL")
    );

    let request = Request::builder()
        .uri("/v1/stream")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
}

#[tokio::test]
async fn dev_endpoints_exist_only_when_enabled_and_seed_real_states() {
    let (_, router) = app(false).await;
    let (status, _) = call(&router, "POST", "/v1/dev/seed", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, router) = app(true).await;
    let (status, seeded) = call(&router, "POST", "/v1/dev/seed", None, None).await;
    assert_eq!(status, StatusCode::OK, "{seeded}");
    let states: Vec<&str> = seeded["tasks"]
        .as_object()
        .unwrap()
        .values()
        .map(|t| t["state"].as_str().unwrap())
        .collect();
    for expected in [
        "QUEUED",
        "AWAITING_INPUT",
        "AWAITING_APPROVAL",
        "SETTLING",
        "COMPLETED",
        "DECLINED",
        "HANDED_OFF",
    ] {
        assert!(
            states.contains(&expected),
            "{expected} missing from {states:?}"
        );
    }
    // Seeding again changes nothing.
    let (_, again) = call(&router, "POST", "/v1/dev/seed", None, None).await;
    assert_eq!(again, seeded);

    // Three days later the undisputed delivery has completed and the unanswered question
    // went to a person.
    let (_, swept) = call(
        &router,
        "POST",
        "/v1/dev/clock",
        None,
        Some(json!({"advance_s": 3 * 86_400 + 1})),
    )
    .await;
    assert!(swept["handled"].as_u64().unwrap() >= 2, "{swept}");
    let settling = seeded["tasks"]["settling"]["task_id"].as_str().unwrap();
    let (_, task) = call(
        &router,
        "GET",
        &format!("/v1/tasks/{settling}"),
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(state(&task), "COMPLETED");
}

/// A lost broker reply through the API: reconciled, one order, delivered complete.
#[tokio::test]
async fn a_lost_broker_reply_is_reconciled_through_the_api() {
    let (_, router) = app(true).await;
    let id = create(&router, Some("2026-02-10")).await;
    let step = advance(&router, &id).await;
    decide(
        &router,
        step["step"]["approval_id"].as_str().unwrap(),
        "bob",
        "approve",
    )
    .await;
    let step = advance(&router, &id).await;
    let fault = json!({"fault": "timeout_after_fill"});
    let (status, _) = call(&router, "POST", "/v1/dev/broker-fault", None, Some(fault)).await;
    assert_eq!(status, StatusCode::OK);
    decide(
        &router,
        step["step"]["approval_id"].as_str().unwrap(),
        "bob",
        "approve",
    )
    .await;
    assert_eq!(
        advance(&router, &id).await["step"]["completeness"],
        "COMPLETE"
    );
    let (_, actions) = call(
        &router,
        "GET",
        &format!("/v1/tasks/{id}/actions"),
        Some("alice"),
        None,
    )
    .await;
    let orders: Vec<&Value> = actions
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["tool_name"] == "submit_paper_orders")
        .collect();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0]["status"], "VERIFIED");
}

/// Diagram 4a over the API: a task out of working time waits for its owner. Only that owner
/// decides; a grant runs it on. A task nobody decides on is delivered at the deadline.
#[tokio::test]
async fn a_task_out_of_budget_waits_for_its_owner() {
    let (_, router) = app(true).await;
    let clock = |s: i64| json!({"advance_s": s});
    let id = create(&router, Some("2026-02-10")).await;
    call(&router, "POST", "/v1/dev/clock", None, Some(clock(1_801))).await;
    let step = advance(&router, &id).await;
    assert_eq!(
        step["step"],
        json!({"kind": "waiting", "state": "LIMIT_REACHED"})
    );
    assert_eq!(step["task"]["wait"]["on_timeout"], "DELIVER");

    let path = format!("/v1/tasks/{id}/limit");
    let grant = json!({"decision": "grant", "extra_turns": 5, "reason": "more time"});
    let (status, body) = call(&router, "POST", &path, Some("alice"), Some(grant.clone())).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("not_allowed"))
    );
    let owner = Some("risk_committee_lead");
    let (status, task) = call(&router, "POST", &path, owner, Some(grant)).await;
    assert_eq!((status, state(&task)), (StatusCode::OK, "RUNNING"));
    assert_eq!(task["usage"]["granted_turns"], 5);
    assert_eq!(
        advance(&router, &id).await["step"]["kind"],
        "needs_approval"
    );

    // Nobody decides on this one: at the deadline what is done is delivered. Nothing was
    // drafted, so a person takes it over.
    let other = create(&router, Some("2026-05-01")).await;
    call(&router, "POST", "/v1/dev/clock", None, Some(clock(1_801))).await;
    assert_eq!(
        advance(&router, &other).await["step"]["state"],
        "LIMIT_REACHED"
    );
    call(&router, "POST", "/v1/dev/clock", None, Some(clock(14_401))).await;
    let (_, task) = call(&router, "GET", &format!("/v1/tasks/{other}"), owner, None).await;
    assert_eq!(state(&task), "HANDED_OFF");
}
