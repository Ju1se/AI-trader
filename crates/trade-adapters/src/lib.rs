#![forbid(unsafe_code)]
//! Standalone simulated broker adapter (`PaperBrokerAdapter`), completely decoupled
//! from the database storage layer (`trade-store`).
//! Supports credential verification, idempotent order placement, independent read-back,
//! and fault injection (transport drop / post-execution response loss) to test `OUTCOME_UNKNOWN`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use trade_core::{sha256_hex, AsOfDate, UsdCents, WeightBps};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerOrderLine {
    pub ticker: String,
    pub weight_bps: WeightBps,
    pub notional_cents: UsdCents,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerOrderReceipt {
    pub receipt_id: String,
    pub idempotency_key: String,
    pub task_id: String,
    pub as_of_date: AsOfDate,
    pub status: String,
    pub orders: Vec<BrokerOrderLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerFaultMode {
    None,
    /// Simulates network timeout BEFORE the broker records the order.
    DropBeforeFill,
    /// Simulates network timeout AFTER the broker fills the order (response lost in transit!).
    TimeoutAfterFill,
}

#[derive(Default)]
struct BrokerBook {
    by_receipt: BTreeMap<String, BrokerOrderReceipt>,
    by_idempotency: BTreeMap<String, String>,
    next_fault: Option<BrokerFaultMode>,
}

#[derive(Clone)]
pub struct PaperBrokerAdapter {
    expected_api_key: String,
    book: Arc<Mutex<BrokerBook>>,
}

impl PaperBrokerAdapter {
    pub fn new(expected_api_key: impl Into<String>) -> Self {
        Self {
            expected_api_key: expected_api_key.into(),
            book: Arc::new(Mutex::new(BrokerBook::default())),
        }
    }

    pub fn inject_next_fault(&self, mode: BrokerFaultMode) {
        let mut guard = self.book.lock().expect("broker mutex");
        guard.next_fault = Some(mode);
    }

    pub fn submit_orders(
        &self,
        api_key: &str,
        idempotency_key: &str,
        task_id: &str,
        as_of_date: &AsOfDate,
        portfolio_capital: UsdCents,
        weights_bps: &BTreeMap<String, WeightBps>,
    ) -> Result<BrokerOrderReceipt, String> {
        if api_key != self.expected_api_key {
            return Err("broker_unauthorized_api_key".into());
        }

        let mut guard = self.book.lock().expect("broker mutex");
        let fault = guard.next_fault.take().unwrap_or(BrokerFaultMode::None);

        if fault == BrokerFaultMode::DropBeforeFill {
            return Err("broker_network_timeout_before_fill".into());
        }

        if let Some(rid) = guard.by_idempotency.get(idempotency_key).cloned() {
            let existing = guard.by_receipt[&rid].clone();
            if fault == BrokerFaultMode::TimeoutAfterFill {
                return Err("broker_response_timeout_after_fill".into());
            }
            return Ok(existing);
        }

        let orders: Vec<BrokerOrderLine> = weights_bps
            .iter()
            .map(|(ticker, &weight_bps)| {
                let notional = ((portfolio_capital.cents() as i128)
                    * i128::from(weight_bps.bps()))
                    / 10_000_i128;
                BrokerOrderLine {
                    ticker: ticker.clone(),
                    weight_bps,
                    notional_cents: UsdCents::from_cents(notional as i64),
                }
            })
            .collect();

        let receipt_id = format!("PO-{}", &sha256_hex(idempotency_key.as_bytes())[..10]);
        let receipt = BrokerOrderReceipt {
            receipt_id: receipt_id.clone(),
            idempotency_key: idempotency_key.to_string(),
            task_id: task_id.to_string(),
            as_of_date: as_of_date.clone(),
            status: "FILLED".into(),
            orders,
        };

        guard
            .by_idempotency
            .insert(idempotency_key.to_string(), receipt_id.clone());
        guard.by_receipt.insert(receipt_id, receipt.clone());

        if fault == BrokerFaultMode::TimeoutAfterFill {
            return Err("broker_response_timeout_after_fill".into());
        }
        Ok(receipt)
    }

    pub fn get_order_by_receipt(&self, receipt_id: &str) -> Option<BrokerOrderReceipt> {
        let guard = self.book.lock().expect("broker mutex");
        guard.by_receipt.get(receipt_id).cloned()
    }

    pub fn get_order_by_idempotency(&self, idempotency_key: &str) -> Option<BrokerOrderReceipt> {
        let guard = self.book.lock().expect("broker mutex");
        let rid = guard.by_idempotency.get(idempotency_key)?;
        guard.by_receipt.get(rid).cloned()
    }

    pub fn count_orders_for_task(&self, task_id: &str) -> usize {
        let guard = self.book.lock().expect("broker mutex");
        guard
            .by_receipt
            .values()
            .filter(|r| r.task_id == task_id)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_idempotency_and_fault_modes() {
        let broker = PaperBrokerAdapter::new("secret-123");
        let dt = AsOfDate::new("2026-02-10").unwrap();
        let mut weights = BTreeMap::new();
        weights.insert("AAPL_MOAT".into(), WeightBps::new(5000).unwrap());

        assert!(broker
            .submit_orders(
                "wrong-key",
                "idem-1",
                "T1",
                &dt,
                UsdCents::from_dollars(100_000.0),
                &weights
            )
            .is_err());

        broker.inject_next_fault(BrokerFaultMode::DropBeforeFill);
        assert!(broker
            .submit_orders(
                "secret-123",
                "idem-1",
                "T1",
                &dt,
                UsdCents::from_dollars(100_000.0),
                &weights
            )
            .is_err());
        assert!(broker.get_order_by_idempotency("idem-1").is_none());

        // TimeoutAfterFill records the order once, allowing reconciliation by idempotency_key!
        broker.inject_next_fault(BrokerFaultMode::TimeoutAfterFill);
        assert!(broker
            .submit_orders(
                "secret-123",
                "idem-1",
                "T1",
                &dt,
                UsdCents::from_dollars(100_000.0),
                &weights
            )
            .is_err());
        let reconciled = broker.get_order_by_idempotency("idem-1").expect("filled");
        assert_eq!(reconciled.status, "FILLED");

        // Subsequent retry with same idempotency_key returns identical receipt without duplicating
        let replay = broker
            .submit_orders(
                "secret-123",
                "idem-1",
                "T1",
                &dt,
                UsdCents::from_dollars(100_000.0),
                &weights,
            )
            .unwrap();
        assert_eq!(replay.receipt_id, reconciled.receipt_id);
        assert_eq!(broker.count_orders_for_task("T1"), 1);
    }
}
