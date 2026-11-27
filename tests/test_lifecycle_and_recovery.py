"""Tests for Agent 7 lifecycle: deduplication, clarification, recovery, verification, settlement & economics."""

from __future__ import annotations

import unittest

from trade_ai.fixtures import seed_pit_universe
from trade_ai.harness.workflow import TradeAIHarness, build_default_config
from trade_ai.schemas import AgentEvidenceOutput, ApprovalToken, AutonomyLevel, BudgetLimits, TaskState
from trade_ai.store import StateAndPITStore


class TestLifecycleAndRecovery(unittest.TestCase):
    def setUp(self) -> None:
        self.store = StateAndPITStore(":memory:")
        seed_pit_universe(self.store)

    def tearDown(self) -> None:
        self.store.close()

    def test_trigger_deduplication_and_ambiguity_clarification(self) -> None:
        harness = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.DRAFT_ONLY))
        t1, dup1 = harness.admit_task("cron-2026-02-10", "pm_alice", "svc", "cron", ("US",))
        t2, dup2 = harness.admit_task("cron-2026-02-10", "pm_alice", "svc", "cron", ("US",))
        self.assertFalse(dup1)
        self.assertTrue(dup2)
        self.assertEqual(t1.task_id, t2.task_id)

        # Missing as_of_date -> AWAITING_CLARIFICATION with WaitMetadata
        paused, contract = harness.frame_task(t1.task_id, "ambiguous goal", None, ("US",), 1_000_000.0, 3)
        self.assertIsNone(contract)
        self.assertEqual(paused.state, TaskState.AWAITING_CLARIFICATION)
        self.assertIsNotNone(paused.wait_meta)
        self.assertEqual(paused.wait_meta.owner_id, "pm_alice")  # type: ignore[union-attr]

        resumed, contract2 = harness.clarify_task(t1.task_id, "clarified goal", "2026-02-10", ("US",), 1_000_000.0, 3)
        self.assertIsNotNone(contract2)
        self.assertEqual(resumed.state, TaskState.RUNNING)
        self.assertIsNone(resumed.wait_meta)

    def test_timeout_outcome_unknown_freeze_reconcile_and_idempotent_retry(self) -> None:
        harness = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.PER_ACTION_APPROVAL))
        task, _ = harness.admit_task("trig-rec", "pm_alice", "svc", "cron", ("US",))
        _, contract = harness.frame_task(task.task_id, "recover test", "2026-02-10", ("US",), 1_000_000.0, 2)
        assert contract is not None

        # Approve R2 draft
        pre = harness.run_research_and_draft(task.task_id)
        draft_args = {
            "draft_id": f"DRAFT-{task.task_id}",
            "as_of_date": contract.as_of_date,
            "weights": pre["weights"],
            "vetoed": pre["vetoed"],
            "vetoed_tickers": sorted(pre["vetoed"].keys()),
            "escalated": pre["escalated"],
            "markets": ["US"],
        }
        d_tok = ApprovalToken(
            "APR-D",
            task.task_id,
            "approver_bob",
            ApprovalToken.compute_payload_hash("create_rebalance_draft", f"portfolio:{contract.as_of_date}", draft_args),
            "2099-12-31T23:59:59+00:00",
        )
        self.store.save_approval(d_tok)
        drafted = harness.submit_rebalance_draft(task.task_id, pre, approval_token=d_tok)
        self.assertEqual(drafted["status"], "EXECUTED")

        # Approve R4 order, simulate timeout -> OUTCOME_UNKNOWN
        order_idem = f"ord:{task.task_id}:1"
        order_args = {
            "draft_id": drafted["draft_id"],
            "as_of_date": contract.as_of_date,
            "weights": drafted["weights"],
            "vetoed_tickers": sorted(drafted["vetoed"].keys()),
            "portfolio_capital_usd": contract.portfolio_capital_usd,
            "markets": ["US"],
        }
        ord_hash = ApprovalToken.compute_payload_hash("submit_paper_orders", f"paper_broker:{contract.as_of_date}", order_args)
        o_tok = ApprovalToken("APR-O1", task.task_id, "approver_bob", ord_hash, "2099-12-31T23:59:59+00:00")
        self.store.save_approval(o_tok)

        dec_to = harness.execute_paper_orders(
            task.task_id, drafted["draft_id"], order_idem, approval_token=o_tok, simulated_timeout=True
        )
        self.assertEqual(dec_to.status, "OUTCOME_UNKNOWN")
        self.assertEqual(self.store.get_task(task.task_id).state, TaskState.OUTCOME_UNKNOWN)  # type: ignore[union-attr]

        # New action while OUTCOME_UNKNOWN is frozen
        frozen = harness.execute_paper_orders(task.task_id, drafted["draft_id"], "ord:different", approval_token=o_tok)
        self.assertEqual(frozen.status, "DENIED")
        self.assertEqual(frozen.reason, "frozen_outcome_unknown_reconcile_first")

        # Reconcile against backend -> safe to retry with same idempotency_key
        rec = harness.reconcile_before_retry(task.task_id, order_idem)
        self.assertTrue(rec["reconciled"])
        self.assertTrue(rec["action_needed"])
        self.assertEqual(self.store.get_task(task.task_id).state, TaskState.RUNNING)  # type: ignore[union-attr]

        # Retry with SAME idempotency key succeeds once
        dec_ok = harness.execute_paper_orders(task.task_id, drafted["draft_id"], order_idem, approval_token=o_tok)
        self.assertEqual(dec_ok.status, "EXECUTED")
        self.assertTrue(dec_ok.readback_verified)

        # Replay with SAME idempotency key returns idempotently without creating duplicate paper_orders
        replay = harness.execute_paper_orders(task.task_id, drafted["draft_id"], order_idem, approval_token=o_tok)
        self.assertEqual(replay.status, "EXECUTED")
        self.assertEqual(replay.reason, "idempotent_replay_verified")
        cnt = self.store.conn.execute("SELECT COUNT(*) FROM paper_orders WHERE task_id = ?", (task.task_id,)).fetchone()[0]
        self.assertEqual(cnt, 1)

    def test_independent_verifier_catches_bad_citation_and_vetoed_holding(self) -> None:
        harness = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.DRAFT_ONLY))
        task, _ = harness.admit_task("trig-ver", "pm_alice", "svc", "cli", ("US",))
        harness.frame_task(task.task_id, "verify check", "2026-02-10", ("US",), 1_000_000.0, 2)
        res = harness.run_research_and_draft(task.task_id)

        # Inject a future citation ID (FIN-LATE-2026 filed 2026-04-15 > 2026-02-10)
        bad_out = AgentEvidenceOutput(
            ticker="AAPL_MOAT",
            as_of_date="2026-02-10",
            agent="moat",
            score=0.5,
            confidence=0.8,
            hard_veto=False,
            evidence=["future leak test"],
            cited_record_ids=["FIN-LATE-2026"],
            missingness=0.0,
            stale_data=False,
        )
        tampered_decisions = list(res["decisions"])
        tampered_decisions[0] = type(tampered_decisions[0])(
            ticker=tampered_decisions[0].ticker,
            as_of_date=tampered_decisions[0].as_of_date,
            quant_base_score=tampered_decisions[0].quant_base_score,
            consensus_score=tampered_decisions[0].consensus_score,
            dispersion=tampered_decisions[0].dispersion,
            hard_veto=tampered_decisions[0].hard_veto,
            veto_reasons=tampered_decisions[0].veto_reasons,
            escalate_review=tampered_decisions[0].escalate_review,
            worker_outputs=[bad_out],
        )
        manifest = harness.verify_and_close(
            task.task_id, tampered_decisions, res["quant_by_ticker"], draft_id=res["draft_id"]
        )
        self.assertEqual(manifest.final_state, TaskState.ESCALATED_TO_HUMAN)
        self.assertFalse(manifest.criteria_results["citations_pit_valid"])

    def test_limit_reached_partial_manifest_settlement_reopen_and_handoff(self) -> None:
        # 1. Limit reached -> still produces DeliveryManifest + HandoffPackage
        tight_cfg = build_default_config(
            autonomy_level=AutonomyLevel.DRAFT_ONLY,
            budget_limits=BudgetLimits(max_turns=2, max_spend_usd=1.0, reserved_close_spend_usd=0.2),
        )
        h_lim = TradeAIHarness(store=self.store, config=tight_cfg)
        t_lim, _ = h_lim.admit_task("trig-lim", "pm_alice", "svc", "cron", ("US",))
        h_lim.frame_task(t_lim.task_id, "tight budget", "2026-02-10", ("US",), 1_000_000.0, 2)
        # Simulate turns already exhausted
        t_lim.turns_used = 2
        self.store.save_task(t_lim, transition_reason="test_exhausted", actor="test")
        res_lim = h_lim.run_research_and_draft(t_lim.task_id)
        self.assertEqual(res_lim["status"], "LIMIT_REACHED")
        manifest_lim = h_lim.verify_and_close(t_lim.task_id, [], {}, draft_id=None)
        self.assertEqual(manifest_lim.completeness, "blocked")
        self.assertEqual(manifest_lim.final_state, TaskState.LIMIT_REACHED)

        pkg = h_lim.create_handoff_package(
            t_lim.task_id,
            original_request="Run tight budget rebalance",
            trigger_reason="limit_reached",
            partial_results=manifest_lim.artifact_summary,
            open_questions=["Increase max_turns budget?"],
        )
        accepted = h_lim.confirm_handoff(pkg, "human_ops_lead")
        self.assertTrue(accepted.accepted_by_human)
        self.assertEqual(self.store.get_task(t_lim.task_id).state, TaskState.ESCALATED_TO_HUMAN)  # type: ignore[union-attr]

        # 2. Normal completion -> settlement -> curated memory -> unit economics -> reopen adjusts verified count
        h_ok = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.DRAFT_ONLY))
        t_ok, _ = h_ok.admit_task("trig-ok", "pm_alice", "svc", "cron", ("US",))
        h_ok.frame_task(t_ok.task_id, "normal close", "2026-02-10", ("US",), 1_000_000.0, 2)
        res_ok = h_ok.run_research_and_draft(t_ok.task_id)
        m_ok = h_ok.verify_and_close(t_ok.task_id, res_ok["decisions"], res_ok["quant_by_ticker"], res_ok["draft_id"])
        self.assertEqual(m_ok.final_state, TaskState.AWAITING_CONFIRMATION)

        h_ok.confirm_settlement(t_ok.task_id, "pm_alice", human_review_minutes=3.0)
        mems = self.store.list_curated_memory(status="pending_review")
        self.assertEqual(len(mems), 1)

        econ_before = self.store.calculate_unit_economics()
        self.assertEqual(econ_before["verified_completed_non_reopened"], 1)
        self.assertIsNotNone(econ_before["cost_per_verified_task_usd"])

        # Reopen after settlement -> verified non-reopened completions drops to 0
        h_ok.reopen_task(t_ok.task_id, "post_close_correction", "pm_alice")
        econ_after = self.store.calculate_unit_economics()
        self.assertEqual(econ_after["verified_completed_non_reopened"], 0)
        self.assertIsNone(econ_after["cost_per_verified_task_usd"])


if __name__ == "__main__":
    unittest.main()
