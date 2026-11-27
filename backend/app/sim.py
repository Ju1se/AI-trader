"""Dev-only helpers: drive a task through the state machine without an agent, and load demo data.

The harness is not implemented yet. These steps stand in for it so the frontend can see
every state and every record. All company names and figures here are made up.
"""

from datetime import timedelta

from sqlalchemy import delete, select
from sqlalchemy.orm import Session

from . import models as m
from . import services as svc
from .config import settings
from .domain import TaskState as S

DEMO_PROFILE = {
    "id": "research_paper_order",
    "version": 1,
    "name": "研究并在模拟盘下单",
    "shape": "single_agent",
    "tool_risk_tiers": {"get_quote": "R0", "search_filings": "R0", "write_report": "R1",
                        "submit_paper_order": "R2", "create_order_request": "R3"},
    "limits": {"max_turns": 40, "max_spend_usd": 5, "max_wall_clock_minutes": 60,
               "max_denials": 3, "max_revisions": 3},
    "autonomy_level": "L2",
    "settle_window_hours": 72,
}

_DELETE_ORDER = [m.CostEntry, m.Verification, m.Delivery, m.AuditRecord, m.Approval, m.Action, m.Wait,
                 m.SessionEvent, m.Plan, m.TaskContract, m.Authority, m.Task, m.TaskTypeProfile]


def _ensure_contract_and_plan(db: Session, task: m.Task) -> None:
    if db.scalar(select(m.TaskContract).where(m.TaskContract.task_id == task.id)) is not None:
        return
    db.add(m.TaskContract(
        task_id=task.id, version=1, goal=task.title,
        success_criteria=[
            {"text": "每个数字都引用官方披露", "check": "citation_audit"},
            {"text": "报告已写入任务的交付位置", "check": "delivery_exists"},
            {"text": "模拟盘订单状态与请求一致", "check": "order_readback"},
        ],
        assumptions=["币种按披露原币种"], created_at=svc.utcnow()))
    db.add(m.Plan(task_id=task.id, version=1, steps=[
        {"id": "s1", "title": "检索官方披露", "reversible": True, "status": "pending"},
        {"id": "s2", "title": "提取并核对数字", "reversible": True, "status": "pending"},
        {"id": "s3", "title": "写报告", "reversible": True, "status": "pending"},
        {"id": "s4", "title": "创建下单请求", "reversible": False, "status": "pending"},
    ]))


# The state each step may start from. Steps not listed rely on the transition rules alone.
_REQUIRES = {
    "start": {S.QUEUED}, "ask": {S.RUNNING}, "request_approval": {S.RUNNING},
    "unknown_outcome": {S.RUNNING}, "resolve_unknown": {S.OUTCOME_UNKNOWN}, "limit": {S.RUNNING},
    "extend_limit": {S.LIMIT_REACHED}, "claim_done": {S.RUNNING, S.LIMIT_REACHED},
    "rework": {S.VERIFYING}, "deliver": {S.VERIFYING},
}


