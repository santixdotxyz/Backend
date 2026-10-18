//! The contract's types, as the indexer sees them.
//!
//! These mirror `smart-contract/src/types.rs` field for field. They are re-declared rather than
//! shared because the contract crate is `no_std` and compiles for wasm; depending on it here
//! would drag a wasm-targeted build into a native tool for the sake of a handful of structs.
//!
//! **The decoders below are the coupling that matters.** If a field is renamed in the contract,
//! `scval::field` fails by name at that point and says which fields were actually present —
//! rather than silently reading a zero into a reconciliation table.

use anyhow::{Context, Result};
use serde::Serialize;
use stellar_xdr::ScVal;

use crate::scval;

#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Treasury {
    pub total_deposited: i128,
    pub total_allocated: i128,
    pub total_settled: i128,
}

impl Treasury {
    /// `total_deposited − total_allocated`. The only pool `allocate` may draw on.
    pub fn unallocated(&self) -> i128 {
        self.total_deposited - self.total_allocated
    }

    /// What the sum of every card balance must equal.
    pub fn allocated_outstanding(&self) -> i128 {
        self.total_allocated - self.total_settled
    }

    pub fn decode(v: &ScVal) -> Result<Self> {
        Ok(Treasury {
            total_deposited: scval::i128v(scval::field(v, "total_deposited")?)?,
            total_allocated: scval::i128v(scval::field(v, "total_allocated")?)?,
            total_settled: scval::i128v(scval::field(v, "total_settled")?)?,
        })
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub id: u32,
    pub category: String,
    pub label: String,
    pub balance: i128,
    pub limit: i128,
    pub limit_window_secs: u64,
    pub window_start: u64,
    pub spent_in_window: i128,
    pub status: String,
}

impl Card {
    pub fn decode(v: &ScVal) -> Result<Self> {
        Ok::<_, anyhow::Error>(Card {
            id: scval::u32v(scval::field(v, "id")?)?,
            category: scval::variant(scval::field(v, "category")?)?,
            label: scval::string(scval::field(v, "label")?)?,
            balance: scval::i128v(scval::field(v, "balance")?)?,
            limit: scval::i128v(scval::field(v, "limit")?)?,
            limit_window_secs: scval::u64v(scval::field(v, "limit_window_secs")?)?,
            window_start: scval::u64v(scval::field(v, "window_start")?)?,
            spent_in_window: scval::i128v(scval::field(v, "spent_in_window")?)?,
            status: scval::variant(scval::field(v, "status")?)?,
        })
        .context("decoding a Card")
    }

