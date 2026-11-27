#![forbid(unsafe_code)]
//! Command line: `trade <serve|agent|demo|backtest|research|bench> [--db URL] [--root DIR]
//! [--as-of DATE] [--addr HOST:PORT] [--dev] [--model-url URL --model NAME]
//! [--approve-as NAME]`. The numbers match the Python MVP (`v0.1-python-mvp`) item by item.
//!
//! With `--model-url` and `--model` (or `AITRADER_MODEL_URL` and `AITRADER_MODEL`), a
//! model behind an OpenAI-compatible API proposes every action; `AITRADER_MODEL_API_KEY`
//! is sent if set. Without them, ACT follows fixed rules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;

use trade_core::{ActionIntentStatus, AsOfDate, PitMarketSource, TaskId};
use trade_quant::{audit_survivorship_bias_exercise, run_pit_backtest, score_pit_universe};
use trade_server::agent::{OBSERVATION, REPLY};
use trade_server::{AgentModel, App, FrameRequest, Framing, Harness, Intake, ORDER_TOOL, Step};

struct Args {
    command: String,
    db: String,
    as_of: String,
    root: PathBuf,
    addr: String,
    dev: bool,
    model_url: Option<String>,
    model: Option<String>,
    approve_as: Option<String>,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args {
        command: "demo".into(),
        db: ":memory:".into(),
        as_of: "2026-02-10".into(),
        root: PathBuf::new(),
        addr: "127.0.0.1:8000".into(),
        dev: std::env::var("AITRADER_ENABLE_DEV_ENDPOINTS").is_ok_and(|v| v == "true"),
        model_url: std::env::var("AITRADER_MODEL_URL").ok(),
        model: std::env::var("AITRADER_MODEL").ok(),
        approve_as: None,
    };
    let mut root = None;
    let mut first = true;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => parsed.db = args.next().unwrap_or_default(),
            "--as-of" => parsed.as_of = args.next().unwrap_or_default(),
            "--root" => root = args.next().map(PathBuf::from),
            "--addr" => parsed.addr = args.next().unwrap_or_default(),
            "--dev" => parsed.dev = true,
            "--model-url" => parsed.model_url = args.next(),
            "--model" => parsed.model = args.next(),
            "--approve-as" => parsed.approve_as = args.next(),
            command if first => parsed.command = command.into(),
            _ => {}
        }
        first = false;
    }
    // The repository root: where config/ and fixtures/ are.
    parsed.root = root.unwrap_or_else(|| {
        if Path::new("config/strategy.toml").exists() {
            PathBuf::from(".")
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
        }
    });
    parsed
}

