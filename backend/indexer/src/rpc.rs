//! A small Soroban RPC client: `getEvents`, `getTransaction`, `getHealth`, `getLatestLedger`.
//!
//! Hand-rolled rather than pulled from a client library, for the same reason the XDR is decoded
//! by hand: these four methods are the whole surface this tool needs, and a dependency that
//! wraps the entire RPC would be more code to audit than the code it replaces.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use stellar_xdr::ScVal;

use crate::scval;

/// One contract event, decoded, with the two things the event body itself cannot carry: the
/// ledger it landed in and the transaction that wrote it.
#[derive(Clone, Debug)]
pub struct RawEvent {
    pub ledger: u32,
    pub topics: Vec<ScVal>,
    pub value: ScVal,
    pub tx_hash: String,
    /// The node's own unique id for this event.
    ///
    /// Needed because one entry in the rebuilt state — the deposit list — is a list rather than a
    /// map keyed by contract id, so it has nothing of its own to deduplicate on. A transaction
    /// hash is not enough: one transaction can invoke `deposit` more than once through a wrapper
    /// contract, and collapsing those would lose a real deposit.
    pub id: String,
}

pub struct Rpc {
    url: String,
    agent: ureq::Agent,
}

/// What `getTransaction` says about a hash.
#[derive(Debug, PartialEq, Eq)]
pub enum TxStatus {
    Success {
        ledger: u32,
    },
    Failed,
    /// Outside the RPC's retention window, or never submitted. **Not** the same as failed, and
    /// reporting it as such would call a perfectly good settlement fraudulent.
    NotFound,
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        Rpc {
            url: url.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(30))
                .build(),
        }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self
            .agent
            .post(&self.url)
            .send_json(body)
            .with_context(|| format!("{method} against {} failed", self.url))?
            .into_json()
            .with_context(|| format!("{method} returned something that is not JSON"))?;

        if let Some(err) = resp.get("error") {
            bail!("{method} was refused by the node: {err}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method} returned neither a result nor an error: {resp}"))
    }

    pub fn latest_ledger(&self) -> Result<u32> {
        let r = self.call("getLatestLedger", json!({}))?;
        r.get("sequence")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .ok_or_else(|| anyhow!("getLatestLedger returned no sequence"))
    }

    /// The oldest ledger the node still holds events for.
    ///
    /// This is the honest starting point for a full rebuild, and the reason `index` reports the
    /// window it actually covered: a node with a seven-day retention cannot show you a treasury
    /// deployed a month ago, and a tool that quietly started from wherever it could would return
    /// a partial state that looks complete.
    pub fn oldest_ledger(&self) -> Result<u32> {
        let r = self.call("getHealth", json!({}))?;
        r.get("oldestLedger")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .ok_or_else(|| anyhow!("getHealth returned no oldestLedger"))
    }

    /// Every event this contract has published since `start_ledger`, in order.
    ///
    /// # An empty page does not mean the end
    ///
    /// This is the trap, and getting it wrong is silent. `getEvents` does **not** scan the whole
    /// retention window in one call: it walks forward from the cursor over a bounded span of
    /// ledgers, returns whatever it found there, and hands back a cursor at the point it stopped.
    /// A contract deployed five minutes ago, read from the node's oldest retained ledger, gives
    /// an empty first page and a cursor a few thousand ledgers along — and a loop that treats
    /// "no events" as "no more events" reports a treasury with nothing in it, cheerfully, with
    /// no error anywhere. That is exactly the kind of result that looks like evidence.
    ///
    /// So the loop follows the cursor until the cursor reaches the node's latest ledger, not
    /// until a page comes back short. Passing `--from-ledger` (which `demo.sh` does, from the
    /// ledger the deploy landed in) makes this quick; without it, it is correct but does a lot
    /// of walking.
    pub fn events(&self, contract: &str, start_ledger: u32) -> Result<Vec<RawEvent>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0u32;

        loop {
            let mut pagination = json!({ "limit": 200 });
            let mut params = json!({
                "filters": [{ "type": "contract", "contractIds": [contract] }],
            });
            match &cursor {
                // `startLedger` and a cursor are mutually exclusive: the cursor already encodes a
                // position, and sending both is an error rather than a narrowing.
                Some(c) => pagination["cursor"] = json!(c),
                None => params["startLedger"] = json!(start_ledger),
            }
            params["pagination"] = pagination;

            let r = self.call("getEvents", params)?;
            let events = r
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let latest = r.get("latestLedger").and_then(Value::as_u64).unwrap_or(0) as u32;

            // The cursor for the next page, taken before the events are consumed. Newer nodes put
            // it at the top level; older ones only give a `pagingToken` on each event, and a node
            // in the middle of an upgrade can do either.
            let next = r
                .get("cursor")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    events
                        .last()
                        .and_then(|e| e.get("pagingToken").or_else(|| e.get("id")))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .filter(|c| !c.is_empty());

            for e in events {
                out.push(decode_event(&e)?);
            }

            let next = match next {
                Some(c) => c,
                // No cursor at all: there is nowhere to go, whatever the page held.
                None => break,
            };
            // The node has caught up to its own tip. Anything further does not exist yet.
            if let Some(at) = ledger_of_cursor(&next) {
                if latest > 0 && at >= latest {
                    break;
                }
            }
            // A cursor that does not move is a loop that does not end. Only reachable if a node
            // misbehaves, and cheaper to guard than to diagnose.
            if cursor.as_deref() == Some(next.as_str()) {
                break;
            }
            pages += 1;
            if pages > 10_000 {
                bail!(
                    "getEvents has returned {pages} pages without reaching the tip. \
                     Pass --from-ledger to start closer to the contract's deployment."
                );
            }
            cursor = Some(next);
        }
        Ok(out)
    }

    pub fn transaction(&self, hash: &str) -> Result<TxStatus> {
        let r = self.call("getTransaction", json!({ "hash": hash }))?;
        let status = r.get("status").and_then(Value::as_str).unwrap_or("");
        Ok(match status {
            "SUCCESS" => TxStatus::Success {
                ledger: r.get("ledger").and_then(Value::as_u64).unwrap_or(0) as u32,
            },
            "FAILED" => TxStatus::Failed,
            _ => TxStatus::NotFound,
        })
    }
}

