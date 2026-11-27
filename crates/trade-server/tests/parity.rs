//! The Rust port reproduces the Python MVP (`trade_ai@f47db05`) item by item: on every
//! rebalance date the universe, factor scores and vetoes, committee results and weights,
//! and the backtest metrics. The golden file was produced by the Python code.

mod common;

use serde_json::Value;
use trade_adapters::FixtureMarket;
use trade_core::{AsOfDate, PitMarketSource};
use trade_quant::{audit_survivorship_bias_exercise, run_pit_backtest, score_pit_universe};
use trade_research::Specialists;
use trade_server::{ORDER_TOOL, Step, Strategy};

fn golden() -> Value {
    serde_json::from_str(include_str!("golden/python-mvp.json")).unwrap()
}

fn near(rust: f64, python: &Value, what: &str) {
    let python = python
        .as_f64()
        .unwrap_or_else(|| panic!("{what}: not a number"));
    assert!(
        (rust - python).abs() < 1e-6,
        "{what}: rust {rust} vs python {python}"
    );
}

fn sorted(values: &Value) -> Vec<String> {
    let mut list: Vec<String> = values
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    list.sort();
    list
}

#[tokio::test]
async fn research_matches_the_python_mvp_on_every_rebalance_date() {
    let golden = golden();
    let h = common::harness().await;
    for (date, want) in golden["dates"].as_object().unwrap() {
        let as_of = AsOfDate::new(date.as_str()).unwrap();
        let snapshot = h
            .market
            .get_pit_snapshot(&as_of, &h.strategy.markets)
            .unwrap();
        let quant = score_pit_universe(&snapshot, &as_of, &h.strategy.factors);
        let mut universe: Vec<String> = quant.iter().map(|q| q.ticker.clone()).collect();
        universe.sort();
        assert_eq!(universe, sorted(&want["universe"]), "{date} universe");
        for q in &quant {
            let expected = &want["quant"][&q.ticker];
            near(
                q.quant_score,
                &expected["score"],
                &format!("{date} {} score", q.ticker),
            );
            assert_eq!(
                Value::from(q.hard_veto),
                expected["hard_veto"],
                "{date} {}",
                q.ticker
            );
            // Same reasons; the golden file lists them alphabetically.
            let mut reasons = q.veto_reasons.clone();
            reasons.sort();
            assert_eq!(
                reasons,
                sorted(&expected["veto_reasons"]),
                "{date} {}",
                q.ticker
            );
        }

        // The committee, through the governed workflow.
        let task = common::start(&h, date).await;
        let step = h.advance(&task).await.unwrap();
        assert!(
            matches!(step, Step::NeedsApproval { .. }),
            "{date}: {step:?}"
        );
        for d in h.store.evaluations(&task).await.unwrap() {
            let expected = &want["committee"][&d.ticker];
            let what = format!("{date} {}", d.ticker);
            near(d.consensus_score, &expected["consensus"], &what);
            near(d.dispersion, &expected["dispersion"], &what);
            assert_eq!(Value::from(d.hard_veto), expected["hard_veto"], "{what}");
            assert_eq!(
                Value::from(d.escalate_review),
                expected["escalate"],
                "{what}"
            );
        }
        let (_, portfolio) = h.proposal(&task).await.unwrap();
        let want_weights = want["weights"].as_object().unwrap();
        assert_eq!(
            portfolio.weights.keys().collect::<Vec<_>>(),
            want_weights.keys().collect::<Vec<_>>(),
            "{date} holdings"
        );
        for (ticker, weight) in &portfolio.weights {
            near(
                weight.as_ratio(),
                &want_weights[ticker],
                &format!("{date} {ticker} weight"),
            );
        }
        let vetoed: Vec<String> = portfolio.vetoed.keys().cloned().collect();
        assert_eq!(vetoed, sorted(&want["vetoed"]), "{date} vetoed");
        assert_eq!(
            serde_json::to_value(&portfolio.escalated).unwrap(),
            want["escalated"],
            "{date} escalated"
        );

        // Once approved, the draft is written with exactly those weights.
        assert_eq!(want["draft_status"], "EXECUTED");
        let step = common::approve(&h, step).await;
        assert!(matches!(step, Step::NeedsApproval { ref tool, .. } if tool == ORDER_TOOL));
        let draft = h.store.latest_draft(&task).await.unwrap().unwrap();
        assert_eq!(draft.weights, portfolio.weights, "{date} draft");
    }
}

#[test]
fn backtest_matches_the_python_mvp() {
    let golden = golden();
    let root = common::root();
    let strategy = Strategy::load(root.join("config/strategy.toml")).unwrap();
    let market = FixtureMarket::load(root.join("fixtures/market.json")).unwrap();
    let bt = run_pit_backtest(
        &market,
        &strategy.rebalance_dates,
        &strategy.backtest,
        &strategy.factors,
    )
    .unwrap();

    let want = &golden["backtest"]["overall"];
    let o = &bt.overall;
    assert_eq!(Value::from(o.periods), want["periods"]);
    for (rust, key) in [
        (o.cumulative_return, "cumulative_return"),
        (o.annualized_return, "annualized_return"),
        (o.annualized_volatility, "annualized_volatility"),
        (o.sharpe_ratio, "sharpe_ratio"),
        (o.max_drawdown, "max_drawdown"),
        (o.avg_turnover, "avg_turnover"),
        (o.avg_rank_ic.unwrap(), "avg_rank_ic"),
    ] {
        // The Python baseline is rounded to four decimals.
        let python = want[key].as_f64().unwrap();
        assert!(
            (rust - python).abs() <= 5e-5,
            "{key}: rust {rust} vs python {python}"
        );
    }
    let want_periods = golden["backtest"]["periods"].as_array().unwrap();
    assert_eq!(bt.periods.len(), want_periods.len());
    for (p, w) in bt.periods.iter().zip(want_periods) {
        let date = p.rebalance_date.as_str();
        assert_eq!(Value::from(date), w["date"]);
        let mut selected = p.selected_tickers.clone();
        selected.sort();
        assert_eq!(selected, sorted(&w["selected"]), "{date} selected");
        let mut vetoed = p.vetoed_tickers.clone();
        vetoed.sort();
        assert_eq!(vetoed, sorted(&w["vetoed"]), "{date} vetoed");
        for (rust, key) in [(p.net_return, "net_return"), (p.cost_drag, "cost_drag")] {
            let python = w[key].as_f64().unwrap();
            assert!(
                (rust - python).abs() <= 5e-5,
                "{date} {key}: {rust} vs {python}"
            );
        }
    }

    let exercise = audit_survivorship_bias_exercise();
    let want = &golden["survivorship_exercise"];
    near(
        exercise.survivors_only_return,
        &want["survivors_only_return"],
        "survivors only",
    );
    near(
        exercise.survivor_bias_free_return,
        &want["survivor_bias_free_return"],
        "bias free",
    );
    near(
        exercise.artificial_gain_pct_points,
        &want["artificial_gain_pct_points"],
        "gain",
    );
}

/// The specialists' values in `config/strategy.toml` are the Python MVP's, which are also
/// the defaults, so the comparisons above cover the file and the defaults alike.
#[test]
fn the_strategy_file_holds_the_specialists_defaults() {
    let strategy = Strategy::load(common::root().join("config/strategy.toml")).unwrap();
    assert_eq!(strategy.specialists, Specialists::default());
}
