//! The model-driven loop against a scripted model behind a real OpenAI-compatible HTTP
//! endpoint. Each test checks what the agent did, what the gate allowed, and what the model
//! was shown in return.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use trade_core::{ApprovalId, Completeness, GateError, TaskId};
use trade_server::agent::{OBSERVATION, REPLY};
use trade_server::{AgentModel, Harness, Step};

/// Replies in order; then it keeps saying `finish`. Keeps every request it receives.
#[derive(Clone, Default)]
struct Script {
    replies: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Script {
    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Everything the model was shown in its latest request.
    fn last_prompt(&self) -> String {
        let requests = self.requests.lock().unwrap();
        requests
            .last()
            .map(|r| r["messages"].to_string())
            .unwrap_or_default()
    }
}

async fn reply(State(script): State<Script>, Json(body): Json<Value>) -> Json<Value> {
    script.requests.lock().unwrap().push(body);
    let content = script
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| r#"{"tool": "finish", "args": {}}"#.into());
    Json(json!({
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 900, "completion_tokens": 40},
    }))
}

/// A harness whose agent is the scripted model, and the script.
async fn agent_harness(h: Harness, replies: &[&str]) -> (Harness, Script) {
    let script = Script::default();
    script
        .replies
        .lock()
        .unwrap()
        .extend(replies.iter().map(|r| r.to_string()));
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(script.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let agent = AgentModel::load(&common::root(), &url, "scripted", None).unwrap();
    (h.with_agent(agent), script)
}

/// The weights the committee suggests for 2026-02-10, as the model would copy them.
const COMMITTEE_DRAFT: &str = r#"{"thought": "use the committee", "tool": "create_rebalance_draft",
    "args": {"weights_bps": {"AAPL_MOAT": 2681, "CAT_IND": 3500, "MSFT_QUAL": 3144}}}"#;

async fn approve(h: &Harness, step: Step, approve: bool) -> Step {
    let Step::NeedsApproval { approval_id, .. } = step else {
        panic!("expected an approval request, got {step:?}");
    };
    let id: ApprovalId = approval_id;
    let task = h
        .decide_approval(&id, "approver_bob", approve, None)
        .await
        .unwrap();
    h.advance(&task.task_id).await.unwrap()
}

async fn transcript(h: &Harness, task: &TaskId) -> Vec<String> {
    h.store
        .events(task)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == REPLY || e.kind == OBSERVATION)
        .map(|e| e.detail)
        .collect()
}

/// The whole of ACT with the model: it researches, proposes a draft the gate refuses
/// twice, corrects it from the reasons it is shown, waits for two approvals, and finishes.
#[tokio::test]
async fn the_model_drives_the_task_to_delivery_and_learns_from_refusals() {
    let (h, script) = agent_harness(
        common::harness().await,
        &[
            r#"{"thought": "start", "tool": "research", "args": {}}"#,
            r#"{"tool": "create_rebalance_draft", "args": {"weights_bps": {"TRAP_CO": 2000, "CAT_IND": 3500}}}"#,
            r#"{"tool": "create_rebalance_draft", "args": {"weights_bps": {"CAT_IND": 4000, "MSFT_QUAL": 3000}}}"#,
            COMMITTEE_DRAFT,
            r#"{"tool": "submit_paper_orders", "args": {}}"#,
            r#"{"tool": "finish", "args": {"note": "draft and orders done"}}"#,
        ],
    )
    .await;
    let task = common::start(&h, "2026-02-10").await;

    let step = h.advance(&task).await.unwrap();
    assert!(
        matches!(step, Step::NeedsApproval { ref tool, .. } if tool == "create_rebalance_draft")
    );
    let said = transcript(&h, &task).await.join("\n");
    assert!(
        said.contains("vetoed_ticker_in_portfolio:TRAP_CO"),
        "{said}"
    );
    assert!(said.contains("single_weight_exceeded:CAT_IND"), "{said}");
    // The second refusal was in front of the model when it proposed the third draft.
    assert!(
        script
            .last_prompt()
            .contains("single_weight_exceeded:CAT_IND")
    );

    // The approved draft is written as approved, without asking the model again.
    let step = approve(&h, step, true).await;
    assert!(matches!(step, Step::NeedsApproval { ref tool, .. } if tool == "submit_paper_orders"));
    let step = approve(&h, step, true).await;
    let Step::Delivered(manifest) = step else {
        panic!("{step:?}")
    };
    assert_eq!(manifest.completeness, Completeness::Complete);
    assert_eq!(script.calls(), 6);
    assert_eq!(h.broker.count_orders_for_task(&task), 1);
    let draft = h.store.latest_draft(&task).await.unwrap().unwrap();
    assert_eq!(draft.weights["CAT_IND"].bps(), 3500);

    // Turns: research's three per examined security, one per model reply, and one per
    // approved action sent, since every gate decision counts.
    let done = h.task(&task).await.unwrap();
    assert_eq!(done.turns_used, 21 + 6 + 2);
    assert_eq!(done.consecutive_denials, 0);
}

