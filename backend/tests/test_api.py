from datetime import timedelta

from app import models, services
from app.db import SessionLocal

from .conftest import ALICE, BOB, step

NEW_TASK = {"profile_id": "research_paper_order", "title": "研究 ACME", "request_text": "只用官方披露"}


def test_health_and_identity(client):
    assert client.get("/health").json() == {"status": "ok"}
    response = client.get("/v1/tasks")
    assert response.status_code == 401
    assert response.json()["error"]["code"] == "missing_identity"


def test_states_metadata_lists_all_thirteen(client):
    states = client.get("/v1/meta/states").json()
    assert len(states) == 13
    queued = next(s for s in states if s["state"] == "QUEUED")
    assert queued["label"] == "排队" and "RUNNING" in queued["next"]


def test_create_task_is_deduplicated(client):
    first = client.post("/v1/tasks", json=NEW_TASK, headers=ALICE)
    second = client.post("/v1/tasks", json=NEW_TASK, headers=ALICE)
    assert (first.status_code, second.status_code) == (201, 200)
    assert first.json()["created"] is True and second.json()["created"] is False
    assert first.json()["task"]["id"] == second.json()["task"]["id"]
    assert client.get("/v1/tasks", headers=ALICE).json()["total"] == 1


def test_unknown_profile_and_task_return_404(client):
    response = client.post("/v1/tasks", json={**NEW_TASK, "profile_id": "nope"}, headers=ALICE)
    assert (response.status_code, response.json()["error"]["code"]) == (404, "profile_not_found")
    assert client.get("/v1/tasks/task_missing", headers=ALICE).status_code == 404


def test_timestamps_are_utc_with_z(client, task_id):
    assert client.get(f"/v1/tasks/{task_id}", headers=ALICE).json()["created_at"].endswith("Z")


def test_full_path_with_approval_delivery_and_settlement(client, task_id):
    assert step(client, task_id, "start")["state"] == "RUNNING"
    detail = step(client, task_id, "request_approval")
    assert detail["state"] == "AWAITING_APPROVAL" and detail["open_wait"]["kind"] == "APPROVAL"

    pending = client.get("/v1/approvals", params={"status": "PENDING"}, headers=BOB).json()
    assert pending["total"] == 1
    approval = pending["items"][0]
    assert approval["evidence"]["payload"]["symbol"] == "ACME"

    own = client.post(f"/v1/approvals/{approval['id']}/decision", json={"decision": "approve"}, headers=ALICE)
    assert (own.status_code, own.json()["error"]["code"]) == (403, "self_approval_forbidden")

    decided = client.post(f"/v1/approvals/{approval['id']}/decision", json={"decision": "approve"}, headers=BOB)
    assert decided.status_code == 200 and decided.json()["status"] == "APPROVED"
    again = client.post(f"/v1/approvals/{approval['id']}/decision", json={"decision": "approve"}, headers=BOB)
    assert again.json()["error"]["code"] == "approval_not_pending"

    actions = client.get(f"/v1/tasks/{task_id}/actions", headers=ALICE).json()
    assert actions[0]["status"] == "APPROVED" and actions[0]["risk_tier"] == "R3"

    step(client, task_id, "claim_done")
    delivered = step(client, task_id, "deliver")
    assert delivered["state"] == "SETTLING" and delivered["delivery"]["outcome_class"] == "complete"

    done = client.post(f"/v1/tasks/{task_id}/settle", json={"decision": "accept"}, headers=BOB).json()
    assert done["state"] == "COMPLETED" and done["outcome_class"] == "complete"

    events = [e["event_type"] for e in client.get(f"/v1/tasks/{task_id}/events", headers=ALICE).json()]
    assert events[0] == "task.created" and "approval.decided" in events and "delivery.settled" in events

    card = client.get("/v1/metrics/scorecard", headers=ALICE).json()
    assert card["completed"] == 1 and card["cost_per_successful_task"] > 0


