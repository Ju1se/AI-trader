# Trade AI — Agent 1–7 Governed Multi-Agent Value Investing MVP

An auditable, Point-in-Time (PIT) value investing research and paper-execution system built directly on the engineering control architecture in `Agent 1`–`Agent 7` and the quantitative value investing design in `资料/deep-research-report.md` and `资料/8 Common Backtesting Pitfalls Exercise.pdf`.

## Architecture Overview

```text
PREPARE (TaskTypeConfig: ShapeLevel, RiskTier R0-R4, AutonomyLevel, BudgetLimits)
   │
   ▼
ADMIT (Trigger Deduplication -> Principal & Scope -> External SQLite State Machine)
   │
   ▼
FRAME (Ambiguity Clarification -> Pinned TaskContract -> Untrusted Data Envelopes)
   │
   ├──► Deterministic 15-Factor Quant Engine (Fixed Workflow: PIT, Industry-Neutral, Hard Vetoes)
   └──► Read-Only Specialist Workers (Moat, Management/CapAlloc, Footnote Forensics)
   │
   ▼
ACT (Fixed 9-Step Gate -> Payload-Bound Approval -> Intent Ledger -> Credential Proxy -> Read-Back)
   │
   ▼
CLOSE (Independent Verifier -> Delivery Manifest -> Settlement/Reopen -> Unit Economics & Curated Memory)
```

See [docs/architecture-mapping.md](docs/architecture-mapping.md) for the full 25-node mapping from `Agent 7 Workflow Trigger to Verified Close.xmind`.

## Zero-Dependency Design
Implemented cleanly in **Python 3.12+ standard library** (`sqlite3`, `dataclasses`, `enum`, `hashlib`, `json`, `unittest`, `urllib.request`) with zero mandatory third-party packages.

## Quickstart

Run the full automated test suite:
```bash
PYTHONPATH=src python3 -m unittest discover -s tests -v
```

Run the end-to-end demo (PIT backtest pitfall audit + full 5-stage `Agent 7` research & paper execution lifecycle):
```bash
PYTHONPATH=src python3 -m trade_ai.cli demo
```

Run standalone backtest or point-in-time research:
```bash
PYTHONPATH=src python3 -m trade_ai.cli backtest
PYTHONPATH=src python3 -m trade_ai.cli research --as-of 2026-02-10
PYTHONPATH=src python3 -m trade_ai.cli research --as-of 2026-05-01
```

## Extending to Live Data & Live LLM Reasoning
- **Live Filings & Market Data**: Ingest SEC EDGAR / Wind / CSMAR snapshots into `pit_securities`, `pit_financials`, `pit_prices`, and `pit_filings` in `StateAndPITStore`, preserving exact `filing_date` timestamps.
- **Live LLM Synthesis**: Configure `TRADE_AI_LLM_API_KEY` and `TRADE_AI_LLM_ENDPOINT` in `CredentialVault` / environment and pass `LLMJsonReasoner` into `TradeAIHarness`. Workers remain read-only and all retrieved filings stay wrapped inside `<untrusted_data>` envelopes.
