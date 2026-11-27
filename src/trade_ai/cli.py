"""CLI entrypoint for backtesting, governed research, and end-to-end Agent 1-7 demo."""

from __future__ import annotations

import argparse
import json
import sys

from trade_ai.fixtures import REBALANCE_DATES, seed_pit_universe
from trade_ai.harness.workflow import TradeAIHarness, build_default_config
from trade_ai.quant.backtest import audit_survivorship_bias_exercise, run_pit_backtest
from trade_ai.schemas import ApprovalToken, AutonomyLevel
from trade_ai.store import StateAndPITStore


def _print_header(title: str) -> None:
    print(f"\n{'=' * 72}\n{title}\n{'=' * 72}")


def run_demo(db_path: str = ":memory:") -> int:
    store = StateAndPITStore(db_path)
    seed_pit_universe(store)

    _print_header("1. Backtesting Pitfalls & Survivorship Bias Audit")
    ex = audit_survivorship_bias_exercise()
    print(
        f"Textbook Exercise -> Survivors Only: {ex['survivors_only_return']:.1%}, "
        f"Survivor-Bias-Free: {ex['survivor_bias_free_return']:.1%}, "
        f"Artificial Gain: +{ex['artificial_gain_pct_points']} pct points"
    )

    bt = run_pit_backtest(
        store=store,
        rebalance_dates=REBALANCE_DATES,
        markets=("US",),
        top_n=3,
        portfolio_capital_usd=1_000_000.0,
        one_way_cost_bps=15.0,
        max_adv_participation=0.10,
        buffer_band_pct=0.20,
        oos_start_date="2026-02-10",
    )
    print(
        f"PIT Backtest Overall -> Periods={bt.overall.periods}, "
        f"CumRet={bt.overall.cumulative_return:.2%}, "
        f"Sharpe={bt.overall.sharpe_ratio:.2f}, "
        f"MaxDD={bt.overall.max_drawdown:.2%}, "
        f"AvgTurnover={bt.overall.avg_turnover:.2%}"
    )
    if bt.in_sample and bt.out_of_sample:
        print(
            f"  In-Sample CumRet={bt.in_sample.cumulative_return:.2%} | "
            f"Out-of-Sample CumRet={bt.out_of_sample.cumulative_return:.2%}"
        )
    for p in bt.periods:
        print(
            f"  [{p.rebalance_date}] Selected={p.selected_tickers} "
            f"Vetoed={p.vetoed_tickers} NetRet={p.net_return:.2%} Cost={p.cost_drag:.4%}"
        )

    _print_header("2. Agent 7 Governed Workflow: PREPARE -> ADMIT -> FRAME -> ACT -> CLOSE")
    config = build_default_config(autonomy_level=AutonomyLevel.PER_ACTION_APPROVAL)
    harness = TradeAIHarness(store=store, config=config)

    # ADMIT
    task, dedup = harness.admit_task(
        idempotency_trigger_key="sched-rebalance-2026-02-10",
        requester_id="pm_alice",
        acting_principal="quant_research_svc",
        trigger_source="monthly_cron",
        authorized_markets=("US",),
    )
    print(f"ADMIT -> task_id={task.task_id}, state={task.state.value}, deduplicated={dedup}")

    # FRAME
    task, contract = harness.frame_task(
        task_id=task.task_id,
        goal="Run PIT value+quality screen and build governed rebalance draft for 2026-02-10",
        as_of_date="2026-02-10",
        universe_markets=("US",),
        portfolio_capital_usd=1_000_000.0,
        max_positions=3,
    )
    assert contract is not None
    print(f"FRAME -> state={task.state.value}, as_of={contract.as_of_date}, capital=${contract.portfolio_capital_usd:,.0f}")

    # Research + Draft (First call triggers NEEDS_APPROVAL for R2 in PER_ACTION_APPROVAL mode)
    res = harness.run_research_and_draft(
        task_id=task.task_id,
        audit_vetoed_tickers=("TRAP_CO",),
    )
    print(f"ACT (Draft Gate) -> status={res['draft_decision'].status}, payload_hash={res['draft_decision'].payload_hash[:16]}...")

    # Approver signs payload-bound token (separation of duties: approver_bob != pm_alice != platform_admin)
    draft_args = {
        "draft_id": f"DRAFT-{task.task_id}",
        "as_of_date": contract.as_of_date,
        "weights": res["weights"],
        "vetoed": res["vetoed"],
        "vetoed_tickers": sorted(res["vetoed"].keys()),
        "escalated": res["escalated"],
        "markets": list(contract.universe_markets),
    }
    draft_token = ApprovalToken(
        approval_id=f"APR-DRAFT-{task.task_id}",
        task_id=task.task_id,
        approver_id="approver_bob",
        payload_hash=ApprovalToken.compute_payload_hash(
            "create_rebalance_draft", f"portfolio:{contract.as_of_date}", draft_args
        ),
        expires_at="2099-12-31T23:59:59+00:00",
    )
    store.save_approval(draft_token)
    res2 = harness.submit_rebalance_draft(
        task_id=task.task_id,
        research_result=res,
        approval_token=draft_token,
    )
    print(f"ACT (Draft Approved) -> status={res2['status']}, draft_id={res2['draft_id']}")

    _print_header("3. Investment Committee Decision Table (2026-02-10)")
    for d in res2["decisions"]:
        print(
            f"  {d.ticker:<10} | Quant={d.quant_base_score:+.3f} | "
            f"Consensus={d.consensus_score:+.3f} | Disp={d.dispersion:.3f} | "
            f"Veto={str(d.hard_veto):<5} | Escalate={str(d.escalate_review):<5}"
        )
    print(f"Target Weights: {json.dumps(res2['weights'])}")
    print(f"Escalated for Review: {res2['escalated']}")
    print(f"Hard Vetoed: {list(res2['vetoed'].keys())}")

    _print_header("4. Consequential Action (R4 Paper Orders) + Payload-Bound Approval + Read-Back")
    order_idem = f"order:{task.task_id}:2026-02-10"
    order_args = {
        "draft_id": res2["draft_id"],
        "as_of_date": contract.as_of_date,
        "weights": res2["weights"],
        "vetoed_tickers": sorted(res2["vetoed"].keys()),
        "portfolio_capital_usd": contract.portfolio_capital_usd,
        "markets": list(contract.universe_markets),
    }
    order_token = ApprovalToken(
        approval_id=f"APR-ORD-{task.task_id}",
        task_id=task.task_id,
        approver_id="approver_bob",
        payload_hash=ApprovalToken.compute_payload_hash(
            "submit_paper_orders", f"paper_broker:{contract.as_of_date}", order_args
        ),
        expires_at="2099-12-31T23:59:59+00:00",
    )
    store.save_approval(order_token)
    exec_dec = harness.execute_paper_orders(
        task_id=task.task_id,
        draft_id=res2["draft_id"],
        idempotency_key=order_idem,
        approval_token=order_token,
    )
    print(
        f"Paper Execution -> status={exec_dec.status}, receipt_id={exec_dec.receipt_id}, "
        f"readback_verified={exec_dec.readback_verified}"
    )

    _print_header("5. CLOSE -> Independent Verification, Manifest, Settlement & Unit Economics")
    manifest = harness.verify_and_close(
        task_id=task.task_id,
        decisions=res2["decisions"],
        quant_by_ticker=res2["quant_by_ticker"],
        draft_id=res2["draft_id"],
    )
    print(
        f"Verify -> final_state={manifest.final_state.value}, completeness={manifest.completeness}, "
        f"criteria={manifest.criteria_results}"
    )
    settled = harness.confirm_settlement(task.task_id, confirmer_id="pm_alice", human_review_minutes=3.0)
    econ = store.calculate_unit_economics()
    print(f"Settled -> state={settled.state.value}")
    print(f"Unit Economics -> {json.dumps(econ)}")

    store.close()
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="trade-ai", description="Agent 1-7 governed value investing CLI")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_demo = sub.add_parser("demo", help="Run full end-to-end backtest + Agent 7 workflow demo")
    p_demo.add_argument("--db", default=":memory:", help="SQLite DB path (default: :memory:)")

    p_bt = sub.add_parser("backtest", help="Run PIT pitfall-safe backtest on seeded universe")
    p_bt.add_argument("--db", default=":memory:", help="SQLite DB path (default: :memory:)")

    p_res = sub.add_parser("research", help="Run single-date PIT quant + committee evaluation")
    p_res.add_argument("--db", default=":memory:", help="SQLite DB path (default: :memory:)")
    p_res.add_argument("--as-of", default="2026-02-10", help="Point-in-time date YYYY-MM-DD")

    args = parser.parse_args(argv)
    if args.cmd == "demo":
        return run_demo(args.db)

    store = StateAndPITStore(args.db)
    seed_pit_universe(store)
    if args.cmd == "backtest":
        bt = run_pit_backtest(store, REBALANCE_DATES, markets=("US",), oos_start_date="2026-02-10")
        print(json.dumps({"overall": bt.overall.__dict__, "periods": [p.__dict__ for p in bt.periods]}, indent=2))
        store.close()
        return 0

    if args.cmd == "research":
        harness = TradeAIHarness(store=store, config=build_default_config(AutonomyLevel.DRAFT_ONLY))
        t, _ = harness.admit_task(f"cli-{args.as_of}", "cli_user", "cli_svc", "cli", ("US",))
        harness.frame_task(t.task_id, "CLI PIT research", args.as_of, ("US",), 1_000_000.0, 3)
        res = harness.run_research_and_draft(t.task_id, audit_vetoed_tickers=("TRAP_CO",))
        print(json.dumps({"weights": res["weights"], "vetoed": res["vetoed"], "escalated": res["escalated"]}, indent=2))
        store.close()
        return 0

    return 1


if __name__ == "__main__":
    sys.exit(main())
