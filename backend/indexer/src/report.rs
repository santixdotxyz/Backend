//! The reconciliation table, rendered for a reader rather than a program.
//!
//! SOW §6 asks that the evidence be reviewable *with minimal technical expertise*. That sets the
//! bar for this file: every figure is in whole USDC rather than smallest units, every hash is a
//! link, and every row says which spend a payment settles. A reviewer should be able to take one
//! row, open the transaction, and see the amount it claims.

use crate::model::units;
use crate::state::State;

pub fn explorer(network: &str) -> &'static str {
    match network {
        "public" | "mainnet" => "https://stellar.expert/explorer/public",
        _ => "https://stellar.expert/explorer/testnet",
    }
}

fn tx_link(base: &str, hash: &str) -> String {
    if hash.is_empty() {
        "—".to_string()
    } else {
        format!("[`{}…`]({base}/tx/{hash})", &hash[..hash.len().min(12)])
    }
}

pub fn render(s: &State) -> String {
    let base = explorer(&s.network);
    let d = s.decimals;
    let mut o = String::new();

    o.push_str("# Saintix — reconciliation report\n\n");
    o.push_str(&format!(
        "Rebuilt from the event stream of [`{}`]({}/contract/{}) on {}.\n\n",
        s.contract, base, s.contract, s.network
    ));
    o.push_str(&format!(
        "State is folded from contract events alone — no database, no cached copy. \
         Read up to ledger {}.\n\n",
        s.last_ledger
    ));

    // ---- treasury ----
    o.push_str("## Treasury\n\n| | |\n|---|---|\n");
    o.push_str(&format!(
        "| Total deposited | {} |\n",
        units(s.treasury.total_deposited, d)
    ));
    o.push_str(&format!(
        "| Allocated to cards | {} |\n",
        units(s.treasury.total_allocated, d)
    ));
    o.push_str(&format!(
        "| Unallocated | {} |\n",
        units(s.treasury.unallocated(), d)
    ));
    o.push_str(&format!(
        "| Settled out | {} |\n\n",
        units(s.treasury.total_settled, d)
    ));

    // ---- cards ----
    //
    // Balance and limit get separate columns, and the reason is the deck's fourth UX principle:
    // a card can be rich and out of limit, or well under its limit and broke, and one combined
    // figure hides whichever of the two is about to decline the next spend.
    o.push_str("## Cards\n\n");
    o.push_str(
        "| # | Category | Label | Balance | Limit | Spent this window | Remaining | Status |\n",
    );
    o.push_str("|---|---|---|---|---|---|---|---|\n");
    for c in s.cards.values() {
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            c.id,
            c.category,
            c.label,
            units(c.balance, d),
            units(c.limit, d),
            units(c.spent_in_window, d),
            units(c.remaining_limit(), d),
            c.status,
        ));
    }
    o.push('\n');

    // ---- spends ----
    o.push_str("## Spend ledger\n\n");
    o.push_str(
        "Declined rows are **transactions that succeeded**. The contract records a refusal \
         rather than reverting, which is what lets you open one and read the reason.\n\n",
    );
    o.push_str("| # | Card | Amount | Merchant | Decision | Reason | Settled | Transaction |\n");
    o.push_str("|---|---|---|---|---|---|---|---|\n");
    for sp in s.spends.values() {
        o.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            sp.id,
            s.cards
                .get(&sp.card_id)
                .map(|c| c.label.clone())
                .unwrap_or_else(|| format!("card {}", sp.card_id)),
            units(sp.amount, d),
            sp.merchant_ref,
            sp.decision,
            if sp.reason_code == "None" {
                "—".to_string()
            } else {
                format!("**{}**", sp.reason_code)
            },
            if sp.settled { "yes" } else { "no" },
            tx_link(base, &sp.tx_hash),
        ));
    }
    o.push('\n');

    // ---- settlements ----
    //
    // This is Deliverable 3's evidence, and the `spend` column is the whole of it: without that
    // link a settlement is a transfer nobody can account for.
    o.push_str("## Reconciliation\n\n");
    let verified = s.settlements.values().filter(|x| x.reconciled).count();
    o.push_str(&format!(
        "{verified} of {} settlements confirmed against the chain.\n\n",
        s.settlements.len()
    ));
    o.push_str("| # | Spend | Card | Amount | To | Transaction | Confirmed |\n");
    o.push_str("|---|---|---|---|---|---|---|\n");
    for st in s.settlements.values() {
        o.push_str(&format!(
            "| {} | sp_{} | {} | {} | `{}…` | {} | {} |\n",
            st.id,
            st.spend_id,
            s.cards
                .get(&st.card_id)
                .map(|c| c.label.clone())
                .unwrap_or_else(|| format!("card {}", st.card_id)),
            units(st.amount, d),
            &st.to[..st.to.len().min(8)],
            tx_link(base, &st.tx_hash),
            if st.reconciled { "✓" } else { "not checked" },
        ));
    }
    o.push('\n');

    // ---- deposits ----
    o.push_str("## Deposits\n\n| From | Amount | Ledger | Transaction |\n|---|---|---|---|\n");
    for dep in &s.deposits {
        o.push_str(&format!(
            "| `{}…` | {} | {} | {} |\n",
            &dep.from[..dep.from.len().min(8)],
            units(dep.amount, d),
            dep.ledger,
            tx_link(base, &dep.tx_hash),
        ));
    }
    o.push('\n');

    // ---- invariants ----
    //
    // Reported rather than merely checked, because "the books balance" is the one claim a
    // reviewer cannot verify by opening a transaction.
    o.push_str("## Invariants\n\n");
    let summed: i128 = s.cards.values().map(|c| c.balance).sum();
    o.push_str(&format!(
        "- **Solvency** — Σ card.balance ({}) + unallocated ({}) = {}, and the treasury holds \
         deposited − settled = {}. {}\n",
        units(summed, d),
        units(s.treasury.unallocated(), d),
        units(summed + s.treasury.unallocated(), d),
        units(s.treasury.total_deposited - s.treasury.total_settled, d),
        if summed + s.treasury.unallocated()
            == s.treasury.total_deposited - s.treasury.total_settled
        {
            "**Holds.**"
        } else {
            "**BROKEN.**"
        }
    ));
    let dupes = {
        let mut seen = std::collections::BTreeSet::new();
        s.settlements
            .values()
            .filter(|x| !seen.insert(x.spend_id))
            .count()
    };
    o.push_str(&format!(
        "- **No double settlement** — {} spend{} settled more than once. {}\n",
        dupes,
        if dupes == 1 { "" } else { "s" },
        if dupes == 0 {
            "**Holds.**"
        } else {
            "**BROKEN.**"
        }
    ));

    if !s.unknown_events.is_empty() {
        o.push_str("\n## Events this tool did not recognise\n\n");
        o.push_str(
            "Usually means the indexer is older than the contract it is reading. Listed rather \
             than dropped, because a silently ignored event is how a report ends up missing a \
             settlement.\n\n",
        );
        for u in &s.unknown_events {
            o.push_str(&format!("- {u}\n"));
        }
    }

    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Card, Settlement, Spend, Treasury};

    fn sample() -> State {
        let mut s = State::new("CTREASURY", "testnet");
        s.treasury = Treasury {
            total_deposited: 2_500_000_000_000,
            total_allocated: 1_825_000_000_000,
            total_settled: 24_000_000_000,
        };
        s.cards.insert(
            1,
            Card {
                id: 1,
                category: "SaaS".into(),
                label: "saas".into(),
                balance: 1_801_000_000_000,
                limit: 500_000_000_000,
                limit_window_secs: 2_592_000,
                window_start: 1_756_000_000,
                spent_in_window: 24_000_000_000,
                status: "Active".into(),
            },
        );
        s.spends.insert(
            1,
            Spend {
                id: 1,
                card_id: 1,
                amount: 24_000_000_000,
                merchant_ref: "aws-invoice-0824".into(),
                decision: "Approved".into(),
                reason_code: "None".into(),
                settled: true,
                ledger_seq: 1_000_000,
                timestamp: 1_756_000_000,
                tx_hash: "a7f2c1000000000000000000000000000000000000000000000000000000dead".into(),
            },
        );
        s.spends.insert(
            2,
            Spend {
                id: 2,
                card_id: 1,
                amount: 189_000_000_000,
                merchant_ref: "openai-sub-0824".into(),
                decision: "Declined".into(),
                reason_code: "LimitExceeded".into(),
                settled: false,
                ledger_seq: 1_000_002,
                timestamp: 1_756_000_100,
                tx_hash: "3e91b7000000000000000000000000000000000000000000000000000000beef".into(),
            },
        );
        s.settlements.insert(
            1,
            Settlement {
                id: 1,
                spend_id: 1,
                card_id: 1,
                amount: 24_000_000_000,
                to: "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI".into(),
                ledger_seq: 1_000_003,
                timestamp: 1_756_000_200,
                tx_hash: "6d409f000000000000000000000000000000000000000000000000000000cafe".into(),
                reconciled: true,
            },
        );
        s
    }

    #[test]
    fn the_report_links_every_hash_and_names_every_reason() {
        let out = render(&sample());

        // A hash a reviewer cannot click is a hash they will not check.
        assert!(
            out.contains("https://stellar.expert/explorer/testnet/tx/a7f2c1"),
            "{out}"
        );
        // A decline says why, in the deck's own vocabulary.
        assert!(out.contains("**LimitExceeded**"), "{out}");
        // And the settlement names the spend it closes, which is the whole of Deliverable 3.
        assert!(out.contains("| sp_1 |"), "{out}");
        assert!(out.contains("1 of 1 settlements confirmed"), "{out}");
    }

    #[test]
    fn figures_are_whole_units_not_smallest_units() {
        let out = render(&sample());
        assert!(out.contains("250,000"), "{out}");
        assert!(out.contains("2,400"), "{out}");
        // The raw smallest-unit figure must not appear anywhere — it is the number a reviewer
        // would compare against an invoice and get wrong by seven orders of magnitude.
        assert!(!out.contains("2500000000000"), "{out}");
    }

    #[test]
    fn a_broken_invariant_is_stated_rather_than_hidden() {
        let mut s = sample();
        s.cards.get_mut(&1).unwrap().balance = 0; // as if a card event had been missed
        let out = render(&s);
        assert!(out.contains("**BROKEN.**"), "{out}");
    }
}