def test_rejection_declines_the_task(client, task_id):
    step(client, task_id, "start")
    step(client, task_id, "request_approval")
    approval = client.get("/v1/approvals", headers=BOB).json()["items"][0]
    client.post(f"/v1/approvals/{approval['id']}/decision", json={"decision": "reject", "reason": "超出额度"},
                headers=BOB)
    assert client.get(f"/v1/tasks/{task_id}", headers=ALICE).json()["state"] == "DECLINED"


def test_expired_approval_is_a_denial(client, task_id):
    step(client, task_id, "start")
    step(client, task_id, "request_approval")
    with SessionLocal() as db:
        approval = db.query(models.Approval).one()
        approval.expires_at = services.utcnow() - timedelta(minutes=1)
        db.commit()
        approval_id = approval.id
    response = client.post(f"/v1/approvals/{approval_id}/decision", json={"decision": "approve"}, headers=BOB)
    assert (response.status_code, response.json()["error"]["code"]) == (409, "approval_expired")
    assert client.get(f"/v1/tasks/{task_id}", headers=ALICE).json()["state"] == "DECLINED"


def test_only_the_requester_answers_a_question(client, task_id):
    step(client, task_id, "start")
    waiting = step(client, task_id, "ask")
    assert waiting["state"] == "AWAITING_INPUT" and waiting["open_wait"]["question"]["options"]
    body = {"answer": {"choice": "只用官方披露"}}
    assert client.post(f"/v1/tasks/{task_id}/answer", json=body, headers=BOB).status_code == 403
    answered = client.post(f"/v1/tasks/{task_id}/answer", json=body, headers=ALICE).json()
    assert answered["state"] == "RUNNING" and answered["open_wait"] is None


def test_illegal_moves_are_rejected(client, task_id):
    response = client.post(f"/v1/dev/tasks/{task_id}/simulate", json={"step": "deliver"})
    assert (response.status_code, response.json()["error"]["code"]) == (409, "invalid_transition")
    settle = client.post(f"/v1/tasks/{task_id}/settle", json={"decision": "accept"}, headers=BOB)
    assert settle.json()["error"]["code"] == "not_settling"


def test_unknown_outcome_blocks_failure_and_cancel_until_reconciled(client, task_id):
    step(client, task_id, "start")
    assert step(client, task_id, "unknown_outcome")["state"] == "OUTCOME_UNKNOWN"
    cancel = client.post(f"/v1/tasks/{task_id}/cancel", json={}, headers=ALICE)
    assert cancel.json()["error"]["code"] == "invalid_transition"
    assert step(client, task_id, "resolve_unknown")["state"] == "RUNNING"
    actions = client.get(f"/v1/tasks/{task_id}/actions", headers=ALICE).json()
    assert actions[0]["status"] == "CONFIRMED" and actions[0]["external_ref"]


def test_partial_delivery_and_return_for_rework(client, task_id):
    step(client, task_id, "start")
    step(client, task_id, "limit")
    step(client, task_id, "claim_done")
    partial = step(client, task_id, "deliver", partial=True)
    assert partial["delivery"]["outcome_class"] == "partial" and partial["delivery"]["manifest"]["gaps"]
    back = client.post(f"/v1/tasks/{task_id}/settle", json={"decision": "return", "comment": "补上缺口"},
                       headers=ALICE).json()
    assert back["state"] == "RUNNING"


def test_cancel_is_limited_to_requester_or_owner(client, task_id):
    assert client.post(f"/v1/tasks/{task_id}/cancel", json={}, headers=BOB).status_code == 403
    assert client.post(f"/v1/tasks/{task_id}/cancel", json={}, headers=ALICE).json()["state"] == "CANCELLED"


def test_seed_loads_one_task_per_demo_state(client):
    assert client.post("/v1/dev/seed").json() == {"profiles": 1, "tasks": 8}
    states = {t["state"] for t in client.get("/v1/tasks", headers=ALICE).json()["items"]}
    assert states == {"QUEUED", "RUNNING", "AWAITING_INPUT", "AWAITING_APPROVAL", "OUTCOME_UNKNOWN",
                      "LIMIT_REACHED", "SETTLING", "COMPLETED"}
    assert client.get("/v1/tasks", params={"state": "SETTLING"}, headers=ALICE).json()["total"] == 1
