#![forbid(unsafe_code)]
//! CLI entrypoint (`demo`, `backtest`, `research`, `bench`) with item-by-item parity
//! against `v0.1-python-mvp` across factor scores, vetoes, weights, and backtest metrics.

use std::collections::BTreeMap;
use std::env;
use std::process;
use std::time::Instant;

use trade_cli::{
    build_default_config, rebalance_dates, seed_pit_universe, TradeAiHarness,
};
use trade_core::{AsOfDate, AutonomyLevel, UsdCents};
use trade_gate::GateOutcome;
use trade_quant::{
    audit_survivorship_bias_exercise, run_pit_backtest, score_pit_universe, BacktestConfig,
};
use trade_store::DbStore;

fn print_header(title: &str) {
    println!("\n{}", "=".repeat(76));
    println!("{title}");
    println!("{}", "=".repeat(76));
}

async fn run_demo(db_url: &str) -> Result<(), String> {
    let store = DbStore::connect(db_url).await.map_err(|e| e.to_string())?;
    seed_pit_universe(&store).await.map_err(|e| e.to_string())?;

    print_header("1. Backtesting Pitfalls, Per-Date Factor Scores, Vetoes & Metrics");
    let ex = audit_survivorship_bias_exercise();
    println!(
        "Textbook Exercise -> Survivors Only: {:.1}%, Survivor-Bias-Free: {:.1}%, Artificial Gain: +{:.1} pct points",
        ex.survivors_only_return * 100.0,
        ex.survivor_bias_free_return * 100.0,
        ex.artificial_gain_pct_points
    );

    let dates = rebalance_dates();
    let markets = vec!["US".to_string()];
    for dt in &dates {
        let snap = store
            .get_pit_snapshot(dt, &markets)
            .await
            .map_err(|e| e.to_string())?;
        let scored = score_pit_universe(&snap, dt, 1_000_000.0);
        println!("\n  --- Factor Scores & Vetoes @ {} ---", dt.as_str());
        for r in &scored {
            println!(
                "    {:<10} | Quant={:+.4} | Alpha={:+.4} | Pen={:.4} | Veto={:<5} | Reasons={:?}",
                r.ticker, r.quant_score, r.alpha_score, r.risk_penalty, r.hard_veto, r.veto_reasons
            );
        }
    }

    let pit_source = store
        .preload_backtest_source(&dates, &markets)
        .await
        .map_err(|e| e.to_string())?;
    let bt = run_pit_backtest(
        &pit_source,
        &dates,
        BacktestConfig {
            markets: markets.clone(),
            oos_start_date: Some(AsOfDate::new("2026-02-10").unwrap()),
            ..Default::default()
        },
    )?;

    println!(
        "\nPIT Backtest Overall -> Periods={}, CumRet={:.2}%, AnnRet={:.2}%, Vol={:.2}%, Sharpe={:.2}, MaxDD={:.2}%, AvgTurnover={:.2}%, AvgRankIC={:.4}",
        bt.overall.periods,
        bt.overall.cumulative_return * 100.0,
        bt.overall.annualized_return * 100.0,
        bt.overall.annualized_volatility * 100.0,
        bt.overall.sharpe_ratio,
        bt.overall.max_drawdown * 100.0,
        bt.overall.avg_turnover * 100.0,
        bt.overall.avg_rank_ic.unwrap_or(0.0)
    );
    if let (Some(is_m), Some(oos_m)) = (&bt.in_sample, &bt.out_of_sample) {
        println!(
            "  In-Sample     -> CumRet={:.2}%, Sharpe={:.2}, AvgTurnover={:.2}%, AvgRankIC={:.4}",
            is_m.cumulative_return * 100.0,
            is_m.sharpe_ratio,
            is_m.avg_turnover * 100.0,
            is_m.avg_rank_ic.unwrap_or(0.0)
        );
        println!(
            "  Out-of-Sample -> CumRet={:.2}%, Sharpe={:.2}, AvgTurnover={:.2}%, AvgRankIC={:.4}",
            oos_m.cumulative_return * 100.0,
            oos_m.sharpe_ratio,
            oos_m.avg_turnover * 100.0,
            oos_m.avg_rank_ic.unwrap_or(0.0)
        );
    }
    for p in &bt.periods {
        let w_json = serde_json::to_string(&p.weights).unwrap();
        println!(
            "  [{}] Selected={:?} Vetoed={:?} Weights={} Turnover={:.2}% GrossRet={:.4}% Cost={:.4}% NetRet={:.2}% RankIC={:?}",
            p.rebalance_date.as_str(),
            p.selected_tickers,
            p.vetoed_tickers,
            w_json,
            p.turnover * 100.0,
            p.gross_return * 100.0,
            p.cost_drag * 100.0,
            p.net_return * 100.0,
            p.rank_ic
        );
    }

    print_header("2. Agent 7 Governed Workflow (DB-Backed Approvals & Diagram 4a States)");
    let config = build_default_config(AutonomyLevel::PerActionApproval, "platform_admin", None);
    let harness = TradeAiHarness::new(config);

    let (task, dedup) = harness
        .admit_task(
            &store,
            "sched-rebalance-2026-02-10",
            "pm_alice",
            "quant_research_svc",
            "monthly_cron",
            &markets,
        )
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "ADMIT -> task_id={}, state={}, version={}, deduplicated={dedup}",
        task.task_id,
        task.state().code(),
        task.version()
    );

    let framed_res = harness
        .frame_task(
            &store,
            &task.task_id,
            "Run PIT value+quality screen and build governed rebalance draft for 2026-02-10",
            Some("2026-02-10"),
            &markets,
            Some(UsdCents::from_dollars(1_000_000.0)),
            3,
        )
        .await
        .map_err(|e| e.to_string())?;
    let (_framed, contract) = framed_res.map_err(|t| format!("unexpected pause: {:?}", t.state()))?;
    let loaded_running = store.get_task(&task.task_id).await.unwrap().unwrap();
    println!(
        "FRAME -> state={}, version={}, as_of={}, capital=${:.0}",
        loaded_running.state().code(),
        loaded_running.version(),
        contract.as_of_date.as_str(),
        contract.portfolio_capital.to_dollars()
    );

    let audit = vec!["TRAP_CO".to_string()];
    let res = harness
        .run_research_and_draft(&store, &task.task_id, None, &audit, 8, 1_000_000.0, None)
        .await
        .map_err(|e| e.to_string())?;

    let draft_approval_id = match res.gate_outcome.as_ref().unwrap() {
        GateOutcome::NeedsApproval {
            approval_id,
            payload_hash,
            risk_tier,
        } => {
            println!(
                "ACT (R2 Draft Gate) -> status=NEEDS_APPROVAL, tier={}, approval_id={approval_id}, payload_hash={}...",
                risk_tier.as_str(),
                &payload_hash[..16]
            );
            approval_id.clone()
        }
        other => return Err(format!("expected NeedsApproval, got {other:?}")),
    };

    // Approver separately approves in DB (`PENDING -> APPROVED`); Gate consumes by `approval_id`!
    store
        .decide_approval(
            &draft_approval_id,
            "approver_bob",
            true,
            Some("Approved R2 draft after reviewing committee dispersion"),
            &harness.config.config_admin_id,
        )
        .await
        .map_err(|e| e.to_string())?;

    let draft_exec = harness
        .submit_rebalance_draft(&store, &task.task_id, &res, Some(&draft_approval_id))
        .await
        .map_err(|e| e.to_string())?;
    let draft_id = match draft_exec {
        GateOutcome::Executed { receipt, .. } => {
            println!("ACT (R2 Draft Approved & Consumed) -> status=EXECUTED, draft_id={receipt}");
            receipt
        }
        other => return Err(format!("expected Executed, got {other:?}")),
    };

    print_header("3. Investment Committee Decision Table (2026-02-10)");
    for d in &res.decisions {
        println!(
            "  {:<10} | Quant={:+.3} | Consensus={:+.3} | Disp={:.3} | Veto={:<5} | Escalate={:<5}",
            d.ticker,
            d.quant_base_score,
            d.consensus_score,
            d.dispersion,
            d.hard_veto,
            d.escalate_review
        );
    }
    let display_weights: BTreeMap<&str, f64> = res
        .payload
        .weights_bps
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_ratio()))
        .collect();
    println!(
        "Target Weights: {}",
        serde_json::to_string(&display_weights).unwrap()
    );
    println!("Escalated in Selected Portfolio: {:?}", res.escalated);
    println!(
        "Hard Vetoed (DB Source of Truth): {:?}",
        res.vetoed.keys().collect::<Vec<_>>()
    );

    print_header("4. Consequential Action (R3 Paper Orders via Standalone Broker Adapter)");
    let order_idem = format!("order:{}:2026-02-10", task.task_id);
    let order_gate_1 = harness
        .execute_paper_orders(&store, &task.task_id, &draft_id, &order_idem, None)
        .await
        .map_err(|e| e.to_string())?;
    let order_approval_id = match order_gate_1 {
        GateOutcome::NeedsApproval {
            approval_id,
            risk_tier,
            ..
        } => {
            println!(
                "ACT (R3 Order Gate) -> status=NEEDS_APPROVAL, tier={}, approval_id={approval_id}",
                risk_tier.as_str()
            );
            approval_id
        }
        other => return Err(format!("expected R3 NeedsApproval, got {other:?}")),
    };

    store
        .decide_approval(
            &order_approval_id,
            "approver_bob",
            true,
            Some("Approved R3 paper order submission"),
            &harness.config.config_admin_id,
        )
        .await
        .map_err(|e| e.to_string())?;

    let order_exec = harness
        .execute_paper_orders(
            &store,
            &task.task_id,
            &draft_id,
            &order_idem,
            Some(&order_approval_id),
        )
        .await
        .map_err(|e| e.to_string())?;
    if let GateOutcome::Executed { receipt, .. } = order_exec {
        println!(
            "Paper Execution -> status=EXECUTED, receipt_id={receipt}, readback_verified=true"
        );
    }

    print_header("5. CLOSE -> DB-Only Independent Verification, SETTLING -> COMPLETED & Economics");
    let verified = harness
        .verify_and_close(&store, &task.task_id)
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "Verify -> final_state={}, completeness={}, criteria={}",
        verified.manifest.final_state.code(),
        verified.manifest.completeness.as_str(),
        serde_json::to_string(&verified.manifest.criteria_results).unwrap()
    );

    let settling_handle = verified
        .settling_task
        .ok_or_else(|| "verification failed unexpectedly".to_string())?;
    let completed = harness
        .confirm_settlement(&store, settling_handle, "pm_alice", 3.0)
        .await
        .map_err(|e| e.to_string())?;
    let econ = store
        .calculate_unit_economics(12_000)
        .await
        .map_err(|e| e.to_string())?;
    println!("Settled -> state={}", completed.record().state().code());
    println!("Unit Economics -> {}", serde_json::to_string(&econ).unwrap());
    Ok(())
}

