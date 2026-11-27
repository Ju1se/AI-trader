import os

# Must be set before the app modules read their settings.
os.environ["AITRADER_DATABASE_URL"] = "sqlite://"
os.environ["AITRADER_AUTO_MIGRATE"] = "false"
os.environ["AITRADER_SEED_ON_START"] = "false"
os.environ["AITRADER_ENABLE_DEV_ENDPOINTS"] = "true"

import pytest
from fastapi.testclient import TestClient

from app import models, sim
from app.db import Base, SessionLocal, engine
from app.main import app

ALICE = {"X-User-Id": "alice"}
BOB = {"X-User-Id": "bob"}


@pytest.fixture
def client():
    Base.metadata.drop_all(engine)
    Base.metadata.create_all(engine)
    with SessionLocal() as db:
        db.add(models.TaskTypeProfile(**sim.DEMO_PROFILE))
        db.commit()
    with TestClient(app) as test_client:
        yield test_client


@pytest.fixture
def task_id(client):
    body = {"profile_id": "research_paper_order", "title": "研究 ACME", "request_text": "只用官方披露"}
    return client.post("/v1/tasks", json=body, headers=ALICE).json()["task"]["id"]


def step(client, task_id, name, **extra):
    response = client.post(f"/v1/dev/tasks/{task_id}/simulate", json={"step": name, **extra})
    assert response.status_code == 200, response.text
    return response.json()
