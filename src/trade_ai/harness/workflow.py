"""Agent 7 5-Stage Workflow Orchestrator: PREPARE -> ADMIT -> FRAME -> ACT -> CLOSE."""

from __future__ import annotations

import json
import uuid
from typing import Any, Sequence

from trade_ai.harness.committee import InvestmentCommittee
from trade_ai.harness.gate import ActionGate, CredentialVault, GateDecision, ToolRegistry, ToolSpec
from trade_ai.harness.workers import (
    FootnoteForensicsWorker,
    LLMJsonReasoner,
    ManagementCapAllocWorker,
    MoatWorker,
)
from trade_ai.quant.factors import QuantSecurityResult, score_pit_universe
from trade_ai.schemas import (
    ActionIntentRecord,
    ApprovalToken,
    AutonomyLevel,
    BudgetLimits,
    CommitteeDecision,
    DeliveryManifest,
    HandoffPackage,
    RiskTier,
    ShapeLevel,
    TaskContract,
    TaskRecord,
    TaskState,
    TaskTypeConfig,
    WaitMetadata,
    utc_now_iso,
)
from trade_ai.store import StateAndPITStore


def build_default_config(
    autonomy_level: AutonomyLevel = AutonomyLevel.PER_ACTION_APPROVAL,
    config_admin_id: str = "platform_admin",
    budget_limits: BudgetLimits | None = None,
) -> TaskTypeConfig:
    """Agent 7 (PREPARE): Default versioned TaskTypeConfig for value committee runs."""
    cfg = TaskTypeConfig(
        task_type="value_committee_rebalance",
        version="1.0.0",
        shape_level=ShapeLevel.MULTI_AGENT,
        shape_rationale=(
            "Deterministic 15-factor fixed workflow handles full-universe PIT math; "
            "read-only specialist workers inspect filings only for screened candidates."
        ),
        autonomy_level=autonomy_level,
        config_admin_id=config_admin_id,
        allowed_tools={
            "read_pit_market": RiskTier.R0_READ_PUBLIC,
            "read_pit_filings": RiskTier.R1_READ_SCOPED,
            "create_rebalance_draft": RiskTier.R2_WRITE_REVERSIBLE,
            "submit_paper_orders": RiskTier.R4_HIGH_STAKES_FUNDS,
        },
        budget_limits=budget_limits or BudgetLimits(),
        done_criteria=[
            "citations_pit_valid",
            "no_vetoed_holdings",
            "portfolio_constraints_valid",
            "executed_actions_readback_verified",
        ],
    )
    cfg.validate()
    return cfg


class IndependentVerifier:
    """Agent 7 (5.1): Verifies outcomes independently of the research/committee actors."""

    def __init__(self, store: StateAndPITStore, max_adv_participation: float = 0.10) -> None:
        self.store = store
        self.max_adv_participation = max_adv_participation

    def verify(
        self,
        task: TaskRecord,
        contract: TaskContract | None,
        decisions: Sequence[CommitteeDecision],
        draft: dict[str, Any] | None,
        quant_by_ticker: dict[str, QuantSecurityResult],
    ) -> tuple[bool, dict[str, bool], list[str]]:
        gaps: list[str] = []
        if contract is None:
            return False, {
                "citations_pit_valid": False,
                "no_vetoed_holdings": False,
                "portfolio_constraints_valid": False,
                "executed_actions_readback_verified": False,
            }, ["missing_task_contract"]

        # 1. Citations exist in PIT DB with filing_date <= as_of_date
        citations_ok = True
        for dec in decisions:
            for out in dec.worker_outputs:
                for rid in out.cited_record_ids:
                    if not self.store.verify_record_pit(rid, contract.as_of_date):
                        citations_ok = False
                        gaps.append(f"invalid_or_future_citation:{out.ticker}:{rid}")

        # 2. Zero hard-vetoed tickers in draft
        vetoed_set = {d.ticker for d in decisions if d.hard_veto}
        no_veto_ok = True
        constraints_ok = True
        if draft is None:
            no_veto_ok = False
            constraints_ok = False
            gaps.append("missing_rebalance_draft")
        else:
            weights: dict[str, float] = draft["weights"]
            for t, w in weights.items():
                if t in vetoed_set:
                    no_veto_ok = False
                    gaps.append(f"vetoed_ticker_in_draft:{t}")
                if w < 0:
                    constraints_ok = False
                    gaps.append(f"negative_weight:{t}")
                if t in quant_by_ticker:
                    adv_cap = (self.max_adv_participation * quant_by_ticker[t].adv_usd) / contract.portfolio_capital_usd
                    if w > adv_cap + 1e-4:
                        constraints_ok = False
                        gaps.append(f"adv_cap_exceeded:{t}")
            if sum(weights.values()) > 1.0 + 1e-6:
                constraints_ok = False
                gaps.append("total_weight_exceeds_1.0")

        # 3. Every consequential action in ledger has readback_verified=True
        actions = self.store.list_task_actions(task.task_id)
        readback_ok = True
        for act in actions:
            if act.status in {"unknown", "failed", "planned", "executed"} or not act.readback_verified:
                readback_ok = False
                gaps.append(f"unverified_action:{act.idempotency_key}:{act.status}")

        criteria = {
            "citations_pit_valid": citations_ok,
            "no_vetoed_holdings": no_veto_ok,
            "portfolio_constraints_valid": constraints_ok,
            "executed_actions_readback_verified": readback_ok,
        }
        return all(criteria.values()), criteria, gaps


