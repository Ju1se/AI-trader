"""Fixed-order 9-step Action Gate, isolated CredentialVault, and payload-bound approvals."""

from __future__ import annotations

from dataclasses import dataclass
import json
from typing import Any, Callable

from trade_ai.schemas import (
    ActionIntentRecord,
    ApprovalToken,
    AutonomyLevel,
    RiskTier,
    TaskRecord,
    TaskState,
    TaskTypeConfig,
    WaitMetadata,
    utc_now_iso,
)
from trade_ai.store import StateAndPITStore, detect_prompt_injection


class CredentialVault:
    """Agent 7 (2.3): Secrets stay strictly outside worker/model sandboxes."""

    def __init__(self) -> None:
        self.__secrets: dict[str, str] = {}

    def set_secret(self, name: str, value: str) -> None:
        self.__secrets[name] = value

    def get_secret(self, name: str) -> str | None:
        return self.__secrets.get(name)


@dataclass(frozen=True)
class ToolSpec:
    name: str
    required_args: tuple[str, ...]
    secret_name: str | None
    handler: Callable[[dict[str, Any], str | None], dict[str, Any]]
    readback_verifier: Callable[[dict[str, Any], dict[str, Any]], bool]


class ToolRegistry:
    """Explicit tool registry; tools not mounted for an actor are unavailable."""

    def __init__(self) -> None:
        self._tools: dict[str, ToolSpec] = {}

    def register(self, spec: ToolSpec) -> None:
        self._tools[spec.name] = spec

    def get(self, name: str) -> ToolSpec | None:
        return self._tools.get(name)


@dataclass(frozen=True)
class GateDecision:
    status: str  # EXECUTED | NEEDS_APPROVAL | DENIED | OUTCOME_UNKNOWN
    reason: str
    risk_tier: RiskTier
    idempotency_key: str | None = None
    receipt_id: str | None = None
    readback_verified: bool = False
    payload_hash: str | None = None
    result: dict[str, Any] | None = None


