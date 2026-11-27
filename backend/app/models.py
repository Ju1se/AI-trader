"""Records kept outside the model (docs/diagrams.md, 图 8).

Datetimes are stored as naive UTC so SQLite and PostgreSQL behave the same.
"""

from datetime import datetime
from decimal import Decimal

from sqlalchemy import ForeignKey, Numeric, String, Text, UniqueConstraint
from sqlalchemy.orm import Mapped, mapped_column

from .db import Base
from .db import JSONType as JSON


class TaskTypeProfile(Base):
    __tablename__ = "task_type_profile"
    id: Mapped[str] = mapped_column(String(64), primary_key=True)
    version: Mapped[int] = mapped_column(default=1)
    name: Mapped[str] = mapped_column(String(200))
    shape: Mapped[str] = mapped_column(String(32))  # single_call | fixed_workflow | single_agent | multi_agent
    tool_risk_tiers: Mapped[dict] = mapped_column(JSON, default=dict)
    limits: Mapped[dict] = mapped_column(JSON, default=dict)
    autonomy_level: Mapped[str] = mapped_column(String(8), default="L2")
    settle_window_hours: Mapped[int] = mapped_column(default=72)


class Task(Base):
    __tablename__ = "task"
    id: Mapped[str] = mapped_column(String(40), primary_key=True)
    profile_id: Mapped[str] = mapped_column(ForeignKey("task_type_profile.id"))
    dedupe_key: Mapped[str] = mapped_column(String(128), unique=True)
    title: Mapped[str] = mapped_column(String(300))
    request_text: Mapped[str] = mapped_column(Text, default="")
    state: Mapped[str] = mapped_column(String(32), index=True)
    state_version: Mapped[int] = mapped_column(default=1)
    requester: Mapped[str] = mapped_column(String(100))
    acting_principal: Mapped[str] = mapped_column(String(100))
    owner: Mapped[str] = mapped_column(String(100))
    outcome_class: Mapped[str | None] = mapped_column(String(32), default=None)
    due_at: Mapped[datetime | None] = mapped_column(default=None)
    created_at: Mapped[datetime]
    updated_at: Mapped[datetime]


class Authority(Base):
    __tablename__ = "authority"
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), primary_key=True)
    roles: Mapped[list] = mapped_column(JSON, default=list)
    data_scope: Mapped[dict] = mapped_column(JSON, default=dict)
    credential_ref: Mapped[str | None] = mapped_column(String(200), default=None)
    expires_at: Mapped[datetime | None] = mapped_column(default=None)


class TaskContract(Base):
    __tablename__ = "task_contract"
    __table_args__ = (UniqueConstraint("task_id", "version"),)
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    version: Mapped[int]
    goal: Mapped[str] = mapped_column(Text)
    success_criteria: Mapped[list] = mapped_column(JSON, default=list)
    assumptions: Mapped[list] = mapped_column(JSON, default=list)
    created_at: Mapped[datetime]


class Plan(Base):
    __tablename__ = "plan"
    __table_args__ = (UniqueConstraint("task_id", "version"),)
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    version: Mapped[int]
    steps: Mapped[list] = mapped_column(JSON, default=list)


class SessionEvent(Base):
    __tablename__ = "session_event"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    event_type: Mapped[str] = mapped_column(String(64))
    payload: Mapped[dict] = mapped_column(JSON, default=dict)
    at: Mapped[datetime]


class Wait(Base):
    __tablename__ = "wait"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    kind: Mapped[str] = mapped_column(String(16))  # INPUT | APPROVAL | REVIEW
    addressee: Mapped[str] = mapped_column(String(100))
    question: Mapped[dict] = mapped_column(JSON, default=dict)
    expires_at: Mapped[datetime]
    on_expiry: Mapped[str] = mapped_column(String(32))
    resolved_at: Mapped[datetime | None] = mapped_column(default=None)
    answer: Mapped[dict | None] = mapped_column(JSON, default=None)


class Action(Base):
    __tablename__ = "action"
    op_id: Mapped[str] = mapped_column(String(40), primary_key=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    tool: Mapped[str] = mapped_column(String(100))
    args: Mapped[dict] = mapped_column(JSON, default=dict)
    idempotency_key: Mapped[str] = mapped_column(String(128), unique=True)
    args_hash: Mapped[str] = mapped_column(String(64))
    risk_tier: Mapped[str] = mapped_column(String(4))
    # INTENDED | APPROVAL_PENDING | APPROVED | CONFIRMED | REJECTED | UNKNOWN
    status: Mapped[str] = mapped_column(String(24))
    external_ref: Mapped[str | None] = mapped_column(String(200), default=None)
    created_at: Mapped[datetime]


class Approval(Base):
    __tablename__ = "approval"
    id: Mapped[str] = mapped_column(String(40), primary_key=True)
    op_id: Mapped[str] = mapped_column(ForeignKey("action.op_id"))
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    args_hash: Mapped[str] = mapped_column(String(64))
    evidence: Mapped[dict] = mapped_column(JSON, default=dict)
    required_role: Mapped[str] = mapped_column(String(64), default="approver")
    # PENDING | APPROVED | REJECTED | EXPIRED | VOID
    status: Mapped[str] = mapped_column(String(16), index=True)
    approver: Mapped[str | None] = mapped_column(String(100), default=None)
    reason: Mapped[str] = mapped_column(Text, default="")
    created_at: Mapped[datetime]
    expires_at: Mapped[datetime]
    decided_at: Mapped[datetime | None] = mapped_column(default=None)


class AuditRecord(Base):
    __tablename__ = "audit_record"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    op_id: Mapped[str | None] = mapped_column(String(40), default=None)
    requester: Mapped[str] = mapped_column(String(100))
    acting_principal: Mapped[str] = mapped_column(String(100))
    approver: Mapped[str | None] = mapped_column(String(100), default=None)
    decision: Mapped[str] = mapped_column(String(64))
    rule_version: Mapped[str] = mapped_column(String(32), default="v0")
    detail: Mapped[dict] = mapped_column(JSON, default=dict)
    at: Mapped[datetime]


class Verification(Base):
    __tablename__ = "verification"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    criterion: Mapped[str] = mapped_column(Text)
    verdict: Mapped[str] = mapped_column(String(16))  # passed | unmet
    evidence: Mapped[dict] = mapped_column(JSON, default=dict)
    at: Mapped[datetime]


class Delivery(Base):
    __tablename__ = "delivery"
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), primary_key=True)
    outcome_class: Mapped[str] = mapped_column(String(32))  # complete | partial
    manifest: Mapped[dict] = mapped_column(JSON, default=dict)
    delivered_at: Mapped[datetime]


class CostEntry(Base):
    __tablename__ = "cost_entry"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    task_id: Mapped[str] = mapped_column(ForeignKey("task.id"), index=True)
    stage: Mapped[str] = mapped_column(String(32))
    kind: Mapped[str] = mapped_column(String(32))  # model | tool | grader | human
    amount: Mapped[Decimal] = mapped_column(Numeric(12, 4), default=Decimal("0"))
    human_minutes: Mapped[int] = mapped_column(default=0)
    at: Mapped[datetime]
