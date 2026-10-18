//! # saintix-indexer — module M16
//!
//! The off-chain half of Deliverable 1 and Deliverable 3: read the treasury's events over
//! Stellar RPC, rebuild its state from them, fetch the transactions the settlement records name,
//! and confirm each one actually happened.
//!
//! # Three commands, and the separation between them is the point
//!
//! ```text
//! index    fold the event stream into state and write it out
//! verify   …then fetch each settlement's transaction and confirm it succeeded
//! report   …then render the reconciliation table a reviewer reads
//! ```
//!
//! `index` never marks anything reconciled. **Reconciled means one specific thing** — the
//! transaction a record claims to live in was fetched from the node and found to have succeeded
//! — and if building state could set that flag, a report would show settlements as corroborated
//! merely because the tool had seen the event announcing them. Seeing an event is not
//! corroboration; it is the claim being corroborated.
//!
//! # Why there is no database
//!
//! There is nothing to keep. The contract is the source of truth and the fold is idempotent, so
//! the honest thing for this tool to do is re-read and re-derive. A cached copy would introduce
//! the one failure mode the whole design avoids: a number in a report that no longer matches
//! the chain, with nothing to say which is right.

mod model;
mod report;
mod rpc;
mod scval;
mod state;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use rpc::{Rpc, TxStatus};
use state::State;

#[derive(Parser)]
#[command(
    name = "saintix-indexer",
    about = "Rebuild a Saintix treasury from its event stream and reconcile its settlements"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fold the event stream into state.
    Index(Args),
    /// Fold, then confirm every settlement's transaction against the chain.
    Verify(Args),
    /// Fold, confirm, and print the reconciliation table as markdown.
    Report(Args),
}

#[derive(clap::Args)]
struct Args {
    /// The treasury contract id.
    #[arg(long)]
    contract: String,

    #[arg(long, default_value = "https://soroban-testnet.stellar.org")]
    rpc: String,

    #[arg(long, default_value = "testnet")]
    network: String,

    /// Start the fold here instead of at the node's oldest retained ledger.
    #[arg(long)]
    from_ledger: Option<u32>,

    /// Write the rebuilt state as JSON.
    #[arg(long)]
    out: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index(a) => {
            let s = build(&a, false)?;
            finish(&a, &s)?;
            summarise(&s);
        }
        Command::Verify(a) => {
            let s = build(&a, true)?;
            finish(&a, &s)?;
            summarise(&s);
        }
        Command::Report(a) => {
            let s = build(&a, true)?;
            finish(&a, &s)?;
            print!("{}", report::render(&s));
        }
    }
    Ok(())
}

fn build(a: &Args, verify: bool) -> Result<State> {
    let rpc = Rpc::new(&a.rpc);

    // Default to the node's oldest retained ledger rather than to zero. Asking for a ledger the
    // node has pruned is an error, not an empty result, so a naive `startLedger: 1` fails against
    // every public node.
    let from = match a.from_ledger {
        Some(l) => l,
        None => rpc
            .oldest_ledger()
            .context("could not ask the node how far back its events go")?,
    };

    let events = rpc
        .events(&a.contract, from)
        .with_context(|| format!("reading events for {} from ledger {from}", a.contract))?;

    eprintln!(
        "read {} events for {} from ledger {from}",
        events.len(),
        a.contract
    );
    if events.is_empty() {
        eprintln!(
            "note: no events. Either the contract has never been used, or it was deployed before \
             ledger {from} and this node no longer retains that history — pass --from-ledger to \
             say otherwise."
        );
    }

    let mut s = State::new(&a.contract, &a.network);
    for e in &events {
        s.apply(e)
            .with_context(|| format!("applying an event from ledger {}", e.ledger))?;
    }

    // The rebuilt cards must sum to what the treasury says. A page of events lost in transit
    // shows up here and nowhere else — the totals stay perfectly self-consistent either way.
    s.check_books()
        .context("the rebuilt state does not balance")?;

    // Say how current this is. A report is a snapshot, and one taken from a node several
    // thousand ledgers behind is a different claim from one taken at the tip — the difference
    // matters when somebody is checking a settlement that happened a minute ago.
    if let Ok(latest) = rpc.latest_ledger() {
        eprintln!(
            "covered ledgers {from}–{} · the node is at {latest}",
            s.last_ledger.max(from)
        );
    }

    if verify {
        reconcile(&rpc, &mut s)?;
    }
    Ok(s)
}

