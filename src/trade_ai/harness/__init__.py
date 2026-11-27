"""Agent 7 control harness: fixed-order gate, read-only workers, committee, and workflow."""

from trade_ai.harness.committee import InvestmentCommittee
from trade_ai.harness.gate import ActionGate, CredentialVault, GateDecision, ToolRegistry, ToolSpec
from trade_ai.harness.workers import (
    FootnoteForensicsWorker,
    LLMJsonReasoner,
    ManagementCapAllocWorker,
    MoatWorker,
)
from trade_ai.harness.workflow import TradeAIHarness, build_default_config

__all__ = [
    "ActionGate",
    "CredentialVault",
    "GateDecision",
    "ToolRegistry",
    "ToolSpec",
    "MoatWorker",
    "ManagementCapAllocWorker",
    "FootnoteForensicsWorker",
    "LLMJsonReasoner",
    "InvestmentCommittee",
    "TradeAIHarness",
    "build_default_config",
]