/// C2 with the model: refused orders are skipped, and the model is told before it finishes.
#[tokio::test]
async fn refused_orders_are_reported_to_the_model_and_the_draft_delivered() {
    let (h, script) = agent_harness(
        common::harness().await,
        &[
            r#"{"tool": "research", "args": {}}"#,
            COMMITTEE_DRAFT,
            r#"{"tool": "submit_paper_orders", "args": {}}"#,
            r#"{"tool": "finish", "args": {"note": "orders refused"}}"#,
        ],
    )
    .await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    let step = approve(&h, step, true).await;
    let step = approve(&h, step, false).await;
    let Step::Delivered(manifest) = step else {
        panic!("{step:?}")
    };
    assert_eq!(manifest.completeness, Completeness::Partial);
    assert!(
        script
            .last_prompt()
            .contains("submit_paper_orders: REJECTED")
    );
    assert_eq!(h.broker.count_orders_for_task(&task), 0);
}

/// A reply that is not a proposal counts as a refusal; finishing without any work fails
/// verification every time, and after the last rework a person takes over.
#[tokio::test]
async fn nonsense_and_empty_finishes_end_with_a_person() {
    let (h, script) = agent_harness(
        common::harness().await,
        &[
            "I think we should buy some value stocks.",
            r#"{"tool": "buy_everything", "args": {}}"#,
        ],
    )
    .await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    assert_eq!(
        step,
        Step::HandedOff(h.task_type.waits.escalation_owner_id.clone())
    );
    let said = transcript(&h, &task).await.join("\n");
    assert!(
        said.contains("Refused: no JSON object in the reply"),
        "{said}"
    );
    assert!(
        said.contains("buy_everything: refused: no such tool"),
        "{said}"
    );
    // After a failed verification the model is shown the gaps before it tries again.
    assert!(script.last_prompt().contains("Verification failed"));
    let handed = h.task(&task).await.unwrap();
    assert_eq!(handed.reworks_used, h.task_type.budget_limits.max_reworks);
}

/// The model cannot keep the task busy forever: the turn limit stops it.
#[tokio::test]
async fn the_turn_limit_stops_a_model_that_never_finishes() {
    let screens = vec![r#"{"tool": "screen", "args": {}}"#; 20];
    let (h, script) = agent_harness(
        common::harness_with(|t| t.budget_limits.max_turns = 5).await,
        &screens,
    )
    .await;
    let task = common::start(&h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    // Nothing was drafted, so verification of what was finished fails and a person takes over.
    assert!(matches!(step, Step::HandedOff(_)), "{step:?}");
    assert_eq!(script.calls(), 5);
    let events = h.store.events(&task).await.unwrap();
    assert!(events.iter().any(|e| e.detail == "limit_reached:max_turns"));
}

/// No model, no progress: the task is left as it was and the error says why.
#[tokio::test]
async fn an_unreachable_model_leaves_the_task_unchanged() {
    let agent = AgentModel::load(&common::root(), "http://127.0.0.1:9/v1", "none", None).unwrap();
    let h = common::harness().await.with_agent(agent);
    let task = common::start(&h, "2026-02-10").await;
    let before = h.task(&task).await.unwrap();
    let err = h.advance(&task).await.unwrap_err();
    assert!(
        matches!(err, GateError::Storage(ref m) if m.starts_with("model_unreachable")),
        "{err:?}"
    );
    let after = h.task(&task).await.unwrap();
    assert_eq!(
        (after.state(), after.turns_used),
        (before.state(), before.turns_used)
    );
}
