#![forbid(unsafe_code)]
//! Command line: `trade <serve|agent|backtest|research> [--db URL] [--root DIR]
//! [--as-of DATE] [--addr HOST:PORT] [--dev] [--model-url URL --model NAME]
//! [--approve-as NAME]`.
//!
//! `serve` and `agent` use a model behind an OpenAI-compatible API when `--model-url` and
//! `--model` are given (or `AITRADER_MODEL_URL` and `AITRADER_MODEL`; `AITRADER_MODEL_API_KEY`
//! is sent if set). Without one, ACT follows fixed rules. `backtest` and `research` never
//! use a model.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process;

use serde_json::json;
use trade_core::TaskState;
use trade_quant::run_pit_backtest;
use trade_server::agent::{OBSERVATION, REPLY};
use trade_server::api::step_view;
use trade_server::{AgentModel, App, Harness, Step};

const USAGE: &str = "usage: trade <serve|agent|backtest|research> [--db URL] [--root DIR] \
    [--as-of DATE] [--addr HOST:PORT] [--dev] [--model-url URL --model NAME] [--approve-as NAME]";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

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

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let env = |name: &str| std::env::var(name).ok();
    let mut parsed = Args {
        command: args.next().ok_or(USAGE)?,
        db: ":memory:".into(),
        as_of: "2026-02-10".into(),
        root: PathBuf::new(),
        addr: "127.0.0.1:8000".into(),
        dev: env("AITRADER_ENABLE_DEV_ENDPOINTS").is_some_and(|v| v == "true"),
        model_url: env("AITRADER_MODEL_URL"),
        model: env("AITRADER_MODEL"),
        approve_as: None,
    };
    let mut root = None;
    while let Some(flag) = args.next() {
        if flag == "--dev" {
            parsed.dev = true;
            continue;
        }
        const FLAGS: [&str; 7] = [
            "--db",
            "--as-of",
            "--root",
            "--addr",
            "--model-url",
            "--model",
            "--approve-as",
        ];
        if !FLAGS.contains(&flag.as_str()) {
            return Err(format!("unknown flag {flag:?}; {USAGE}").into());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} needs a value; {USAGE}"))?;
        match flag.as_str() {
            "--db" => parsed.db = value,
            "--as-of" => parsed.as_of = value,
            "--root" => root = Some(PathBuf::from(value)),
            "--addr" => parsed.addr = value,
            "--model-url" => parsed.model_url = Some(value),
            "--model" => parsed.model = Some(value),
            _ => parsed.approve_as = Some(value),
        }
    }
    // The repository root: where config/ and fixtures/ are.
    parsed.root = root.unwrap_or_else(|| {
        if Path::new("config/strategy.toml").exists() {
            PathBuf::from(".")
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
        }
    });
    Ok(parsed)
}

/// The database address without its password, for the log.
fn redacted(url: &str) -> String {
    let (Some(scheme), Some(at)) = (url.find("://"), url.rfind('@')) else {
        return url.into();
    };
    match url[scheme + 3..at].find(':') {
        Some(colon) => format!("{}:***{}", &url[..scheme + 3 + colon], &url[at..]),
        None => url.into(),
    }
}

/// The HTTP API, with the deadline sweeper running every 30 seconds.
async fn serve(h: Harness, args: &Args) -> Result<()> {
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
    let listener = tokio::net::TcpListener::bind(&args.addr).await?;
    println!(
        "listening on http://{} (database: {}, dev endpoints: {})",
        args.addr,
        redacted(&args.db),
        if args.dev { "on" } else { "off" }
    );
    axum::serve(listener, app.router(&origins)).await?;
    Ok(())
}

/// One task from framing to delivery, run by the model if there is one and by the fixed
/// rules otherwise. With `--approve-as`, that person approves each action as it comes;
/// without it, the run stops at the first approval.
async fn agent(h: &Harness, args: &Args) -> Result<()> {
    let mut request = h.rebalance_request(&args.as_of);
    request.trigger_key = Some(trade_store::new_id(&format!("agent-{}", args.as_of)));
    let (task, _) = h.start("pm_alice", "cli", &request).await?;
    let id = task.task_id.clone();
    if !matches!(task.state(), TaskState::Running) {
        return Err(format!("task {id} is {}, not ready to run", task.state().code()).into());
    }
    println!("task {id}");
    let mut step = h.advance(&id).await?;
    while let Step::NeedsApproval { approval_id, tool } = &step {
        let Some(person) = &args.approve_as else {
            println!("{tool} waits for approval {approval_id}; decide it through the API.");
            break;
        };
        println!("{person} approves {tool} ({approval_id})");
        let note = Some("approved from the command line");
        h.decide_approval(approval_id, person, true, note).await?;
        step = h.advance(&id).await?;
    }
    for e in h.store.events(&id).await? {
        match e.kind.as_str() {
            REPLY => println!("model> {}", e.detail),
            OBSERVATION => println!("harness> {}\n", e.detail),
            _ => {}
        }
    }
    let done = h.task(&id).await?;
    let outcome = json!({
        "task": id,
        "state": done.state().code(),
        "turns": done.turns_used,
        "step": step_view(&step),
    });
    println!("outcome {outcome}");
    Ok(())
}

async fn run() -> Result<()> {
    let args = parse_args()?;
    let mut h = Harness::open(&args.root, &args.db).await?;
    let uses_model = matches!(args.command.as_str(), "serve" | "agent");
    if let (true, Some(url), Some(model)) = (uses_model, &args.model_url, &args.model) {
        let key = std::env::var("AITRADER_MODEL_API_KEY").ok();
        h = h.with_agent(AgentModel::load(&args.root, url, model, key)?);
    }
    let s = &h.strategy;
    let out = match args.command.as_str() {
        "serve" => return serve(h, &args).await,
        "agent" => return agent(&h, &args).await,
        "backtest" => {
            let bt = run_pit_backtest(&h.market, &s.rebalance_dates, &s.backtest, &s.factors)?;
            json!({
                "overall": bt.overall,
                "in_sample": bt.in_sample,
                "out_of_sample": bt.out_of_sample,
                "periods": bt.periods,
            })
        }
        "research" => {
            let mut request = h.rebalance_request(&args.as_of);
            request.trigger_key = Some(format!("cli-{}", args.as_of));
            let (task, _) = h.start("pm_alice", "cli", &request).await?;
            let step = h.advance(&task.task_id).await?;
            let (_, portfolio) = h.proposal(&task.task_id).await?;
            let weights: BTreeMap<&str, f64> = portfolio
                .weights
                .iter()
                .map(|(t, w)| (t.as_str(), w.as_ratio()))
                .collect();
            json!({
                "as_of_date": args.as_of,
                "weights": weights,
                "vetoed": portfolio.vetoed,
                "escalated": portfolio.escalated,
                "next_step": step_view(&step),
            })
        }
        other => return Err(format!("unknown command {other:?}; {USAGE}").into()),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
