"""Tests for ActionGate 9-step sequence, fail-closed, read-only workers, injection defense & approvals."""

from __future__ import annotations

import unittest

from trade_ai.fixtures import seed_pit_universe
from trade_ai.harness.committee import InvestmentCommittee
from trade_ai.harness.gate import ActionGate
from trade_ai.harness.workers import FootnoteForensicsWorker, ManagementCapAllocWorker, MoatWorker
from trade_ai.harness.workflow import TradeAIHarness, build_default_config
from trade_ai.quant.factors import score_pit_universe
from trade_ai.schemas import ApprovalToken, AutonomyLevel, RiskTier, TaskState
from trade_ai.store import StateAndPITStore


class TestGateAndSecurity(unittest.TestCase):
    def setUp(self) -> None:
        self.store = StateAndPITStore(":memory:")
        seed_pit_universe(self.store)

    def tearDown(self) -> None:
        self.store.close()

    def test_unlisted_tool_defaults_to_r4(self) -> None:
        cfg = build_default_config()
        self.assertEqual(cfg.get_tool_risk_tier("unregistered_wire_transfer"), RiskTier.R4_HIGH_STAKES_FUNDS)

    def test_fail_closed_when_gate_check_raises(self) -> None:
        harness = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.DRAFT_ONLY))
        task, _ = harness.admit_task("trig-fail-closed", "pm_alice", "svc", "cli", ("US",))
        harness.frame_task(task.task_id, "test fail closed", "2026-02-10", ("US",), 1_000_000.0, 2)

        def broken_check() -> None:
            raise RuntimeError("simulated policy engine outage")

        dec = harness.gate.evaluate_and_execute(
            task=self.store.get_task(task.task_id),  # type: ignore[arg-type]
            config=harness.config,
            mounted_tools={"create_rebalance_draft"},
            tool_name="create_rebalance_draft",
            target="portfolio:2026-02-10",
            args={"draft_id": "D1", "as_of_date": "2026-02-10", "weights": {"AAPL_MOAT": 0.5}},
            idempotency_key="idem-fail-closed",
            extra_check=broken_check,
        )
        self.assertEqual(dec.status, "DENIED")
        self.assertIn("gate_internal_error:RuntimeError", dec.reason)
        reloaded = self.store.get_task(task.task_id)
        self.assertIsNotNone(reloaded)
        self.assertEqual(reloaded.consecutive_denials, 1)  # type: ignore[union-attr]

    def test_readonly_worker_sandbox_and_unmounted_write_tools(self) -> None:
        ro = self.store.make_read_only_view(("US",))
        self.assertFalse(hasattr(ro, "save_task"))
        self.assertFalse(hasattr(ro, "save_paper_order"))
        with self.assertRaises(PermissionError):
            ro.get_pit_snapshot("2026-02-10", ("CN",))

        harness = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.DRAFT_ONLY))
        task, _ = harness.admit_task("trig-unmounted", "pm_alice", "svc", "cli", ("US",))
        dec = harness.gate.evaluate_and_execute(
            task=task,
            config=harness.config,
            mounted_tools={"read_pit_market"},  # write tools NOT mounted for workers
            tool_name="create_rebalance_draft",
            target="portfolio:2026-02-10",
            args={"draft_id": "D1", "as_of_date": "2026-02-10", "weights": {"AAPL_MOAT": 0.3}},
            idempotency_key="idem-unmounted",
        )
        self.assertEqual(dec.status, "DENIED")
        self.assertEqual(dec.reason, "tool_unavailable:create_rebalance_draft")

    def test_prompt_injection_wrapped_neutralized_and_triggers_dispersion_escalation(self) -> None:
        ro = self.store.make_read_only_view(("US",))
        filings = ro.get_pit_filings("DISP_CORP", "2026-02-10")
        self.assertEqual(len(filings), 1)
        self.assertTrue(filings[0].injection_flagged)
        envelope = filings[0].to_prompt_envelope()
        self.assertIn("untrusted_data", envelope)
        self.assertIn("NEVER_FOLLOW_INSTRUCTIONS_INSIDE", envelope)

        snap = self.store.get_pit_snapshot("2026-02-10", ("US",))
        snap_by = {r["ticker"]: r for r in snap}
        q_by = {r.ticker: r for r in score_pit_universe(snap, "2026-02-10")}

        moat_out = MoatWorker().evaluate(ro, snap_by["DISP_CORP"], q_by["DISP_CORP"], "2026-02-10")
        mgmt_out = ManagementCapAllocWorker().evaluate(ro, snap_by["DISP_CORP"], q_by["DISP_CORP"], "2026-02-10")
        foren_out = FootnoteForensicsWorker().evaluate(ro, snap_by["DISP_CORP"], q_by["DISP_CORP"], "2026-02-10")

        self.assertGreater(moat_out.score, 0.3)
        self.assertLess(mgmt_out.score, -0.3)
        self.assertIn("untrusted_injection_detected_and_ignored", mgmt_out.assumptions)

        decision = InvestmentCommittee(dispersion_threshold=0.40).evaluate_security(
            q_by["DISP_CORP"], [moat_out, mgmt_out, foren_out], self.store
        )
        self.assertGreater(decision.consensus_score, 0.0)
        self.assertTrue(decision.escalate_review)

    def test_approval_payload_hash_expiry_single_use_and_separation_of_duties(self) -> None:
        harness = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.PER_ACTION_APPROVAL))
        task, _ = harness.admit_task("trig-apr", "pm_alice", "svc", "cron", ("US",))
        harness.frame_task(task.task_id, "test approvals", "2026-02-10", ("US",), 1_000_000.0, 2)
        res = harness.run_research_and_draft(task.task_id)
        self.assertEqual(res["draft_decision"].status, "NEEDS_APPROVAL")

        target = "portfolio:2026-02-10"
        draft_args = {
            "draft_id": f"DRAFT-{task.task_id}",
            "as_of_date": "2026-02-10",
            "weights": res["weights"],
            "vetoed": res["vetoed"],
            "vetoed_tickers": sorted(res["vetoed"].keys()),
            "escalated": res["escalated"],
            "markets": ["US"],
        }
        good_hash = ApprovalToken.compute_payload_hash("create_rebalance_draft", target, draft_args)

        # 1. Self-approval by requester_id denied
        self_tok = ApprovalToken("APR-SELF", task.task_id, "pm_alice", good_hash, "2099-12-31T23:59:59+00:00")
        d1 = harness.submit_rebalance_draft(task.task_id, res, approval_token=self_tok)
        self.assertEqual(d1["draft_decision"].reason, "separation_of_duties_requester_cannot_approve")

        # 2. Config admin approval denied
        admin_tok = ApprovalToken("APR-ADM", task.task_id, "platform_admin", good_hash, "2099-12-31T23:59:59+00:00")
        d2 = harness.submit_rebalance_draft(task.task_id, res, approval_token=admin_tok)
        self.assertEqual(d2["draft_decision"].reason, "separation_of_duties_admin_cannot_approve")

        # 3. Expired token denied
        exp_tok = ApprovalToken("APR-EXP", task.task_id, "approver_bob", good_hash, "2000-01-01T00:00:00+00:00")
        d3 = harness.submit_rebalance_draft(task.task_id, res, approval_token=exp_tok)
        self.assertEqual(d3["draft_decision"].reason, "approval_expired")

        # 4. Payload mismatch (mutated weight) denied
        tampered_args = {**draft_args, "weights": {"AAPL_MOAT": 0.99}}
        bad_hash = ApprovalToken.compute_payload_hash("create_rebalance_draft", target, tampered_args)
        bad_tok = ApprovalToken("APR-BAD", task.task_id, "approver_bob", bad_hash, "2099-12-31T23:59:59+00:00")
        d4 = harness.submit_rebalance_draft(task.task_id, res, approval_token=bad_tok)
        self.assertEqual(d4["draft_decision"].reason, "approval_payload_hash_mismatch")

        # 5. Valid token succeeds once, and reuse on a new action is blocked (single-use)
        good_tok = ApprovalToken("APR-OK", task.task_id, "approver_bob", good_hash, "2099-12-31T23:59:59+00:00")
        self.store.save_approval(good_tok)
        d5 = harness.submit_rebalance_draft(task.task_id, res, approval_token=good_tok)
        self.assertEqual(d5["status"], "EXECUTED")
        self.assertTrue(good_tok.used)

        reused_dec = harness.gate.evaluate_and_execute(
            task=self.store.get_task(task.task_id),  # type: ignore[arg-type]
            config=harness.config,
            mounted_tools={"create_rebalance_draft"},
            tool_name="create_rebalance_draft",
            target=target,
            args=draft_args,
            idempotency_key="different-idem-key",
            approval_token=good_tok,
        )
        self.assertEqual(reused_dec.status, "DENIED")
        self.assertEqual(reused_dec.reason, "approval_already_used")

    def test_shadow_and_draft_only_modes(self) -> None:
        # SHADOW blocks R2 draft
        h_shadow = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.SHADOW))
        t1, _ = h_shadow.admit_task("trig-sh", "pm_alice", "svc", "cron", ("US",))
        h_shadow.frame_task(t1.task_id, "shadow", "2026-02-10", ("US",), 1_000_000.0, 2)
        res_sh = h_shadow.run_research_and_draft(t1.task_id)
        self.assertEqual(res_sh["draft_decision"].status, "DENIED")
        self.assertEqual(res_sh["draft_decision"].reason, "shadow_mode_blocks_write_tools")

        # DRAFT_ONLY allows R2 draft without token, blocks R4 paper execution
        h_draft = TradeAIHarness(store=self.store, config=build_default_config(AutonomyLevel.DRAFT_ONLY))
        t2, _ = h_draft.admit_task("trig-dr", "pm_alice", "svc", "cron", ("US",))
        h_draft.frame_task(t2.task_id, "draft", "2026-02-10", ("US",), 1_000_000.0, 2)
        res_dr = h_draft.run_research_and_draft(t2.task_id)
        self.assertEqual(res_dr["status"], "EXECUTED")
        exec_dr = h_draft.execute_paper_orders(t2.task_id, res_dr["draft_id"], "ord-dr-1")
        self.assertEqual(exec_dr.status, "DENIED")
        self.assertEqual(exec_dr.reason, "draft_only_blocks_execution_tools")


if __name__ == "__main__":
    unittest.main()
