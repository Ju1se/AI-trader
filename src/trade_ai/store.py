"""External SQLite state store and Point-in-Time (PIT) market/fundamental repository."""

from __future__ import annotations

import json
import re
import sqlite3
from typing import Any, Sequence

from trade_ai.schemas import (
    ActionIntentRecord,
    ApprovalToken,
    SchemaValidationError,
    TaskContract,
    TaskRecord,
    TaskState,
    UntrustedContextItem,
    WaitMetadata,
    utc_now_iso,
)

_INJECTION_PATTERNS: tuple[re.Pattern[str], ...] = tuple(
    re.compile(pat, re.IGNORECASE)
    for pat in (
        r"ignore\s+(all\s+)?(previous|prior)\s+instructions",
        r"system\s+override",
        r"system\s+prompt",
        r"set\s+hard_veto\s*=",
        r"set\s+score\s*=",
        r"execute\s+(buy|sell|order)\s+immediately",
        r"bypass\s+(gate|approval)",
    )
)


def detect_prompt_injection(text: str) -> bool:
    """Detect common adversarial instruction patterns inside untrusted retrieved documents."""
    if not text:
        return False
    return any(pattern.search(text) for pattern in _INJECTION_PATTERNS)


class ReadOnlyDataView:
    """Sandboxed read-only facade passed to specialist Workers (no state/write access)."""

    def __init__(self, store: StateAndPITStore, authorized_markets: Sequence[str]) -> None:
        self.__store = store
        self.__authorized_markets = tuple(authorized_markets)

    @property
    def authorized_markets(self) -> tuple[str, ...]:
        return self.__authorized_markets

    def get_pit_snapshot(self, as_of_date: str, markets: Sequence[str] | None = None) -> list[dict[str, Any]]:
        requested = tuple(markets) if markets is not None else self.__authorized_markets
        for m in requested:
            if m not in self.__authorized_markets:
                raise PermissionError(f"Market {m!r} is outside worker authorized scope {self.__authorized_markets}")
        return self.__store.get_pit_snapshot(as_of_date=as_of_date, markets=requested)

    def get_pit_filings(self, ticker: str, as_of_date: str) -> list[UntrustedContextItem]:
        sec = self.__store.get_security(ticker)
        if sec is None or sec["market"] not in self.__authorized_markets:
            raise PermissionError(f"Ticker {ticker!r} is outside worker authorized markets")
        return self.__store.get_pit_filings(ticker=ticker, as_of_date=as_of_date)

    def get_agent_calibration(self, agent_name: str) -> float:
        return self.__store.get_agent_calibration(agent_name)