    /// What is left of the limit in the window the card is currently in.
    ///
    /// Deliberately **not** rolled forward here. The contract rolls lazily and this tool reports
    /// what the chain says; a report that quietly reset a window would show headroom that the
    /// next `authorize_spend` may or may not agree with, depending on when it runs.
    pub fn remaining_limit(&self) -> i128 {
        (self.limit - self.spent_in_window).max(0)
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Spend {
    pub id: u32,
    pub card_id: u32,
    pub amount: i128,
    pub merchant_ref: String,
    pub decision: String,
    pub reason_code: String,
    pub settled: bool,
    pub ledger_seq: u32,
    pub timestamp: u64,
    /// From the RPC envelope, not the event body. This is what makes a record openable on an
    /// explorer, and it is the reason a declined spend is worth writing at all.
    pub tx_hash: String,
}

impl Spend {
    pub fn decode(v: &ScVal, tx_hash: &str) -> Result<Self> {
        Ok::<_, anyhow::Error>(Spend {
            id: scval::u32v(scval::field(v, "id")?)?,
            card_id: scval::u32v(scval::field(v, "card_id")?)?,
            amount: scval::i128v(scval::field(v, "amount")?)?,
            merchant_ref: scval::string(scval::field(v, "merchant_ref")?)?,
            decision: scval::variant(scval::field(v, "decision")?)?,
            reason_code: scval::variant(scval::field(v, "reason_code")?)?,
            settled: scval::boolv(scval::field(v, "settled")?)?,
            ledger_seq: scval::u32v(scval::field(v, "ledger_seq")?)?,
            timestamp: scval::u64v(scval::field(v, "timestamp")?)?,
            tx_hash: tx_hash.to_string(),
        })
        .context("decoding a SpendRecord")
    }

    pub fn approved(&self) -> bool {
        self.decision == "Approved"
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Settlement {
    pub id: u32,
    pub spend_id: u32,
    pub card_id: u32,
    pub amount: i128,
    pub to: String,
    pub ledger_seq: u32,
    pub timestamp: u64,
    pub tx_hash: String,
    /// Set by `verify`, never by `index`.
    ///
    /// **Reconciled means one specific thing:** the transaction this record claims to have been
    /// written in was fetched from RPC and confirmed to have succeeded. An unreconciled
    /// settlement is not necessarily wrong — it may simply not have been checked — which is why
    /// the two commands are separate and why the report says which it is looking at.
    pub reconciled: bool,
}

impl Settlement {
    pub fn decode(v: &ScVal, tx_hash: &str) -> Result<Self> {
        Ok::<_, anyhow::Error>(Settlement {
            id: scval::u32v(scval::field(v, "id")?)?,
            spend_id: scval::u32v(scval::field(v, "spend_id")?)?,
            card_id: scval::u32v(scval::field(v, "card_id")?)?,
            amount: scval::i128v(scval::field(v, "amount")?)?,
            to: scval::address(scval::field(v, "to")?)?,
            ledger_seq: scval::u32v(scval::field(v, "ledger_seq")?)?,
            timestamp: scval::u64v(scval::field(v, "timestamp")?)?,
            tx_hash: tx_hash.to_string(),
            reconciled: false,
        })
        .context("decoding a Settlement")
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Deposit {
    pub from: String,
    pub amount: i128,
    pub ledger: u32,
    pub tx_hash: String,
    pub reconciled: bool,
}

/// Render an amount in whole units, given the token's decimals.
///
/// String arithmetic rather than a float. `250_000.0000001` has more significant digits than an
/// `f64` carries, so a formatter that went through one would print a number that is nearly right
/// — which in a reconciliation table is worse than one that is obviously wrong.
pub fn units(amount: i128, decimals: u32) -> String {
    let neg = amount < 0;
    let s = amount.unsigned_abs().to_string();
    let d = decimals as usize;
    let (whole, frac) = if s.len() > d {
        (s[..s.len() - d].to_string(), s[s.len() - d..].to_string())
    } else {
        ("0".to_string(), format!("{:0>width$}", s, width = d))
    };
    let frac = frac.trim_end_matches('0');
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    // Thousands separators, because the figures involved are six digits long and a reviewer is
    // being asked to compare them against an invoice by eye.
    let bytes = whole.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_render_without_going_through_a_float() {
        assert_eq!(units(2_500_000_000_000, 7), "250,000");
        assert_eq!(units(24_000_000_000, 7), "2,400");
        assert_eq!(units(1, 7), "0.0000001");
        assert_eq!(units(0, 7), "0");
        assert_eq!(units(10_000_000, 7), "1");
        assert_eq!(units(15_000_000, 7), "1.5");
        assert_eq!(units(-24_000_000_000, 7), "-2,400");
        // A figure with more significant digits than an f64 carries. Printed exactly, or the
        // formatter is lying about a balance.
        assert_eq!(units(2_500_000_000_000_001, 7), "250,000,000.0000001");
    }

    #[test]
    fn remaining_limit_never_goes_negative() {
        let c = Card {
            id: 1,
            category: "SaaS".into(),
            label: "saas".into(),
            balance: 100,
            limit: 50,
            limit_window_secs: 2_592_000,
            window_start: 0,
            spent_in_window: 80, // a limit lowered below what was already spent
            status: "Active".into(),
        };
        assert_eq!(c.remaining_limit(), 0);
    }

    #[test]
    fn treasury_derivations() {
        let t = Treasury {
            total_deposited: 250,
            total_allocated: 180,
            total_settled: 20,
        };
        assert_eq!(t.unallocated(), 70);
        assert_eq!(t.allocated_outstanding(), 160);
    }
}
