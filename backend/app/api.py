"""HTTP API. Paths under /v1 are the contract with the frontend; /v1/dev is for integration testing only."""

import asyncio
import json
from typing import Annotated

from fastapi import APIRouter, Depends, Header, Query, Request, Response
from fastapi.responses import StreamingResponse
from sqlalchemy import func, select
from sqlalchemy.orm import Session

from . import models as m
from . import schemas as s
from . import services as svc
from . import sim
from .db import get_db
from .domain import STATE_INFO, TERMINAL, allowed_next
from .domain import TaskState as S
from .events import bus
from .services import ApiError

ERRORS = {code: {"model": s.ErrorBody} for code in (401, 403, 404, 409)}

router = APIRouter(responses=ERRORS)
dev_router = APIRouter(prefix="/v1/dev", tags=["开发联调"], responses=ERRORS)


def current_user(
    x_user_id: Annotated[str | None, Header(description="开发期的身份占位:直接写用户名,例如 alice。正式登录接入后替换。")] = None,
) -> str:
    if not x_user_id:
        raise ApiError(401, "missing_identity", "send the X-User-Id header")
    return x_user_id


Db = Annotated[Session, Depends(get_db)]
User = Annotated[str, Depends(current_user)]


def _summary_fields(task: m.Task) -> dict:
    info = STATE_INFO[S(task.state)]
    return dict(id=task.id, profile_id=task.profile_id, title=task.title, state=task.state,
                state_label=info["label"], owner_role=info["owner_role"], requester=task.requester,
                owner=task.owner, outcome_class=task.outcome_class, due_at=task.due_at,
                created_at=task.created_at, updated_at=task.updated_at)


def _detail(db: Session, task: m.Task) -> s.TaskDetail:
    contract = db.scalar(select(m.TaskContract).where(m.TaskContract.task_id == task.id)
                         .order_by(m.TaskContract.version.desc()))
    plan = db.scalar(select(m.Plan).where(m.Plan.task_id == task.id).order_by(m.Plan.version.desc()))
    wait = svc.open_wait(db, task.id)
    delivery = db.get(m.Delivery, task.id)
    return s.TaskDetail(
        **_summary_fields(task), request_text=task.request_text, acting_principal=task.acting_principal,
        state_version=task.state_version, allowed_next_states=sorted(allowed_next(S(task.state))),
        contract=s.ContractOut.model_validate(contract) if contract else None,
        plan=s.PlanOut.model_validate(plan) if plan else None,
        open_wait=s.WaitOut.model_validate(wait) if wait else None,
        delivery=s.DeliveryOut.model_validate(delivery) if delivery else None)


@router.get("/health", tags=["元数据"], summary="健康检查")
def health() -> dict:
    return {"status": "ok"}


@router.get("/v1/meta/states", tags=["元数据"], response_model=list[s.StateInfo], summary="任务状态表")
def list_states() -> list[s.StateInfo]:
    """13 个任务状态的中文名、此刻该谁行动、允许迁移到哪些状态。前端用它渲染看板列和状态标签。"""
    return [s.StateInfo(state=state, label=info["label"], owner_role=info["owner_role"],
                        terminal=state in TERMINAL, next=sorted(allowed_next(state)))
            for state, info in STATE_INFO.items()]


@router.get("/v1/profiles", tags=["元数据"], response_model=list[s.ProfileOut], summary="任务类型配置")
def list_profiles(db: Db) -> list[m.TaskTypeProfile]:
    return list(db.scalars(select(m.TaskTypeProfile).order_by(m.TaskTypeProfile.id)))


@router.post("/v1/tasks", tags=["任务"], response_model=s.TaskCreated, status_code=201, summary="创建任务")
def create_task(body: s.TaskCreate, response: Response, db: Db, user: User) -> s.TaskCreated:
    """受理一个请求。相同的去重键只建一个任务:第二次调用返回 200 和已有任务,`created` 为 false。"""
    task, created = svc.create_task(db, profile_id=body.profile_id, title=body.title,
                                    request_text=body.request_text, requester=user,
                                    dedupe_key=body.dedupe_key, due_at=body.due_at)
    if not created:
        response.status_code = 200
    return s.TaskCreated(task=s.TaskSummary(**_summary_fields(task)), created=created)


@router.get("/v1/tasks", tags=["任务"], response_model=s.TaskList, summary="任务列表")
def list_tasks(db: Db, user: User,
               state: Annotated[S | None, Query(description="只看某个状态")] = None,
               requester: Annotated[str | None, Query(description="只看某个发起人")] = None,
               limit: Annotated[int, Query(ge=1, le=200)] = 50,
               offset: Annotated[int, Query(ge=0)] = 0) -> s.TaskList:
    query = select(m.Task)
    if state:
        query = query.where(m.Task.state == state)
    if requester:
        query = query.where(m.Task.requester == requester)
    total = db.scalar(select(func.count()).select_from(query.subquery()))
    rows = db.scalars(query.order_by(m.Task.updated_at.desc(), m.Task.id).limit(limit).offset(offset))
    return s.TaskList(items=[s.TaskSummary(**_summary_fields(t)) for t in rows], total=total)


@router.get("/v1/tasks/{task_id}", tags=["任务"], response_model=s.TaskDetail, summary="任务详情")
def get_task(task_id: str, db: Db, user: User) -> s.TaskDetail:
    """任务本身,加上最新的合约、计划、未解决的等待项和交付清单。"""
    return _detail(db, svc.get_task(db, task_id))


