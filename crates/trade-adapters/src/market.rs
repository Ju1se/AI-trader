//! Point-in-time market data from a fixture file such as `fixtures/market.json`.
//!
//! For each date it shows only what was public then: securities listed and not yet
//! delisted, each one's latest financials filed by that date, and its latest price on or
//! before it. Realised forward returns are kept apart and only the backtest asks for them.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value};
use trade_core::{
    AsOfDate, FilingDate, PeriodEndDate, PitMarketSource, PitSecuritySnapshot, ReadOnlyPitView,
    UntrustedFiling,
};

/// Fields the point-in-time rules look at are typed; `rest` keeps the others as they are.
#[derive(Debug, Deserialize)]
struct Security {
    ticker: String,
    market: String,
    list_date: String,
    delist_date: Option<String>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct Financial {
    record_id: String,
    ticker: String,
    filing_date: String,
    period_end: String,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct Price {
    ticker: String,
    trade_date: String,
    /// Known only a month later. Never part of a snapshot.
    forward_1m_return: f64,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct Filing {
    record_id: String,
    ticker: String,
    source: String,
    period_end: String,
    filing_date: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    securities: Vec<Security>,
    financials: Vec<Financial>,
    prices: Vec<Price>,
    filings: Vec<Filing>,
    #[serde(default)]
    agent_calibration: BTreeMap<String, f64>,
}

/// Market data loaded from a fixture. Cheap to clone.
#[derive(Debug, Clone)]
pub struct FixtureMarket(Arc<Fixture>);

impl FixtureMarket {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(json: &str) -> Result<Self, String> {
        let mut fixture: Fixture =
            serde_json::from_str(json).map_err(|e| format!("bad market fixture: {e}"))?;
        fixture.securities.sort_by(|a, b| a.ticker.cmp(&b.ticker));
        Ok(Self(Arc::new(fixture)))
    }

    /// Starting calibration per agent, to seed the store with.
    pub fn seed_calibrations(&self) -> &BTreeMap<String, f64> {
        &self.0.agent_calibration
    }

    /// What a worker may read: filings in `markets` only, and the given calibrations.
    pub fn view(&self, markets: Vec<String>, calibrations: BTreeMap<String, f64>) -> ScopedView {
        ScopedView {
            market: self.clone(),
            markets,
            calibrations,
        }
    }

    /// Securities active on `as_of` in `markets`, with their latest financials filed by
    /// then and their latest price on or before it, by ticker.
    fn active(&self, as_of: &AsOfDate, markets: &[String]) -> Vec<(&Security, &Financial, &Price)> {
        let day = as_of.as_str();
        self.0
            .securities
            .iter()
            .filter(|s| markets.contains(&s.market) && s.list_date.as_str() <= day)
            .filter(|s| s.delist_date.as_deref().is_none_or(|d| d >= day))
            .filter_map(|s| {
                let financial = self
                    .0
                    .financials
                    .iter()
                    .filter(|f| f.ticker == s.ticker && f.filing_date.as_str() <= day)
                    .max_by(|a, b| {
                        (&a.filing_date, &a.period_end).cmp(&(&b.filing_date, &b.period_end))
                    })?;
                let price = self
                    .0
                    .prices
                    .iter()
                    .filter(|p| p.ticker == s.ticker && p.trade_date.as_str() <= day)
                    .max_by(|a, b| a.trade_date.cmp(&b.trade_date))?;
                Some((s, financial, price))
            })
            .collect()
    }

    /// Whether a filing or financial record exists and was public on `as_of`. Used to check
    /// what the specialists cite.
    pub fn record_public(&self, record_id: &str, as_of: &AsOfDate) -> bool {
        let filed = |date: &str| date <= as_of.as_str();
        self.0
            .filings
            .iter()
            .any(|f| f.record_id == record_id && filed(&f.filing_date))
            || self
                .0
                .financials
                .iter()
                .any(|f| f.record_id == record_id && filed(&f.filing_date))
    }

    fn market_of(&self, ticker: &str) -> Option<&str> {
        self.0
            .securities
            .iter()
            .find(|s| s.ticker == ticker)
            .map(|s| s.market.as_str())
    }

    fn filings(&self, ticker: &str, as_of: &AsOfDate) -> Result<Vec<UntrustedFiling>, String> {
        let mut visible: Vec<&Filing> = self
            .0
            .filings
            .iter()
            .filter(|f| f.ticker == ticker && f.filing_date.as_str() <= as_of.as_str())
            .collect();
        visible.sort_by(|a, b| {
            b.filing_date
                .cmp(&a.filing_date)
                .then_with(|| a.record_id.cmp(&b.record_id))
        });
        visible
            .into_iter()
            .map(|f| {
                let period_end = PeriodEndDate::new(&f.period_end).map_err(|e| e.0)?;
                let filing_date = FilingDate::new(&f.filing_date).map_err(|e| e.0)?;
                UntrustedFiling::new(
                    &f.record_id,
                    &f.ticker,
                    &f.source,
                    period_end,
                    filing_date,
                    &f.content,
                )
                .map_err(|e| e.0)
            })
            .collect()
    }
}

/// One snapshot row: the security's name and delisting, the financials' market, industry
/// and figures, and the price, as in the store's old point-in-time query.
fn snapshot_of(s: &Security, f: &Financial, p: &Price) -> Result<PitSecuritySnapshot, String> {
    let mut row = s.rest.clone();
    row.extend(f.rest.clone());
    row.extend(p.rest.clone());
    for (key, value) in [
        ("ticker", Value::from(s.ticker.as_str())),
        ("list_date", Value::from(s.list_date.as_str())),
        ("delist_date", Value::from(s.delist_date.clone())),
        ("fin_record_id", Value::from(f.record_id.as_str())),
        ("period_end", Value::from(f.period_end.as_str())),
        ("filing_date", Value::from(f.filing_date.as_str())),
        ("trade_date", Value::from(p.trade_date.as_str())),
    ] {
        row.insert(key.into(), value);
    }
    serde_json::from_value(Value::Object(row)).map_err(|e| format!("bad row for {}: {e}", s.ticker))
}

impl PitMarketSource for FixtureMarket {
    fn get_pit_snapshot(
        &self,
        as_of: &AsOfDate,
        markets: &[String],
    ) -> Result<Vec<PitSecuritySnapshot>, String> {
        self.active(as_of, markets)
            .into_iter()
            .map(|(s, f, p)| snapshot_of(s, f, p))
            .collect()
    }

    fn get_forward_returns(
        &self,
        as_of: &AsOfDate,
        markets: &[String],
    ) -> Result<BTreeMap<String, f64>, String> {
        Ok(self
            .active(as_of, markets)
            .into_iter()
            .map(|(s, _, p)| (s.ticker.clone(), p.forward_1m_return))
            .collect())
    }
}

/// A worker's read-only window: filings in its authorised markets, and calibrations.
#[derive(Debug, Clone)]
pub struct ScopedView {
    market: FixtureMarket,
    markets: Vec<String>,
    calibrations: BTreeMap<String, f64>,
}

impl ReadOnlyPitView for ScopedView {
    fn authorized_markets(&self) -> &[String] {
        &self.markets
    }

    fn get_pit_filings(
        &self,
        ticker: &str,
        as_of: &AsOfDate,
    ) -> Result<Vec<UntrustedFiling>, String> {
        match self.market.market_of(ticker) {
            None => Err(format!("unknown_ticker:{ticker}")),
            Some(m) if !self.markets.iter().any(|a| a == m) => Err(format!(
                "permission_denied:{ticker} is in {m}, outside {:?}",
                self.markets
            )),
            Some(_) => self.market.filings(ticker, as_of),
        }
    }

    fn get_agent_calibration(&self, agent_name: &str) -> f64 {
        self.calibrations.get(agent_name).copied().unwrap_or(0.75)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> FixtureMarket {
        FixtureMarket::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/market.json"
        ))
        .unwrap()
    }

    fn tickers(market: &FixtureMarket, day: &str) -> Vec<String> {
        let as_of = AsOfDate::new(day).unwrap();
        market
            .get_pit_snapshot(&as_of, &["US".to_string()])
            .unwrap()
            .into_iter()
            .map(|s| s.ticker)
            .collect()
    }

    #[test]
    fn snapshots_show_only_what_was_public_on_the_day() {
        let market = market();
        let us = ["US".to_string()];
        for day in ["2024-06-30", "2025-06-30", "2026-02-10", "2026-05-01"] {
            let as_of = AsOfDate::new(day).unwrap();
            let snapshot = market.get_pit_snapshot(&as_of, &us).unwrap();
            assert!(!snapshot.is_empty(), "{day}");
            for row in &snapshot {
                assert!(
                    row.filing_date.is_visible_at(&as_of),
                    "{} on {day}",
                    row.ticker
                );
                assert!(row.trade_date.as_str() <= day);
            }
            // Forward returns exist for exactly the securities in the snapshot.
            let forward = market.get_forward_returns(&as_of, &us).unwrap();
            let names: Vec<&String> = snapshot.iter().map(|s| &s.ticker).collect();
            assert_eq!(forward.keys().collect::<Vec<_>>(), names);
        }
        // Delisted on 2026-03-15: present before, gone after.
        assert!(tickers(&market, "2026-02-10").contains(&"DEBT_FAIL".to_string()));
        assert!(!tickers(&market, "2026-05-01").contains(&"DEBT_FAIL".to_string()));
        assert!(
            market
                .get_pit_snapshot(&AsOfDate::new("2026-02-10").unwrap(), &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn workers_read_filings_only_in_their_markets() {
        let market = market();
        let as_of = AsOfDate::new("2026-02-10").unwrap();
        let us = market.view(vec!["US".into()], BTreeMap::new());
        assert!(!us.get_pit_filings("TRAP_CO", &as_of).unwrap().is_empty());
        let other = market.view(vec!["HK".into()], BTreeMap::new());
        assert!(
            other
                .get_pit_filings("TRAP_CO", &as_of)
                .unwrap_err()
                .starts_with("permission_denied")
        );
        assert!(us.get_pit_filings("NOPE", &as_of).is_err());
        assert_eq!(us.get_agent_calibration("moat"), 0.75);
    }
}