def simulate(db: Session, task: m.Task, step: str, partial: bool = False) -> None:
    allowed = _REQUIRES.get(step)
    if allowed and S(task.state) not in allowed:
        raise svc.ApiError(409, "invalid_transition", f"step {step} is not allowed in state {task.state}")
    now = svc.utcnow()
    if step == "start":
        svc.transition(db, task, S.RUNNING, actor="harness", reason="claimed")
        _ensure_contract_and_plan(db, task)
        svc.add_cost(db, task.id, "frame", "model", "0.0420")
    elif step == "ask":
        svc.transition(db, task, S.AWAITING_INPUT, actor="harness", reason="clarification needed")
        question = {"text": "资料范围只用官方披露,还是可以包含媒体报道?",
                    "options": ["只用官方披露", "可以包含媒体报道"], "default": "只用官方披露"}
        db.add(m.Wait(task_id=task.id, kind="INPUT", addressee=task.requester, question=question,
                      expires_at=now + timedelta(hours=settings.input_ttl_hours), on_expiry="hand_off"))
        svc.log_event(db, task.id, "input.requested", question)
    elif step == "request_approval":
        svc.transition(db, task, S.AWAITING_APPROVAL, actor="gate", reason="tier R3 needs a human approval")
        args = {"symbol": "ACME", "side": "buy", "quantity": 100, "order_type": "limit",
                "limit_price": "12.50", "venue": "paper"}
        op_id, digest = svc.new_id("op"), svc.hash_call("create_order_request", args)
        db.add(m.Action(op_id=op_id, task_id=task.id, tool="create_order_request", args=args,
                        idempotency_key=f"{task.id}:{op_id}", args_hash=digest, risk_tier="R3",
                        status="APPROVAL_PENDING", created_at=now))
        db.flush()  # the action row must exist before the approval that references it
        approval = m.Approval(
            id=svc.new_id("apr"), op_id=op_id, task_id=task.id, args_hash=digest, status="PENDING",
            evidence={"payload": args, "rule": "create_order_request 属于 R3,必须人工批准",
                      "estimated_notional": "1250.00", "position_before": 0, "position_after": 100,
                      "basis": ["ACME 第三季度公告", "研究报告草稿 v1"],
                      "model_note": {"text": "限价低于最近成交价", "trusted": False}},
            created_at=now, expires_at=now + timedelta(hours=settings.approval_ttl_hours))
        db.add(approval)
        db.add(m.Wait(task_id=task.id, kind="APPROVAL", addressee="approver",
                      question={"approval_id": approval.id}, expires_at=approval.expires_at, on_expiry="decline"))
        db.add(m.AuditRecord(task_id=task.id, op_id=op_id, requester=task.requester,
                             acting_principal=task.acting_principal, decision="gate:ask",
                             detail={"check": 5, "tier": "R3"}, at=now))
        svc.log_event(db, task.id, "approval.requested", {"approval_id": approval.id, "op_id": op_id})
    elif step == "unknown_outcome":
        svc.transition(db, task, S.OUTCOME_UNKNOWN, actor="harness", reason="no definite result after dispatch")
        action = db.scalar(select(m.Action).where(m.Action.task_id == task.id, m.Action.status == "APPROVED"))
        if action is None:
            args = {"symbol": "ACME", "side": "sell", "quantity": 50, "venue": "paper"}
            op_id = svc.new_id("op")
            action = m.Action(op_id=op_id, task_id=task.id, tool="submit_paper_order", args=args,
                              idempotency_key=f"{task.id}:{op_id}", risk_tier="R2", status="INTENDED",
                              args_hash=svc.hash_call("submit_paper_order", args), created_at=now)
            db.add(action)
        action.status = "UNKNOWN"
        svc.log_event(db, task.id, "action.outcome_unknown", {"op_id": action.op_id})
    elif step == "resolve_unknown":
        for action in db.scalars(select(m.Action).where(m.Action.task_id == task.id, m.Action.status == "UNKNOWN")):
            action.status, action.external_ref = "CONFIRMED", f"paper-{action.op_id[-6:]}"
        svc.transition(db, task, S.RUNNING, actor="reconciler", reason="found by idempotency key")
    elif step == "limit":
        svc.transition(db, task, S.LIMIT_REACHED, actor="harness", reason="spend limit reached")
    elif step == "extend_limit":
        svc.transition(db, task, S.RUNNING, actor="owner", reason="limit extended")
    elif step == "claim_done":
        svc.transition(db, task, S.VERIFYING, actor="harness", reason="model proposed no further action")
    elif step == "rework":
        svc.transition(db, task, S.RUNNING, actor="verifier", reason="a criterion is unmet")
    elif step == "deliver":
        svc.transition(db, task, S.SETTLING, actor="verifier", reason="verified and delivered")
        contract = db.scalar(select(m.TaskContract).where(m.TaskContract.task_id == task.id)
                             .order_by(m.TaskContract.version.desc()))
        criteria = [c["text"] for c in contract.success_criteria] if contract else ["完成请求"]
        unmet = criteria[-1:] if partial else []
        for text in criteria:
            db.add(m.Verification(task_id=task.id, criterion=text, at=now,
                                  verdict="unmet" if text in unmet else "passed",
                                  evidence={"checker": "code", "detail": "演示数据"}))
        manifest = {"summary": "演示交付:研究要点三条,引用两份公告。", "criteria_met": len(criteria) - len(unmet),
                    "criteria_total": len(criteria), "gaps": unmet, "assumptions": ["币种按披露原币种"],
                    "artifacts": [{"name": "report.md", "kind": "report"}], "next_action": "请验收"}
        delivery = db.get(m.Delivery, task.id) or m.Delivery(task_id=task.id)
        delivery.outcome_class = "partial" if partial else "complete"
        delivery.manifest, delivery.delivered_at = manifest, now
        db.add(delivery)
        db.add(m.Wait(task_id=task.id, kind="REVIEW", addressee=task.requester, question={},
                      expires_at=now + timedelta(hours=DEMO_PROFILE["settle_window_hours"]), on_expiry="complete"))
        svc.add_cost(db, task.id, "verify", "grader", "0.0110")
    elif step == "fail":
        svc.transition(db, task, S.FAILED, actor="harness", reason="unrecoverable error")
    elif step == "hand_off":
        svc.transition(db, task, S.HANDED_OFF, actor="harness", reason="escalation rule hit")
        svc.log_event(db, task.id, "handoff.package", {
            "reason": "需要人工判断", "done": ["检索官方披露"], "not_done": ["创建下单请求"],
            "open_questions": ["是否接受媒体报道作为依据"]})
    db.commit()
    svc.notify(task)


