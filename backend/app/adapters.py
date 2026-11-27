"""Boundaries to the outside world. Only interfaces and a simulated broker exist so far.

The harness sits behind HarnessAdapter so it can be swapped (hosted loop or self-hosted)
without touching the control plane. Nothing here is wired into the API yet.
"""

from dataclasses import dataclass, field
from typing import Protocol


class HarnessAdapter(Protocol):
    def start_session(self, task_id: str, contract: dict) -> str:
        """Start an agent session for a task and return its session id."""

    def send_tool_result(self, session_id: str, call_id: str, result: dict, is_error: bool = False) -> None:
        """Return a tool result (or a gate denial) to the model as data."""

    def interrupt(self, session_id: str) -> None:
        """Stop the session: limit reached, cancel, or kill switch."""


class MarketDataAdapter(Protocol):
    def get_quote(self, symbol: str) -> dict:
        """Return a quote with its source and timestamp."""


class BrokerAdapter(Protocol):
    def submit_order(self, idempotency_key: str, order: dict) -> dict: ...

    def get_order(self, order_id: str) -> dict | None: ...

    def find_by_key(self, idempotency_key: str) -> dict | None: ...


@dataclass
class SimBroker:
    """In-memory paper broker. The same idempotency key always returns the first order."""

    _by_key: dict[str, dict] = field(default_factory=dict)
    _by_id: dict[str, dict] = field(default_factory=dict)

    def submit_order(self, idempotency_key: str, order: dict) -> dict:
        existing = self._by_key.get(idempotency_key)
        if existing is not None:
            if existing["order"] != order:
                raise ValueError("idempotency key reused with a different order")
            return existing
        record = {"order_id": f"paper-{len(self._by_id) + 1:06d}", "status": "accepted", "order": dict(order)}
        self._by_key[idempotency_key] = record
        self._by_id[record["order_id"]] = record
        return record

    def get_order(self, order_id: str) -> dict | None:
        return self._by_id.get(order_id)

    def find_by_key(self, idempotency_key: str) -> dict | None:
        return self._by_key.get(idempotency_key)
