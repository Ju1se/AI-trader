# Trade AI — Pure Rust Agent 1–7 Governed Multi-Agent Value Investing System

A 100% safe-Rust (`#![forbid(unsafe_code)]` across all workspace crates) quantitative value investing research, governance, and paper-execution system implementing `Agent 1`–`Agent 7` (Diagrams 3a, 3b, 4a, 4b), `资料/deep-research-report.md`, and `资料/8 Common Backtesting Pitfalls Exercise.pdf`.

## Cargo Workspace Crate Isolation

```text
                       ┌──────────────────┐
                       │    trade-core    │  sha2, serde, serde_json
                       │ 13 States (4a),  │  Private state + transition()
                       │ R0-R4 Tiers (3a),│  PreDispatch vs PostDispatchUnknown
                       │ PIT Date Types   │
                       └────────┬─────────┘
                                │
        ┌───────────────────────┼───────────────────────┬───────────────────┐
        ▼                       ▼                       ▼                   ▼
┌───────────────┐       ┌───────────────┐       ┌───────────────┐   ┌───────────────┐
│  trade-quant  │       │  trade-gate   │       │  trade-store  │   │ trade-broker  │
│ 15 Factors &  │       │ ONLY depends  │       │ ONLY crate w/ │   │ Standalone    │
│ PIT Backtest  │       │ on trade-core!│       │ sqlx! SQLite &│   │ PaperBroker   │
└───────┬───────┘       │ Generic Tool  │       │ PostgreSQL +  │   │ Adapter       │
        │               │ & GateLedger  │       │ migrations/   │   └───────┬───────┘
        ▼               └───────┬───────┘       └───────┬───────┘           │
┌───────────────┐               │                       │                   │
│ trade-workers │               │                       │                   │
│ Read-Only     │               │                       │                   │
│ Workers +     │               │                       │                   │
│ Committee     │               │                       │                   │
└───────┬───────┘               │                       │                   │
        └───────────────────────┴───────────┬───────────┴───────────────────┘
                                            ▼
                                    ┌───────────────┐
                                    │   trade-cli   │
                                    │ Tool Impls,   │
                                    │ DB Verifier,  │
                                    │ CLI & Parity  │
                                    └───────────────┘
```

## Key Architectural Guarantees

1. **Physical Crate Isolation**:
   - `trade-store` is the **only** crate depending on `sqlx`, managing schema via portable ANSI SQL in [migrations/](migrations/) (seamless switch between `sqlite://` and `postgres://`).
   - `trade-gate` depends **only** on `trade-core` and invokes tools through the generic `Tool` trait (zero hardcoded tool names).
   - `trade-workers` hosts read-only specialist workers and `InvestmentCommittee` with zero database or broker dependencies.
   - `trade-broker` provides an independent `PaperBrokerAdapter` decoupled from the storage layer.
2. **Diagram 4a State Machine & Optimistic Concurrency**:
   - Exact 13 states: `QUEUED`, `RUNNING`, `AWAITING_INPUT`, `AWAITING_APPROVAL`, `OUTCOME_UNKNOWN`, `LIMIT_REACHED`, `VERIFYING`, `SETTLING`, `COMPLETED`, `FAILED`, `HANDED_OFF`, `DECLINED`, `CANCELLED`.
   - `TaskRecord.state` is private and only mutable via `TaskRecord::transition()`, persisted with `version` optimistic locking.
3. **Diagram 3a & 3b Gate & Approval Rules**:
   - Unconfigured tools are immediately rejected; `R4` is permanently prohibited; order submission is `R3`; draft creation is `R2`.
   - Approvals live exclusively in the database (`PENDING -> APPROVED | REJECTED -> CONSUMED` in a single transaction with `DISPATCHED` intent registration).
   - Authoritative veto lists and verification inputs are read directly from `DbStore` & `PaperBrokerAdapter`, never accepted from callers.
   - Algebraic `GateError` separates `PreDispatch` rejections from `PostDispatchUnknown` errors, unconditionally freezing tasks in `OUTCOME_UNKNOWN` upon any post-dispatch failure.

## Quickstart

```bash
# Run all unit and integration tests across all 6 crates
cargo test --workspace --offline

# Run end-to-end demo (item-by-item parity with v0.1-python-mvp)
cargo run --offline -p trade-cli -- demo

# Standalone backtest, single-date research, and Rust throughput benchmark
cargo run --offline -p trade-cli -- backtest
cargo run --offline -p trade-cli -- research --as-of 2026-02-10
cargo run --release --offline -p trade-cli -- bench
```
