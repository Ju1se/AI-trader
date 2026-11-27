"""Tests for 15-factor quant scoring and all 5 backtesting pitfalls."""

from __future__ import annotations

import unittest

from trade_ai.fixtures import REBALANCE_DATES, seed_pit_universe
from trade_ai.quant.backtest import audit_survivorship_bias_exercise, run_pit_backtest
from trade_ai.quant.factors import score_pit_universe
from trade_ai.store import StateAndPITStore


class TestBacktestPitfalls(unittest.TestCase):
    def setUp(self) -> None:
        self.store = StateAndPITStore(":memory:")
        seed_pit_universe(self.store)

    def tearDown(self) -> None:
        self.store.close()

    def test_survivorship_bias_exercise_exact_numbers(self) -> None:
        audit = audit_survivorship_bias_exercise()
        self.assertAlmostEqual(audit["survivors_only_return"], 0.05, places=4)
        self.assertAlmostEqual(audit["survivor_bias_free_return"], -0.2667, places=4)
        self.assertAlmostEqual(audit["artificial_gain_pct_points"], 31.7, places=1)

    def test_look_ahead_bias_blocked_by_filing_date(self) -> None:
        snap_feb = {r["ticker"]: r for r in self.store.get_pit_snapshot("2026-02-10", ("US",))}
        self.assertEqual(snap_feb["LATE_FILER"]["fin_record_id"], "FIN-LATE-2025")
        self.assertEqual(snap_feb["LATE_FILER"]["filing_date"], "2025-03-31")

        filings_feb = self.store.get_pit_filings("LATE_FILER", "2026-02-10")
        self.assertEqual([f.record_id for f in filings_feb], ["FIL-LATE-10K-2025"])

        snap_may = {r["ticker"]: r for r in self.store.get_pit_snapshot("2026-05-01", ("US",))}
        self.assertEqual(snap_may["LATE_FILER"]["fin_record_id"], "FIN-LATE-2026")
        self.assertEqual(snap_may["LATE_FILER"]["filing_date"], "2026-04-15")

        filings_may = self.store.get_pit_filings("LATE_FILER", "2026-05-01")
        self.assertEqual(filings_may[0].record_id, "FIL-LATE-10K-2026")

    def test_delisted_security_included_before_delist_and_excluded_after(self) -> None:
        snap_feb = {r["ticker"]: r for r in self.store.get_pit_snapshot("2026-02-10", ("US",))}
        self.assertIn("DEBT_FAIL", snap_feb)
        self.assertAlmostEqual(snap_feb["DEBT_FAIL"]["delist_return"], -0.90, places=4)

        snap_may = {r["ticker"]: r for r in self.store.get_pit_snapshot("2026-05-01", ("US",))}
        self.assertNotIn("DEBT_FAIL", snap_may)

    def test_hard_veto_blocks_value_trap_and_distress(self) -> None:
        snap = self.store.get_pit_snapshot("2026-02-10", ("US",))
        scored = {r.ticker: r for r in score_pit_universe(snap, as_of_date="2026-02-10")}

        self.assertTrue(scored["TRAP_CO"].hard_veto)
        self.assertIn("negative_cfo_positive_ni", scored["TRAP_CO"].veto_reasons)
        self.assertTrue(scored["DEBT_FAIL"].hard_veto)
        self.assertTrue(any("distress_zscore" in r for r in scored["DEBT_FAIL"].veto_reasons))
        self.assertFalse(scored["AAPL_MOAT"].hard_veto)

    def test_backtest_costs_oos_split_and_adv_cap(self) -> None:
        bt = run_pit_backtest(
            store=self.store,
            rebalance_dates=REBALANCE_DATES,
            markets=("US",),
            top_n=3,
            portfolio_capital_usd=1_000_000.0,
            one_way_cost_bps=15.0,
            max_adv_participation=0.10,
            buffer_band_pct=0.20,
            oos_start_date="2026-02-10",
        )
        self.assertEqual(bt.overall.periods, 4)
        self.assertIsNotNone(bt.in_sample)
        self.assertIsNotNone(bt.out_of_sample)
        self.assertEqual(bt.in_sample.periods, 2)  # type: ignore[union-attr]
        self.assertEqual(bt.out_of_sample.periods, 2)  # type: ignore[union-attr]
        for p in bt.periods:
            self.assertNotIn("TRAP_CO", p.selected_tickers)
            self.assertNotIn("DEBT_FAIL", p.selected_tickers)
        self.assertGreater(bt.periods[0].cost_drag, 0.0)
        self.assertGreater(sum(p.cost_drag for p in bt.periods), 0.0)

        # Extreme capital should cap individual weights by max_adv_participation * adv_usd / capital
        bt_large = run_pit_backtest(
            store=self.store,
            rebalance_dates=("2026-02-10",),
            markets=("US",),
            top_n=3,
            portfolio_capital_usd=200_000_000.0,
            max_adv_participation=0.10,
        )
        weights = bt_large.periods[0].weights
        self.assertLess(sum(weights.values()), 0.15)


if __name__ == "__main__":
    unittest.main()
