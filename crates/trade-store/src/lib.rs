#![forbid(unsafe_code)]
//! Dedicated database persistence crate (`trade-store`) — the ONLY crate in the workspace
//! allowed to depend on `sqlx`.
//! - Uses `sqlx::AnyPool` + `sqlx::migrate!("../../migrations")` for seamless SQLite <-> PostgreSQL switching.
//! - Uses standard ANSI SQL (`ON CONFLICT ... DO UPDATE`) across all queries.
//! - Enforces optimistic concurrency (`version`) on `tasks` and single-transaction approval consumption.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyPool, Row};
use trade_core::{
    sha256_hex, ActionIntentRecord, ActionIntentStatus, AgentEvidenceOutput, ApprovalStatus,
    AsOfDate, CommitteeDecision, FilingDate, GateError, PeriodEndDate, PitSecuritySnapshot,
    PortfolioActionPayload, PreDispatchError, TaskContract, TaskRecord, TaskState, TaskTypeConfig,
    UntrustedFiling, UsdCents, WaitMetadata, WeightBps,
};
use trade_gate::{GateLedger, VerifiedGateTicket};
use trade_quant::{PitMarketSource, QuantSecurityResult};
use trade_workers::ReadOnlyPitView;

static INSTALL_DRIVERS: Once = Once::new();
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn now_epoch_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn unique_id(prefix: &str, seed: &str) -> String {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let h = sha256_hex(format!("{prefix}:{seed}:{nanos}:{seq}").as_bytes());
    format!("{prefix}-{}", &h[..12])
}

pub fn normalize_database_url(url_or_path: &str) -> String {
    if url_or_path == ":memory:" {
        "sqlite::memory:".to_string()
    } else if url_or_path.starts_with("sqlite:")
        || url_or_path.starts_with("postgres://")
        || url_or_path.starts_with("postgresql://")
    {
        url_or_path.to_string()
    } else {
        format!("sqlite://{url_or_path}?mode=rwc")
    }
}

