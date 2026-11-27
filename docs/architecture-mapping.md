# Architecture Mapping: Agent 1–7 Controls × Multi-Agent Value Investing

This document maps every engineering diagram (`Agent 1`–`Agent 7`) and domain research reference (`资料/deep-research-report.md`, `资料/8 Common Backtesting Pitfalls Exercise.pdf`) in the workspace to concrete Python modules in `src/trade_ai/`.

---

## 1. Cross-Document Mapping Matrix

| Source Document | Core Principle / Requirement | Concrete Module & Mechanism |
|---|---|---|
| [Agent 1 Control From Intelligence to Reliable Work.pdf](../Agent%201%20Control%20From%20Intelligence%20to%20Reliable%20Work.pdf) | LLM vs Agent, 3 responsibility layers (Model / Harness / Enterprise Backend), external state, verified outcomes | `schemas.py` (`TaskRecord`, `TaskState`), `store.py` (SQLite external state), `harness/gate.py` (backend re-verification + read-back) |
| [Agent 2 Decisions Goals to Tools.pdf](../Agent%202%20Decisions%20Goals%20to%20Tools.pdf) | Clarify contract, decompose DAG, replan on feedback, route exact math to deterministic code, justify multi-agent via task success | `quant/factors.py` & `quant/backtest.py` (deterministic math), `harness/workflow.py` (`frame_task`), `harness/workers.py` (read-only specialist workers only where text/qualitative synthesis adds value) |
| [agent 3 Authorize Actions, Contain Risk.pdf](../agent%203%20Authorize%20Actions,%20Contain%20Risk.pdf) | Identify principal & data scope, authorize tools (`canUseTool` fallback only), isolate sandbox, treat retrieved text as untrusted data, bind human approval, verify real outcomes | `harness/gate.py` (9-step fixed-order gate + fail-closed), `store.py` (`ReadOnlyDataView`, `detect_prompt_injection`), `schemas.py` (`UntrustedContextItem`, `ApprovalToken`), `harness/workflow.py` (independent verifier) |
| [agent 4 Persist Context, Recover Work.pdf](../agent%204%20Persist%20Context,%20Recover%20Work.pdf) | Separate session/context/task state/persistent memory; stable task IDs; track waiting states; checkpoint & reconcile before retry | `store.py` (`tasks`, `task_transitions`, `action_ledger`, `curated_memory`), `harness/workflow.py` (`reconcile_before_retry`) |
| [agent 5 Four Workflows Controls in Practice.pdf](../agent%205%20Four%20Workflows%20Controls%20in%20Practice.pdf) | Combine Investment Research controls (official sources, split specialists, verify citations) + Financial Workflow controls (draft, validate, payload-bound approval, execute) | `harness/workers.py` (`MoatWorker`, `ManagementCapAllocWorker`, `FootnoteForensicsWorker`), `harness/committee.py`, `harness/gate.py` |
| [Agent 6 Economics Value to Durable Moats.pdf](../Agent%206%20Economics%20Value%20to%20Durable%20Moats.pdf) | Cost per successful (non-reopened) task including retries & human review; simplest shape shadow comparison | `store.py` (`cost_ledger`), `harness/workflow.py` (`calculate_unit_economics`, `reopen_task`) |
| [Agent 7 Workflow Trigger to Verified Close.xmind](../Agent%207%20Workflow%20Trigger%20to%20Verified%20Close.xmind) | Complete 5-stage lifecycle (`PREPARE -> ADMIT -> FRAME -> ACT -> CLOSE`), 13-state machine, 9-step gate, R0–R4 tiers, separation of duties | `schemas.py`, `harness/gate.py`, `harness/workflow.py` |
| [deep-research-report.md](../资料/deep-research-report.md) | 15 starting factors, Main Alpha Buckets + Hard Veto Risk Overlay, Investment Committee dispersion (`Disp_i`), unified JSON protocol | `quant/factors.py`, `harness/workers.py`, `harness/committee.py`, `schemas.py` (`AgentEvidenceOutput`, `CommitteeDecision`) |
| [8 Common Backtesting Pitfalls Exercise.pdf](../资料/8%20Common%20Backtesting%20Pitfalls%20Exercise.pdf) | Eliminate Look-Ahead Bias, Survivorship Bias, Overfitting, Transaction Cost Ignorance, Liquidity Neglect | `store.py` (`filing_date <= as_of_date`, delisting returns), `quant/backtest.py` (`run_pit_backtest`, `audit_survivorship_bias_exercise`) |

---

## 2. Full 5-Stage / 25-Node Spec from `Agent 7 Workflow Trigger to Verified Close`

### Stage 1: PREPARE — Rules Before Any Run
1. **Choose the simplest shape that passes**: Ladder `SINGLE_CALL -> FIXED_WORKFLOW -> SINGLE_AGENT -> MULTI_AGENT` (`ShapeLevel`). Record shape and rationale in `TaskTypeConfig` (`quant` uses `FIXED_WORKFLOW`; deep research uses `MULTI_AGENT` only on top non-vetoed candidates).
2. **Assign roles, risk tiers and limits**: Define proposer, gatekeeper, approver, verifier. Every tool has risk tier `R0`–`R4` (`RiskTier`); unlisted tools default to `R4_HIGH_STAKES_FUNDS`. Separation of duties: `approver_id` cannot equal `requester_id` or `config_admin_id`.
3. **Define done as checkable system state**: Express completion criteria as verifiable database/system checks before reading untrusted external content.
4. **Test success and adversarial cases first**: Verify normal runs, denied requests, and prompt-injected filings in `tests/` before promotion.
5. **Raise autonomy only on evidence**: `SHADOW -> DRAFT_ONLY -> PER_ACTION_APPROVAL -> BOUNDED_AUTO` (`AutonomyLevel`), driven by verification pass rate and override rate.

