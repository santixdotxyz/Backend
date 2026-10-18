//! The fold: an event stream in, the contract's state out.
//!
//! # Why this can be a fold at all
//!
//! Because the contract emits **whole structs rather than deltas** (see
//! `smart-contract/src/events.rs`). The last event for a card id *is* the card. That makes the
//! fold idempotent and order-tolerant: replaying a page changes nothing, and a page arriving out
//! of order is corrected by the next one that carries a later state.
//!
//! A delta stream would need every event, exactly once, in order — and would be silently wrong
//! rather than loudly wrong when it did not get them. The contract's test suite asserts the
//! same reconstruction property in-process (`rebuild_test.rs`); this is the same fold against a
//! real network.
//!
//! # What "reconciled" means here
//!
//! Nothing in this file sets it. `index` builds state; `verify` fetches each settlement's
//! transaction and confirms it succeeded before marking it. Keeping them apart means a report
//! can never show a settlement as corroborated because the tool happened to have seen the event.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::model::{Card, Deposit, Settlement, Spend, Treasury};
use crate::rpc::RawEvent;
use crate::scval;

#[derive(Serialize, Default, Debug)]
pub struct State {
    pub contract: String,
    pub network: String,
    /// The ledger the fold has consumed up to. Where a later run resumes from.
    pub last_ledger: u32,
    pub treasury: Treasury,
    pub cards: BTreeMap<u32, Card>,
    pub spends: BTreeMap<u32, Spend>,
    pub settlements: BTreeMap<u32, Settlement>,
    pub whitelist: BTreeMap<String, bool>,
    pub deposits: Vec<Deposit>,
    pub roles: BTreeMap<String, String>,
    pub token: Option<String>,
    pub decimals: u32,
    /// Events seen but not understood. Reported rather than dropped: an unknown event usually
    /// means this tool is older than the contract it is pointed at, and silently ignoring one is
    /// how a report ends up missing a settlement.
    pub unknown_events: Vec<String>,

    /// Event ids already folded into [`Self::deposits`].
    ///
    /// Every other collection here is a map keyed by the contract's own ids, so applying an event
    /// twice simply overwrites and the fold is idempotent for free. `deposits` is a list — it has
    /// no such key — so without this, re-reading a page of events would report the same deposit
    /// twice and the treasury's history would disagree with itself.
    ///
    /// Skipped when serialising: it is bookkeeping for the fold, not part of the rebuilt state.
    #[serde(skip)]
    seen_deposits: BTreeSet<String>,
}

impl State {
    pub fn new(contract: &str, network: &str) -> Self {
        State {
            contract: contract.to_string(),
            network: network.to_string(),
            decimals: 7,
            ..Default::default()
        }
    }

    pub fn apply(&mut self, ev: &RawEvent) -> Result<()> {
        let name = match ev.topics.first() {
            Some(t) => scval::symbol(t).unwrap_or_default(),
            None => return Ok(()),
        };
        self.last_ledger = self.last_ledger.max(ev.ledger);

        match name.as_str() {
            "init" => {
                self.token = Some(scval::address(scval::field(&ev.value, "token")?)?);
                self.decimals = scval::u32v(scval::field(&ev.value, "decimals")?)?;
            }
            "role" => {
                let who = scval::address(ev.topics.get(1).context("a role event with no topic")?)?;
                match scval::optional(scval::field(&ev.value, "role")?, scval::variant)? {
                    Some(role) => self.roles.insert(who, role),
                    // A revoke is "this address now holds nothing", which needs no separate case
                    // beyond removing the entry.
                    None => self.roles.remove(&who),
                };
            }
            "deposit" => {
                // The treasury is a whole struct, so re-applying it is harmless. The deposit is
                // a list entry, so it is guarded by the event's own id.
                self.treasury = Treasury::decode(scval::field(&ev.value, "treasury")?)?;
                if self.seen_deposits.insert(ev.id.clone()) {
                    self.deposits.push(Deposit {
                        from: scval::address(ev.topics.get(1).context("a deposit with no from")?)?,
                        amount: scval::i128v(scval::field(&ev.value, "amount")?)?,
                        ledger: ev.ledger,
                        tx_hash: ev.tx_hash.clone(),
                        reconciled: false,
                    });
                }
            }
            "card" => {
                let card = Card::decode(scval::field(&ev.value, "card")?)?;
                self.treasury = Treasury::decode(scval::field(&ev.value, "treasury")?)?;
                self.cards.insert(card.id, card);
            }
            "spend" => {
                let spend = Spend::decode(scval::field(&ev.value, "spend")?, &ev.tx_hash)?;
                // `settle` re-emits the record with `settled` true, and it must win over the
                // authorization event. Insert-by-id gives that for free as long as events are
                // applied in ledger order, which they are.
                self.spends.insert(spend.id, spend);
            }
            "settle" => {
                let s = Settlement::decode(scval::field(&ev.value, "settlement")?, &ev.tx_hash)?;
                self.treasury = Treasury::decode(scval::field(&ev.value, "treasury")?)?;
                self.settlements.insert(s.id, s);
            }
            "whitelist" => {
                let who = scval::address(
                    ev.topics
                        .get(1)
                        .context("a whitelist event with no topic")?,
                )?;
                let allowed = scval::boolv(scval::field(&ev.value, "allowed")?)?;
                self.whitelist.insert(who, allowed);
            }
            other => {
                let note = format!("ledger {} · unrecognised event '{other}'", ev.ledger);
                if !self.unknown_events.contains(&note) {
                    self.unknown_events.push(note);
                }
            }
        }
        Ok(())
    }