#[derive(Debug, Clone)]
pub struct SecurityInsert {
    pub ticker: String,
    pub name: String,
    pub market: String,
    pub industry: String,
    pub list_date: String,
    pub delist_date: Option<String>,
    pub delist_return: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct FundamentalInsert {
    pub record_id: String,
    pub ticker: String,
    pub market: String,
    pub industry: String,
    pub period_end: PeriodEndDate,
    pub filing_date: FilingDate,
    pub book_value: f64,
    pub market_cap: f64,
    pub enterprise_value: f64,
    pub revenue: f64,
    pub revenue_3y_ago: f64,
    pub gross_profit: f64,
    pub ebit: f64,
    pub net_income: f64,
    pub interest_expense: f64,
    pub cfo: f64,
    pub capex: f64,
    pub total_assets: f64,
    pub prev_total_assets: f64,
    pub current_assets: f64,
    pub current_liabilities: f64,
    pub total_debt: f64,
    pub cash: f64,
    pub retained_earnings: f64,
    pub total_liabilities: f64,
    pub shares_out: f64,
    pub prev_shares_out: f64,
    pub goodwill: f64,
    pub prev_goodwill: f64,
    pub sbc_expense: f64,
    pub insider_net_buy_usd: f64,
    pub beneish_m: f64,
    pub roic: f64,
    pub wacc: f64,
}

/// Read-only view of an approval row stored in DB.
/// Private `_db_only` field forbids external construction (`Requirement #1`).
#[derive(Debug, Clone)]
pub struct StoredApproval {
    pub approval_id: String,
    pub task_id: String,
    pub requester_id: String,
    pub tool_name: String,
    pub target: String,
    pub payload_hash: String,
    pub args_json: String,
    pub status: ApprovalStatus,
    pub approver_id: Option<String>,
    pub decision_note: Option<String>,
    pub expires_at_epoch_s: i64,
    _db_only: (),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvaluation {
    pub ticker: String,
    pub as_of_date: AsOfDate,
    pub industry: String,
    pub adv_usd: f64,
    pub quant_score: f64,
    pub consensus_score: f64,
    pub dispersion: f64,
    pub hard_veto: bool,
    pub veto_reasons: Vec<String>,
    pub escalate_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDraft {
    pub draft_id: String,
    pub task_id: String,
    pub as_of_date: AsOfDate,
    pub weights: BTreeMap<String, WeightBps>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedMemoryItem {
    pub memory_id: String,
    pub source_task_id: String,
    pub summary: String,
    pub status: String,
    pub reviewed_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitEconomicsSummary {
    pub total_tasks: u64,
    pub verified_completed_non_reopened: u64,
    pub total_api_and_tool_usd: f64,
    pub total_human_minutes: f64,
    pub total_all_in_cost_usd: f64,
    pub cost_per_verified_task_usd: Option<f64>,
}

pub struct PreloadedPitSource {
    snapshots: BTreeMap<(AsOfDate, Vec<String>), Vec<PitSecuritySnapshot>>,
}

impl PitMarketSource for PreloadedPitSource {
    fn get_pit_snapshot(
        &self,
        as_of: &AsOfDate,
        markets: &[String],
    ) -> Result<Vec<PitSecuritySnapshot>, String> {
        self.snapshots
            .get(&(as_of.clone(), markets.to_vec()))
            .cloned()
            .ok_or_else(|| format!("missing_preloaded_snapshot:{}", as_of.as_str()))
    }
}

pub struct ScopedReadOnlyView {
    markets: Vec<String>,
    ticker_markets: BTreeMap<String, String>,
    filings_by_ticker: BTreeMap<String, Vec<UntrustedFiling>>,
    calibrations: BTreeMap<String, f64>,
}

impl ReadOnlyPitView for ScopedReadOnlyView {
    fn authorized_markets(&self) -> &[String] {
        &self.markets
    }

    fn get_pit_filings(
        &self,
        ticker: &str,
        _as_of: &AsOfDate,
    ) -> Result<Vec<UntrustedFiling>, String> {
        let Some(mkt) = self.ticker_markets.get(ticker) else {
            return Err(format!("unknown_ticker:{ticker}"));
        };
        if !self.markets.contains(mkt) {
            return Err(format!(
                "permission_denied:ticker {ticker} in market {mkt} outside {:?}",
                self.markets
            ));
        }
        Ok(self.filings_by_ticker.get(ticker).cloned().unwrap_or_default())
    }

    fn get_agent_calibration(&self, agent_name: &str) -> f64 {
        self.calibrations.get(agent_name).copied().unwrap_or(0.75)
    }
}

#[derive(Clone)]
pub struct DbStore {
    pool: AnyPool,
    is_sqlite: bool,
    approval_ttl_seconds: i64,
}

impl DbStore {
    pub async fn connect(url_or_path: &str) -> Result<Self, GateError> {
        INSTALL_DRIVERS.call_once(|| {
            sqlx::any::install_default_drivers();
        });
        let url = normalize_database_url(url_or_path);
        let is_sqlite = url.starts_with("sqlite:");
        let max_conns = if url.contains(":memory:") { 1 } else { 5 };

        let pool = AnyPoolOptions::new()
            .max_connections(max_conns)
            .connect(&url)
            .await
            .map_err(|e| GateError::Storage(format!("db_connect_error:{e}")))?;

        if is_sqlite {
            sqlx::query("PRAGMA foreign_keys = ON;")
                .execute(&pool)
                .await
                .map_err(|e| GateError::Storage(format!("pragma_fk_error:{e}")))?;
        }

        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .map_err(|e| GateError::Storage(format!("migration_error:{e}")))?;

        Ok(Self {
            pool,
            is_sqlite,
            approval_ttl_seconds: 3_600,
        })
    }

    pub fn with_approval_ttl(mut self, ttl_seconds: i64) -> Self {
        self.approval_ttl_seconds = ttl_seconds;
        self
    }

    pub fn backend_kind(&self) -> &'static str {
        if self.is_sqlite {
            "sqlite"
        } else {
            "postgresql"
        }
    }

    // --- PIT Market & Fundamental Ingestion ---

    pub async fn upsert_security(&self, sec: &SecurityInsert) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO pit_securities (ticker, name, market, industry, list_date, delist_date, delist_return)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (ticker) DO UPDATE SET
                 name = EXCLUDED.name,
                 market = EXCLUDED.market,
                 industry = EXCLUDED.industry,
                 list_date = EXCLUDED.list_date,
                 delist_date = EXCLUDED.delist_date,
                 delist_return = EXCLUDED.delist_return;",
        )
        .bind(&sec.ticker)
        .bind(&sec.name)
        .bind(&sec.market)
        .bind(&sec.industry)
        .bind(&sec.list_date)
        .bind(&sec.delist_date)
        .bind(sec.delist_return)
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    pub async fn insert_financial(&self, fin: &FundamentalInsert) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO pit_financials (
                record_id, ticker, market, industry, period_end, filing_date,
                book_value, market_cap, enterprise_value, revenue, revenue_3y_ago,
                gross_profit, ebit, net_income, interest_expense, cfo, capex,
                total_assets, prev_total_assets, current_assets, current_liabilities,
                total_debt, cash, retained_earnings, total_liabilities,
                shares_out, prev_shares_out, goodwill, prev_goodwill,
                sbc_expense, insider_net_buy_usd, beneish_m, roic, wacc
            ) VALUES (
                $1, $2, $3, $4, $5, $6,
                $7, $8, $9, $10, $11,
                $12, $13, $14, $15, $16, $17,
                $18, $19, $20, $21,
                $22, $23, $24, $25,
                $26, $27, $28, $29,
                $30, $31, $32, $33, $34
            )
            ON CONFLICT (record_id) DO UPDATE SET
                filing_date = EXCLUDED.filing_date,
                book_value = EXCLUDED.book_value,
                market_cap = EXCLUDED.market_cap,
                enterprise_value = EXCLUDED.enterprise_value;",
        )
        .bind(&fin.record_id)
        .bind(&fin.ticker)
        .bind(&fin.market)
        .bind(&fin.industry)
        .bind(fin.period_end.as_str())
        .bind(fin.filing_date.as_str())
        .bind(fin.book_value)
        .bind(fin.market_cap)
        .bind(fin.enterprise_value)
        .bind(fin.revenue)
        .bind(fin.revenue_3y_ago)
        .bind(fin.gross_profit)
        .bind(fin.ebit)
        .bind(fin.net_income)
        .bind(fin.interest_expense)
        .bind(fin.cfo)
        .bind(fin.capex)
        .bind(fin.total_assets)
        .bind(fin.prev_total_assets)
        .bind(fin.current_assets)
        .bind(fin.current_liabilities)
        .bind(fin.total_debt)
        .bind(fin.cash)
        .bind(fin.retained_earnings)
        .bind(fin.total_liabilities)
        .bind(fin.shares_out)
        .bind(fin.prev_shares_out)
        .bind(fin.goodwill)
        .bind(fin.prev_goodwill)
        .bind(fin.sbc_expense)
        .bind(fin.insider_net_buy_usd)
        .bind(fin.beneish_m)
        .bind(fin.roic)
        .bind(fin.wacc)
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_price(
        &self,
        ticker: &str,
        trade_date: &str,
        close_price: f64,
        forward_1m_return: f64,
        adv_usd: f64,
        amihud_illiq: f64,
        momentum_12_1: f64,
        is_halted: bool,
        is_st: bool,
    ) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO pit_prices (
                ticker, trade_date, close_price, forward_1m_return,
                adv_usd, amihud_illiq, momentum_12_1, is_halted, is_st
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (ticker, trade_date) DO UPDATE SET
                close_price = EXCLUDED.close_price,
                forward_1m_return = EXCLUDED.forward_1m_return,
                adv_usd = EXCLUDED.adv_usd;",
        )
        .bind(ticker)
        .bind(trade_date)
        .bind(close_price)
        .bind(forward_1m_return)
        .bind(adv_usd)
        .bind(amihud_illiq)
        .bind(momentum_12_1)
        .bind(i64::from(is_halted))
        .bind(i64::from(is_st))
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_filing(
        &self,
        record_id: &str,
        ticker: &str,
        market: &str,
        period_end: PeriodEndDate,
        filing_date: FilingDate,
        section: &str,
        source: &str,
        content: &str,
    ) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO pit_filings (
                record_id, ticker, market, period_end, filing_date, section, source, content
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (record_id) DO UPDATE SET content = EXCLUDED.content;",
        )
        .bind(record_id)
        .bind(ticker)
        .bind(market)
        .bind(period_end.as_str())
        .bind(filing_date.as_str())
        .bind(section)
        .bind(source)
        .bind(content)
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    pub async fn get_pit_snapshot(
        &self,
        as_of: &AsOfDate,
        markets: &[String],
    ) -> Result<Vec<PitSecuritySnapshot>, GateError> {
        if markets.is_empty() {
            return Ok(Vec::new());
        }
        let mkt_placeholders: Vec<String> = (1..=markets.len()).map(|i| format!("${i}")).collect();
        let base = markets.len();
        let p_list = base + 1;
        let p_delist = base + 2;
        let p_fin = base + 3;
        let p_px = base + 4;

        let sql = format!(
            "
            WITH active_sec AS (
                SELECT *
                FROM pit_securities
                WHERE market IN ({})
                  AND list_date <= ${p_list}
                  AND (delist_date IS NULL OR delist_date >= ${p_delist})
            ),
            latest_fin AS (
                SELECT f.*
                FROM pit_financials f
                INNER JOIN (
                    SELECT ticker, MAX(filing_date || '|' || period_end) AS max_key
                    FROM pit_financials
                    WHERE filing_date <= ${p_fin}
                    GROUP BY ticker
                ) m ON f.ticker = m.ticker AND (f.filing_date || '|' || f.period_end) = m.max_key
            ),
            latest_px AS (
                SELECT p.*
                FROM pit_prices p
                INNER JOIN (
                    SELECT ticker, MAX(trade_date) AS max_dt
                    FROM pit_prices
                    WHERE trade_date <= ${p_px}
                    GROUP BY ticker
                ) mp ON p.ticker = mp.ticker AND p.trade_date = mp.max_dt
            )
            SELECT
                s.ticker, s.name, s.list_date, s.delist_date, s.delist_return,
                f.record_id AS fin_record_id, f.market, f.industry, f.period_end, f.filing_date,
                f.book_value, f.market_cap, f.enterprise_value, f.revenue, f.revenue_3y_ago,
                f.gross_profit, f.ebit, f.net_income, f.interest_expense, f.cfo, f.capex,
                f.total_assets, f.prev_total_assets, f.current_assets, f.current_liabilities,
                f.total_debt, f.cash, f.retained_earnings, f.total_liabilities,
                f.shares_out, f.prev_shares_out, f.goodwill, f.prev_goodwill,
                f.sbc_expense, f.insider_net_buy_usd, f.beneish_m, f.roic, f.wacc,
                p.trade_date, p.close_price, p.forward_1m_return, p.adv_usd,
                p.amihud_illiq, p.momentum_12_1, p.is_halted, p.is_st
            FROM active_sec s
            INNER JOIN latest_fin f ON s.ticker = f.ticker
            INNER JOIN latest_px p ON s.ticker = p.ticker
            ORDER BY s.ticker ASC;
            ",
            mkt_placeholders.join(", ")
        );

        let mut q = sqlx::query(&sql);
        for m in markets {
            q = q.bind(m);
        }
        q = q
            .bind(as_of.as_str())
            .bind(as_of.as_str())
            .bind(as_of.as_str())
            .bind(as_of.as_str());

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let period_end = PeriodEndDate::new(
                r.try_get::<String, _>("period_end")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?;
            let filing_date = FilingDate::new(
                r.try_get::<String, _>("filing_date")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?;
            if !filing_date.is_visible_at(as_of) {
                return Err(GateError::Storage(format!(
                    "pit_look_ahead_violation:{} > {}",
                    filing_date.as_str(),
                    as_of.as_str()
                )));
            }
            out.push(PitSecuritySnapshot {
                ticker: r.try_get("ticker").map_err(|e| GateError::Storage(e.to_string()))?,
                name: r.try_get("name").map_err(|e| GateError::Storage(e.to_string()))?,
                market: r.try_get("market").map_err(|e| GateError::Storage(e.to_string()))?,
                industry: r.try_get("industry").map_err(|e| GateError::Storage(e.to_string()))?,
                list_date: r.try_get("list_date").map_err(|e| GateError::Storage(e.to_string()))?,
                delist_date: r.try_get("delist_date").map_err(|e| GateError::Storage(e.to_string()))?,
                delist_return: r.try_get("delist_return").map_err(|e| GateError::Storage(e.to_string()))?,
                fin_record_id: r.try_get("fin_record_id").map_err(|e| GateError::Storage(e.to_string()))?,
                period_end,
                filing_date,
                book_value: r.try_get("book_value").map_err(|e| GateError::Storage(e.to_string()))?,
                market_cap: r.try_get("market_cap").map_err(|e| GateError::Storage(e.to_string()))?,
                enterprise_value: r.try_get("enterprise_value").map_err(|e| GateError::Storage(e.to_string()))?,
                revenue: r.try_get("revenue").map_err(|e| GateError::Storage(e.to_string()))?,
                revenue_3y_ago: r.try_get("revenue_3y_ago").map_err(|e| GateError::Storage(e.to_string()))?,
                gross_profit: r.try_get("gross_profit").map_err(|e| GateError::Storage(e.to_string()))?,
                ebit: r.try_get("ebit").map_err(|e| GateError::Storage(e.to_string()))?,
                net_income: r.try_get("net_income").map_err(|e| GateError::Storage(e.to_string()))?,
                interest_expense: r.try_get("interest_expense").map_err(|e| GateError::Storage(e.to_string()))?,
                cfo: r.try_get("cfo").map_err(|e| GateError::Storage(e.to_string()))?,
                capex: r.try_get("capex").map_err(|e| GateError::Storage(e.to_string()))?,
                total_assets: r.try_get("total_assets").map_err(|e| GateError::Storage(e.to_string()))?,
                prev_total_assets: r.try_get("prev_total_assets").map_err(|e| GateError::Storage(e.to_string()))?,
                current_assets: r.try_get("current_assets").map_err(|e| GateError::Storage(e.to_string()))?,
                current_liabilities: r.try_get("current_liabilities").map_err(|e| GateError::Storage(e.to_string()))?,
                total_debt: r.try_get("total_debt").map_err(|e| GateError::Storage(e.to_string()))?,
                cash: r.try_get("cash").map_err(|e| GateError::Storage(e.to_string()))?,
                retained_earnings: r.try_get("retained_earnings").map_err(|e| GateError::Storage(e.to_string()))?,
                total_liabilities: r.try_get("total_liabilities").map_err(|e| GateError::Storage(e.to_string()))?,
                shares_out: r.try_get("shares_out").map_err(|e| GateError::Storage(e.to_string()))?,
                prev_shares_out: r.try_get("prev_shares_out").map_err(|e| GateError::Storage(e.to_string()))?,
                goodwill: r.try_get("goodwill").map_err(|e| GateError::Storage(e.to_string()))?,
                prev_goodwill: r.try_get("prev_goodwill").map_err(|e| GateError::Storage(e.to_string()))?,
                sbc_expense: r.try_get("sbc_expense").map_err(|e| GateError::Storage(e.to_string()))?,
                insider_net_buy_usd: r.try_get("insider_net_buy_usd").map_err(|e| GateError::Storage(e.to_string()))?,
                beneish_m: r.try_get("beneish_m").map_err(|e| GateError::Storage(e.to_string()))?,
                roic: r.try_get("roic").map_err(|e| GateError::Storage(e.to_string()))?,
                wacc: r.try_get("wacc").map_err(|e| GateError::Storage(e.to_string()))?,
                trade_date: r.try_get("trade_date").map_err(|e| GateError::Storage(e.to_string()))?,
                close_price: r.try_get("close_price").map_err(|e| GateError::Storage(e.to_string()))?,
                forward_1m_return: r.try_get("forward_1m_return").map_err(|e| GateError::Storage(e.to_string()))?,
                adv_usd: r.try_get("adv_usd").map_err(|e| GateError::Storage(e.to_string()))?,
                amihud_illiq: r.try_get("amihud_illiq").map_err(|e| GateError::Storage(e.to_string()))?,
                momentum_12_1: r.try_get("momentum_12_1").map_err(|e| GateError::Storage(e.to_string()))?,
                is_halted: r.try_get::<i64, _>("is_halted").map_err(|e| GateError::Storage(e.to_string()))? != 0,
                is_st: r.try_get::<i64, _>("is_st").map_err(|e| GateError::Storage(e.to_string()))? != 0,
            });
        }
        Ok(out)
    }

    pub async fn preload_backtest_source(
        &self,
        dates: &[AsOfDate],
        markets: &[String],
    ) -> Result<PreloadedPitSource, GateError> {
        let mut snapshots = BTreeMap::new();
        for dt in dates {
            let snap = self.get_pit_snapshot(dt, markets).await?;
            snapshots.insert((dt.clone(), markets.to_vec()), snap);
        }
        Ok(PreloadedPitSource { snapshots })
    }

    pub async fn get_pit_filings_for_ticker(
        &self,
        ticker: &str,
        as_of: &AsOfDate,
    ) -> Result<Vec<UntrustedFiling>, GateError> {
        let rows = sqlx::query(
            "SELECT record_id, ticker, source, period_end, filing_date, content
             FROM pit_filings
             WHERE ticker = $1 AND filing_date <= $2
             ORDER BY filing_date DESC, record_id ASC;",
        )
        .bind(ticker)
        .bind(as_of.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let pe = PeriodEndDate::new(
                r.try_get::<String, _>("period_end")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?;
            let fd = FilingDate::new(
                r.try_get::<String, _>("filing_date")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?;
            if !fd.is_visible_at(as_of) {
                return Err(GateError::Storage("pit_filing_look_ahead_violation".into()));
            }
            out.push(UntrustedFiling::new(
                r.try_get::<String, _>("record_id")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
                r.try_get::<String, _>("ticker")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
                r.try_get::<String, _>("source")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
                pe,
                fd,
                r.try_get::<String, _>("content")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?);
        }
        Ok(out)
    }

    pub async fn build_scoped_readonly_view(
        &self,
        authorized_markets: &[String],
        as_of: &AsOfDate,
    ) -> Result<ScopedReadOnlyView, GateError> {
        let sec_rows = sqlx::query("SELECT ticker, market FROM pit_securities;")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let mut ticker_markets = BTreeMap::new();
        let mut filings_by_ticker = BTreeMap::new();
        for r in sec_rows {
            let t: String = r
                .try_get("ticker")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            let m: String = r
                .try_get("market")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            if authorized_markets.contains(&m) {
                let fils = self.get_pit_filings_for_ticker(&t, as_of).await?;
                filings_by_ticker.insert(t.clone(), fils);
            }
            ticker_markets.insert(t, m);
        }

        let cal_rows =
            sqlx::query("SELECT agent_name, calibrated_confidence FROM agent_calibration;")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| GateError::Storage(e.to_string()))?;
        let mut calibrations = BTreeMap::new();
        for r in cal_rows {
            let name: String = r
                .try_get("agent_name")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            let conf: f64 = r
                .try_get("calibrated_confidence")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            calibrations.insert(name, conf);
        }

        Ok(ScopedReadOnlyView {
            markets: authorized_markets.to_vec(),
            ticker_markets,
            filings_by_ticker,
            calibrations,
        })
    }

    pub async fn verify_record_pit(
        &self,
        record_id: &str,
        as_of: &AsOfDate,
    ) -> Result<bool, GateError> {
        if let Some(r) = sqlx::query("SELECT filing_date FROM pit_filings WHERE record_id = $1;")
            .bind(record_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?
        {
            let fd = FilingDate::new(
                r.try_get::<String, _>("filing_date")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?;
            return Ok(fd.is_visible_at(as_of));
        }

        if let Some(r) = sqlx::query("SELECT filing_date FROM pit_financials WHERE record_id = $1;")
            .bind(record_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?
        {
            let fd = FilingDate::new(
                r.try_get::<String, _>("filing_date")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?;
            return Ok(fd.is_visible_at(as_of));
        }
        Ok(false)
    }

    // --- Task Persistence with Optimistic Concurrency (`version`) ---

    fn row_to_task(&self, r: &AnyRow) -> Result<TaskRecord, GateError> {
        let code: String = r
            .try_get("state_code")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let detail: Option<String> = r
            .try_get("state_detail")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let wait_owner: Option<String> = r
            .try_get("wait_owner")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let wait_deadline: Option<String> = r
            .try_get("wait_deadline")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let wait_action: Option<String> = r
            .try_get("wait_timeout_action")
            .map_err(|e| GateError::Storage(e.to_string()))?;

        let wait_meta = match (wait_owner, wait_deadline, wait_action) {
            (Some(o), Some(d), Some(a)) => Some(WaitMetadata::new(o, d, a)?),
            _ => None,
        };

        let state = TaskState::from_db_parts(&code, detail, wait_meta)?;
        let mkts_json: String = r
            .try_get("authorized_markets_json")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let mkts: Vec<String> =
            serde_json::from_str(&mkts_json).map_err(|e| GateError::Storage(e.to_string()))?;

        Ok(TaskRecord::rehydrate_from_db(
            r.try_get("task_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            r.try_get("idempotency_trigger_key")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            r.try_get("task_type")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            r.try_get("config_version")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            r.try_get("requester_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            r.try_get("acting_principal")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            r.try_get("trigger_source")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            mkts,
            state,
            r.try_get("version")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            r.try_get::<i64, _>("turns_used")
                .map_err(|e| GateError::Storage(e.to_string()))? as u32,
            UsdCents::from_cents(
                r.try_get("spend_cents")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            ),
            r.try_get::<i64, _>("retries_used")
                .map_err(|e| GateError::Storage(e.to_string()))? as u32,
            r.try_get::<i64, _>("consecutive_denials")
                .map_err(|e| GateError::Storage(e.to_string()))? as u32,
            r.try_get::<i64, _>("reworks_used")
                .map_err(|e| GateError::Storage(e.to_string()))? as u32,
            r.try_get::<i64, _>("reopened_count")
                .map_err(|e| GateError::Storage(e.to_string()))? as u32,
        )?)
    }

    pub async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, GateError> {
        let row = sqlx::query("SELECT * FROM tasks WHERE task_id = $1;")
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        match row {
            Some(r) => Ok(Some(self.row_to_task(&r)?)),
            None => Ok(None),
        }
    }

    pub async fn get_task_by_trigger_key(
        &self,
        trigger_key: &str,
    ) -> Result<Option<TaskRecord>, GateError> {
        let row = sqlx::query("SELECT * FROM tasks WHERE idempotency_trigger_key = $1;")
            .bind(trigger_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        match row {
            Some(r) => Ok(Some(self.row_to_task(&r)?)),
            None => Ok(None),
        }
    }

    pub async fn create_task(&self, task: &TaskRecord, actor: &str) -> Result<(), GateError> {
        task.validate()?;
        let wm = task.state().wait_meta();
        let mkts_json = serde_json::to_string(&task.authorized_markets)
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let ts = now_epoch_s();

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        sqlx::query(
            "INSERT INTO tasks (
                task_id, idempotency_trigger_key, task_type, config_version,
                requester_id, acting_principal, trigger_source, authorized_markets_json,
                state_code, state_detail, wait_owner, wait_deadline, wait_timeout_action,
                version, turns_used, spend_cents, retries_used, consecutive_denials,
                reworks_used, reopened_count, updated_epoch_s
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8,
                $9, $10, $11, $12, $13,
                $14, $15, $16, $17, $18, $19, $20, $21
            );",
        )
        .bind(&task.task_id)
        .bind(&task.idempotency_trigger_key)
        .bind(&task.task_type)
        .bind(&task.config_version)
        .bind(&task.requester_id)
        .bind(&task.acting_principal)
        .bind(&task.trigger_source)
        .bind(&mkts_json)
        .bind(task.state().code())
        .bind(task.state().detail().map(str::to_string))
        .bind(wm.map(|w| w.owner_id.clone()))
        .bind(wm.map(|w| w.deadline_iso.clone()))
        .bind(wm.map(|w| w.timeout_action.clone()))
        .bind(task.version())
        .bind(i64::from(task.turns_used))
        .bind(task.spend.cents())
        .bind(i64::from(task.retries_used))
        .bind(i64::from(task.consecutive_denials))
        .bind(i64::from(task.reworks_used))
        .bind(i64::from(task.reopened_count))
        .bind(ts)
        .execute(&mut *tx)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        sqlx::query(
            "INSERT INTO task_transitions (
                transition_id, task_id, from_state, to_state, version, reason, actor, created_epoch_s
            ) VALUES ($1, $2, NULL, $3, $4, 'task_queued', $5, $6);",
        )
        .bind(unique_id("TR", &task.task_id))
        .bind(&task.task_id)
        .bind(task.state().code())
        .bind(task.version())
        .bind(actor)
        .bind(ts)
        .execute(&mut *tx)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))
    }

    pub async fn save_task_optimistic(
        &self,
        task: &mut TaskRecord,
        reason: &str,
        actor: &str,
    ) -> Result<(), GateError> {
        task.validate()?;
        let expected_version = task.version();
        let next_version = expected_version + 1;
        let wm = task.state().wait_meta();
        let ts = now_epoch_s();

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let prev_row = sqlx::query("SELECT state_code FROM tasks WHERE task_id = $1;")
            .bind(&task.task_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?
            .ok_or_else(|| GateError::Storage(format!("task_not_found:{}", task.task_id)))?;
        let prev_state: String = prev_row
            .try_get("state_code")
            .map_err(|e| GateError::Storage(e.to_string()))?;

        let res = sqlx::query(
            "UPDATE tasks SET
                state_code = $1,
                state_detail = $2,
                wait_owner = $3,
                wait_deadline = $4,
                wait_timeout_action = $5,
                version = $6,
                turns_used = $7,
                spend_cents = $8,
                retries_used = $9,
                consecutive_denials = $10,
                reworks_used = $11,
                reopened_count = $12,
                updated_epoch_s = $13
             WHERE task_id = $14 AND version = $15;",
        )
        .bind(task.state().code())
        .bind(task.state().detail().map(str::to_string))
        .bind(wm.map(|w| w.owner_id.clone()))
        .bind(wm.map(|w| w.deadline_iso.clone()))
        .bind(wm.map(|w| w.timeout_action.clone()))
        .bind(next_version)
        .bind(i64::from(task.turns_used))
        .bind(task.spend.cents())
        .bind(i64::from(task.retries_used))
        .bind(i64::from(task.consecutive_denials))
        .bind(i64::from(task.reworks_used))
        .bind(i64::from(task.reopened_count))
        .bind(ts)
        .bind(&task.task_id)
        .bind(expected_version)
        .execute(&mut *tx)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        if res.rows_affected() != 1 {
            return Err(GateError::Storage(format!(
                "optimistic_lock_conflict:task={} expected_version={expected_version}",
                task.task_id
            )));
        }

        sqlx::query(
            "INSERT INTO task_transitions (
                transition_id, task_id, from_state, to_state, version, reason, actor, created_epoch_s
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8);",
        )
        .bind(unique_id("TR", &task.task_id))
        .bind(&task.task_id)
        .bind(&prev_state)
        .bind(task.state().code())
        .bind(next_version)
        .bind(reason)
        .bind(actor)
        .bind(ts)
        .execute(&mut *tx)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        task.advance_persisted_version(next_version);
        Ok(())
    }

    // --- Contracts, Authoritative DB Evaluations & Drafts ---

    pub async fn save_contract(&self, c: &TaskContract) -> Result<(), GateError> {
        c.validate()?;
        let json = serde_json::to_string(c).map_err(|e| GateError::Storage(e.to_string()))?;
        sqlx::query(
            "INSERT INTO contracts (task_id, contract_json, created_epoch_s)
             VALUES ($1, $2, $3)
             ON CONFLICT (task_id) DO UPDATE SET contract_json = EXCLUDED.contract_json;",
        )
        .bind(&c.task_id)
        .bind(&json)
        .bind(now_epoch_s())
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    pub async fn get_contract(&self, task_id: &str) -> Result<Option<TaskContract>, GateError> {
        let row = sqlx::query("SELECT contract_json FROM contracts WHERE task_id = $1;")
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        match row {
            Some(r) => {
                let raw: String = r
                    .try_get("contract_json")
                    .map_err(|e| GateError::Storage(e.to_string()))?;
                let c: TaskContract =
                    serde_json::from_str(&raw).map_err(|e| GateError::Storage(e.to_string()))?;
                c.validate()?;
                Ok(Some(c))
            }
            None => Ok(None),
        }
    }

    pub async fn save_research_evaluations(
        &self,
        task_id: &str,
        decisions: &[CommitteeDecision],
        quant_by_ticker: &BTreeMap<String, QuantSecurityResult>,
    ) -> Result<(), GateError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        sqlx::query("DELETE FROM worker_evidences WHERE task_id = $1;")
            .bind(task_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

        for dec in decisions {
            let q = quant_by_ticker
                .get(&dec.ticker)
                .ok_or_else(|| GateError::Storage(format!("missing_quant:{}", dec.ticker)))?;
            let reasons_json = serde_json::to_string(&dec.veto_reasons)
                .map_err(|e| GateError::Storage(e.to_string()))?;

            sqlx::query(
                "INSERT INTO security_evaluations (
                    task_id, ticker, as_of_date, industry, adv_usd,
                    quant_score, consensus_score, dispersion, hard_veto,
                    veto_reasons_json, escalate_review
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (task_id, ticker) DO UPDATE SET
                    quant_score = EXCLUDED.quant_score,
                    consensus_score = EXCLUDED.consensus_score,
                    dispersion = EXCLUDED.dispersion,
                    hard_veto = EXCLUDED.hard_veto,
                    veto_reasons_json = EXCLUDED.veto_reasons_json,
                    escalate_review = EXCLUDED.escalate_review;",
            )
            .bind(task_id)
            .bind(&dec.ticker)
            .bind(dec.as_of_date.as_str())
            .bind(&q.industry)
            .bind(q.adv_usd)
            .bind(dec.quant_base_score)
            .bind(dec.consensus_score)
            .bind(dec.dispersion)
            .bind(i64::from(dec.hard_veto))
            .bind(&reasons_json)
            .bind(i64::from(dec.escalate_review))
            .execute(&mut *tx)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

            for out in &dec.worker_outputs {
                let out_json =
                    serde_json::to_string(out).map_err(|e| GateError::Storage(e.to_string()))?;
                sqlx::query(
                    "INSERT INTO worker_evidences (evidence_id, task_id, ticker, agent, output_json)
                     VALUES ($1, $2, $3, $4, $5);",
                )
                .bind(unique_id(
                    "EV",
                    &format!("{task_id}:{}:{}", dec.ticker, out.agent),
                ))
                .bind(task_id)
                .bind(&dec.ticker)
                .bind(&out.agent)
                .bind(&out_json)
                .execute(&mut *tx)
                .await
                .map_err(|e| GateError::Storage(e.to_string()))?;
            }
        }
        tx.commit()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))
    }

    pub async fn get_stored_evaluations(
        &self,
        task_id: &str,
    ) -> Result<BTreeMap<String, StoredEvaluation>, GateError> {
        let rows = sqlx::query("SELECT * FROM security_evaluations WHERE task_id = $1;")
            .bind(task_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

        let mut map = BTreeMap::new();
        for r in rows {
            let ticker: String = r
                .try_get("ticker")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            let as_of = AsOfDate::new(
                r.try_get::<String, _>("as_of_date")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            )?;
            let reasons_raw: String = r
                .try_get("veto_reasons_json")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            let veto_reasons: Vec<String> = serde_json::from_str(&reasons_raw)
                .map_err(|e| GateError::Storage(e.to_string()))?;
            map.insert(
                ticker.clone(),
                StoredEvaluation {
                    ticker,
                    as_of_date: as_of,
                    industry: r
                        .try_get("industry")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    adv_usd: r
                        .try_get("adv_usd")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    quant_score: r
                        .try_get("quant_score")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    consensus_score: r
                        .try_get("consensus_score")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    dispersion: r
                        .try_get("dispersion")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    hard_veto: r
                        .try_get::<i64, _>("hard_veto")
                        .map_err(|e| GateError::Storage(e.to_string()))?
                        != 0,
                    veto_reasons,
                    escalate_review: r
                        .try_get::<i64, _>("escalate_review")
                        .map_err(|e| GateError::Storage(e.to_string()))?
                        != 0,
                },
            );
        }
        Ok(map)
    }

    pub async fn get_vetoed_tickers_from_db(
        &self,
        task_id: &str,
    ) -> Result<BTreeSet<String>, GateError> {
        let rows = sqlx::query(
            "SELECT ticker FROM security_evaluations WHERE task_id = $1 AND hard_veto = 1;",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        let mut set = BTreeSet::new();
        for r in rows {
            set.insert(
                r.try_get("ticker")
                    .map_err(|e| GateError::Storage(e.to_string()))?,
            );
        }
        Ok(set)
    }

    pub async fn get_worker_evidences_from_db(
        &self,
        task_id: &str,
    ) -> Result<Vec<AgentEvidenceOutput>, GateError> {
        let rows = sqlx::query(
            "SELECT output_json FROM worker_evidences WHERE task_id = $1 ORDER BY evidence_id ASC;",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let raw: String = r
                .try_get("output_json")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            let ev: AgentEvidenceOutput =
                serde_json::from_str(&raw).map_err(|e| GateError::Storage(e.to_string()))?;
            ev.validate()?;
            out.push(ev);
        }
        Ok(out)
    }

    /// Capability-guarded rebalance draft write (`Requires &VerifiedGateTicket`).
    pub async fn write_rebalance_draft(
        &self,
        _ticket: &VerifiedGateTicket,
        task_id: &str,
        payload: &PortfolioActionPayload,
    ) -> Result<String, GateError> {
        let weights_json = serde_json::to_string(&payload.weights_bps)
            .map_err(|e| GateError::Storage(e.to_string()))?;
        sqlx::query(
            "INSERT INTO rebalance_drafts (draft_id, task_id, as_of_date, weights_json, created_epoch_s)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (draft_id) DO UPDATE SET weights_json = EXCLUDED.weights_json;",
        )
        .bind(&payload.draft_id)
        .bind(task_id)
        .bind(payload.as_of_date.as_str())
        .bind(&weights_json)
        .bind(now_epoch_s())
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(payload.draft_id.clone())
    }

    pub async fn readback_rebalance_draft(
        &self,
        draft_id: &str,
    ) -> Result<Option<StoredDraft>, GateError> {
        let row = sqlx::query("SELECT * FROM rebalance_drafts WHERE draft_id = $1;")
            .bind(draft_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let Some(r) = row else {
            return Ok(None);
        };
        let as_of = AsOfDate::new(
            r.try_get::<String, _>("as_of_date")
                .map_err(|e| GateError::Storage(e.to_string()))?,
        )?;
        let w_raw: String = r
            .try_get("weights_json")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let weights: BTreeMap<String, WeightBps> =
            serde_json::from_str(&w_raw).map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(Some(StoredDraft {
            draft_id: r
                .try_get("draft_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            task_id: r
                .try_get("task_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            as_of_date: as_of,
            weights,
        }))
    }

    pub async fn get_latest_draft_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<StoredDraft>, GateError> {
        let row = sqlx::query(
            "SELECT draft_id FROM rebalance_drafts WHERE task_id = $1 ORDER BY created_epoch_s DESC LIMIT 1;",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        let Some(r) = row else {
            return Ok(None);
        };
        let did: String = r
            .try_get("draft_id")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        self.readback_rebalance_draft(&did).await
    }

    // --- DB-Backed Approval Queries & Human Decision API (`Requirement #1`) ---

    fn row_to_approval(&self, r: &AnyRow) -> Result<StoredApproval, GateError> {
        let status_raw: String = r
            .try_get("status")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let status = ApprovalStatus::from_db_str(&status_raw)?;
        Ok(StoredApproval {
            approval_id: r
                .try_get("approval_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            task_id: r
                .try_get("task_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            requester_id: r
                .try_get("requester_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            tool_name: r
                .try_get("tool_name")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            target: r
                .try_get("target")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            payload_hash: r
                .try_get("payload_hash")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            args_json: r
                .try_get("args_json")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            status,
            approver_id: r
                .try_get("approver_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            decision_note: r
                .try_get("decision_note")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            expires_at_epoch_s: r
                .try_get("expires_at_epoch_s")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            _db_only: (),
        })
    }

    pub async fn get_approval(&self, approval_id: &str) -> Result<Option<StoredApproval>, GateError> {
        let row = sqlx::query("SELECT * FROM approvals WHERE approval_id = $1;")
            .bind(approval_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        match row {
            Some(r) => Ok(Some(self.row_to_approval(&r)?)),
            None => Ok(None),
        }
    }

    pub async fn decide_approval(
        &self,
        approval_id: &str,
        approver_id: &str,
        approved: bool,
        note: Option<&str>,
        config_admin_id: &str,
    ) -> Result<StoredApproval, GateError> {
        let rec = self
            .get_approval(approval_id)
            .await?
            .ok_or_else(|| PreDispatchError::ApprovalNotFound(approval_id.into()))?;
        if rec.status != ApprovalStatus::Pending {
            return Err(PreDispatchError::ApprovalNotApproved(rec.status.as_str().into()).into());
        }
        if approver_id == rec.requester_id {
            return Err(
                PreDispatchError::SeparationOfDutiesViolation("requester_cannot_approve".into())
                    .into(),
            );
        }
        if approver_id == config_admin_id {
            return Err(
                PreDispatchError::SeparationOfDutiesViolation("admin_cannot_approve".into()).into(),
            );
        }
        let now = now_epoch_s();
        if rec.expires_at_epoch_s <= now {
            sqlx::query("UPDATE approvals SET status = 'EXPIRED' WHERE approval_id = $1;")
                .bind(approval_id)
                .execute(&self.pool)
                .await
                .map_err(|e| GateError::Storage(e.to_string()))?;
            return Err(PreDispatchError::ApprovalExpired(approval_id.into()).into());
        }

        let new_status = if approved {
            ApprovalStatus::Approved.as_str()
        } else {
            ApprovalStatus::Rejected.as_str()
        };
        let res = sqlx::query(
            "UPDATE approvals
             SET status = $1, approver_id = $2, decision_note = $3, decided_at_epoch_s = $4
             WHERE approval_id = $5 AND status = 'PENDING';",
        )
        .bind(new_status)
        .bind(approver_id)
        .bind(note)
        .bind(now)
        .bind(approval_id)
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        if res.rows_affected() != 1 {
            return Err(GateError::Storage("approval_concurrent_state_change".into()));
        }
        self.get_approval(approval_id)
            .await?
            .ok_or_else(|| GateError::Storage("approval_disappeared".into()))
    }

    // --- Action Ledger Queries ---

    fn row_to_intent(&self, r: &AnyRow) -> Result<ActionIntentRecord, GateError> {
        let status_raw: String = r
            .try_get("status")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let status = ActionIntentStatus::from_db_str(&status_raw)?;
        Ok(ActionIntentRecord {
            idempotency_key: r
                .try_get("idempotency_key")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            task_id: r
                .try_get("task_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            tool_name: r
                .try_get("tool_name")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            target: r
                .try_get("target")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            args_json: r
                .try_get("args_json")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            payload_hash: r
                .try_get("payload_hash")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            status,
            backend_receipt_id: r
                .try_get("backend_receipt_id")
                .map_err(|e| GateError::Storage(e.to_string()))?,
            readback_verified: r
                .try_get::<i64, _>("readback_verified")
                .map_err(|e| GateError::Storage(e.to_string()))?
                != 0,
        })
    }

    pub async fn list_task_actions(
        &self,
        task_id: &str,
    ) -> Result<Vec<ActionIntentRecord>, GateError> {
        let rows = sqlx::query(
            "SELECT * FROM action_ledger WHERE task_id = $1 ORDER BY updated_epoch_s ASC, idempotency_key ASC;",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        rows.iter().map(|r| self.row_to_intent(r)).collect()
    }

    /// Re-open a `CONSUMED` approval back to `APPROVED` ONLY when `reconcile_before_retry` proves
    /// the prior `DISPATCHED`/`UNKNOWN` call dropped BEFORE reaching the external broker!
    pub async fn reset_unexecuted_intent_for_retry(
        &self,
        idempotency_key: &str,
    ) -> Result<(), GateError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

        let row = sqlx::query("SELECT approval_id FROM action_ledger WHERE idempotency_key = $1;")
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

        if let Some(r) = row {
            let aid: Option<String> = r
                .try_get("approval_id")
                .map_err(|e| GateError::Storage(e.to_string()))?;
            if let Some(aid_val) = aid {
                sqlx::query(
                    "UPDATE approvals SET status = 'APPROVED', consumed_at_epoch_s = NULL
                     WHERE approval_id = $1 AND status = 'CONSUMED';",
                )
                .bind(&aid_val)
                .execute(&mut *tx)
                .await
                .map_err(|e| GateError::Storage(e.to_string()))?;
            }
        }

        sqlx::query(
            "UPDATE action_ledger SET status = 'PLANNED', updated_epoch_s = $1 WHERE idempotency_key = $2;",
        )
        .bind(now_epoch_s())
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))
    }

    // --- Cost Ledger, Calibration & Curated Memory ---

    pub async fn record_cost(
        &self,
        task_id: &str,
        category: &str,
        amount: UsdCents,
        human_minutes: f64,
        note: &str,
    ) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO cost_ledger (
                cost_id, task_id, category, amount_cents, human_minutes, note, created_epoch_s
            ) VALUES ($1, $2, $3, $4, $5, $6, $7);",
        )
        .bind(unique_id("COST", task_id))
        .bind(task_id)
        .bind(category)
        .bind(amount.cents())
        .bind(human_minutes)
        .bind(note)
        .bind(now_epoch_s())
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    pub async fn set_agent_calibration(
        &self,
        agent_name: &str,
        calibrated_confidence: f64,
        historical_ic: f64,
    ) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO agent_calibration (agent_name, calibrated_confidence, historical_ic)
             VALUES ($1, $2, $3)
             ON CONFLICT (agent_name) DO UPDATE SET
                 calibrated_confidence = EXCLUDED.calibrated_confidence,
                 historical_ic = EXCLUDED.historical_ic;",
        )
        .bind(agent_name)
        .bind(calibrated_confidence)
        .bind(historical_ic)
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    pub async fn add_curated_memory(
        &self,
        memory_id: &str,
        source_task_id: &str,
        summary: &str,
    ) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO curated_memory (memory_id, source_task_id, summary, status, reviewed_by)
             VALUES ($1, $2, $3, 'pending_review', NULL)
             ON CONFLICT (memory_id) DO UPDATE SET summary = EXCLUDED.summary;",
        )
        .bind(memory_id)
        .bind(source_task_id)
        .bind(summary)
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }

    pub async fn list_curated_memory(
        &self,
        status_filter: Option<&str>,
    ) -> Result<Vec<CuratedMemoryItem>, GateError> {
        let rows = match status_filter {
            Some(st) => sqlx::query("SELECT * FROM curated_memory WHERE status = $1;")
                .bind(st)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| GateError::Storage(e.to_string()))?,
            None => sqlx::query("SELECT * FROM curated_memory;")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| GateError::Storage(e.to_string()))?,
        };
        rows.iter()
            .map(|r| {
                Ok(CuratedMemoryItem {
                    memory_id: r
                        .try_get("memory_id")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    source_task_id: r
                        .try_get("source_task_id")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    summary: r
                        .try_get("summary")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    status: r
                        .try_get("status")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                    reviewed_by: r
                        .try_get("reviewed_by")
                        .map_err(|e| GateError::Storage(e.to_string()))?,
                })
            })
            .collect()
    }

    pub async fn calculate_unit_economics(
        &self,
        human_hourly_rate_cents: i64,
    ) -> Result<UnitEconomicsSummary, GateError> {
        let cost_row = sqlx::query(
            "SELECT COALESCE(SUM(amount_cents), 0) AS api_cents,
                    COALESCE(SUM(human_minutes), 0.0) AS human_min
             FROM cost_ledger;",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        let api_cents: i64 = cost_row
            .try_get("api_cents")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let human_min: f64 = cost_row
            .try_get("human_min")
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let human_cents = ((human_min / 60.0) * (human_hourly_rate_cents as f64)).round() as i64;
        let total_cents = api_cents.saturating_add(human_cents);

        let completed_row = sqlx::query(
            "SELECT COUNT(*) AS cnt FROM tasks WHERE state_code = 'COMPLETED' AND reopened_count = 0;",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        let verified_cnt = completed_row
            .try_get::<i64, _>("cnt")
            .map_err(|e| GateError::Storage(e.to_string()))? as u64;

        let total_row = sqlx::query("SELECT COUNT(*) AS cnt FROM tasks;")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        let total_tasks = total_row
            .try_get::<i64, _>("cnt")
            .map_err(|e| GateError::Storage(e.to_string()))? as u64;

        let cpv = if verified_cnt > 0 {
            Some(UsdCents::from_cents(total_cents / (verified_cnt as i64)).to_dollars())
        } else {
            None
        };

        Ok(UnitEconomicsSummary {
            total_tasks,
            verified_completed_non_reopened: verified_cnt,
            total_api_and_tool_usd: UsdCents::from_cents(api_cents).to_dollars(),
            total_human_minutes: human_min,
            total_all_in_cost_usd: UsdCents::from_cents(total_cents).to_dollars(),
            cost_per_verified_task_usd: cpv,
        })
    }
}

impl GateLedger for DbStore {
    async fn save_task(
        &self,
        task: &mut TaskRecord,
        reason: &str,
        actor: &str,
    ) -> Result<(), GateError> {
        self.save_task_optimistic(task, reason, actor).await
    }

    async fn get_action_intent(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<ActionIntentRecord>, GateError> {
        let row = sqlx::query("SELECT * FROM action_ledger WHERE idempotency_key = $1;")
            .bind(idempotency_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;
        match row {
            Some(r) => Ok(Some(self.row_to_intent(&r)?)),
            None => Ok(None),
        }
    }

    async fn create_pending_approval(
        &self,
        task_id: &str,
        requester_id: &str,
        tool_name: &str,
        target: &str,
        payload_hash: &str,
        canonical_args: &str,
    ) -> Result<String, GateError> {
        let now = now_epoch_s();
        let approval_id = unique_id("APR", &format!("{task_id}:{tool_name}:{payload_hash}"));
        sqlx::query(
            "INSERT INTO approvals (
                approval_id, task_id, requester_id, tool_name, target,
                payload_hash, args_json, status, approver_id, decision_note,
                expires_at_epoch_s, created_at_epoch_s, decided_at_epoch_s, consumed_at_epoch_s
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, 'PENDING', NULL, NULL,
                $8, $9, NULL, NULL
            );",
        )
        .bind(&approval_id)
        .bind(task_id)
        .bind(requester_id)
        .bind(tool_name)
        .bind(target)
        .bind(payload_hash)
        .bind(canonical_args)
        .bind(now + self.approval_ttl_seconds)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(approval_id)
    }

    async fn consume_approval_and_mark_dispatched(
        &self,
        approval_id: Option<&str>,
        task: &TaskRecord,
        config: &TaskTypeConfig,
        intent: &ActionIntentRecord,
    ) -> Result<(), GateError> {
        let now = now_epoch_s();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

        if let Some(aid) = approval_id {
            let row_opt = sqlx::query("SELECT * FROM approvals WHERE approval_id = $1;")
                .bind(aid)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| GateError::Storage(e.to_string()))?;
            let Some(row) = row_opt else {
                return Err(PreDispatchError::ApprovalNotFound(aid.into()).into());
            };
            let rec = self.row_to_approval(&row)?;

            if rec.task_id != task.task_id {
                return Err(PreDispatchError::ApprovalTaskMismatch.into());
            }
            if rec.status == ApprovalStatus::Rejected {
                return Err(PreDispatchError::ApprovalRejected(aid.into()).into());
            }
            if rec.status == ApprovalStatus::Consumed {
                return Err(PreDispatchError::ApprovalAlreadyUsed(aid.into()).into());
            }
            if rec.status != ApprovalStatus::Approved {
                return Err(PreDispatchError::ApprovalNotApproved(rec.status.as_str().into()).into());
            }
            if rec.expires_at_epoch_s <= now {
                return Err(PreDispatchError::ApprovalExpired(aid.into()).into());
            }
            if rec.payload_hash != intent.payload_hash {
                return Err(PreDispatchError::ApprovalPayloadMismatch.into());
            }
            let Some(approver) = rec.approver_id.as_deref() else {
                return Err(
                    PreDispatchError::SeparationOfDutiesViolation("missing_approver".into()).into(),
                );
            };
            if approver == task.requester_id {
                return Err(PreDispatchError::SeparationOfDutiesViolation(
                    "requester_cannot_approve".into(),
                )
                .into());
            }
            if approver == config.config_admin_id {
                return Err(PreDispatchError::SeparationOfDutiesViolation(
                    "admin_cannot_approve".into(),
                )
                .into());
            }

            let upd = sqlx::query(
                "UPDATE approvals
                 SET status = 'CONSUMED', consumed_at_epoch_s = $1
                 WHERE approval_id = $2 AND status = 'APPROVED';",
            )
            .bind(now)
            .bind(aid)
            .execute(&mut *tx)
            .await
            .map_err(|e| GateError::Storage(e.to_string()))?;

            if upd.rows_affected() != 1 {
                return Err(PreDispatchError::ApprovalAlreadyUsed(aid.into()).into());
            }
        }

        sqlx::query(
            "INSERT INTO action_ledger (
                idempotency_key, task_id, tool_name, target, args_json,
                payload_hash, approval_id, status, backend_receipt_id,
                readback_verified, updated_epoch_s
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (idempotency_key) DO UPDATE SET
                approval_id = EXCLUDED.approval_id,
                status = EXCLUDED.status,
                backend_receipt_id = EXCLUDED.backend_receipt_id,
                readback_verified = EXCLUDED.readback_verified,
                updated_epoch_s = EXCLUDED.updated_epoch_s;",
        )
        .bind(&intent.idempotency_key)
        .bind(&intent.task_id)
        .bind(&intent.tool_name)
        .bind(&intent.target)
        .bind(&intent.args_json)
        .bind(&intent.payload_hash)
        .bind(approval_id)
        .bind(intent.status.as_str())
        .bind(&intent.backend_receipt_id)
        .bind(i64::from(intent.readback_verified))
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| GateError::Storage(e.to_string()))
    }

    async fn update_action_post_dispatch(
        &self,
        intent: &ActionIntentRecord,
        approval_id: Option<&str>,
    ) -> Result<(), GateError> {
        sqlx::query(
            "INSERT INTO action_ledger (
                idempotency_key, task_id, tool_name, target, args_json,
                payload_hash, approval_id, status, backend_receipt_id,
                readback_verified, updated_epoch_s
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (idempotency_key) DO UPDATE SET
                status = EXCLUDED.status,
                backend_receipt_id = EXCLUDED.backend_receipt_id,
                readback_verified = EXCLUDED.readback_verified,
                updated_epoch_s = EXCLUDED.updated_epoch_s;",
        )
        .bind(&intent.idempotency_key)
        .bind(&intent.task_id)
        .bind(&intent.tool_name)
        .bind(&intent.target)
        .bind(&intent.args_json)
        .bind(&intent.payload_hash)
        .bind(approval_id)
        .bind(intent.status.as_str())
        .bind(&intent.backend_receipt_id)
        .bind(i64::from(intent.readback_verified))
        .bind(now_epoch_s())
        .execute(&self.pool)
        .await
        .map_err(|e| GateError::Storage(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_core::{AutonomyLevel, BudgetLimits, RiskTier, ShapeLevel};

    #[tokio::test]
    async fn optimistic_concurrency_and_corrupt_status_rejection() {
        let store = DbStore::connect(":memory:").await.unwrap();
        let t = TaskRecord::new_queued(
            "T-OPT",
            "trig-opt",
            "rebalance",
            "2.0",
            "alice",
            "svc",
            "cron",
            vec!["US".into()],
        )
        .unwrap();
        store.create_task(&t, "test").await.unwrap();

        let mut copy_a = store.get_task("T-OPT").await.unwrap().unwrap();
        let mut copy_b = store.get_task("T-OPT").await.unwrap().unwrap();
        copy_a.transition(TaskState::Running).unwrap();
        copy_b
            .transition(TaskState::Cancelled {
                reason: "race".into(),
            })
            .unwrap();

        store
            .save_task_optimistic(&mut copy_a, "win", "worker_a")
            .await
            .unwrap();
        assert_eq!(copy_a.version(), 2);

        let conflict = store
            .save_task_optimistic(&mut copy_b, "lose", "worker_b")
            .await
            .unwrap_err();
        assert!(conflict.to_string().contains("optimistic_lock_conflict"));

        // Corrupt state_code in DB -> get_task MUST return Err, never default to Failed!
        sqlx::query("UPDATE tasks SET state_code = 'NOT_A_VALID_STATE' WHERE task_id = 'T-OPT';")
            .execute(&store.pool)
            .await
            .unwrap();
        let parse_err = store.get_task("T-OPT").await.unwrap_err();
        assert!(parse_err.to_string().contains("unknown_task_state"));
    }

    #[tokio::test]
    async fn atomic_db_approval_consumption_and_separation_of_duties() {
        let store = DbStore::connect(":memory:").await.unwrap();
        let t = TaskRecord::new_queued(
            "T-APR",
            "trig-apr",
            "rebalance",
            "2.0",
            "alice",
            "svc",
            "cron",
            vec!["US".into()],
        )
        .unwrap();
        store.create_task(&t, "test").await.unwrap();

        let aid = store
            .create_pending_approval("T-APR", "alice", "submit_paper_orders", "tgt", "hash-1", "{}")
            .await
            .unwrap();

        // Requester self-approval rejected
        assert!(store
            .decide_approval(&aid, "alice", true, None, "admin")
            .await
            .is_err());
        // Config admin approval rejected
        assert!(store
            .decide_approval(&aid, "admin", true, None, "admin")
            .await
            .is_err());
        // Independent approver succeeds
        let decided = store
            .decide_approval(&aid, "bob", true, Some("lgtm"), "admin")
            .await
            .unwrap();
        assert_eq!(decided.status, ApprovalStatus::Approved);

        let cfg = TaskTypeConfig {
            task_type: "rebalance".into(),
            version: "2.0".into(),
            shape_level: ShapeLevel::MultiAgent,
            shape_rationale: "r".into(),
            autonomy_level: AutonomyLevel::PerActionApproval,
            config_admin_id: "admin".into(),
            allowed_tools: BTreeMap::from([("submit_paper_orders".into(), RiskTier::R3)]),
            budget_limits: BudgetLimits::default(),
            done_criteria: vec!["ok".into()],
        };
        let intent = ActionIntentRecord {
            idempotency_key: "idem-1".into(),
            task_id: "T-APR".into(),
            tool_name: "submit_paper_orders".into(),
            target: "tgt".into(),
            args_json: "{}".into(),
            payload_hash: "hash-1".into(),
            status: ActionIntentStatus::Dispatched,
            backend_receipt_id: None,
            readback_verified: false,
        };
        store
            .consume_approval_and_mark_dispatched(Some(&aid), &t, &cfg, &intent)
            .await
            .unwrap();

        let after = store.get_approval(&aid).await.unwrap().unwrap();
        assert_eq!(after.status, ApprovalStatus::Consumed);

        // Second attempt to consume same approval_id fails with ApprovalAlreadyUsed!
        let intent2 = ActionIntentRecord {
            idempotency_key: "idem-2".into(),
            ..intent
        };
        let err = store
            .consume_approval_and_mark_dispatched(Some(&aid), &t, &cfg, &intent2)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            GateError::PreDispatch(PreDispatchError::ApprovalAlreadyUsed(_))
        ));
    }
}