### Stage 2: ADMIT — Task, Identity, Boundary
6. **Deduplicate triggers into one task record**: Map triggers to a stable `task_id` via `idempotency_trigger_key` (duplicate triggers return the existing task).
7. **Authenticate requester, name the acting principal**: Record `requester_id`, `acting_principal`, and `trigger_source` separately before any model inspects data; compute `authorized_markets`.
8. **Sandbox execution, keep credentials outside**: Workers receive only `ReadOnlyDataView` (`R0`/`R1` read tools); write tools are unmounted. Credentials live exclusively in `CredentialVault` outside worker scope.
9. **Arm turn, spend and time limits**: Enforce `BudgetLimits` (`max_turns`, `max_spend_usd`, `max_wall_seconds`, `max_retries`, `max_consecutive_denials`, `max_reworks`) with `reserved_close_spend_usd` reserved for verification and delivery.
10. **Keep task state outside the model**: Only the application harness writes `TaskState` across the **13 states**:
    `ADMITTED`, `RUNNING`, `AWAITING_CLARIFICATION`, `AWAITING_APPROVAL`, `OUTCOME_UNKNOWN`, `LIMIT_REACHED`, `VERIFYING`, `AWAITING_CONFIRMATION`, `ESCALATED_TO_HUMAN`, `REJECTED`, `CANCELLED`, `COMPLETED`, `FAILED`.
    Every waiting state requires `WaitMetadata(owner_id, deadline_iso, timeout_action)`.

### Stage 3: FRAME — Contract Before Action
11. **Retrieve authorized context as labelled data**: Fetch only authorized Point-in-Time records with source and `filing_date`. Wrap external text in `<untrusted_data>` envelopes (`UntrustedContextItem`).
12. **Ask when ambiguous, pin the contract**: Ambiguity in target markets, `as_of_date`, or capital transitions to `AWAITING_CLARIFICATION`. Store `TaskContract` outside model context.
13. **Route models and tools by difficulty**: Deterministic factors/backtesting run in pure Python; structured qualitative synthesis routes to specialist workers (with optional LLM adapter + deterministic rubric fallback).
14. **Plan irreversible steps after all checks**: Reversible reads and draft creation precede irreversible paper/order submission (`irreversible_steps_deferred=True`).
15. **Brief workers without passing write authority**: Each worker receives scoped read-only context and returns structured `AgentEvidenceOutput` with `cited_record_ids`.

### Stage 4: ACT — One Gated Action
16. **Gate every call in fixed order**: 9-step fixed sequence (`Availability -> Limits -> Schema -> Hard Deny -> Permissions & Scope -> Business Rules & Risk Tier -> Human Approval -> Intent Registration -> Execution-Site Re-check & Read-Back`). Any internal gate check error fails closed (`DENIED`).
17. **Bind human approval to exact payload**: `ApprovalToken` binds `SHA-256(tool_name, target, canonical_args)`, expires, is single-use, and enforces separation of duties.
18. **Record intent, execute once, read back**: Persist `ActionIntentRecord(status="planned")` with `idempotency_key` before executing; backend executes once with isolated credentials and verifies via independent read-back (`readback_verified=True`).
19. **Replan inside contract and limits**: Workers/committee adapt to missing or conflicting signals within `TaskContract` and `BudgetLimits` without mutating contract or permissions.
20. **Reconcile before any retry or resume**: On timeout/interruption (`OUTCOME_UNKNOWN`), freeze new actions, query backend by `idempotency_key`, reconcile ledger, and only retry with the identical `idempotency_key`.

### Stage 5: CLOSE — Prove, Deliver, Learn
21. **Verify outcomes independently of the actor**: Transition to `VERIFYING`; an independent verifier validates every citation (`filing_date <= as_of_date`), zero `hard_veto` holdings, weight/ADV invariants, and `readback_verified` execution receipts.
22. **Deliver results, even partial, with manifest**: Always emit `DeliveryManifest` including completeness, criterion checks, assumptions, gaps, spend, and next owner—even when `LIMIT_REACHED` or `ESCALATED_TO_HUMAN`.
23. **Settle after confirmation, allow reopening**: Transition `AWAITING_CONFIRMATION -> COMPLETED` only after confirmation; support `reopen_task()` which deducts from verified completions.
24. **Hand off with a usable package**: Build `HandoffPackage` with original request, actions taken/pending, partial artifacts, open questions, and explicit human acceptance (`confirm_handoff`).
25. **Measure cost, curate memory, revisit design**: Track `cost_per_verified_task = total_spend_all_attempts / non_reopened_verified_completions`. Curated memory entries start as `pending_review` and only extract from settled tasks.
