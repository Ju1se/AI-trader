"""Request and response shapes of the HTTP API. These drive the generated OpenAPI document."""

from datetime import UTC, datetime
from typing import Annotated, Any, Literal

from pydantic import BaseModel, ConfigDict, Field, PlainSerializer


def _iso_z(value: datetime) -> str:
    return value.replace(tzinfo=UTC).isoformat().replace("+00:00", "Z")


# Stored as naive UTC; always sent to clients as ISO 8601 with a trailing Z.
UtcDatetime = Annotated[datetime, PlainSerializer(_iso_z, return_type=str)]


class Orm(BaseModel):
    model_config = ConfigDict(from_attributes=True)


class ErrorDetail(BaseModel):
    code: str = Field(examples=["invalid_transition"])
    message: str


class ErrorBody(BaseModel):
    error: ErrorDetail


class ProfileOut(Orm):
    id: str
    version: int
    name: str
    shape: str = Field(description="single_call | fixed_workflow | single_agent | multi_agent")
    tool_risk_tiers: dict[str, str] = Field(description="工具名到风险级别 R0–R4 的映射")
    limits: dict[str, Any]
    autonomy_level: str
    settle_window_hours: int


class StateInfo(BaseModel):
    state: str
    label: str = Field(description="给人看的中文名")
    owner_role: str | None = Field(description="任务停在这个状态时,轮到谁行动")
    terminal: bool
    next: list[str] = Field(description="允许迁移到的状态")


class TaskCreate(BaseModel):
    profile_id: str = Field(examples=["research_paper_order"])
    title: str = Field(min_length=1, max_length=300, examples=["研究 ACME 第三季度财报并给出要点"])
    request_text: str = Field(default="", examples=["只用公司公告和官方披露,列出收入、毛利率、现金流三项的变化。"])
    dedupe_key: str | None = Field(
        default=None,
        description="触发事件自带的稳定标识。不传时由任务类型、发起人、标题和请求内容算出。相同的键只会建一个任务。",
    )
    due_at: datetime | None = None


class TaskSummary(BaseModel):
    id: str
    profile_id: str
    title: str
    state: str
    state_label: str
    owner_role: str | None
    requester: str
    owner: str
    outcome_class: str | None
    due_at: UtcDatetime | None
    created_at: UtcDatetime
    updated_at: UtcDatetime


class TaskCreated(BaseModel):
    task: TaskSummary
    created: bool = Field(description="false 表示去重命中,返回的是已有任务")


class TaskList(BaseModel):
    items: list[TaskSummary]
    total: int


class ContractOut(Orm):
    version: int
    goal: str
    success_criteria: list[Any]
    assumptions: list[Any]


class PlanOut(Orm):
    version: int
    steps: list[Any]


class WaitOut(Orm):
    id: int
    kind: str = Field(description="INPUT | APPROVAL | REVIEW")
    addressee: str
    question: dict[str, Any]
    expires_at: UtcDatetime
    on_expiry: str


class DeliveryOut(Orm):
    outcome_class: str = Field(description="complete | partial")
    manifest: dict[str, Any]
    delivered_at: UtcDatetime


class TaskDetail(TaskSummary):
    request_text: str
    acting_principal: str
    state_version: int
    allowed_next_states: list[str]
    contract: ContractOut | None
    plan: PlanOut | None
    open_wait: WaitOut | None
    delivery: DeliveryOut | None


class EventOut(Orm):
    id: int
    event_type: str
    payload: dict[str, Any]
    at: UtcDatetime


class ActionOut(Orm):
    op_id: str
    task_id: str
    tool: str
    args: dict[str, Any]
    idempotency_key: str
    args_hash: str
    risk_tier: str
    status: str = Field(description="INTENDED | APPROVAL_PENDING | APPROVED | CONFIRMED | REJECTED | UNKNOWN")
    external_ref: str | None
    created_at: UtcDatetime


class ApprovalOut(Orm):
    id: str
    op_id: str
    task_id: str
    args_hash: str
    evidence: dict[str, Any] = Field(description="由代码整理的证据包:确切内容、触发规则、差异")
    required_role: str
    status: str = Field(description="PENDING | APPROVED | REJECTED | EXPIRED | VOID")
    approver: str | None
    reason: str
    created_at: UtcDatetime
    expires_at: UtcDatetime
    decided_at: UtcDatetime | None


class ApprovalList(BaseModel):
    items: list[ApprovalOut]
    total: int


class ApprovalDecisionIn(BaseModel):
    decision: Literal["approve", "reject"]
    reason: str = ""


class AnswerIn(BaseModel):
    answer: dict[str, Any] = Field(examples=[{"choice": "只用官方披露"}])


class SettleIn(BaseModel):
    decision: Literal["accept", "return"] = Field(description="accept 结案;return 退回返工")
    comment: str = ""


class CancelIn(BaseModel):
    reason: str = ""


class Scorecard(BaseModel):
    tasks_by_state: dict[str, int]
    total_tasks: int
    completed: int
    closed: int = Field(description="处于任一终止状态的任务数")
    total_cost: float = Field(description="所有任务的成本合计,失败和转人工的也算")
    human_minutes: int
    cost_per_successful_task: float | None = Field(description="total_cost ÷ completed;没有已完成任务时为 null")
    approvals_by_status: dict[str, int]


SimStep = Literal[
    "start", "ask", "request_approval", "unknown_outcome", "resolve_unknown", "limit",
    "extend_limit", "claim_done", "rework", "deliver", "fail", "hand_off",
]


class SimulateIn(BaseModel):
    step: SimStep
    partial: bool = Field(default=False, description="只对 deliver 有效:按部分交付处理")