fn section(title: &str) {
    println!("\n{}\n{title}\n{}", "=".repeat(76), "=".repeat(76));
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn ratios(weights: &BTreeMap<String, trade_core::WeightBps>) -> BTreeMap<&str, f64> {
    weights
        .iter()
        .map(|(t, w)| (t.as_str(), w.as_ratio()))
        .collect()
}

fn intake(trigger_key: &str, h: &Harness) -> Intake {
    Intake {
        trigger_key: trigger_key.into(),
        requester_id: "pm_alice".into(),
        acting_principal: "quant_research_svc".into(),
        trigger_source: "monthly_cron".into(),
        markets: h.strategy.markets.clone(),
    }
}

fn frame_request(as_of: &str, h: &Harness) -> FrameRequest {
    FrameRequest {
        goal: format!(
            "Point-in-time value and quality screen and a governed rebalance for {as_of}"
        ),
        as_of_date: Some(as_of.into()),
        markets: h.strategy.markets.clone(),
        portfolio_capital: Some(h.strategy.research.portfolio_capital),
        max_positions: h.strategy.research.max_positions,
        orders_optional: true,
    }
}

/// ADMIT and FRAME a rebalance task for `as_of`.
async fn start(h: &Harness, trigger_key: &str, as_of: &str) -> Result<TaskId, String> {
    let (task, _) = h.admit(&intake(trigger_key, h)).await.map_err(err)?;
    match h
        .frame(&task.task_id, &frame_request(as_of, h))
        .await
        .map_err(err)?
    {
        Framing::Ready(_) => Ok(task.task_id),
        Framing::NeedsInput(questions) => Err(format!("request is ambiguous: {questions:?}")),
    }
}

fn backtest_section(h: &Harness) -> Result<(), String> {
    let s = &h.strategy;
    section("1. Backtesting pitfalls, factor scores and vetoes per date, backtest metrics");
    let ex = audit_survivorship_bias_exercise();
    println!(
        "Textbook exercise -> survivors only: {:.1}%, survivor-bias-free: {:.1}%, artificial gain: +{:.1} pct points",
        ex.survivors_only_return * 100.0,
        ex.survivor_bias_free_return * 100.0,
        ex.artificial_gain_pct_points
    );
    for date in &s.rebalance_dates {
        let snapshot = h.market.get_pit_snapshot(date, &s.markets)?;
        println!("\n  --- Factor scores and vetoes @ {} ---", date.as_str());
        for r in score_pit_universe(&snapshot, date, &s.factors) {
            println!(
                "    {:<10} | Quant={:+.4} | Alpha={:+.4} | Pen={:.4} | Veto={:<5} | Reasons={:?}",
                r.ticker, r.quant_score, r.alpha_score, r.risk_penalty, r.hard_veto, r.veto_reasons
            );
        }
    }
    let bt = run_pit_backtest(&h.market, &s.rebalance_dates, &s.backtest, &s.factors)?;
    let o = &bt.overall;
    println!(
        "\nPIT backtest overall -> periods={}, cum={:.2}%, ann={:.2}%, vol={:.2}%, sharpe={:.2}, maxDD={:.2}%, turnover={:.2}%, rankIC={:.4}",
        o.periods,
        o.cumulative_return * 100.0,
        o.annualized_return * 100.0,
        o.annualized_volatility * 100.0,
        o.sharpe_ratio,
        o.max_drawdown * 100.0,
        o.avg_turnover * 100.0,
        o.avg_rank_ic.unwrap_or(0.0)
    );
    for (name, m) in [
        ("In-sample", &bt.in_sample),
        ("Out-of-sample", &bt.out_of_sample),
    ] {
        if let Some(m) = m {
            println!(
                "  {name:<13} -> cum={:.2}%, sharpe={:.2}, turnover={:.2}%, rankIC={:.4}",
                m.cumulative_return * 100.0,
                m.sharpe_ratio,
                m.avg_turnover * 100.0,
                m.avg_rank_ic.unwrap_or(0.0)
            );
        }
    }
    for p in &bt.periods {
        println!(
            "  [{}] selected={:?} vetoed={:?} weights={} turnover={:.2}% gross={:.4}% cost={:.4}% net={:.2}% rankIC={:?}",
            p.rebalance_date.as_str(),
            p.selected_tickers,
            p.vetoed_tickers,
            serde_json::to_string(&p.weights).map_err(err)?,
            p.turnover * 100.0,
            p.gross_return * 100.0,
            p.cost_drag * 100.0,
            p.net_return * 100.0,
            p.rank_ic
        );
    }
    Ok(())
}

/// Approve the pending action as a second person and run on.
async fn approve(h: &Harness, step: Step, tier: &str, note: &str) -> Result<Step, String> {
    let Step::NeedsApproval { approval_id, tool } = step else {
        return Err(format!("expected an approval request, got {step:?}"));
    };
    let approval = h
        .store
        .approval(&approval_id)
        .await
        .map_err(err)?
        .ok_or("approval missing")?;
    println!(
        "ACT ({tier} {tool}) -> needs approval, approval_id={approval_id}, payload_hash={}...",
        &approval.payload_hash[..16]
    );
    let task = h
        .decide_approval(&approval_id, "approver_bob", true, Some(note))
        .await
        .map_err(err)?;
    let step = h.advance(&task.task_id).await.map_err(err)?;
    println!("ACT ({tier} {tool}) -> approved by approver_bob, used once, executed");
    Ok(step)
}

async fn demo(h: &Harness) -> Result<(), String> {
    backtest_section(h)?;

    section("2. Governed workflow (Diagrams 0, 3a, 3b and 4a)");
    let (task, created) = h
        .admit(&intake("sched-rebalance-2026-02-10", h))
        .await
        .map_err(err)?;
    println!(
        "ADMIT -> task_id={}, state={}, version={}, deduplicated={}",
        task.task_id,
        task.state().code(),
        task.version(),
        !created
    );
    let Framing::Ready(contract) = h
        .frame(&task.task_id, &frame_request("2026-02-10", h))
        .await
        .map_err(err)?
    else {
        return Err("unexpected request for input".into());
    };
    let framed = h.task(&task.task_id).await.map_err(err)?;
    println!(
        "FRAME -> state={}, version={}, as_of={}, capital=${:.0}",
        framed.state().code(),
        framed.version(),
        contract.as_of_date.as_str(),
        contract.portfolio_capital.to_dollars()
    );
    let step = h.advance(&task.task_id).await.map_err(err)?;
    let step = approve(
        h,
        step,
        "R2",
        "Approved R2 draft after reviewing committee dispersion",
    )
    .await?;

    section("3. Investment committee decisions (2026-02-10)");
    for d in h.store.evaluations(&task.task_id).await.map_err(err)? {
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
    let (_, portfolio) = h.proposal(&task.task_id).await.map_err(err)?;
    println!(
        "Target weights: {}",
        serde_json::to_string(&ratios(&portfolio.weights)).map_err(err)?
    );
    println!(
        "Escalated in the selected portfolio: {:?}",
        portfolio.escalated
    );
    println!(
        "Hard vetoed (stored in the database): {:?}",
        portfolio.vetoed.keys().collect::<Vec<_>>()
    );

    section("4. Consequential action: R3 paper orders through the paper broker");
    let step = approve(h, step, "R3", "Approved R3 paper order submission").await?;
    for a in h.store.actions(&task.task_id).await.map_err(err)? {
        if a.tool_name == ORDER_TOOL && a.status == ActionIntentStatus::Verified {
            println!(
                "Paper execution -> receipt_id={}, read back and verified",
                a.backend_receipt_id.unwrap_or_default()
            );
        }
    }

    section("5. CLOSE: independent verification, settlement and unit economics");
    let Step::Delivered(manifest) = step else {
        return Err(format!("expected a delivery, got {step:?}"));
    };
    println!(
        "Verify -> state={}, completeness={}, criteria={}",
        manifest.final_state.code(),
        manifest.completeness.as_str(),
        serde_json::to_string(&manifest.criteria_results).map_err(err)?
    );
    let done = h
        .confirm_settlement(&task.task_id, "pm_alice", 3.0)
        .await
        .map_err(err)?;
    println!("Settled -> state={}", done.state().code());
    let economics = h.store.unit_economics(12_000).await.map_err(err)?;
    println!(
        "Unit economics -> {}",
        serde_json::to_string(&economics).map_err(err)?
    );
    Ok(())
}

/// The HTTP API, with the deadline sweeper running every 30 seconds.
async fn serve(h: Harness, args: &Args) -> Result<(), String> {
    let origins: Vec<String> = std::env::var("AITRADER_CORS_ORIGINS")
        .unwrap_or_else(|_| "http://localhost:3000".into())
        .split(',')
        .map(|o| o.trim().to_string())
        .collect();
    let app = App::new(h, args.dev);
    let sweeper = app.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            every.tick().await;
            if let Err(e) = sweeper.sweep().await {
                eprintln!("sweep failed: {e}");
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .map_err(err)?;
    println!(
        "listening on http://{} (database: {}, dev endpoints: {})",
        args.addr,
        args.db,
        if args.dev { "on" } else { "off" }
    );
    axum::serve(listener, app.router(&origins))
        .await
        .map_err(err)
}

/// One task driven by the model, from framing to delivery. With `--approve-as`, that
/// person approves each action as it comes; without it, the run stops at the first one.
async fn agent(h: &Harness, args: &Args) -> Result<(), String> {
    let key = trade_store::new_id(&format!("agent-{}", args.as_of));
    let task = start(h, &key, &args.as_of).await?;
    println!("task {task}");
    let mut step = h.advance(&task).await.map_err(err)?;
    while let Step::NeedsApproval { approval_id, tool } = &step {
        let Some(person) = &args.approve_as else {
            println!("{tool} waits for approval {approval_id}; decide it through the API.");
            break;
        };
        println!("{person} approves {tool} ({approval_id})");
        let note = Some("approved from the command line");
        h.decide_approval(approval_id, person, true, note)
            .await
            .map_err(err)?;
        step = h.advance(&task).await.map_err(err)?;
    }
    section("Transcript");
    for e in h.store.events(&task).await.map_err(err)? {
        match e.kind.as_str() {
            REPLY => println!("model> {}", e.detail),
            OBSERVATION => println!("harness> {}\n", e.detail),
            _ => {}
        }
    }
    section("Outcome");
    let done = h.task(&task).await.map_err(err)?;
    println!(
        "state={} turns={} step={step:?}",
        done.state().code(),
        done.turns_used
    );
    Ok(())
}

async fn run() -> Result<(), String> {
    let args = parse_args();
    let mut h = Harness::open(&args.root, &args.db).await?;
    if let (Some(url), Some(model)) = (&args.model_url, &args.model) {
        let key = std::env::var("AITRADER_MODEL_API_KEY").ok();
        h = h.with_agent(AgentModel::load(&args.root, url, model, key)?);
    }
    if args.command == "serve" {
        return serve(h, &args).await;
    }
    let s = &h.strategy;
    match args.command.as_str() {
        "agent" if h.agent.is_none() => Err(
            "agent needs a model: --model-url URL --model NAME, or AITRADER_MODEL_URL and AITRADER_MODEL"
                .into(),
        ),
        "agent" => agent(&h, &args).await,
        "demo" => demo(&h).await,
        "backtest" => {
            let bt = run_pit_backtest(&h.market, &s.rebalance_dates, &s.backtest, &s.factors)?;
            let out = serde_json::json!({
                "overall": bt.overall,
                "in_sample": bt.in_sample,
                "out_of_sample": bt.out_of_sample,
                "periods": bt.periods,
            });
            println!("{}", serde_json::to_string_pretty(&out).map_err(err)?);
            Ok(())
        }
        "research" => {
            let task_id = start(&h, &format!("cli-{}", args.as_of), &args.as_of).await?;
            let step = h.advance(&task_id).await.map_err(err)?;
            let (_, portfolio) = h.proposal(&task_id).await.map_err(err)?;
            let out = serde_json::json!({
                "as_of_date": args.as_of,
                "weights": ratios(&portfolio.weights),
                "vetoed": portfolio.vetoed,
                "escalated": portfolio.escalated,
                "next_step": format!("{step:?}"),
            });
            println!("{}", serde_json::to_string_pretty(&out).map_err(err)?);
            Ok(())
        }
        "bench" => {
            let date = AsOfDate::new(args.as_of.as_str()).map_err(|e| e.0)?;
            let snapshot = h.market.get_pit_snapshot(&date, &s.markets)?;
            let iterations = 2_000;
            let started = Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(score_pit_universe(&snapshot, &date, &s.factors));
            }
            let elapsed = started.elapsed().as_secs_f64();
            println!(
                "15-factor cross-sectional engine -> {iterations} iterations in {:.2} ms ({:.1} us each)",
                elapsed * 1_000.0,
                elapsed * 1_000_000.0 / f64::from(iterations)
            );
            Ok(())
        }
        other => Err(format!(
            "unknown command {other:?}; usage: trade <serve|agent|demo|backtest|research|bench> [--db URL] [--root DIR] [--as-of DATE] [--addr HOST:PORT] [--dev] [--model-url URL --model NAME] [--approve-as NAME]"
        )),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