/// Fetch every transaction the records name and confirm it succeeded.
///
/// # What a failure here means
///
/// A settlement record that names a transaction the chain does not corroborate is the single
/// most serious thing this tool can find, so it is an error rather than a warning: the run stops
/// and no report is written. The alternative — a table with one row quietly marked "not
/// confirmed" — is a document that looks like evidence and is not.
///
/// `NOT_FOUND` is treated separately from `FAILED`, and the distinction matters. A transaction
/// outside the node's retention window is not a fraudulent one, and reporting it as failed would
/// call a perfectly good settlement into question because the tool was run a month late.
fn reconcile(rpc: &Rpc, s: &mut State) -> Result<()> {
    let mut unconfirmed = Vec::new();

    for id in s.unverified() {
        let (hash, spend_id) = {
            let st = &s.settlements[&id];
            (st.tx_hash.clone(), st.spend_id)
        };
        if hash.is_empty() {
            unconfirmed.push(format!("settlement {id} carries no transaction hash"));
            continue;
        }
        match rpc.transaction(&hash)? {
            TxStatus::Success { .. } => {
                s.settlements.get_mut(&id).unwrap().reconciled = true;
            }
            TxStatus::Failed => anyhow::bail!(
                "settlement {id} (spend {spend_id}) names transaction {hash}, and the chain says \
                 that transaction FAILED. A settlement record that the ledger does not corroborate \
                 is not evidence of a payment."
            ),
            TxStatus::NotFound => unconfirmed.push(format!(
                "settlement {id} names transaction {hash}, which this node no longer holds \
                 (outside its retention window)"
            )),
        }
    }

    // Deposits are checked too, because Deliverable 1's evidence is "the treasury received
    // stablecoin" and an unopenable deposit hash proves nothing.
    let hashes: Vec<String> = s.deposits.iter().map(|d| d.tx_hash.clone()).collect();
    for (i, hash) in hashes.into_iter().enumerate() {
        if hash.is_empty() {
            continue;
        }
        if let TxStatus::Success { .. } = rpc.transaction(&hash)? {
            s.deposits[i].reconciled = true;
        }
    }

    if !unconfirmed.is_empty() {
        eprintln!("could not confirm {} record(s):", unconfirmed.len());
        for u in &unconfirmed {
            eprintln!("  {u}");
        }
        eprintln!(
            "These are reported rather than treated as failures — see the note on retention."
        );
    }
    Ok(())
}

fn finish(a: &Args, s: &State) -> Result<()> {
    if let Some(path) = &a.out {
        std::fs::write(path, serde_json::to_string_pretty(s)?)
            .with_context(|| format!("writing {path}"))?;
        eprintln!("state written to {path}");
    }
    Ok(())
}

fn summarise(s: &State) {
    let d = s.decimals;
    let confirmed = s.settlements.values().filter(|x| x.reconciled).count();
    eprintln!(
        "treasury {} deposited · {} allocated · {} unallocated · {} settled",
        model::units(s.treasury.total_deposited, d),
        model::units(s.treasury.total_allocated, d),
        model::units(s.treasury.unallocated(), d),
        model::units(s.treasury.total_settled, d),
    );
    eprintln!(
        "{} cards · {} spends ({} declined) · {} settlements ({confirmed} confirmed)",
        s.cards.len(),
        s.spends.len(),
        s.spends.values().filter(|x| !x.approved()).count(),
        s.settlements.len(),
    );
    for u in &s.unknown_events {
        eprintln!("unrecognised: {u}");
    }
}