async fn run_cli() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("demo");

    let mut db_url = ":memory:".to_string();
    let mut as_of_str = "2026-02-10".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--db" if i + 1 < args.len() => {
                db_url = args[i + 1].clone();
                i += 2;
            }
            "--as-of" if i + 1 < args.len() => {
                as_of_str = args[i + 1].clone();
                i += 2;
            }
            _ => i += 1,
        }
    }

    match cmd {
        "demo" => run_demo(&db_url).await,
        "backtest" => {
            let store = DbStore::connect(&db_url).await.map_err(|e| e.to_string())?;
            seed_pit_universe(&store).await.map_err(|e| e.to_string())?;
            let dates = rebalance_dates();
            let markets = vec!["US".to_string()];
            let src = store
                .preload_backtest_source(&dates, &markets)
                .await
                .map_err(|e| e.to_string())?;
            let bt = run_pit_backtest(
                &src,
                &dates,
                BacktestConfig {
                    markets,
                    oos_start_date: Some(AsOfDate::new("2026-02-10").unwrap()),
                    ..Default::default()
                },
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "overall": bt.overall,
                    "in_sample": bt.in_sample,
                    "out_of_sample": bt.out_of_sample,
                    "periods": bt.periods,
                }))
                .unwrap()
            );
            Ok(())
        }
        "research" => {
            let store = DbStore::connect(&db_url).await.map_err(|e| e.to_string())?;
            seed_pit_universe(&store).await.map_err(|e| e.to_string())?;
            let harness = TradeAiHarness::new(build_default_config(
                AutonomyLevel::DraftOnly,
                "platform_admin",
                None,
            ));
            let markets = vec!["US".to_string()];
            let (task, _) = harness
                .admit_task(&store, &format!("cli-{as_of_str}"), "cli_user", "cli_svc", "cli", &markets)
                .await
                .map_err(|e| e.to_string())?;
            harness
                .frame_task(
                    &store,
                    &task.task_id,
                    "CLI PIT research",
                    Some(&as_of_str),
                    &markets,
                    Some(UsdCents::from_dollars(1_000_000.0)),
                    3,
                )
                .await
                .map_err(|e| e.to_string())?
                .map_err(|_| "ambiguous cli parameters".to_string())?;
            let res = harness
                .run_research_and_draft(
                    &store,
                    &task.task_id,
                    None,
                    &["TRAP_CO".to_string()],
                    8,
                    1_000_000.0,
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
            let weights: BTreeMap<&str, f64> = res
                .payload
                .weights_bps
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_ratio()))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "as_of_date": as_of_str,
                    "weights": weights,
                    "vetoed": res.vetoed,
                    "escalated": res.escalated,
                }))
                .unwrap()
            );
            Ok(())
        }
        "bench" => {
            let store = DbStore::connect(":memory:").await.map_err(|e| e.to_string())?;
            seed_pit_universe(&store).await.map_err(|e| e.to_string())?;
            let dt = AsOfDate::new("2026-02-10").unwrap();
            let snap = store
                .get_pit_snapshot(&dt, &["US".to_string()])
                .await
                .map_err(|e| e.to_string())?;
            let iters = 2_000;
            let t0 = Instant::now();
            for _ in 0..iters {
                let _ = score_pit_universe(&snap, &dt, 1_000_000.0);
            }
            let elapsed = t0.elapsed();
            println!(
                "Rust 15-Factor Cross-Sectional Engine -> {iters} iterations in {:.2} ms ({:.1} us/iter)",
                elapsed.as_secs_f64() * 1000.0,
                (elapsed.as_secs_f64() * 1_000_000.0) / (iters as f64)
            );
            Ok(())
        }
        other => Err(format!(
            "Unknown command {other:?}. Usage: trade-cli <demo|backtest|research|bench>"
        )),
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run_cli().await {
        eprintln!("error: {err}");
        process::exit(1);
    }
}
