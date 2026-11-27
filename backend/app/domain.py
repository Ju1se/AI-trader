"""Task states, legal transitions and risk tiers.

Pure logic with no I/O, so the rules in docs/diagrams.md (图 4a, 图 3a) can be tested directly.
"""

from enum import StrEnum


class TaskState(StrEnum):
    QUEUED = "QUEUED"
    RUNNING = "RUNNING"
    AWAITING_INPUT = "AWAITING_INPUT"
    AWAITING_APPROVAL = "AWAITING_APPROVAL"
    OUTCOME_UNKNOWN = "OUTCOME_UNKNOWN"
    LIMIT_REACHED = "LIMIT_REACHED"
    VERIFYING = "VERIFYING"
    SETTLING = "SETTLING"
    COMPLETED = "COMPLETED"
    FAILED = "FAILED"
    HANDED_OFF = "HANDED_OFF"
    DECLINED = "DECLINED"
    CANCELLED = "CANCELLED"


S = TaskState

# label: name shown to people. owner_role: who has to act while the task sits in this state.
STATE_INFO: dict[TaskState, dict] = {
    S.QUEUED: {"label": "排队", "owner_role": "operator"},
    S.RUNNING: {"label": "运行中", "owner_role": "operator"},
    S.AWAITING_INPUT: {"label": "等回答", "owner_role": "requester"},
    S.AWAITING_APPROVAL: {"label": "等批准", "owner_role": "approver"},
    S.OUTCOME_UNKNOWN: {"label": "结果未知", "owner_role": "reconciler"},
    S.LIMIT_REACHED: {"label": "触上限", "owner_role": "owner"},
    S.VERIFYING: {"label": "验收中", "owner_role": "verifier"},
    S.SETTLING: {"label": "待确认", "owner_role": "reviewer"},
    S.COMPLETED: {"label": "已完成", "owner_role": None},
    S.FAILED: {"label": "已失败", "owner_role": "owner"},
    S.HANDED_OFF: {"label": "转人工", "owner_role": "assignee"},
    S.DECLINED: {"label": "被否决", "owner_role": "requester"},
    S.CANCELLED: {"label": "已取消", "owner_role": None},
}

TERMINAL = frozenset({S.COMPLETED, S.FAILED, S.HANDED_OFF, S.DECLINED, S.CANCELLED})

_EXPLICIT: dict[TaskState, set[TaskState]] = {
    S.QUEUED: {S.RUNNING, S.FAILED},
    S.RUNNING: {S.AWAITING_INPUT, S.AWAITING_APPROVAL, S.OUTCOME_UNKNOWN, S.LIMIT_REACHED, S.VERIFYING, S.FAILED},
    S.AWAITING_INPUT: {S.RUNNING},
    S.AWAITING_APPROVAL: {S.RUNNING, S.DECLINED},
    S.OUTCOME_UNKNOWN: {S.RUNNING},
    S.LIMIT_REACHED: {S.RUNNING, S.VERIFYING},
    S.VERIFYING: {S.RUNNING, S.SETTLING, S.FAILED},
    S.SETTLING: {S.COMPLETED, S.RUNNING},
    S.COMPLETED: {S.RUNNING},  # reopened after a later check finds the outcome wrong
    S.HANDED_OFF: {S.RUNNING},  # a person answers and hands the task back
    S.FAILED: set(),
    S.DECLINED: set(),
    S.CANCELLED: set(),
}


def allowed_next(state: TaskState) -> set[TaskState]:
    nxt = set(_EXPLICIT[state])
    if state not in TERMINAL:
        nxt.add(S.HANDED_OFF)
        # An unknown outcome must be reconciled before the task can count as cancelled.
        if state is not S.OUTCOME_UNKNOWN:
            nxt.add(S.CANCELLED)
    return nxt


def can_transition(frm: TaskState, to: TaskState) -> bool:
    return to in allowed_next(frm)


class RiskTier(StrEnum):
    R0 = "R0"  # read inside the authorised scope
    R1 = "R1"  # reversible write in the task's own workspace
    R2 = "R2"  # compensable write to a system of record, within business limits
    R3 = "R3"  # irreversible, over limit, bulk, new counterparty
    R4 = "R4"  # touches its own permissions, limits, audit log or credentials


class GateDecision(StrEnum):
    ALLOW = "allow"
    DENY = "deny"
    ASK = "ask"  # hold the call for a human approval
    STOP = "stop"  # the run itself must stop: limit reached or task frozen