class TradeAIHarness:
    """Complete Agent 1-7 governed research & paper-execution orchestrator."""

    def __init__(
        self,
        store: StateAndPITStore,
        config: TaskTypeConfig | None = None,
        vault: CredentialVault | None = None,
        reasoner: LLMJsonReasoner | None = None,
        committee: InvestmentCommittee | None = None,
    ) -> None:
        self.store = store
        self.config = config or build_default_config()
        self.vault = vault or CredentialVault()
        if self.vault.get_secret("paper_broker_api_key") is None:
            self.vault.set_secret("paper_broker_api_key", "vault_demo_paper_secret_key")

        self.registry = ToolRegistry()
        self._register_backend_tools()
        self.gate = ActionGate(store=self.store, registry=self.registry, vault=self.vault)

        self.reasoner = reasoner
        self.moat_worker = MoatWorker()
        self.mgmt_worker = ManagementCapAllocWorker()
        self.forensics_worker = FootnoteForensicsWorker()
        self.committee = committee or InvestmentCommittee()
        self.verifier = IndependentVerifier(store=self.store)

    def _register_backend_tools(self) -> None:
        def create_draft_handler(args: dict[str, Any], _secret: str | None) -> dict[str, Any]:
            draft_id = args["draft_id"]
            self.store.save_rebalance_draft(
                draft_id=draft_id,
                task_id=args["task_id"],
                as_of_date=args["as_of_date"],
                weights=args["weights"],
                vetoed=args.get("vetoed", {}),
                escalated=args.get("escalated", []),
            )
            return {"receipt_id": draft_id, "draft_id": draft_id}

        def create_draft_readback(args: dict[str, Any], res: dict[str, Any]) -> bool:
            stored = self.store.get_rebalance_draft(res["draft_id"])
            if stored is None:
                return False
            return stored["weights"] == {k: float(v) for k, v in args["weights"].items()}

        def submit_orders_handler(args: dict[str, Any], secret: str | None) -> dict[str, Any]:
            if not secret:
                raise PermissionError("missing_vault_credential_for_paper_broker")
            idem = args["idempotency_key"]
            existing = self.store.get_paper_order_by_idempotency(idem)
            if existing is not None:
                return {"receipt_id": existing["receipt_id"], "status": existing["status"], "orders": existing["orders"]}
            receipt_id = f"PO-{uuid.uuid4().hex[:10]}"
            orders = [
                {
                    "ticker": t,
                    "target_weight": float(w),
                    "notional_usd": round(float(w) * float(args["portfolio_capital_usd"]), 2),
                }
                for t, w in sorted(args["weights"].items())
            ]
            self.store.save_paper_order(
                receipt_id=receipt_id,
                idempotency_key=idem,
                task_id=args["task_id"],
                as_of_date=args["as_of_date"],
                orders=orders,
                status="FILLED",
            )
            return {"receipt_id": receipt_id, "status": "FILLED", "orders": orders}

        def submit_orders_readback(args: dict[str, Any], res: dict[str, Any]) -> bool:
            stored = self.store.get_paper_order_by_idempotency(res.get("idempotency_key") or args.get("_idem", ""))
            if stored is None:
                # fallback look up by iterating task actions if needed, or pass receipt check:
                row = self.store.conn.execute(
                    "SELECT * FROM paper_orders WHERE receipt_id = ?",
                    (res["receipt_id"],),
                ).fetchone()
                if row is None:
                    return False
                stored_orders = json.loads(row["orders_json"])
                return len(stored_orders) == len(args["weights"]) and row["status"] == "FILLED"
            return len(stored["orders"]) == len(args["weights"]) and stored["status"] == "FILLED"

        self.registry.register(
            ToolSpec(
                name="create_rebalance_draft",
                required_args=("draft_id", "as_of_date", "weights"),
                secret_name=None,
                handler=create_draft_handler,
                readback_verifier=create_draft_readback,
            )
        )
        self.registry.register(
            ToolSpec(
                name="submit_paper_orders",
                required_args=("as_of_date", "weights", "portfolio_capital_usd"),
                secret_name="paper_broker_api_key",
                handler=submit_orders_handler,
                readback_verifier=submit_orders_readback,
            )
        )

    # --- Stage 2: ADMIT ---

    def admit_task(
        self,
        idempotency_trigger_key: str,
        requester_id: str,
        acting_principal: str,
        trigger_source: str,
        authorized_markets: Sequence[str] = ("US",),
    ) -> tuple[TaskRecord, bool]:
        """Deduplicate incoming triggers into one stable external TaskRecord."""
        existing = self.store.get_task_by_trigger_key(idempotency_trigger_key)
        if existing is not None:
            return existing, True

        task = TaskRecord(
            task_id=f"TASK-{uuid.uuid4().hex[:10]}",
            idempotency_trigger_key=idempotency_trigger_key,
            task_type=self.config.task_type,
            config_version=self.config.version,
            requester_id=requester_id,
            acting_principal=acting_principal,
            trigger_source=trigger_source,
            authorized_markets=tuple(authorized_markets),
            state=TaskState.ADMITTED,
        )
        self.store.create_task(task, actor=trigger_source)
        return task, False

    # --- Stage 3: FRAME ---

    def frame_task(
        self,
        task_id: str,
        goal: str,
        as_of_date: str | None,
        universe_markets: Sequence[str],
        portfolio_capital_usd: float | None,
        max_positions: int = 3,
    ) -> tuple[TaskRecord, TaskContract | None]:
        """Pin contract outside the model; pause in AWAITING_CLARIFICATION if key parameters are ambiguous."""
        task = self.store.get_task(task_id)
        if task is None:
            raise KeyError(f"Unknown task_id: {task_id}")

        ambiguities: list[str] = []
        if not as_of_date or not as_of_date.strip():
            ambiguities.append("missing_as_of_date")
        if portfolio_capital_usd is None or portfolio_capital_usd <= 0:
            ambiguities.append("missing_or_non_positive_capital")
        if not universe_markets:
            ambiguities.append("empty_universe_markets")
        elif any(m not in task.authorized_markets for m in universe_markets):
            ambiguities.append("requested_market_outside_authorized_scope")

        if ambiguities:
            task.state = TaskState.AWAITING_CLARIFICATION
            task.wait_meta = WaitMetadata(
                owner_id=task.requester_id,
                deadline_iso="2099-12-31T23:59:59+00:00",
                timeout_action="cancel_unclarified_task",
            )
            self.store.save_task(task, transition_reason=f"clarification_needed:{','.join(ambiguities)}", actor="harness")
            return task, None

        contract = TaskContract(
            task_id=task.task_id,
            goal=goal,
            as_of_date=as_of_date,  # type: ignore[arg-type]
            universe_markets=tuple(universe_markets),
            portfolio_capital_usd=float(portfolio_capital_usd),  # type: ignore[arg-type]
            max_positions=max_positions,
            expected_outputs=["quant_ranking", "committee_decisions", "rebalance_draft", "delivery_manifest"],
            done_checks=list(self.config.done_criteria),
            assumptions=["accounting_lag_enforced_by_filing_date", "pit_survivor_bias_free"],
            irreversible_steps_deferred=True,
        )
        self.store.save_contract(contract)
        task.state = TaskState.RUNNING
        task.wait_meta = None
        self.store.save_task(task, transition_reason="contract_pinned", actor="harness")
        return task, contract

    def clarify_task(
        self,
        task_id: str,
        goal: str,
        as_of_date: str,
        universe_markets: Sequence[str],
        portfolio_capital_usd: float,
        max_positions: int = 3,
    ) -> tuple[TaskRecord, TaskContract | None]:
        return self.frame_task(
            task_id=task_id,
            goal=goal,
            as_of_date=as_of_date,
            universe_markets=universe_markets,
            portfolio_capital_usd=portfolio_capital_usd,
            max_positions=max_positions,
        )

    # --- Stage 3/4: Research + Draft ---

    def run_research_and_draft(
        self,
        task_id: str,
        draft_approval_token: ApprovalToken | None = None,
        audit_vetoed_tickers: Sequence[str] = (),
        max_research_candidates: int = 8,
        min_adv_usd: float = 1_000_000.0,
    ) -> dict[str, Any]:
        task = self.store.get_task(task_id)
        contract = self.store.get_contract(task_id)
        if task is None or contract is None:
            raise RuntimeError("Task and pinned TaskContract required before running research")

        # Enforce turn & spend budget before heavy work
        if (
            task.turns_used >= self.config.budget_limits.max_turns
            or task.spend_usd >= self.config.budget_limits.usable_execution_spend_usd
        ):
            task.state = TaskState.LIMIT_REACHED
            self.store.save_task(task, transition_reason="pre_research_limit_reached", actor="harness")
            return {"status": "LIMIT_REACHED", "quant_results": [], "decisions": [], "draft_decision": None}

        snapshot = self.store.get_pit_snapshot(as_of_date=contract.as_of_date, markets=contract.universe_markets)
        snap_by_ticker = {r["ticker"]: r for r in snapshot}
        quant_results = score_pit_universe(snapshot, as_of_date=contract.as_of_date, min_adv_usd=min_adv_usd)
        quant_by_ticker = {r.ticker: r for r in quant_results}

        # Select candidates for specialist workers: non-vetoed top K + explicit audit tickers
        non_vetoed = [r for r in quant_results if not r.hard_veto][:max_research_candidates]
        research_set: list[QuantSecurityResult] = list(non_vetoed)
        for t in audit_vetoed_tickers:
            if t in quant_by_ticker and quant_by_ticker[t] not in research_set:
                research_set.append(quant_by_ticker[t])

        # Sandboxed read-only view for workers (no write authority)
        ro_view = self.store.make_read_only_view(task.authorized_markets)
        decisions: list[CommitteeDecision] = []

        for q in quant_results:
            worker_outputs = []
            if q in research_set:
                snap_row = snap_by_ticker[q.ticker]
                worker_outputs = [
                    self.moat_worker.evaluate(ro_view, snap_row, q, contract.as_of_date, self.reasoner),
                    self.mgmt_worker.evaluate(ro_view, snap_row, q, contract.as_of_date, self.reasoner),
                    self.forensics_worker.evaluate(ro_view, snap_row, q, contract.as_of_date, self.reasoner),
                ]
                task.turns_used += 3
                worker_cost = 0.03
                task.spend_usd = round(task.spend_usd + worker_cost, 4)
                self.store.record_cost(task.task_id, "specialist_workers", worker_cost, note=f"workers:{q.ticker}")
            dec = self.committee.evaluate_security(q, worker_outputs, self.store)
            decisions.append(dec)

        self.store.save_task(task, transition_reason="committee_scored", actor="harness")

        weights, vetoed, escalated = self.committee.build_target_weights(
            decisions=decisions,
            quant_by_ticker=quant_by_ticker,
            portfolio_capital_usd=contract.portfolio_capital_usd,
            max_positions=contract.max_positions,
        )

        draft_id = f"DRAFT-{task.task_id}"
        mounted_harness_tools = {"create_rebalance_draft", "submit_paper_orders"}
        draft_args = {
            "draft_id": draft_id,
            "as_of_date": contract.as_of_date,
            "weights": weights,
            "vetoed": vetoed,
            "vetoed_tickers": sorted(vetoed.keys()),
            "escalated": escalated,
            "markets": list(contract.universe_markets),
        }
        gate_dec = self.gate.evaluate_and_execute(
            task=task,
            config=self.config,
            mounted_tools=mounted_harness_tools,
            tool_name="create_rebalance_draft",
            target=f"portfolio:{contract.as_of_date}",
            args=draft_args,
            idempotency_key=f"draft:{task.task_id}:{contract.as_of_date}",
            approval_token=draft_approval_token,
        )

        return {
            "status": gate_dec.status,
            "draft_id": draft_id if gate_dec.status == "EXECUTED" else None,
            "draft_decision": gate_dec,
            "quant_results": quant_results,
            "quant_by_ticker": quant_by_ticker,
            "decisions": decisions,
            "weights": weights,
            "vetoed": vetoed,
            "escalated": escalated,
        }

    def submit_rebalance_draft(
        self,
        task_id: str,
        research_result: dict[str, Any],
        approval_token: ApprovalToken | None = None,
    ) -> dict[str, Any]:
        """Submit or retry only the gated create_rebalance_draft step without re-running workers."""
        task = self.store.get_task(task_id)
        contract = self.store.get_contract(task_id)
        if task is None or contract is None:
            raise RuntimeError("Task and Contract must exist before submitting rebalance draft")

        draft_id = f"DRAFT-{task.task_id}"
        draft_args = {
            "draft_id": draft_id,
            "as_of_date": contract.as_of_date,
            "weights": research_result["weights"],
            "vetoed": research_result["vetoed"],
            "vetoed_tickers": sorted(research_result["vetoed"].keys()),
            "escalated": research_result["escalated"],
            "markets": list(contract.universe_markets),
        }
        gate_dec = self.gate.evaluate_and_execute(
            task=task,
            config=self.config,
            mounted_tools={"create_rebalance_draft", "submit_paper_orders"},
            tool_name="create_rebalance_draft",
            target=f"portfolio:{contract.as_of_date}",
            args=draft_args,
            idempotency_key=f"draft:{task.task_id}:{contract.as_of_date}",
            approval_token=approval_token,
        )
        return {
            **research_result,
            "status": gate_dec.status,
            "draft_id": draft_id if gate_dec.status == "EXECUTED" else None,
            "draft_decision": gate_dec,
        }

    def execute_paper_orders(
        self,
        task_id: str,
        draft_id: str,
        idempotency_key: str,
        approval_token: ApprovalToken | None = None,
        simulated_timeout: bool = False,
    ) -> GateDecision:
        task = self.store.get_task(task_id)
        contract = self.store.get_contract(task_id)
        draft = self.store.get_rebalance_draft(draft_id)
        if task is None or contract is None or draft is None:
            raise RuntimeError("Task, Contract, and Draft must exist before paper execution")

        order_args = {
            "draft_id": draft_id,
            "as_of_date": contract.as_of_date,
            "weights": draft["weights"],
            "vetoed_tickers": sorted(draft["vetoed"].keys()),
            "portfolio_capital_usd": contract.portfolio_capital_usd,
            "markets": list(contract.universe_markets),
        }
        dec = self.gate.evaluate_and_execute(
            task=task,
            config=self.config,
            mounted_tools={"create_rebalance_draft", "submit_paper_orders"},
            tool_name="submit_paper_orders",
            target=f"paper_broker:{contract.as_of_date}",
            args=order_args,
            idempotency_key=idempotency_key,
            approval_token=approval_token,
            simulated_timeout=simulated_timeout,
        )
        if dec.status == "EXECUTED":
            self.store.record_cost(task_id, "paper_execution", 0.01, note=f"receipt:{dec.receipt_id}")
        return dec

    def reconcile_before_retry(self, task_id: str, idempotency_key: str) -> dict[str, Any]:
        """Agent 7 (4.5): Reconcile OUTCOME_UNKNOWN against backend before any retry or resume."""
        task = self.store.get_task(task_id)
        if task is None:
            raise KeyError(f"Unknown task_id: {task_id}")
        if task.state != TaskState.OUTCOME_UNKNOWN:
            return {"reconciled": False, "reason": f"task_not_outcome_unknown:{task.state.value}"}

        intent = self.store.get_action_intent(idempotency_key)
        backend_order = self.store.get_paper_order_by_idempotency(idempotency_key)

        if backend_order is not None:
            if intent is not None:
                intent.status = "verified"
                intent.backend_receipt_id = backend_order["receipt_id"]
                intent.readback_verified = True
                self.store.upsert_action_intent(intent)
            task.state = TaskState.RUNNING
            self.store.save_task(task, transition_reason="reconciled_already_executed", actor="reconciler")
            return {"reconciled": True, "action_needed": False, "receipt_id": backend_order["receipt_id"]}

        # Not executed in backend: reset intent to planned so caller may safely retry with SAME idempotency_key
        if intent is not None:
            intent.status = "planned"
            self.store.upsert_action_intent(intent)
        task.retries_used += 1
        task.state = TaskState.RUNNING
        self.store.save_task(task, transition_reason="reconciled_not_executed_safe_to_retry", actor="reconciler")
        return {"reconciled": True, "action_needed": True, "idempotency_key": idempotency_key}

    # --- Stage 5: CLOSE ---

    def verify_and_close(
        self,
        task_id: str,
        decisions: Sequence[CommitteeDecision],
        quant_by_ticker: dict[str, QuantSecurityResult],
        draft_id: str | None = None,
    ) -> DeliveryManifest:
        """Agent 7 (5.1-5.2): Independent verification + DeliveryManifest (even for partial outcomes)."""
        task = self.store.get_task(task_id)
        if task is None:
            raise KeyError(f"Unknown task_id: {task_id}")
        contract = self.store.get_contract(task_id)
        draft = self.store.get_rebalance_draft(draft_id) if draft_id else None

        if task.state in {TaskState.LIMIT_REACHED, TaskState.ESCALATED_TO_HUMAN, TaskState.OUTCOME_UNKNOWN}:
            completeness = "partial" if draft is not None else "blocked"
            manifest = DeliveryManifest(
                task_id=task.task_id,
                final_state=task.state,
                completeness=completeness,
                criteria_results={c: False for c in self.config.done_criteria},
                assumptions=contract.assumptions if contract else [],
                gaps=[f"stopped_in_state:{task.state.value}"],
                total_spend_usd=round(task.spend_usd, 4),
                next_owner=task.wait_meta.owner_id if task.wait_meta else "human_operator",
                artifact_summary={
                    "draft_id": draft_id,
                    "weights": draft["weights"] if draft else {},
                    "decisions_count": len(decisions),
                },
            )
            manifest.validate()
            return manifest

        task.state = TaskState.VERIFYING
        self.store.save_task(task, transition_reason="independent_verification_started", actor="verifier")
        self.store.record_cost(task.task_id, "independent_verifier", 0.01, note="close_verification")

        passed, criteria, gaps = self.verifier.verify(
            task=task,
            contract=contract,
            decisions=decisions,
            draft=draft,
            quant_by_ticker=quant_by_ticker,
        )

        if passed:
            task.state = TaskState.AWAITING_CONFIRMATION
            task.wait_meta = WaitMetadata(
                owner_id=task.requester_id,
                deadline_iso="2099-12-31T23:59:59+00:00",
                timeout_action="auto_escalate_unconfirmed",
            )
            self.store.save_task(task, transition_reason="verification_passed_awaiting_confirmation", actor="verifier")
            completeness = "complete"
            next_owner = task.requester_id
        else:
            task.state = TaskState.ESCALATED_TO_HUMAN
            task.wait_meta = WaitMetadata(
                owner_id="risk_committee_lead",
                deadline_iso="2099-12-31T23:59:59+00:00",
                timeout_action="manual_review_required",
            )
            self.store.save_task(task, transition_reason=f"verification_failed:{','.join(gaps)}", actor="verifier")
            completeness = "partial" if draft is not None else "blocked"
            next_owner = "risk_committee_lead"

        manifest = DeliveryManifest(
            task_id=task.task_id,
            final_state=task.state,
            completeness=completeness,
            criteria_results=criteria,
            assumptions=contract.assumptions if contract else [],
            gaps=gaps,
            total_spend_usd=round(task.spend_usd, 4),
            next_owner=next_owner,
            artifact_summary={
                "draft_id": draft_id,
                "weights": draft["weights"] if draft else {},
                "vetoed": draft["vetoed"] if draft else {},
                "escalated": draft["escalated"] if draft else [],
            },
        )
        manifest.validate()
        return manifest

    def confirm_settlement(self, task_id: str, confirmer_id: str, human_review_minutes: float = 2.0) -> TaskRecord:
        """Agent 7 (5.3 & 5.5): Settle after confirmation and extract curated memory pending review."""
        task = self.store.get_task(task_id)
        if task is None:
            raise KeyError(f"Unknown task_id: {task_id}")
        if task.state != TaskState.AWAITING_CONFIRMATION:
            raise RuntimeError(f"Cannot settle task in state {task.state.value}")

        task.state = TaskState.COMPLETED
        task.wait_meta = None
        self.store.save_task(task, transition_reason=f"settled_by:{confirmer_id}", actor=confirmer_id)
        self.store.record_cost(task.task_id, "human_settlement_review", 0.0, human_minutes=human_review_minutes, note=confirmer_id)
        self.store.add_curated_memory(
            memory_id=f"MEM-{task.task_id}",
            source_task_id=task.task_id,
            summary=f"Settled {task.task_type} v{task.config_version} with {task.turns_used} turns.",
        )
        return task

    def reopen_task(self, task_id: str, reason: str, actor: str) -> TaskRecord:
        """Agent 7 (5.3): Allow reopening after close; deducts from non-reopened verified count."""
        task = self.store.get_task(task_id)
        if task is None:
            raise KeyError(f"Unknown task_id: {task_id}")
        task.reopened_count += 1
        task.reworks_used += 1
        task.state = TaskState.RUNNING
        task.wait_meta = None
        self.store.save_task(task, transition_reason=f"reopened:{reason}", actor=actor)
        return task

    def create_handoff_package(
        self,
        task_id: str,
        original_request: str,
        trigger_reason: str,
        partial_results: dict[str, Any],
        open_questions: list[str],
    ) -> HandoffPackage:
        """Agent 7 (5.4): Create explicit handoff package for human takeover."""
        task = self.store.get_task(task_id)
        if task is None:
            raise KeyError(f"Unknown task_id: {task_id}")
        actions = self.store.list_task_actions(task_id)
        completed = [f"{a.tool_name}:{a.idempotency_key}:{a.status}" for a in actions if a.readback_verified]
        pending = [f"{a.tool_name}:{a.idempotency_key}:{a.status}" for a in actions if not a.readback_verified]
        pkg = HandoffPackage(
            task_id=task_id,
            requester_id=task.requester_id,
            original_request=original_request,
            trigger_reason=trigger_reason,
            completed_actions=completed,
            pending_actions=pending,
            partial_results=partial_results,
            open_questions=open_questions,
        )
        pkg.validate()
        return pkg

    def confirm_handoff(self, package: HandoffPackage, human_owner_id: str) -> HandoffPackage:
        task = self.store.get_task(package.task_id)
        if task is None:
            raise KeyError(f"Unknown task_id: {package.task_id}")
        package.assigned_human_owner = human_owner_id
        package.accepted_by_human = True
        task.state = TaskState.ESCALATED_TO_HUMAN
        task.wait_meta = WaitMetadata(
            owner_id=human_owner_id,
            deadline_iso="2099-12-31T23:59:59+00:00",
            timeout_action="human_owner_follow_up",
        )
        self.store.save_task(task, transition_reason=f"handoff_accepted:{human_owner_id}", actor=human_owner_id)
        return package
