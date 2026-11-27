import pytest

from app.adapters import SimBroker
from app.domain import TERMINAL, GateDecision, allowed_next, can_transition
from app.domain import TaskState as S
from app.gate import GatePolicy, ProposedCall, evaluate

TIERS = {"get_quote": "R0", "write_report": "R1", "submit_paper_order": "R2",
         "create_order_request": "R3", "change_limits": "R4"}


def call(tool, **kwargs):
    return ProposedCall(tool=tool, args={}, task_state=kwargs.pop("state", S.RUNNING), **kwargs)


def test_completed_is_reachable_only_through_verifying_and_settling():
    assert [s for s in S if can_transition(s, S.COMPLETED)] == [S.SETTLING]
    assert [s for s in S if can_transition(s, S.SETTLING)] == [S.VERIFYING]


def test_declined_only_follows_an_approval_wait():
    assert [s for s in S if can_transition(s, S.DECLINED)] == [S.AWAITING_APPROVAL]


def test_every_live_state_can_be_handed_off():
    for state in S:
        assert can_transition(state, S.HANDED_OFF) == (state not in TERMINAL)


def test_unknown_outcome_cannot_be_cancelled_before_reconciling():
    assert not can_transition(S.OUTCOME_UNKNOWN, S.CANCELLED)
    assert can_transition(S.RUNNING, S.CANCELLED)


def test_closed_states_have_no_exit_except_reopen_and_hand_back():
    assert allowed_next(S.FAILED) == set()
    assert allowed_next(S.DECLINED) == set()
    assert allowed_next(S.CANCELLED) == set()
    assert allowed_next(S.COMPLETED) == {S.RUNNING}
    assert allowed_next(S.HANDED_OFF) == {S.RUNNING}


@pytest.mark.parametrize("tool,expected", [
    ("get_quote", GateDecision.ALLOW), ("write_report", GateDecision.ALLOW),
    ("submit_paper_order", GateDecision.ALLOW), ("create_order_request", GateDecision.ASK),
    ("change_limits", GateDecision.DENY), ("not_exposed", GateDecision.DENY),
])
def test_gate_routes_by_tier(tool, expected):
    assert evaluate(call(tool), GatePolicy(tool_tiers=TIERS)).decision is expected


def test_gate_stops_when_frozen_or_out_of_limits():
    policy = GatePolicy(tool_tiers=TIERS)
    assert evaluate(call("get_quote", state=S.OUTCOME_UNKNOWN), policy).decision is GateDecision.STOP
    assert evaluate(call("get_quote", limits_left=False), policy).decision is GateDecision.STOP


def test_gate_order_denies_before_asking():
    policy = GatePolicy(tool_tiers=TIERS, hard_denied_tools={"create_order_request"})
    result = evaluate(call("create_order_request"), policy)
    assert (result.decision, result.check) == (GateDecision.DENY, 3)


def test_gate_fails_closed_when_a_check_raises():
    def broken(tool, args):
        raise RuntimeError("policy service down")

    result = evaluate(call("get_quote"), GatePolicy(tool_tiers=TIERS, authorize=broken))
    assert result.decision is GateDecision.DENY


def test_tainted_run_needs_approval_for_r2_unless_tool_is_narrow():
    wide = GatePolicy(tool_tiers=TIERS)
    narrow = GatePolicy(tool_tiers=TIERS, narrow_tools={"submit_paper_order"})
    assert evaluate(call("submit_paper_order", tainted=True), wide).decision is GateDecision.ASK
    assert evaluate(call("submit_paper_order", tainted=True), narrow).decision is GateDecision.ALLOW


def test_sim_broker_returns_the_first_order_for_a_repeated_key():
    broker = SimBroker()
    order = {"symbol": "ACME", "side": "buy", "quantity": 100}
    first = broker.submit_order("task1:op1", order)
    assert broker.submit_order("task1:op1", order) is first
    assert broker.find_by_key("task1:op1")["order_id"] == first["order_id"]
    with pytest.raises(ValueError):
        broker.submit_order("task1:op1", {**order, "quantity": 200})
