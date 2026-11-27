# Architecture Mapping: Pure Rust Cargo Workspace × Agent 1–7 Controls

This document maps the engineering diagrams (`Agent 1`–`Agent 7`, including Diagrams 3a, 3b, 4a, 4b) and quantitative value investing references (`资料/deep-research-report.md`, `资料/8 Common Backtesting Pitfalls Exercise.pdf`) to the 6-crate Rust workspace.

---

## 1. Crate Responsibility & Dependency Boundaries

| Crate | Allowed Dependencies | Responsibility |
|---|---|---|
| `crates/trade-core` | `serde`, `serde_json`, `sha2` | Compile-time PIT date separation (`FilingDate` vs `PeriodEndDate` vs `AsOfDate`), fixed-point `WeightBps` / `UsdCents`, Diagram 4a 13-state `TaskRecord` (private `state` + `transition()` + `version`), Diagram 3a `RiskTier` (`R0`–`R4`), `GateError` (`PreDispatch` vs `PostDispatchUnknown`), and `<untrusted_data>` envelopes |
| `crates/trade-quant` | `trade-core`, `serde` | Deterministic 15-factor cross-sectional engine, industry/market standardization, Hard Veto rules, and pitfall-safe PIT backtester |
| `crates/trade-workers` | `trade-core`, `trade-quant`, `serde` | Physically isolated read-only workers (`MoatWorker`, `ManagementCapAllocWorker`, `FootnoteForensicsWorker`) and `InvestmentCommittee` consensus/dispersion calculator |
| `crates/trade-gate` | **`trade-core` ONLY** | Generic 9-step fixed-order `ActionGate`, `VerifiedGateTicket` capability token, `Tool` trait, and `GateLedger` trait (zero hardcoded tool names, zero DB driver coupling) |
| `crates/trade-broker` | `trade-core`, `serde`, `serde_json` | Standalone simulated `PaperBrokerAdapter` with API-key auth, idempotent order book, independent read-back, and pre/post-fill fault injection |
| `crates/trade-store` | `sqlx`, `serde`, `serde_json`, `trade-core`, `trade-quant`, `trade-workers`, `trade-gate` | **Sole database crate** using `sqlx::AnyPool` + [migrations/](../migrations/) ANSI SQL (`ON CONFLICT ... DO UPDATE`), compatible with both SQLite and PostgreSQL |
| `crates/trade-cli` | Workspace crates + `tokio`, `serde`, `serde_json` | Concrete `CreateRebalanceDraftTool` (`R2`) & `SubmitPaperOrdersTool` (`R3`), DB-only `IndependentVerifier`, `TradeAiHarness`, fixtures, and CLI (`demo`, `backtest`, `research`, `bench`) |

---

## 2. Diagram 3a, 3b, 4a, 4b Control Enforcement

1. **DB-Backed Approvals (Diagram 3b)**:
   - External callers cannot construct approval tokens.
   - When approval is required and `approval_id` is `None`, `ActionGate` creates a `PENDING` row in `approvals` and transitions the task to `AWAITING_APPROVAL`.
   - An independent approver (`!= requester_id` and `!= config_admin_id`) transitions `PENDING -> APPROVED` or `REJECTED` via `DbStore::decide_approval`.
   - On re-entry with `approval_id`, `DbStore::consume_approval_and_mark_dispatched` verifies status, payload SHA-256 hash, expiration, and separation of duties, marks `CONSUMED`, and records `ActionIntentStatus::Dispatched` in **one atomic SQL transaction**.
2. **Authoritative DB Veto & Verification Source of Truth**:
   - `PortfolioActionPayload` carries no veto list; `CreateRebalanceDraftTool` and `SubmitPaperOrdersTool` read `security_evaluations` directly from `DbStore`.
   - `IndependentVerifier::verify_from_db` accepts only `task_id` and queries `contracts`, `security_evaluations`, `worker_evidences`, `rebalance_drafts`, `action_ledger`, and `PaperBrokerAdapter` directly.
3. **Pre-Dispatch vs Post-Dispatch Error Semantics**:
   - `GateError::PreDispatch(PreDispatchError)` covers all rejections prior to `DISPATCHED` without freezing the task.
   - `GateError::PostDispatchUnknown` captures every error or read-back mismatch after `DISPATCHED`, setting action status to `UNKNOWN` and transitioning the task to `OUTCOME_UNKNOWN` (freezing new actions until `reconcile_before_retry`).
   - All DB status parsers (`TaskState::from_db_parts`, `ActionIntentStatus::from_db_str`, `ApprovalStatus::from_db_str`) fail explicitly on unrecognized strings rather than defaulting to `FAILED`.
4. **Diagram 4a State Machine & Optimistic Concurrency**:
   - States: `QUEUED`, `RUNNING`, `AWAITING_INPUT`, `AWAITING_APPROVAL`, `OUTCOME_UNKNOWN`, `LIMIT_REACHED`, `VERIFYING`, `SETTLING`, `COMPLETED`, `FAILED`, `HANDED_OFF`, `DECLINED`, `CANCELLED`.
   - `TaskRecord.state` is private, mutated solely via `TaskRecord::transition()`, and persisted via `UPDATE tasks ... WHERE task_id = $14 AND version = $15`.
5. **Diagram 3a Risk Tiers**:
   - Unlisted tools in `TaskTypeConfig.allowed_tools` are rejected (`ToolNotConfigured`).
   - `R4` is permanently prohibited (`R4PermanentlyProhibited`).
   - `submit_paper_orders` is `R3`; `create_rebalance_draft` is `R2`.