class StateAndPITStore:
    """Persistent external state machine + Point-in-Time financial/filing database."""

    def __init__(self, db_path: str = ":memory:") -> None:
        self.db_path = db_path
        self.conn = sqlite3.connect(db_path)
        self.conn.row_factory = sqlite3.Row
        self.conn.execute("PRAGMA foreign_keys = ON;")
        self._init_schema()

    def close(self) -> None:
        self.conn.close()

    def _init_schema(self) -> None:
        self.conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS pit_securities (
                ticker TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                market TEXT NOT NULL,
                industry TEXT NOT NULL,
                list_date TEXT NOT NULL,
                delist_date TEXT,
                delist_return REAL
            );

            CREATE TABLE IF NOT EXISTS pit_financials (
                record_id TEXT PRIMARY KEY,
                ticker TEXT NOT NULL,
                market TEXT NOT NULL,
                industry TEXT NOT NULL,
                period_end TEXT NOT NULL,
                filing_date TEXT NOT NULL,
                book_value REAL NOT NULL,
                market_cap REAL NOT NULL,
                enterprise_value REAL NOT NULL,
                revenue REAL NOT NULL,
                revenue_3y_ago REAL NOT NULL,
                gross_profit REAL NOT NULL,
                ebit REAL NOT NULL,
                net_income REAL NOT NULL,
                interest_expense REAL NOT NULL,
                cfo REAL NOT NULL,
                capex REAL NOT NULL,
                total_assets REAL NOT NULL,
                prev_total_assets REAL NOT NULL,
                current_assets REAL NOT NULL,
                current_liabilities REAL NOT NULL,
                total_debt REAL NOT NULL,
                cash REAL NOT NULL,
                retained_earnings REAL NOT NULL,
                total_liabilities REAL NOT NULL,
                shares_out REAL NOT NULL,
                prev_shares_out REAL NOT NULL,
                goodwill REAL NOT NULL,
                prev_goodwill REAL NOT NULL,
                sbc_expense REAL NOT NULL,
                insider_net_buy_usd REAL NOT NULL,
                beneish_m REAL NOT NULL,
                roic REAL NOT NULL,
                wacc REAL NOT NULL,
                FOREIGN KEY (ticker) REFERENCES pit_securities(ticker)
            );

            CREATE INDEX IF NOT EXISTS idx_pit_fin_ticker_filing
                ON pit_financials(ticker, filing_date, period_end);

            CREATE TABLE IF NOT EXISTS pit_prices (
                ticker TEXT NOT NULL,
                trade_date TEXT NOT NULL,
                close_price REAL NOT NULL,
                forward_1m_return REAL NOT NULL,
                adv_usd REAL NOT NULL,
                amihud_illiq REAL NOT NULL,
                momentum_12_1 REAL NOT NULL,
                is_halted INTEGER NOT NULL DEFAULT 0,
                is_st INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (ticker, trade_date),
                FOREIGN KEY (ticker) REFERENCES pit_securities(ticker)
            );

            CREATE TABLE IF NOT EXISTS pit_filings (
                record_id TEXT PRIMARY KEY,
                ticker TEXT NOT NULL,
                market TEXT NOT NULL,
                period_end TEXT NOT NULL,
                filing_date TEXT NOT NULL,
                section TEXT NOT NULL,
                source TEXT NOT NULL,
                content TEXT NOT NULL,
                FOREIGN KEY (ticker) REFERENCES pit_securities(ticker)
            );

            CREATE INDEX IF NOT EXISTS idx_pit_filings_ticker_date
                ON pit_filings(ticker, filing_date);

            CREATE TABLE IF NOT EXISTS tasks (
                task_id TEXT PRIMARY KEY,
                idempotency_trigger_key TEXT NOT NULL UNIQUE,
                task_type TEXT NOT NULL,
                config_version TEXT NOT NULL,
                requester_id TEXT NOT NULL,
                acting_principal TEXT NOT NULL,
                trigger_source TEXT NOT NULL,
                authorized_markets_json TEXT NOT NULL,
                state TEXT NOT NULL,
                wait_meta_json TEXT,
                turns_used INTEGER NOT NULL DEFAULT 0,
                spend_usd REAL NOT NULL DEFAULT 0.0,
                retries_used INTEGER NOT NULL DEFAULT 0,
                consecutive_denials INTEGER NOT NULL DEFAULT 0,
                reworks_used INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                reopened_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS task_transitions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                from_state TEXT,
                to_state TEXT NOT NULL,
                reason TEXT NOT NULL,
                actor TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS contracts (
                task_id TEXT PRIMARY KEY,
                contract_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS approvals (
                approval_id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                approver_id TEXT NOT NULL,
                payload_hash TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                used INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS action_ledger (
                idempotency_key TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                target TEXT NOT NULL,
                args_json TEXT NOT NULL,
                payload_hash TEXT NOT NULL,
                status TEXT NOT NULL,
                backend_receipt_id TEXT,
                readback_verified INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS rebalance_drafts (
                draft_id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                as_of_date TEXT NOT NULL,
                weights_json TEXT NOT NULL,
                vetoed_json TEXT NOT NULL,
                escalated_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS paper_orders (
                receipt_id TEXT PRIMARY KEY,
                idempotency_key TEXT NOT NULL UNIQUE,
                task_id TEXT NOT NULL,
                as_of_date TEXT NOT NULL,
                orders_json TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS cost_ledger (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL,
                category TEXT NOT NULL,
                amount_usd REAL NOT NULL,
                human_minutes REAL NOT NULL DEFAULT 0.0,
                note TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS agent_calibration (
                agent_name TEXT PRIMARY KEY,
                calibrated_confidence REAL NOT NULL,
                historical_ic REAL NOT NULL DEFAULT 0.0,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS curated_memory (
                memory_id TEXT PRIMARY KEY,
                source_task_id TEXT NOT NULL,
                summary TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending_review',
                reviewed_by TEXT,
                created_at TEXT NOT NULL,
                FOREIGN KEY (source_task_id) REFERENCES tasks(task_id)
            );
            """
        )
        self.conn.commit()

    def make_read_only_view(self, authorized_markets: Sequence[str]) -> ReadOnlyDataView:
        return ReadOnlyDataView(self, authorized_markets)

    # --- PIT Market & Fundamental Ingestion / Queries ---

    def upsert_security(self, sec: dict[str, Any]) -> None:
        self.conn.execute(
            """
            INSERT INTO pit_securities (ticker, name, market, industry, list_date, delist_date, delist_return)
            VALUES (:ticker, :name, :market, :industry, :list_date, :delist_date, :delist_return)
            ON CONFLICT(ticker) DO UPDATE SET
                name=excluded.name,
                market=excluded.market,
                industry=excluded.industry,
                list_date=excluded.list_date,
                delist_date=excluded.delist_date,
                delist_return=excluded.delist_return
            """,
            {
                "ticker": sec["ticker"],
                "name": sec["name"],
                "market": sec["market"],
                "industry": sec["industry"],
                "list_date": sec["list_date"],
                "delist_date": sec.get("delist_date"),
                "delist_return": sec.get("delist_return"),
            },
        )
        self.conn.commit()

    def insert_financial(self, fin: dict[str, Any]) -> None:
        cols = [
            "record_id", "ticker", "market", "industry", "period_end", "filing_date",
            "book_value", "market_cap", "enterprise_value", "revenue", "revenue_3y_ago",
            "gross_profit", "ebit", "net_income", "interest_expense", "cfo", "capex",
            "total_assets", "prev_total_assets", "current_assets", "current_liabilities",
            "total_debt", "cash", "retained_earnings", "total_liabilities",
            "shares_out", "prev_shares_out", "goodwill", "prev_goodwill",
            "sbc_expense", "insider_net_buy_usd", "beneish_m", "roic", "wacc",
        ]
        placeholders = ", ".join(f":{c}" for c in cols)
        col_list = ", ".join(cols)
        self.conn.execute(
            f"INSERT OR REPLACE INTO pit_financials ({col_list}) VALUES ({placeholders})",
            fin,
        )
        self.conn.commit()

    def insert_price(self, price: dict[str, Any]) -> None:
        self.conn.execute(
            """
            INSERT OR REPLACE INTO pit_prices (
                ticker, trade_date, close_price, forward_1m_return,
                adv_usd, amihud_illiq, momentum_12_1, is_halted, is_st
            ) VALUES (
                :ticker, :trade_date, :close_price, :forward_1m_return,
                :adv_usd, :amihud_illiq, :momentum_12_1, :is_halted, :is_st
            )
            """,
            {
                "ticker": price["ticker"],
                "trade_date": price["trade_date"],
                "close_price": price["close_price"],
                "forward_1m_return": price["forward_1m_return"],
                "adv_usd": price["adv_usd"],
                "amihud_illiq": price["amihud_illiq"],
                "momentum_12_1": price["momentum_12_1"],
                "is_halted": int(bool(price.get("is_halted", False))),
                "is_st": int(bool(price.get("is_st", False))),
            },
        )
        self.conn.commit()

    def insert_filing(self, filing: dict[str, Any]) -> None:
        self.conn.execute(
            """
            INSERT OR REPLACE INTO pit_filings (
                record_id, ticker, market, period_end, filing_date, section, source, content
            ) VALUES (
                :record_id, :ticker, :market, :period_end, :filing_date, :section, :source, :content
            )
            """,
            filing,
        )
        self.conn.commit()

    def get_security(self, ticker: str) -> dict[str, Any] | None:
        row = self.conn.execute("SELECT * FROM pit_securities WHERE ticker = ?", (ticker,)).fetchone()
        return dict(row) if row is not None else None

    def get_pit_snapshot(self, as_of_date: str, markets: Sequence[str]) -> list[dict[str, Any]]:
        """
        Point-in-Time universe query:
        - Security must be listed on or before as_of_date and not yet delisted prior to as_of_date.
        - Financials strictly filtered by filing_date <= as_of_date (NEVER period_end!).
        - Prices strictly filtered by trade_date <= as_of_date.
        """
        if not markets:
            return []
        placeholders = ", ".join("?" for _ in markets)
        query = f"""
        WITH active_sec AS (
            SELECT *
            FROM pit_securities
            WHERE market IN ({placeholders})
              AND list_date <= ?
              AND (delist_date IS NULL OR delist_date >= ?)
        ),
        latest_fin AS (
            SELECT f.*
            FROM pit_financials f
            INNER JOIN (
                SELECT ticker, MAX(filing_date || '|' || period_end) AS max_key
                FROM pit_financials
                WHERE filing_date <= ?
                GROUP BY ticker
            ) m ON f.ticker = m.ticker AND (f.filing_date || '|' || f.period_end) = m.max_key
        ),
        latest_px AS (
            SELECT p.*
            FROM pit_prices p
            INNER JOIN (
                SELECT ticker, MAX(trade_date) AS max_dt
                FROM pit_prices
                WHERE trade_date <= ?
                GROUP BY ticker
            ) mp ON p.ticker = mp.ticker AND p.trade_date = mp.max_dt
        )
        SELECT
            s.ticker,
            s.name,
            s.list_date,
            s.delist_date,
            s.delist_return,
            f.record_id AS fin_record_id,
            f.market,
            f.industry,
            f.period_end,
            f.filing_date,
            f.book_value,
            f.market_cap,
            f.enterprise_value,
            f.revenue,
            f.revenue_3y_ago,
            f.gross_profit,
            f.ebit,
            f.net_income,
            f.interest_expense,
            f.cfo,
            f.capex,
            f.total_assets,
            f.prev_total_assets,
            f.current_assets,
            f.current_liabilities,
            f.total_debt,
            f.cash,
            f.retained_earnings,
            f.total_liabilities,
            f.shares_out,
            f.prev_shares_out,
            f.goodwill,
            f.prev_goodwill,
            f.sbc_expense,
            f.insider_net_buy_usd,
            f.beneish_m,
            f.roic,
            f.wacc,
            p.trade_date,
            p.close_price,
            p.forward_1m_return,
            p.adv_usd,
            p.amihud_illiq,
            p.momentum_12_1,
            p.is_halted,
            p.is_st
        FROM active_sec s
        INNER JOIN latest_fin f ON s.ticker = f.ticker
        INNER JOIN latest_px p ON s.ticker = p.ticker
        ORDER BY s.ticker ASC
        """
        params = tuple(markets) + (as_of_date, as_of_date, as_of_date, as_of_date)
        rows = self.conn.execute(query, params).fetchall()
        return [dict(r) for r in rows]

    def get_pit_filings(self, ticker: str, as_of_date: str) -> list[UntrustedContextItem]:
        rows = self.conn.execute(
            """
            SELECT record_id, ticker, source, filing_date, content
            FROM pit_filings
            WHERE ticker = ? AND filing_date <= ?
            ORDER BY filing_date DESC, record_id ASC
            """,
            (ticker, as_of_date),
        ).fetchall()
        items: list[UntrustedContextItem] = []
        for r in rows:
            flagged = detect_prompt_injection(r["content"])
            item = UntrustedContextItem(
                record_id=r["record_id"],
                ticker=r["ticker"],
                source=r["source"],
                filing_date=r["filing_date"],
                content=r["content"],
                trust_label="untrusted_data",
                injection_flagged=flagged,
            )
            item.validate()
            items.append(item)
        return items

    def verify_record_pit(self, record_id: str, as_of_date: str) -> bool:
        """Independent verifier check: record_id exists in filings or financials on/before as_of_date."""
        row = self.conn.execute(
            "SELECT 1 FROM pit_filings WHERE record_id = ? AND filing_date <= ?",
            (record_id, as_of_date),
        ).fetchone()
        if row is not None:
            return True
        row2 = self.conn.execute(
            "SELECT 1 FROM pit_financials WHERE record_id = ? AND filing_date <= ?",
            (record_id, as_of_date),
        ).fetchone()
        return row2 is not None

    # --- Task State Machine Persistence ---

    def _row_to_task(self, row: sqlite3.Row) -> TaskRecord:
        wait_meta = None
        if row["wait_meta_json"]:
            wm = json.loads(row["wait_meta_json"])
            wait_meta = WaitMetadata(
                owner_id=wm["owner_id"],
                deadline_iso=wm["deadline_iso"],
                timeout_action=wm["timeout_action"],
            )
        task = TaskRecord(
            task_id=row["task_id"],
            idempotency_trigger_key=row["idempotency_trigger_key"],
            task_type=row["task_type"],
            config_version=row["config_version"],
            requester_id=row["requester_id"],
            acting_principal=row["acting_principal"],
            trigger_source=row["trigger_source"],
            authorized_markets=tuple(json.loads(row["authorized_markets_json"])),
            state=TaskState(row["state"]),
            wait_meta=wait_meta,
            turns_used=row["turns_used"],
            spend_usd=row["spend_usd"],
            retries_used=row["retries_used"],
            consecutive_denials=row["consecutive_denials"],
            reworks_used=row["reworks_used"],
            created_at=row["created_at"],
            updated_at=row["updated_at"],
            reopened_count=row["reopened_count"],
        )
        task.validate()
        return task

    def get_task(self, task_id: str) -> TaskRecord | None:
        row = self.conn.execute("SELECT * FROM tasks WHERE task_id = ?", (task_id,)).fetchone()
        return self._row_to_task(row) if row is not None else None

    def get_task_by_trigger_key(self, idempotency_trigger_key: str) -> TaskRecord | None:
        row = self.conn.execute(
            "SELECT * FROM tasks WHERE idempotency_trigger_key = ?",
            (idempotency_trigger_key,),
        ).fetchone()
        return self._row_to_task(row) if row is not None else None

    def create_task(self, task: TaskRecord, actor: str = "harness") -> TaskRecord:
        task.validate()
        wait_json = (
            json.dumps(
                {
                    "owner_id": task.wait_meta.owner_id,
                    "deadline_iso": task.wait_meta.deadline_iso,
                    "timeout_action": task.wait_meta.timeout_action,
                }
            )
            if task.wait_meta is not None
            else None
        )
        self.conn.execute(
            """
            INSERT INTO tasks (
                task_id, idempotency_trigger_key, task_type, config_version,
                requester_id, acting_principal, trigger_source, authorized_markets_json,
                state, wait_meta_json, turns_used, spend_usd, retries_used,
                consecutive_denials, reworks_used, created_at, updated_at, reopened_count
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                task.task_id,
                task.idempotency_trigger_key,
                task.task_type,
                task.config_version,
                task.requester_id,
                task.acting_principal,
                task.trigger_source,
                json.dumps(list(task.authorized_markets)),
                task.state.value,
                wait_json,
                task.turns_used,
                task.spend_usd,
                task.retries_used,
                task.consecutive_denials,
                task.reworks_used,
                task.created_at,
                task.updated_at,
                task.reopened_count,
            ),
        )
        self.conn.execute(
            """
            INSERT INTO task_transitions (task_id, from_state, to_state, reason, actor, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (task.task_id, None, task.state.value, "task_admitted", actor, task.created_at),
        )
        self.conn.commit()
        return task

    def save_task(
        self,
        task: TaskRecord,
        transition_reason: str | None = None,
        actor: str = "harness",
    ) -> TaskRecord:
        existing = self.get_task(task.task_id)
        if existing is None:
            raise SchemaValidationError(f"Task {task.task_id!r} does not exist")
        task.updated_at = utc_now_iso()
        task.validate()
        wait_json = (
            json.dumps(
                {
                    "owner_id": task.wait_meta.owner_id,
                    "deadline_iso": task.wait_meta.deadline_iso,
                    "timeout_action": task.wait_meta.timeout_action,
                }
            )
            if task.wait_meta is not None
            else None
        )
        self.conn.execute(
            """
            UPDATE tasks SET
                state = ?,
                wait_meta_json = ?,
                turns_used = ?,
                spend_usd = ?,
                retries_used = ?,
                consecutive_denials = ?,
                reworks_used = ?,
                updated_at = ?,
                reopened_count = ?
            WHERE task_id = ?
            """,
            (
                task.state.value,
                wait_json,
                task.turns_used,
                task.spend_usd,
                task.retries_used,
                task.consecutive_denials,
                task.reworks_used,
                task.updated_at,
                task.reopened_count,
                task.task_id,
            ),
        )
        if existing.state != task.state or transition_reason:
            self.conn.execute(
                """
                INSERT INTO task_transitions (task_id, from_state, to_state, reason, actor, created_at)
                VALUES (?, ?, ?, ?, ?, ?)
                """,
                (
                    task.task_id,
                    existing.state.value,
                    task.state.value,
                    transition_reason or "state_update",
                    actor,
                    task.updated_at,
                ),
            )
        self.conn.commit()
        return task

    def get_task_transitions(self, task_id: str) -> list[dict[str, Any]]:
        rows = self.conn.execute(
            "SELECT * FROM task_transitions WHERE task_id = ? ORDER BY id ASC",
            (task_id,),
        ).fetchall()
        return [dict(r) for r in rows]

    # --- Contracts, Approvals, Action Ledger, Drafts & Paper Broker ---

    def save_contract(self, contract: TaskContract) -> None:
        contract.validate()
        payload = {
            "task_id": contract.task_id,
            "goal": contract.goal,
            "as_of_date": contract.as_of_date,
            "universe_markets": list(contract.universe_markets),
            "portfolio_capital_usd": contract.portfolio_capital_usd,
            "max_positions": contract.max_positions,
            "expected_outputs": contract.expected_outputs,
            "done_checks": contract.done_checks,
            "assumptions": contract.assumptions,
            "irreversible_steps_deferred": contract.irreversible_steps_deferred,
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO contracts (task_id, contract_json, created_at) VALUES (?, ?, ?)",
            (contract.task_id, json.dumps(payload), utc_now_iso()),
        )
        self.conn.commit()

    def get_contract(self, task_id: str) -> TaskContract | None:
        row = self.conn.execute("SELECT contract_json FROM contracts WHERE task_id = ?", (task_id,)).fetchone()
        if row is None:
            return None
        d = json.loads(row["contract_json"])
        return TaskContract(
            task_id=d["task_id"],
            goal=d["goal"],
            as_of_date=d["as_of_date"],
            universe_markets=tuple(d["universe_markets"]),
            portfolio_capital_usd=float(d["portfolio_capital_usd"]),
            max_positions=int(d["max_positions"]),
            expected_outputs=list(d["expected_outputs"]),
            done_checks=list(d["done_checks"]),
            assumptions=list(d.get("assumptions", [])),
            irreversible_steps_deferred=bool(d.get("irreversible_steps_deferred", True)),
        )

    def save_approval(self, token: ApprovalToken) -> None:
        token.validate()
        self.conn.execute(
            """
            INSERT OR REPLACE INTO approvals (
                approval_id, task_id, approver_id, payload_hash, expires_at, used, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (
                token.approval_id,
                token.task_id,
                token.approver_id,
                token.payload_hash,
                token.expires_at,
                int(token.used),
                token.created_at,
            ),
        )
        self.conn.commit()

    def get_approval(self, approval_id: str) -> ApprovalToken | None:
        row = self.conn.execute("SELECT * FROM approvals WHERE approval_id = ?", (approval_id,)).fetchone()
        if row is None:
            return None
        return ApprovalToken(
            approval_id=row["approval_id"],
            task_id=row["task_id"],
            approver_id=row["approver_id"],
            payload_hash=row["payload_hash"],
            expires_at=row["expires_at"],
            used=bool(row["used"]),
            created_at=row["created_at"],
        )

    def mark_approval_used(self, approval_id: str) -> None:
        self.conn.execute("UPDATE approvals SET used = 1 WHERE approval_id = ?", (approval_id,))
        self.conn.commit()

    def upsert_action_intent(self, record: ActionIntentRecord) -> None:
        record.validate()
        record.updated_at = utc_now_iso()
        self.conn.execute(
            """
            INSERT INTO action_ledger (
                idempotency_key, task_id, tool_name, target, args_json,
                payload_hash, status, backend_receipt_id, readback_verified,
                created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(idempotency_key) DO UPDATE SET
                status = excluded.status,
                backend_receipt_id = excluded.backend_receipt_id,
                readback_verified = excluded.readback_verified,
                updated_at = excluded.updated_at
            """,
            (
                record.idempotency_key,
                record.task_id,
                record.tool_name,
                record.target,
                record.args_json,
                record.payload_hash,
                record.status,
                record.backend_receipt_id,
                int(record.readback_verified),
                record.created_at,
                record.updated_at,
            ),
        )
        self.conn.commit()

    def get_action_intent(self, idempotency_key: str) -> ActionIntentRecord | None:
        row = self.conn.execute(
            "SELECT * FROM action_ledger WHERE idempotency_key = ?",
            (idempotency_key,),
        ).fetchone()
        if row is None:
            return None
        return ActionIntentRecord(
            idempotency_key=row["idempotency_key"],
            task_id=row["task_id"],
            tool_name=row["tool_name"],
            target=row["target"],
            args_json=row["args_json"],
            payload_hash=row["payload_hash"],
            status=row["status"],
            backend_receipt_id=row["backend_receipt_id"],
            readback_verified=bool(row["readback_verified"]),
            created_at=row["created_at"],
            updated_at=row["updated_at"],
        )

    def list_task_actions(self, task_id: str) -> list[ActionIntentRecord]:
        rows = self.conn.execute(
            "SELECT * FROM action_ledger WHERE task_id = ? ORDER BY created_at ASC",
            (task_id,),
        ).fetchall()
        return [
            ActionIntentRecord(
                idempotency_key=r["idempotency_key"],
                task_id=r["task_id"],
                tool_name=r["tool_name"],
                target=r["target"],
                args_json=r["args_json"],
                payload_hash=r["payload_hash"],
                status=r["status"],
                backend_receipt_id=r["backend_receipt_id"],
                readback_verified=bool(r["readback_verified"]),
                created_at=r["created_at"],
                updated_at=r["updated_at"],
            )
            for r in rows
        ]

    def save_rebalance_draft(
        self,
        draft_id: str,
        task_id: str,
        as_of_date: str,
        weights: dict[str, float],
        vetoed: dict[str, list[str]],
        escalated: list[str],
    ) -> None:
        self.conn.execute(
            """
            INSERT OR REPLACE INTO rebalance_drafts (
                draft_id, task_id, as_of_date, weights_json, vetoed_json, escalated_json, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (
                draft_id,
                task_id,
                as_of_date,
                json.dumps(weights, sort_keys=True),
                json.dumps(vetoed, sort_keys=True),
                json.dumps(escalated),
                utc_now_iso(),
            ),
        )
        self.conn.commit()

    def get_rebalance_draft(self, draft_id: str) -> dict[str, Any] | None:
        row = self.conn.execute("SELECT * FROM rebalance_drafts WHERE draft_id = ?", (draft_id,)).fetchone()
        if row is None:
            return None
        return {
            "draft_id": row["draft_id"],
            "task_id": row["task_id"],
            "as_of_date": row["as_of_date"],
            "weights": json.loads(row["weights_json"]),
            "vetoed": json.loads(row["vetoed_json"]),
            "escalated": json.loads(row["escalated_json"]),
            "created_at": row["created_at"],
        }

    def save_paper_order(
        self,
        receipt_id: str,
        idempotency_key: str,
        task_id: str,
        as_of_date: str,
        orders: list[dict[str, Any]],
        status: str = "FILLED",
    ) -> None:
        self.conn.execute(
            """
            INSERT OR IGNORE INTO paper_orders (
                receipt_id, idempotency_key, task_id, as_of_date, orders_json, status, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (receipt_id, idempotency_key, task_id, as_of_date, json.dumps(orders), status, utc_now_iso()),
        )
        self.conn.commit()

    def get_paper_order_by_idempotency(self, idempotency_key: str) -> dict[str, Any] | None:
        row = self.conn.execute(
            "SELECT * FROM paper_orders WHERE idempotency_key = ?",
            (idempotency_key,),
        ).fetchone()
        if row is None:
            return None
        return {
            "receipt_id": row["receipt_id"],
            "idempotency_key": row["idempotency_key"],
            "task_id": row["task_id"],
            "as_of_date": row["as_of_date"],
            "orders": json.loads(row["orders_json"]),
            "status": row["status"],
            "created_at": row["created_at"],
        }

    # --- Cost Ledger, Calibration & Curated Memory ---

    def record_cost(
        self,
        task_id: str,
        category: str,
        amount_usd: float,
        human_minutes: float = 0.0,
        note: str = "",
    ) -> None:
        self.conn.execute(
            """
            INSERT INTO cost_ledger (task_id, category, amount_usd, human_minutes, note, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (task_id, category, amount_usd, human_minutes, note, utc_now_iso()),
        )
        self.conn.commit()

    def set_agent_calibration(self, agent_name: str, calibrated_confidence: float, historical_ic: float = 0.0) -> None:
        self.conn.execute(
            """
            INSERT OR REPLACE INTO agent_calibration (agent_name, calibrated_confidence, historical_ic, updated_at)
            VALUES (?, ?, ?, ?)
            """,
            (agent_name, calibrated_confidence, historical_ic, utc_now_iso()),
        )
        self.conn.commit()

    def get_agent_calibration(self, agent_name: str) -> float:
        row = self.conn.execute(
            "SELECT calibrated_confidence FROM agent_calibration WHERE agent_name = ?",
            (agent_name,),
        ).fetchone()
        return float(row["calibrated_confidence"]) if row is not None else 0.75

    def add_curated_memory(self, memory_id: str, source_task_id: str, summary: str) -> None:
        self.conn.execute(
            """
            INSERT OR REPLACE INTO curated_memory (memory_id, source_task_id, summary, status, created_at)
            VALUES (?, ?, ?, 'pending_review', ?)
            """,
            (memory_id, source_task_id, summary, utc_now_iso()),
        )
        self.conn.commit()

    def approve_curated_memory(self, memory_id: str, reviewer_id: str) -> None:
        self.conn.execute(
            "UPDATE curated_memory SET status = 'approved', reviewed_by = ? WHERE memory_id = ?",
            (reviewer_id, memory_id),
        )
        self.conn.commit()

    def list_curated_memory(self, status: str | None = None) -> list[dict[str, Any]]:
        if status:
            rows = self.conn.execute("SELECT * FROM curated_memory WHERE status = ?", (status,)).fetchall()
        else:
            rows = self.conn.execute("SELECT * FROM curated_memory").fetchall()
        return [dict(r) for r in rows]

    def calculate_unit_economics(self, human_hourly_rate_usd: float = 120.0) -> dict[str, Any]:
        """Agent 6 & Agent 7 (5.5): Total cost across all attempts / non-reopened verified completions."""
        row = self.conn.execute(
            """
            SELECT
                COALESCE(SUM(amount_usd), 0.0) AS total_api_usd,
                COALESCE(SUM(human_minutes), 0.0) AS total_human_minutes
            FROM cost_ledger
            """
        ).fetchone()
        total_api = float(row["total_api_usd"])
        total_human_min = float(row["total_human_minutes"])
        total_cost = total_api + (total_human_min / 60.0) * human_hourly_rate_usd

        completed_non_reopened = self.conn.execute(
            "SELECT COUNT(*) AS cnt FROM tasks WHERE state = ? AND reopened_count = 0",
            (TaskState.COMPLETED.value,),
        ).fetchone()["cnt"]
        total_tasks = self.conn.execute("SELECT COUNT(*) AS cnt FROM tasks").fetchone()["cnt"]

        cost_per_verified = (
            round(total_cost / completed_non_reopened, 4) if completed_non_reopened > 0 else None
        )
        return {
            "total_tasks": int(total_tasks),
            "verified_completed_non_reopened": int(completed_non_reopened),
            "total_api_and_tool_usd": round(total_api, 4),
            "total_human_minutes": round(total_human_min, 2),
            "total_all_in_cost_usd": round(total_cost, 4),
            "cost_per_verified_task_usd": cost_per_verified,
        }
