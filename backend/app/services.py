"""Use cases of the control plane. Every state change goes through transition()."""

import hashlib
import json
import uuid
from datetime import UTC, datetime
from decimal import Decimal

from sqlalchemy import func, select, update
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session

from . import models as m
from .domain import TERMINAL, can_transition
from .domain import TaskState as S
from .events import bus


class ApiError(Exception):
    def __init__(self, status: int, code: str, message: str) -> None:
        super().__init__(message)
        self.status, self.code, self.message = status, code, message


def utcnow() -> datetime:
    return datetime.now(UTC).replace(tzinfo=None)


def to_naive_utc(value: datetime | None) -> datetime | None:
    if value is None or value.tzinfo is None:
        return value
    return value.astimezone(UTC).replace(tzinfo=None)


def new_id(prefix: str) -> str:
    return f"{prefix}_{uuid.uuid4().hex[:16]}"


def hash_call(tool: str, args: dict) -> str:
    raw = json.dumps({"tool": tool, "args": args}, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(raw.encode()).hexdigest()


def log_event(db: Session, task_id: str, event_type: str, payload: dict | None = None) -> None:
    db.add(m.SessionEvent(task_id=task_id, event_type=event_type, payload=payload or {}, at=utcnow()))


def add_cost(db: Session, task_id: str, stage: str, kind: str, amount: str = "0", human_minutes: int = 0) -> None:
    db.add(m.CostEntry(task_id=task_id, stage=stage, kind=kind, amount=Decimal(amount),
                       human_minutes=human_minutes, at=utcnow()))


def notify(task: m.Task, event_type: str = "task.updated") -> None:
    bus.publish({"type": event_type, "task_id": task.id, "state": task.state, "state_version": task.state_version})


def get_task(db: Session, task_id: str) -> m.Task:
    task = db.get(m.Task, task_id)
    if task is None:
        raise ApiError(404, "task_not_found", f"no task {task_id}")
    return task


def create_task(db: Session, *, profile_id: str, title: str, request_text: str, requester: str,
                dedupe_key: str | None = None, due_at: datetime | None = None) -> tuple[m.Task, bool]:
    """INTAKE: one task per trigger. A repeated trigger returns the existing task."""
    if db.get(m.TaskTypeProfile, profile_id) is None:
        raise ApiError(404, "profile_not_found", f"no task type {profile_id}")
    key = dedupe_key or hashlib.sha256(f"{profile_id}|{requester}|{title}|{request_text}".encode()).hexdigest()
    existing = db.scalar(select(m.Task).where(m.Task.dedupe_key == key))
    if existing is not None:
        return existing, False
    now = utcnow()
    task = m.Task(id=new_id("task"), profile_id=profile_id, dedupe_key=key, title=title,
                  request_text=request_text, state=S.QUEUED, state_version=1, requester=requester,
                  acting_principal=f"agent:{profile_id}", owner=requester,
                  due_at=to_naive_utc(due_at), created_at=now, updated_at=now)
    db.add(task)
    try:
        db.flush()  # the task row must exist before the rows that reference it
    except IntegrityError:  # another request with the same key won the race
        db.rollback()
        winner = db.scalar(select(m.Task).where(m.Task.dedupe_key == key))
        if winner is None:
            raise
        return winner, False
    db.add(m.Authority(task_id=task.id, roles=["requester"], data_scope={"owner": requester}))
    log_event(db, task.id, "task.created", {"title": title, "requester": requester})
    db.commit()
    notify(task, "task.created")
    return task, True


def has_unresolved_actions(db: Session, task_id: str) -> bool:
    count = db.scalar(select(func.count()).select_from(m.Action).where(
        m.Action.task_id == task_id, m.Action.status.in_(("INTENDED", "UNKNOWN"))))
    return bool(count)


def transition(db: Session, task: m.Task, to: S, *, actor: str, reason: str = "") -> None:
    """Move a task to another state. Does not commit; the caller commits and notifies."""
    frm = S(task.state)
    if not can_transition(frm, to):
        raise ApiError(409, "invalid_transition", f"{frm} -> {to} is not allowed")
    if to is S.FAILED and has_unresolved_actions(db, task.id):
        # A task with an action in flight is not failed, its outcome is unknown.
        raise ApiError(409, "unresolved_actions", "resolve in-flight actions before failing the task")
    # Compare-and-set on the version: a concurrent writer makes this update match no row.
    result = db.execute(
        update(m.Task)
        .where(m.Task.id == task.id, m.Task.state_version == task.state_version)
        .values(state=to, state_version=task.state_version + 1, updated_at=utcnow())
    )
    if result.rowcount != 1:
        raise ApiError(409, "state_conflict", "the task changed concurrently; reload and retry")
    db.refresh(task)
    log_event(db, task.id, "task.state_changed", {"from": frm, "to": to, "actor": actor, "reason": reason})


def open_wait(db: Session, task_id: str) -> m.Wait | None:
    return db.scalar(select(m.Wait).where(m.Wait.task_id == task_id, m.Wait.resolved_at.is_(None))
                     .order_by(m.Wait.id.desc()))


def resolve_waits(db: Session, task_id: str, kind: str | None = None, answer: dict | None = None) -> None:
    query = select(m.Wait).where(m.Wait.task_id == task_id, m.Wait.resolved_at.is_(None))
    if kind:
        query = query.where(m.Wait.kind == kind)
    for wait in db.scalars(query):
        wait.resolved_at, wait.answer = utcnow(), answer


def answer_input(db: Session, task: m.Task, *, user: str, answer: dict) -> None:
    if S(task.state) is not S.AWAITING_INPUT:
        raise ApiError(409, "not_awaiting_input", "the task is not waiting for an answer")
    if user != task.requester:
        raise ApiError(403, "not_requester", "only the requester may answer")
    resolve_waits(db, task.id, "INPUT", answer)
    log_event(db, task.id, "input.answered", {"by": user, "answer": answer})
    transition(db, task, S.RUNNING, actor=user, reason="requester answered")
    db.commit()
    notify(task)


def decide_approval(db: Session, approval_id: str, *, user: str, decision: str, reason: str = "") -> m.Approval:
    approval = db.get(m.Approval, approval_id)
    if approval is None:
        raise ApiError(404, "approval_not_found", f"no approval {approval_id}")
    task = get_task(db, approval.task_id)
    action = db.get(m.Action, approval.op_id)
    if approval.status != "PENDING":
        raise ApiError(409, "approval_not_pending", f"approval is {approval.status}")
    if approval.expires_at <= utcnow():
        # Expiry is a denial, never a release.
        approval.status, action.status = "EXPIRED", "REJECTED"
        resolve_waits(db, task.id, "APPROVAL")
        transition(db, task, S.DECLINED, actor="timer", reason="approval expired")
        db.commit()
        notify(task)
        raise ApiError(409, "approval_expired", "the approval expired and the task was declined")
    if user == task.requester:
        raise ApiError(403, "self_approval_forbidden", "the requester cannot approve their own task")
    if action.args_hash != approval.args_hash:
        approval.status = "VOID"
        db.commit()
        raise ApiError(409, "payload_changed", "the action changed after approval was requested")

    approval.approver, approval.reason, approval.decided_at = user, reason, utcnow()
    if decision == "approve":
        approval.status, action.status = "APPROVED", "APPROVED"
        transition(db, task, S.RUNNING, actor=user, reason="approval granted")
    else:
        # Until the harness can replan around a rejected action, a rejection ends the task.
        approval.status, action.status = "REJECTED", "REJECTED"
        transition(db, task, S.DECLINED, actor=user, reason=reason or "approval rejected")
    resolve_waits(db, task.id, "APPROVAL")
    db.add(m.AuditRecord(task_id=task.id, op_id=action.op_id, requester=task.requester,
                         acting_principal=task.acting_principal, approver=user,
                         decision=f"approval:{approval.status}", detail={"reason": reason}, at=utcnow()))
    log_event(db, task.id, "approval.decided", {"approval_id": approval.id, "status": approval.status, "by": user})
    db.commit()
    notify(task)
    return approval


def settle(db: Session, task: m.Task, *, user: str, decision: str, comment: str = "") -> None:
    if S(task.state) is not S.SETTLING:
        raise ApiError(409, "not_settling", "the task is not waiting for confirmation")
    if decision == "accept":
        delivery = db.get(m.Delivery, task.id)
        task.outcome_class = delivery.outcome_class if delivery else "complete"
        transition(db, task, S.COMPLETED, actor=user, reason=comment or "accepted")
    else:
        transition(db, task, S.RUNNING, actor=user, reason=comment or "returned for rework")
    resolve_waits(db, task.id, "REVIEW")
    log_event(db, task.id, "delivery.settled", {"by": user, "decision": decision, "comment": comment})
    db.commit()
    notify(task)


def cancel(db: Session, task: m.Task, *, user: str, reason: str = "") -> None:
    if user not in (task.requester, task.owner):
        raise ApiError(403, "not_allowed", "only the requester or the owner may cancel")
    transition(db, task, S.CANCELLED, actor=user, reason=reason or "cancelled")
    for approval in db.scalars(select(m.Approval).where(m.Approval.task_id == task.id,
                                                        m.Approval.status == "PENDING")):
        approval.status = "VOID"
    resolve_waits(db, task.id)
    db.commit()
    notify(task)


def scorecard(db: Session) -> dict:
    by_state = {state: count for state, count in
                db.execute(select(m.Task.state, func.count()).group_by(m.Task.state))}
    total_cost = float(db.scalar(select(func.coalesce(func.sum(m.CostEntry.amount), 0))))
    minutes = int(db.scalar(select(func.coalesce(func.sum(m.CostEntry.human_minutes), 0))))
    completed = by_state.get(S.COMPLETED, 0)
    approvals = {status: count for status, count in
                 db.execute(select(m.Approval.status, func.count()).group_by(m.Approval.status))}
    return {
        "tasks_by_state": by_state,
        "total_tasks": sum(by_state.values()),
        "completed": completed,
        "closed": sum(by_state.get(state, 0) for state in TERMINAL),
        "total_cost": round(total_cost, 4),
        "human_minutes": minutes,
        # Failed and handed-off attempts stay in the numerator; only completed tasks count below.
        "cost_per_successful_task": round(total_cost / completed, 4) if completed else None,
        "approvals_by_status": approvals,
    }