class ActionGate:
    """
    Agent 7 (4.1): Fixed-order 9-step gate with fail-closed guarantee.
    Order:
      1. Availability
      2. Limits
      3. Schema
      4. Hard deny & untrusted input guard
      5. Permissions & scope
      6. Business rules & risk tier (unlisted -> R4)
      7. Human approval (payload-bound, expiring, single-use, separation of duties)
      8. Intent & idempotency registration
      9. Execution-site re-check, credential proxy & independent read-back
    """

    def __init__(
        self,
        store: StateAndPITStore,
        registry: ToolRegistry,
        vault: CredentialVault,
    ) -> None:
        self.store = store
        self.registry = registry
        self.vault = vault

    def _deny(
        self,
        task: TaskRecord,
        reason: str,
        risk_tier: RiskTier,
        idempotency_key: str | None = None,
        payload_hash: str | None = None,
    ) -> GateDecision:
        task.consecutive_denials += 1
        self.store.save_task(task, transition_reason=f"gate_denied:{reason}", actor="action_gate")
        return GateDecision(
            status="DENIED",
            reason=reason,
            risk_tier=risk_tier,
            idempotency_key=idempotency_key,
            payload_hash=payload_hash,
        )

    def evaluate_and_execute(
        self,
        task: TaskRecord,
        config: TaskTypeConfig,
        mounted_tools: set[str],
        tool_name: str,
        target: str,
        args: dict[str, Any],
        idempotency_key: str,
        approval_token: ApprovalToken | None = None,
        wall_elapsed_seconds: float = 0.0,
        simulated_timeout: bool = False,
        is_close_phase: bool = False,
        extra_check: Callable[[], None] | None = None,
    ) -> GateDecision:
        risk_tier = config.get_tool_risk_tier(tool_name)
        try:
            # 1. Availability
            spec = self.registry.get(tool_name)
            if spec is None or tool_name not in mounted_tools:
                return self._deny(task, f"tool_unavailable:{tool_name}", risk_tier, idempotency_key)

            # 2. Limits
            limits = config.budget_limits
            spend_cap = limits.max_spend_usd if is_close_phase else limits.usable_execution_spend_usd
            if (
                task.turns_used >= limits.max_turns
                or task.spend_usd >= spend_cap
                or wall_elapsed_seconds > limits.max_wall_seconds
                or task.retries_used > limits.max_retries
                or task.consecutive_denials >= limits.max_consecutive_denials
                or task.reworks_used > limits.max_reworks
            ):
                task.state = TaskState.LIMIT_REACHED
                self.store.save_task(task, transition_reason="budget_or_safety_limit_reached", actor="action_gate")
                return GateDecision(
                    status="DENIED",
                    reason="limit_reached",
                    risk_tier=risk_tier,
                    idempotency_key=idempotency_key,
                )

            if task.state == TaskState.OUTCOME_UNKNOWN:
                return self._deny(task, "frozen_outcome_unknown_reconcile_first", risk_tier, idempotency_key)

            # Optional injected check for fail-closed testing
            if extra_check is not None:
                extra_check()

            # 3. Schema
            if not isinstance(args, dict):
                return self._deny(task, "invalid_args_type", risk_tier, idempotency_key)
            for req in spec.required_args:
                if req not in args or args[req] is None:
                    return self._deny(task, f"missing_required_arg:{req}", risk_tier, idempotency_key)

            # 4. Hard Deny & Untrusted Input Guard
            if not target or not isinstance(target, str):
                return self._deny(task, "invalid_target", risk_tier, idempotency_key)
            serialized_args = json.dumps(args, sort_keys=True)
            if detect_prompt_injection(serialized_args) or detect_prompt_injection(target):
                return self._deny(task, "prompt_injection_detected_in_args", risk_tier, idempotency_key)

            # 5. Permissions & Scope
            req_markets = args.get("markets")
            if isinstance(req_markets, (list, tuple)):
                for m in req_markets:
                    if m not in task.authorized_markets:
                        return self._deny(task, f"unauthorized_market:{m}", risk_tier, idempotency_key)
            if "market" in args and args["market"] not in task.authorized_markets:
                return self._deny(task, f"unauthorized_market:{args['market']}", risk_tier, idempotency_key)

            # 6. Business Rules & Risk Tier
            weights = args.get("weights")
            vetoed = set(args.get("vetoed_tickers", []))
            if isinstance(weights, dict):
                if any(float(w) < 0 for w in weights.values()):
                    return self._deny(task, "negative_weight_forbidden", risk_tier, idempotency_key)
                if sum(float(w) for w in weights.values()) > 1.0 + 1e-6:
                    return self._deny(task, "total_weight_exceeds_100pct", risk_tier, idempotency_key)
                if any(float(w) > 0.45 + 1e-6 for w in weights.values()):
                    return self._deny(task, "single_position_exceeds_cap", risk_tier, idempotency_key)
                for t in weights:
                    if t in vetoed:
                        return self._deny(task, f"vetoed_ticker_in_portfolio:{t}", risk_tier, idempotency_key)

            # 7. Human Approval & Progressive Autonomy
            payload_hash = ApprovalToken.compute_payload_hash(tool_name, target, args)
            if config.autonomy_level == AutonomyLevel.SHADOW and risk_tier in {
                RiskTier.R2_WRITE_REVERSIBLE,
                RiskTier.R3_WRITE_CONSEQUENTIAL,
                RiskTier.R4_HIGH_STAKES_FUNDS,
            }:
                return self._deny(task, "shadow_mode_blocks_write_tools", risk_tier, idempotency_key, payload_hash)

            if config.autonomy_level == AutonomyLevel.DRAFT_ONLY and risk_tier in {
                RiskTier.R3_WRITE_CONSEQUENTIAL,
                RiskTier.R4_HIGH_STAKES_FUNDS,
            }:
                return self._deny(task, "draft_only_blocks_execution_tools", risk_tier, idempotency_key, payload_hash)

            needs_approval = risk_tier in {
                RiskTier.R3_WRITE_CONSEQUENTIAL,
                RiskTier.R4_HIGH_STAKES_FUNDS,
            } or (
                config.autonomy_level == AutonomyLevel.PER_ACTION_APPROVAL
                and risk_tier == RiskTier.R2_WRITE_REVERSIBLE
            )

            if needs_approval:
                if approval_token is None:
                    task.state = TaskState.AWAITING_APPROVAL
                    task.wait_meta = WaitMetadata(
                        owner_id="portfolio_approver",
                        deadline_iso="2099-12-31T23:59:59+00:00",
                        timeout_action="expire_and_reject",
                    )
                    self.store.save_task(task, transition_reason=f"awaiting_approval:{tool_name}", actor="action_gate")
                    return GateDecision(
                        status="NEEDS_APPROVAL",
                        reason="human_approval_required",
                        risk_tier=risk_tier,
                        idempotency_key=idempotency_key,
                        payload_hash=payload_hash,
                    )

                existing_intent = self.store.get_action_intent(idempotency_key)
                is_exact_verified_replay = (
                    existing_intent is not None
                    and existing_intent.status == "verified"
                    and existing_intent.readback_verified
                    and existing_intent.payload_hash == payload_hash
                )
                if approval_token.task_id != task.task_id:
                    return self._deny(task, "approval_task_mismatch", risk_tier, idempotency_key, payload_hash)
                if approval_token.used and not is_exact_verified_replay:
                    return self._deny(task, "approval_already_used", risk_tier, idempotency_key, payload_hash)
                if approval_token.expires_at <= utc_now_iso():
                    return self._deny(task, "approval_expired", risk_tier, idempotency_key, payload_hash)
                if approval_token.payload_hash != payload_hash:
                    return self._deny(task, "approval_payload_hash_mismatch", risk_tier, idempotency_key, payload_hash)
                if approval_token.approver_id == task.requester_id:
                    return self._deny(task, "separation_of_duties_requester_cannot_approve", risk_tier, idempotency_key, payload_hash)
                if approval_token.approver_id == config.config_admin_id:
                    return self._deny(task, "separation_of_duties_admin_cannot_approve", risk_tier, idempotency_key, payload_hash)

            # 8. Intent & Idempotency Registration
            existing_intent = self.store.get_action_intent(idempotency_key)
            if existing_intent is not None:
                if existing_intent.status == "verified" and existing_intent.readback_verified:
                    return GateDecision(
                        status="EXECUTED",
                        reason="idempotent_replay_verified",
                        risk_tier=risk_tier,
                        idempotency_key=idempotency_key,
                        receipt_id=existing_intent.backend_receipt_id,
                        readback_verified=True,
                        payload_hash=payload_hash,
                    )
                if existing_intent.status == "unknown":
                    return self._deny(task, "existing_intent_unknown_reconcile_required", risk_tier, idempotency_key, payload_hash)

            intent = ActionIntentRecord(
                idempotency_key=idempotency_key,
                task_id=task.task_id,
                tool_name=tool_name,
                target=target,
                args_json=serialized_args,
                payload_hash=payload_hash,
                status="planned",
            )
            self.store.upsert_action_intent(intent)

            # 9. Execution-Site Re-check, Credential Proxy & Independent Read-Back
            if simulated_timeout:
                intent.status = "unknown"
                self.store.upsert_action_intent(intent)
                task.state = TaskState.OUTCOME_UNKNOWN
                self.store.save_task(task, transition_reason=f"timeout_during:{tool_name}", actor="action_gate")
                return GateDecision(
                    status="OUTCOME_UNKNOWN",
                    reason="backend_timeout_outcome_unknown",
                    risk_tier=risk_tier,
                    idempotency_key=idempotency_key,
                    payload_hash=payload_hash,
                )

            secret = self.vault.get_secret(spec.secret_name) if spec.secret_name else None
            backend_result = spec.handler(
                {**args, "task_id": task.task_id, "idempotency_key": idempotency_key, "target": target},
                secret,
            )
            if approval_token is not None:
                approval_token.used = True
                self.store.save_approval(approval_token)
                self.store.mark_approval_used(approval_token.approval_id)

            intent.status = "executed"
            intent.backend_receipt_id = str(backend_result.get("receipt_id", "")) or None
            self.store.upsert_action_intent(intent)

            readback_ok = bool(spec.readback_verifier(args, backend_result))
            if not readback_ok:
                intent.status = "failed"
                intent.readback_verified = False
                self.store.upsert_action_intent(intent)
                return self._deny(task, "backend_readback_verification_failed", risk_tier, idempotency_key, payload_hash)

            intent.status = "verified"
            intent.readback_verified = True
            self.store.upsert_action_intent(intent)

            task.consecutive_denials = 0
            task.state = TaskState.RUNNING
            task.wait_meta = None
            self.store.save_task(task, transition_reason=f"action_verified:{tool_name}", actor="action_gate")

            return GateDecision(
                status="EXECUTED",
                reason="verified_execution",
                risk_tier=risk_tier,
                idempotency_key=idempotency_key,
                receipt_id=intent.backend_receipt_id,
                readback_verified=True,
                payload_hash=payload_hash,
                result=backend_result,
            )
        except Exception as exc:
            # Fail-closed guarantee (Agent 7 4.1)
            return self._deny(task, f"gate_internal_error:{type(exc).__name__}:{exc}", risk_tier, idempotency_key)