/// Pull one event out of the JSON the node returns.
///
/// `value` has been both a bare base64 string and `{"xdr": "..."}` across RPC releases, and a
/// node in the middle of an upgrade can serve either. Both are accepted, because failing on the
/// shape of a wrapper would make this tool break for a reason that has nothing to do with the
/// contract it is reading.
fn decode_event(e: &Value) -> Result<RawEvent> {
    let ledger = e.get("ledger").and_then(Value::as_u64).unwrap_or(0) as u32;
    let tx_hash = e
        .get("txHash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // `id` is the node's unique handle for the event; `pagingToken` is the older spelling of the
    // same thing. Falling back to ledger+hash keeps the fold working against a node that sends
    // neither, at the cost of collapsing two deposits made in one transaction — stated here
    // rather than left to be discovered.
    let id = e
        .get("id")
        .or_else(|| e.get("pagingToken"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{ledger}-{tx_hash}"));

    let topics = e
        .get("topic")
        .or_else(|| e.get("topics"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("an event arrived with no topics: {e}"))?
        .iter()
        .map(|t| xdr_string(t).and_then(|s| scval::from_base64(&s)))
        .collect::<Result<Vec<_>>>()?;

    let value = match e.get("value") {
        Some(v) => scval::from_base64(&xdr_string(v)?)?,
        None => ScVal::Void,
    };

    Ok(RawEvent {
        ledger,
        topics,
        value,
        tx_hash,
        id,
    })
}

/// The ledger a paging cursor sits at.
///
/// Cursors are `"<ledger padded to 19 digits>-<event index>"`. Reading the ledger out of one is
/// how the loop knows it has reached the tip, and it is the only part of the cursor's shape this
/// tool depends on — an unparseable cursor falls back to following it until the node stops
/// producing one, which is slower but never wrong.
fn ledger_of_cursor(cursor: &str) -> Option<u32> {
    cursor
        .split('-')
        .next()?
        .trim_start_matches('0')
        .parse()
        .ok()
}

fn xdr_string(v: &Value) -> Result<String> {
    if let Some(s) = v.as_str() {
        return Ok(s.to_string());
    }
    v.get("xdr")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("expected base64 XDR, got {v}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both wrapper shapes decode, and neither is silently skipped.
    #[test]
    fn an_event_decodes_from_either_rpc_shape() {
        // ScVal::Symbol("card"), base64 XDR.
        let sym = {
            use stellar_xdr::{Limits, ScSymbol, WriteXdr};
            ScVal::Symbol(ScSymbol("card".try_into().unwrap()))
                .to_xdr_base64(Limits::none())
                .unwrap()
        };

        let flat = serde_json::json!({
            "ledger": 42,
            "txHash": "abc",
            "topic": [sym],
            "value": sym,
        });
        let wrapped = serde_json::json!({
            "ledger": 42,
            "txHash": "abc",
            "topic": [{ "xdr": sym }],
            "value": { "xdr": sym },
        });

        for shape in [flat, wrapped] {
            let ev = decode_event(&shape).unwrap();
            assert_eq!(ev.ledger, 42);
            assert_eq!(ev.tx_hash, "abc");
            assert_eq!(scval::symbol(&ev.topics[0]).unwrap(), "card");
        }
    }

    /// The cursor's ledger is what tells the paging loop it has reached the tip. An empty page is
    /// routine — `getEvents` walks a bounded span per call — so mis-parsing this is the
    /// difference between reading a whole contract's history and reporting it as empty.
    #[test]
    fn a_cursor_says_which_ledger_it_sits_at() {
        assert_eq!(ledger_of_cursor("0004189843-0000000000"), Some(4_189_843));
        assert_eq!(ledger_of_cursor("0000000001-0000000004"), Some(1));
        // Unparseable falls back to None, which makes the loop follow the cursor until the node
        // stops giving one — slower, never wrong.
        assert_eq!(ledger_of_cursor("not-a-cursor"), None);
        assert_eq!(ledger_of_cursor(""), None);
    }

    #[test]
    fn an_event_with_no_topics_is_an_error_not_an_empty_event() {
        // Dropping it would leave a gap in the rebuild with nothing to say so.
        let e = serde_json::json!({ "ledger": 1, "txHash": "x" });
        assert!(decode_event(&e).is_err());
    }
}
