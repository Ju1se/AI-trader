"""The gate in front of every tool call (docs/diagrams.md, 图 3a).

Checks 0 to 5 run here, in a fixed order. Checks 6 to 8 (human approval, reserving the
idempotency key, re-checking at the point of use) belong to the approval and execution
services and are not part of this skeleton yet.
"""

from collections.abc import Callable
from dataclasses import dataclass, field

from .domain import GateDecision, RiskTier, TaskState


@dataclass
class ProposedCall:
    tool: str
    args: dict
    task_state: TaskState
    limits_left: bool = True
    # True once the run has read content an outsider could have written.
    tainted: bool = False


@dataclass
class GateResult:
    decision: GateDecision
    check: int
    reason: str
    tier: RiskTier | None = None


@dataclass
class GatePolicy:
    tool_tiers: dict[str, str]
    hard_denied_tools: set[str] = field(default_factory=set)
    # R2 tools whose arguments the backend validates field by field; they stay automatic after taint.
    narrow_tools: set[str] = field(default_factory=set)
    validate_args: Callable[[str, dict], str | None] = lambda tool, args: None
    authorize: Callable[[str, dict], bool] = lambda tool, args: True
    # Returns "allow", "ask" or "deny" for calls that business rules decide (limits, counterparties).
    business_rule: Callable[[str, dict], str] | None = None


def evaluate(call: ProposedCall, policy: GatePolicy) -> GateResult:
    try:
        return _evaluate(call, policy)
    except Exception as exc:  # a check that breaks must never let a call through
        return GateResult(GateDecision.DENY, -1, f"gate check failed: {exc}")


def _evaluate(call: ProposedCall, policy: GatePolicy) -> GateResult:
    if call.tool not in policy.tool_tiers:
        return GateResult(GateDecision.DENY, 0, "tool is not exposed for this task type")
    if not call.limits_left or call.task_state is TaskState.OUTCOME_UNKNOWN:
        return GateResult(GateDecision.STOP, 1, "limit reached or an outcome is still unknown")
    problem = policy.validate_args(call.tool, call.args)
    if problem:
        return GateResult(GateDecision.DENY, 2, problem)
    if call.tool in policy.hard_denied_tools:
        return GateResult(GateDecision.DENY, 3, "tool is on the never list")
    if not policy.authorize(call.tool, call.args):
        return GateResult(GateDecision.DENY, 4, "not authorised for this target")

    tier = RiskTier(policy.tool_tiers[call.tool])
    if tier is RiskTier.R4:
        return GateResult(GateDecision.DENY, 5, "tier R4 is never allowed", tier)
    if tier is RiskTier.R3:
        return GateResult(GateDecision.ASK, 5, "tier R3 needs a human approval", tier)
    if policy.business_rule is not None:
        verdict = GateDecision(policy.business_rule(call.tool, call.args))
        if verdict is not GateDecision.ALLOW:
            return GateResult(verdict, 5, "business rule", tier)
    if tier is RiskTier.R2 and call.tainted and call.tool not in policy.narrow_tools:
        return GateResult(GateDecision.ASK, 5, "R2 write after untrusted content was read", tier)
    return GateResult(GateDecision.ALLOW, 5, "within tier and limits", tier)