@router.get("/v1/tasks/{task_id}/events", tags=["任务"], response_model=list[s.EventOut], summary="任务时间线")
def list_events(task_id: str, db: Db, user: User) -> list[m.SessionEvent]:
    svc.get_task(db, task_id)
    return list(db.scalars(select(m.SessionEvent).where(m.SessionEvent.task_id == task_id)
                           .order_by(m.SessionEvent.id)))


@router.get("/v1/tasks/{task_id}/actions", tags=["任务"], response_model=list[s.ActionOut], summary="任务的动作记录")
def list_actions(task_id: str, db: Db, user: User) -> list[m.Action]:
    svc.get_task(db, task_id)
    return list(db.scalars(select(m.Action).where(m.Action.task_id == task_id).order_by(m.Action.created_at)))


@router.post("/v1/tasks/{task_id}/answer", tags=["任务"], response_model=s.TaskDetail, summary="回答澄清问题")
def answer(task_id: str, body: s.AnswerIn, db: Db, user: User) -> s.TaskDetail:
    """任务处于 AWAITING_INPUT 时,由发起人回答。成功后任务回到 RUNNING。"""
    task = svc.get_task(db, task_id)
    svc.answer_input(db, task, user=user, answer=body.answer)
    return _detail(db, task)


@router.post("/v1/tasks/{task_id}/settle", tags=["任务"], response_model=s.TaskDetail, summary="验收或退回")
def settle(task_id: str, body: s.SettleIn, db: Db, user: User) -> s.TaskDetail:
    """任务处于 SETTLING 时调用。accept 结案为 COMPLETED;return 退回 RUNNING 返工。"""
    task = svc.get_task(db, task_id)
    svc.settle(db, task, user=user, decision=body.decision, comment=body.comment)
    return _detail(db, task)


@router.post("/v1/tasks/{task_id}/cancel", tags=["任务"], response_model=s.TaskDetail, summary="取消任务")
def cancel(task_id: str, body: s.CancelIn, db: Db, user: User) -> s.TaskDetail:
    """只有发起人或负责人可以取消。结果未知的任务要先对账,不能直接取消。"""
    task = svc.get_task(db, task_id)
    svc.cancel(db, task, user=user, reason=body.reason)
    return _detail(db, task)


@router.get("/v1/approvals", tags=["批准"], response_model=s.ApprovalList, summary="批准收件箱")
def list_approvals(db: Db, user: User,
                   status: Annotated[str | None, Query(description="PENDING | APPROVED | REJECTED | EXPIRED | VOID")] = None,
                   limit: Annotated[int, Query(ge=1, le=200)] = 50,
                   offset: Annotated[int, Query(ge=0)] = 0) -> s.ApprovalList:
    query = select(m.Approval)
    if status:
        query = query.where(m.Approval.status == status)
    total = db.scalar(select(func.count()).select_from(query.subquery()))
    rows = db.scalars(query.order_by(m.Approval.created_at.desc()).limit(limit).offset(offset))
    return s.ApprovalList(items=list(rows), total=total)


@router.get("/v1/approvals/{approval_id}", tags=["批准"], response_model=s.ApprovalOut, summary="批准详情")
def get_approval(approval_id: str, db: Db, user: User) -> m.Approval:
    approval = db.get(m.Approval, approval_id)
    if approval is None:
        raise ApiError(404, "approval_not_found", f"no approval {approval_id}")
    return approval


@router.post("/v1/approvals/{approval_id}/decision", tags=["批准"], response_model=s.ApprovalOut, summary="批准或否决")
def decide(approval_id: str, body: s.ApprovalDecisionIn, db: Db, user: User) -> m.Approval:
    """批准人不能是任务的发起人。批准后任务回到 RUNNING;否决或过期后任务变为 DECLINED。"""
    return svc.decide_approval(db, approval_id, user=user, decision=body.decision, reason=body.reason)


@router.get("/v1/metrics/scorecard", tags=["指标"], response_model=s.Scorecard, summary="记分卡")
def get_scorecard(db: Db, user: User) -> dict:
    return svc.scorecard(db)


@router.get("/v1/stream", tags=["实时"], summary="任务变化的事件流(SSE)", response_class=StreamingResponse,
            responses={200: {"content": {"text/event-stream": {}}, "description": "事件流,每 15 秒一条心跳注释"}})
async def stream(request: Request) -> StreamingResponse:
    """事件名是 `task.created` 或 `task.updated`,数据是 `{type, task_id, state, state_version}`。收到后重新取任务详情。"""

    async def events():
        queue = bus.subscribe()
        try:
            yield ": connected\n\n"
            while not await request.is_disconnected():
                try:
                    event = await asyncio.wait_for(queue.get(), timeout=15)
                    yield f"event: {event['type']}\ndata: {json.dumps(event, ensure_ascii=False)}\n\n"
                except TimeoutError:
                    yield ": ping\n\n"
        finally:
            bus.unsubscribe(queue)

    return StreamingResponse(events(), media_type="text/event-stream", headers={"Cache-Control": "no-cache"})


@dev_router.post("/seed", summary="重置并载入演示数据")
def seed(db: Db) -> dict:
    """清空所有表,载入一个任务类型和八个处于不同状态的任务。发起人是 alice,批准人用 bob。"""
    return sim.load(db, reset=True)


@dev_router.post("/tasks/{task_id}/simulate", response_model=s.TaskDetail, summary="模拟 Agent 的一步")
def simulate(task_id: str, body: s.SimulateIn, db: Db) -> s.TaskDetail:
    """Agent 还没接入,用这个接口代替它推进任务,并生成对应的记录(等待项、动作、批准、交付)。"""
    task = svc.get_task(db, task_id)
    sim.simulate(db, task, body.step, partial=body.partial)
    return _detail(db, task)
