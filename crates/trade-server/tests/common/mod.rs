//! Shared set-up for the integration tests: the repository's own config and fixtures.

#![allow(dead_code)]

use std::path::PathBuf;

use trade_adapters::{FixtureMarket, PaperBroker};
use trade_core::{TaskId, TaskTypeConfig};
use trade_server::{Harness, PAPER_BROKER_KEY, Step, Strategy, TaskRequest, load_task_types};
use trade_store::Store;

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub async fn harness() -> Harness {
    Harness::open(&root(), ":memory:").await.unwrap()
}

/// A harness on `db` with the given broker, whose task type is first changed by `tweak`
/// and registered as a new version, as a real change would be.
pub async fn harness_on(
    db: &str,
    broker: PaperBroker,
    api_key: Option<&str>,
    tweak: impl FnOnce(&mut TaskTypeConfig),
) -> Harness {
    let root = root();
    let strategy = Strategy::load(root.join("config/strategy.toml")).unwrap();
    let mut task_types = load_task_types(root.join("config/task_types")).unwrap();
    tweak(&mut task_types[0]);
    let market = FixtureMarket::load(root.join("fixtures/market.json")).unwrap();
    let store = Store::connect(db).await.unwrap();
    let key = api_key.map(str::to_string);
    Harness::new(store, market, broker, key, strategy, task_types)
        .await
        .unwrap()
}

pub async fn harness_with(tweak: impl FnOnce(&mut TaskTypeConfig)) -> Harness {
    let broker = PaperBroker::new(PAPER_BROKER_KEY);
    harness_on(":memory:", broker, Some(PAPER_BROKER_KEY), |t| {
        tweak(t);
        t.version.push_str("-test");
    })
    .await
}

pub fn request(as_of: &str, orders_optional: bool) -> TaskRequest {
    TaskRequest {
        goal: format!("rebalance for {as_of}"),
        as_of_date: Some(as_of.into()),
        markets: Some(vec!["US".into()]),
        portfolio_capital_usd: Some(1_000_000.0),
        max_positions: Some(3),
        orders_optional,
        trigger_key: None,
    }
}

/// ADMIT and FRAME a task for `as_of`.
pub async fn start_with(h: &Harness, as_of: &str, orders_optional: bool) -> TaskId {
    let mut request = h.rebalance_request(as_of);
    request.trigger_key = Some(format!("trigger-{as_of}"));
    request.orders_optional = orders_optional;
    let (task, _) = h.start("pm_alice", "test", &request).await.unwrap();
    assert_eq!(task.state().code(), "RUNNING");
    task.task_id
}

pub async fn start(h: &Harness, as_of: &str) -> TaskId {
    start_with(h, as_of, true).await
}

/// Decide the pending approval as `approver_bob`, then advance.
pub async fn decide(h: &Harness, step: Step, approve: bool) -> Step {
    let Step::NeedsApproval { approval_id, .. } = step else {
        panic!("expected an approval request, got {step:?}");
    };
    let task = h
        .decide_approval(&approval_id, "approver_bob", approve, None)
        .await
        .unwrap();
    h.advance(&task.task_id).await.unwrap()
}

pub async fn approve(h: &Harness, step: Step) -> Step {
    decide(h, step, true).await
}

/// Run a task for 2026-02-10 to delivery, approving both actions.
pub async fn delivered(h: &Harness) -> (TaskId, Step) {
    let task = start(h, "2026-02-10").await;
    let step = h.advance(&task).await.unwrap();
    let step = approve(h, step).await;
    let step = approve(h, step).await;
    assert!(matches!(step, Step::Delivered(_)), "{step:?}");
    (task, step)
}