    /// The accounting identity, checked against the rebuilt cards rather than against the
    /// treasury's own running totals.
    ///
    /// This is the same assertion the contract's test harness makes, and it is worth repeating
    /// here for a different reason: it also proves the **fold** is right. A dropped card event
    /// leaves the totals intact and the sum short, and there is no other symptom.
    pub fn check_books(&self) -> Result<()> {
        let summed: i128 = self.cards.values().map(|c| c.balance).sum();
        anyhow::ensure!(
            summed == self.treasury.allocated_outstanding(),
            "the rebuilt cards sum to {summed}, and the treasury says {}. \
             Either an event was missed or the contract's accounting is wrong.",
            self.treasury.allocated_outstanding()
        );
        anyhow::ensure!(
            self.treasury.unallocated() >= 0,
            "unallocated is negative: {}",
            self.treasury.unallocated()
        );

        // I3, from the rebuild's point of view: no spend appears twice in the settlement ledger.
        let mut seen = std::collections::BTreeSet::new();
        for s in self.settlements.values() {
            anyhow::ensure!(
                seen.insert(s.spend_id),
                "spend {} has two settlements — invariant I3 is broken on chain",
                s.spend_id
            );
        }
        Ok(())
    }

    /// Settlements whose transaction has not been confirmed. What `verify` works through.
    pub fn unverified(&self) -> Vec<u32> {
        self.settlements
            .values()
            .filter(|s| !s.reconciled)
            .map(|s| s.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::RawEvent;
    use stellar_xdr::{Int128Parts, ScMap, ScMapEntry, ScString, ScSymbol, ScVal, ScVec, VecM};

    fn sym(s: &str) -> ScVal {
        ScVal::Symbol(ScSymbol(s.try_into().unwrap()))
    }

    fn map(pairs: Vec<(&str, ScVal)>) -> ScVal {
        let entries: Vec<ScMapEntry> = pairs
            .into_iter()
            .map(|(k, v)| ScMapEntry {
                key: sym(k),
                val: v,
            })
            .collect();
        ScVal::Map(Some(ScMap(entries.try_into().unwrap())))
    }

    fn amount(n: i128) -> ScVal {
        ScVal::I128(Int128Parts {
            hi: (n >> 64) as i64,
            lo: n as u64,
        })
    }

    fn unit(name: &str) -> ScVal {
        ScVal::Vec(Some(ScVec(VecM::try_from(vec![sym(name)]).unwrap())))
    }

    fn treasury(dep: i128, alloc: i128, settled: i128) -> ScVal {
        map(vec![
            ("total_deposited", amount(dep)),
            ("total_allocated", amount(alloc)),
            ("total_settled", amount(settled)),
        ])
    }

    fn card(id: u32, balance: i128, spent: i128) -> ScVal {
        map(vec![
            ("id", ScVal::U32(id)),
            ("category", unit("SaaS")),
            ("label", sym("saas")),
            ("balance", amount(balance)),
            ("limit", amount(1_000)),
            ("limit_window_secs", ScVal::U64(2_592_000)),
            ("window_start", ScVal::U64(1_756_000_000)),
            ("spent_in_window", amount(spent)),
            ("status", unit("Active")),
        ])
    }

    fn spend(id: u32, settled: bool) -> ScVal {
        map(vec![
            ("id", ScVal::U32(id)),
            ("card_id", ScVal::U32(1)),
            ("amount", amount(100)),
            (
                "merchant_ref",
                ScVal::String(ScString("aws".try_into().unwrap())),
            ),
            ("decision", unit("Approved")),
            ("reason_code", unit("None")),
            ("settled", ScVal::Bool(settled)),
            ("ledger_seq", ScVal::U32(1_000_000)),
            ("timestamp", ScVal::U64(1_756_000_000)),
        ])
    }

    fn settlement(id: u32, spend_id: u32) -> ScVal {
        map(vec![
            ("id", ScVal::U32(id)),
            ("spend_id", ScVal::U32(spend_id)),
            ("card_id", ScVal::U32(1)),
            ("amount", amount(100)),
            (
                "to",
                ScVal::Address(
                    "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI"
                        .parse()
                        .unwrap(),
                ),
            ),
            ("ledger_seq", ScVal::U32(1_000_001)),
            ("timestamp", ScVal::U64(1_756_000_060)),
        ])
    }

    fn ev(ledger: u32, topics: Vec<ScVal>, value: ScVal, tx: &str) -> RawEvent {
        RawEvent {
            ledger,
            topics,
            value,
            tx_hash: tx.to_string(),
            // One id per event, as a node would give. Replaying the same event means replaying
            // the same id, which is exactly what the deposit guard keys on.
            id: format!("{ledger}-{tx}"),
        }
    }

    /// The whole point of the design, in one test: the fold reproduces state, and running it
    /// again over the same events changes nothing.
    #[test]
    fn the_fold_is_idempotent() {
        let stream = vec![
            ev(
                10,
                vec![
                    sym("deposit"),
                    ScVal::Address(
                        "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI"
                            .parse()
                            .unwrap(),
                    ),
                ],
                map(vec![
                    ("amount", amount(1_000)),
                    ("treasury", treasury(1_000, 0, 0)),
                ]),
                "aa",
            ),
            ev(
                11,
                vec![sym("card"), ScVal::U32(1), sym("alloc")],
                map(vec![
                    ("card", card(1, 400, 0)),
                    ("treasury", treasury(1_000, 400, 0)),
                ]),
                "bb",
            ),
            ev(
                12,
                vec![sym("spend"), ScVal::U32(1), ScVal::U32(1)],
                map(vec![("spend", spend(1, false))]),
                "cc",
            ),
            ev(
                13,
                vec![sym("spend"), ScVal::U32(1), ScVal::U32(1)],
                map(vec![("spend", spend(1, true))]),
                "dd",
            ),
            ev(
                13,
                vec![sym("card"), ScVal::U32(1), sym("settle")],
                map(vec![
                    ("card", card(1, 300, 100)),
                    ("treasury", treasury(1_000, 400, 100)),
                ]),
                "dd",
            ),
            ev(
                13,
                vec![sym("settle"), ScVal::U32(1), ScVal::U32(1)],
                map(vec![
                    ("settlement", settlement(1, 1)),
                    ("treasury", treasury(1_000, 400, 100)),
                ]),
                "dd",
            ),
        ];

        let mut once = State::new("C1", "testnet");
        for e in &stream {
            once.apply(e).unwrap();
        }
        once.check_books().unwrap();

        assert_eq!(once.cards[&1].balance, 300);
        assert_eq!(once.treasury.unallocated(), 600);
        assert_eq!(once.settlements.len(), 1);
        assert_eq!(once.deposits.len(), 1);
        assert_eq!(once.last_ledger, 13);
        // The re-emitted record won, which is what stops a paid invoice showing as outstanding.
        assert!(once.spends[&1].settled);
        // And it carries the SETTLEMENT's transaction, which is the one a reviewer opens.
        assert_eq!(once.spends[&1].tx_hash, "dd");

        let mut twice = State::new("C1", "testnet");
        for _ in 0..3 {
            for e in &stream {
                twice.apply(e).unwrap();
            }
        }
        twice.check_books().unwrap();

        assert_eq!(twice.cards, once.cards);
        assert_eq!(twice.settlements, once.settlements);
        assert_eq!(twice.spends, once.spends);
        assert_eq!(twice.treasury, once.treasury);
        // Deposits too. This is the one collection with no key of its own, so it is guarded by
        // the event id — it used to duplicate on every replay, which made "the fold is
        // idempotent" false for the one part of the report a reviewer reads as a history.
        assert_eq!(
            twice.deposits.len(),
            1,
            "replaying must not duplicate a deposit"
        );
        assert_eq!(twice.deposits, once.deposits);
    }

    /// A missed card event breaks the identity, and the fold says so rather than reporting a
    /// treasury that looks fine.
    #[test]
    fn a_dropped_event_is_caught_by_the_book_check() {
        let mut s = State::new("C1", "testnet");
        s.apply(&ev(
            10,
            vec![
                sym("deposit"),
                ScVal::Address(
                    "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI"
                        .parse()
                        .unwrap(),
                ),
            ],
            map(vec![
                ("amount", amount(1_000)),
                ("treasury", treasury(1_000, 400, 0)),
            ]),
            "aa",
        ))
        .unwrap();

        // The treasury says 400 is allocated and no card event ever arrived to say to whom.
        let err = s.check_books().unwrap_err().to_string();
        assert!(err.contains("sum to 0"), "{err}");
    }

    #[test]
    fn an_unrecognised_event_is_reported_rather_than_dropped() {
        let mut s = State::new("C1", "testnet");
        s.apply(&ev(5, vec![sym("newthing")], ScVal::Void, "aa"))
            .unwrap();
        assert_eq!(s.unknown_events.len(), 1);
        assert!(s.unknown_events[0].contains("newthing"));
    }

    #[test]
    fn a_revoked_role_is_removed_rather_than_left_behind() {
        let who: ScVal = ScVal::Address(
            "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI"
                .parse()
                .unwrap(),
        );
        let mut s = State::new("C1", "testnet");
        s.apply(&ev(
            1,
            vec![sym("role"), who.clone()],
            map(vec![("role", unit("Operator"))]),
            "aa",
        ))
        .unwrap();
        assert_eq!(s.roles.len(), 1);

        s.apply(&ev(
            2,
            vec![sym("role"), who],
            map(vec![("role", ScVal::Void)]),
            "bb",
        ))
        .unwrap();
        assert!(s.roles.is_empty());
    }
}