def load(db: Session, reset: bool = True) -> dict:
    """Load demo data: one task type and one task in each state the frontend needs to render."""
    if reset:
        for table in _DELETE_ORDER:
            db.execute(delete(table))
        db.commit()
    if db.get(m.TaskTypeProfile, DEMO_PROFILE["id"]) is None:
        db.add(m.TaskTypeProfile(**DEMO_PROFILE))
        db.commit()

    demo = [
        ("研究 ACME 第三季度财报并给出要点", ["start"]),
        ("核对 ACME 与同业的毛利率差异", ["start", "ask"]),
        ("ACME 模拟盘买入 100 股,限价 12.50", ["start", "request_approval"]),
        ("ACME 模拟盘卖出 50 股", ["start", "request_approval", "approve", "unknown_outcome"]),
        ("汇总 ACME 近三年现金流", ["start", "limit"]),
        ("整理 ACME 风险因素清单", ["start", "claim_done", "deliver"]),
        ("ACME 估值区间研究", ["start", "claim_done", "deliver", "accept"]),
        ("ACME 管理层变动梳理", []),
    ]
    for index, (title, steps) in enumerate(demo):
        task, _ = svc.create_task(db, profile_id=DEMO_PROFILE["id"], title=title, requester="alice",
                                  dedupe_key=f"demo:{index}",
                                  request_text="只用公司公告和官方披露。演示数据,公司和数字均为虚构。")
        for step in steps:
            if step == "approve":
                approval = db.scalar(select(m.Approval).where(m.Approval.task_id == task.id,
                                                              m.Approval.status == "PENDING"))
                svc.decide_approval(db, approval.id, user="bob", decision="approve", reason="演示批准")
                svc.add_cost(db, task.id, "approve", "human", human_minutes=3)
                db.commit()
            elif step == "accept":
                svc.settle(db, task, user="bob", decision="accept", comment="演示验收")
                svc.add_cost(db, task.id, "settle", "human", human_minutes=8)
                db.commit()
            else:
                simulate(db, task, step)
    return {"profiles": 1, "tasks": len(demo)}
