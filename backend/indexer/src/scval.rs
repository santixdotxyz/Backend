//! Reading Soroban values back out of XDR.
//!
//! # Why this exists rather than a JSON flag
//!
//! Soroban RPC returns event topics and bodies as base64 XDR. Some versions can render them as
//! JSON instead, and relying on that would be shorter — and would make this tool depend on a
//! server-side convenience whose shape has changed between releases and is not part of any
//! interface the contract controls. Decoding the XDR ourselves means the indexer reads exactly
//! what the ledger holds.
//!
//! # The shapes a `#[contractevent]` produces
//!
//! Only four, and they are worth stating because everything below is a helper for one of them:
//!
//! | Rust | ScVal |
//! |---|---|
//! | `struct` with named fields | `Map`, keyed by field name as a `Symbol`, in the SDK's order |
//! | unit-variant `enum` | `Vec` of one `Symbol` — the variant name |
//! | `Option::None` | `Void` |
//! | `i128` | `I128` as a hi/lo pair, which has to be reassembled |
//!
//! Everything else — `u32`, `u64`, `bool`, `String`, `Address` — maps to the obvious scalar.

use anyhow::{anyhow, bail, Context, Result};
use stellar_xdr::{Limits, ReadXdr, ScVal};

pub fn from_base64(s: &str) -> Result<ScVal> {
    ScVal::from_xdr_base64(s, Limits::none())
        .with_context(|| format!("could not decode an ScVal from base64: {s}"))
}

pub fn symbol(v: &ScVal) -> Result<String> {
    match v {
        ScVal::Symbol(s) => Ok(s.to_utf8_string()?),
        other => bail!("expected a symbol, got {}", kind(other)),
    }
}

pub fn string(v: &ScVal) -> Result<String> {
    match v {
        ScVal::String(s) => Ok(s.to_utf8_string()?),
        ScVal::Symbol(s) => Ok(s.to_utf8_string()?),
        other => bail!("expected a string, got {}", kind(other)),
    }
}

pub fn u32v(v: &ScVal) -> Result<u32> {
    match v {
        ScVal::U32(n) => Ok(*n),
        other => bail!("expected a u32, got {}", kind(other)),
    }
}

pub fn u64v(v: &ScVal) -> Result<u64> {
    match v {
        ScVal::U64(n) => Ok(*n),
        ScVal::Timepoint(t) => Ok(t.0),
        other => bail!("expected a u64, got {}", kind(other)),
    }
}

pub fn boolv(v: &ScVal) -> Result<bool> {
    match v {
        ScVal::Bool(b) => Ok(*b),
        other => bail!("expected a bool, got {}", kind(other)),
    }
}

/// `i128` arrives as a signed high word and an unsigned low word. Reassembling it by hand is the
/// one place in this file where a plausible-looking shortcut is wrong: `hi as i128 * 2^64 + lo`
/// overflows for large negatives, and `(hi, lo)` swapped produces a number that looks like a
/// balance and is not one.
pub fn i128v(v: &ScVal) -> Result<i128> {
    match v {
        ScVal::I128(p) => Ok(((p.hi as i128) << 64) | (p.lo as i128)),
        ScVal::U64(n) => Ok(*n as i128),
        ScVal::I64(n) => Ok(*n as i128),
        other => bail!("expected an i128, got {}", kind(other)),
    }
}

/// An address in the strkey form a human reads and an explorer links to.
pub fn address(v: &ScVal) -> Result<String> {
    match v {
        ScVal::Address(a) => Ok(a.to_string()),
        other => bail!("expected an address, got {}", kind(other)),
    }
}

/// A unit-variant enum: a `Vec` holding exactly one symbol.
pub fn variant(v: &ScVal) -> Result<String> {
    match v {
        ScVal::Vec(Some(items)) if items.len() == 1 => symbol(&items[0]),
        ScVal::Symbol(s) => Ok(s.to_utf8_string()?),
        other => bail!("expected a unit enum variant, got {}", kind(other)),
    }
}

/// `Option::None` is `Void`; `Some(x)` is just `x`.
pub fn optional<T>(v: &ScVal, f: impl Fn(&ScVal) -> Result<T>) -> Result<Option<T>> {
    match v {
        ScVal::Void => Ok(None),
        other => f(other).map(Some),
    }
}

/// One field of a struct, by name.
///
/// A linear scan rather than a lookup, because the maps here have single-digit entry counts and a
/// `HashMap` built per event would cost more than the scan it replaced.
pub fn field<'a>(v: &'a ScVal, name: &str) -> Result<&'a ScVal> {
    let entries = match v {
        ScVal::Map(Some(m)) => m,
        other => bail!("expected a struct, got {}", kind(other)),
    };
    for e in entries.iter() {
        if symbol(&e.key).ok().as_deref() == Some(name) {
            return Ok(&e.val);
        }
    }
    Err(anyhow!(
        "the event has no field '{name}'. Present: {}",
        entries
            .iter()
            .filter_map(|e| symbol(&e.key).ok())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn kind(v: &ScVal) -> &'static str {
    match v {
        ScVal::Bool(_) => "bool",
        ScVal::Void => "void",
        ScVal::U32(_) => "u32",
        ScVal::I32(_) => "i32",
        ScVal::U64(_) => "u64",
        ScVal::I64(_) => "i64",
        ScVal::U128(_) => "u128",
        ScVal::I128(_) => "i128",
        ScVal::Timepoint(_) => "timepoint",
        ScVal::Symbol(_) => "symbol",
        ScVal::String(_) => "string",
        ScVal::Vec(_) => "vec",
        ScVal::Map(_) => "map",
        ScVal::Address(_) => "address",
        _ => "some other ScVal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{Int128Parts, ScMap, ScMapEntry, ScSymbol, ScVec, VecM};

    fn sym(s: &str) -> ScVal {
        ScVal::Symbol(ScSymbol(s.try_into().unwrap()))
    }

    fn map(pairs: &[(&str, ScVal)]) -> ScVal {
        let entries: Vec<ScMapEntry> = pairs
            .iter()
            .map(|(k, v)| ScMapEntry {
                key: sym(k),
                val: v.clone(),
            })
            .collect();
        ScVal::Map(Some(ScMap(entries.try_into().unwrap())))
    }

    fn i128val(n: i128) -> ScVal {
        ScVal::I128(Int128Parts {
            hi: (n >> 64) as i64,
            lo: n as u64,
        })
    }

    #[test]
    fn i128_round_trips_across_the_word_boundary() {
        // The values that a naive hi/lo reassembly gets wrong: exactly one word, one below it,
        // and a large negative.
        for n in [
            0i128,
            1,
            u64::MAX as i128,
            u64::MAX as i128 + 1,
            250_000 * 10_000_000,
            i128::MAX,
            -1,
            i128::MIN,
        ] {
            assert_eq!(i128v(&i128val(n)).unwrap(), n, "failed at {n}");
        }
    }

    #[test]
    fn a_missing_field_says_what_was_there_instead() {
        let m = map(&[("balance", i128val(5)), ("id", ScVal::U32(1))]);
        let err = field(&m, "limit").unwrap_err().to_string();
        assert!(err.contains("no field 'limit'"), "{err}");
        // The message names the fields that ARE present, because the usual cause is a rename in
        // the contract and the fix is obvious once you can see both sides.
        assert!(err.contains("balance"), "{err}");
        assert!(err.contains("id"), "{err}");
    }

    #[test]
    fn a_unit_enum_variant_reads_as_its_name() {
        let v = ScVal::Vec(Some(ScVec(
            VecM::try_from(vec![sym("LimitExceeded")]).unwrap(),
        )));
        assert_eq!(variant(&v).unwrap(), "LimitExceeded");
    }

    #[test]
    fn none_is_void_and_some_is_the_value_itself() {
        assert_eq!(optional(&ScVal::Void, u32v).unwrap(), None);
        assert_eq!(optional(&ScVal::U32(7), u32v).unwrap(), Some(7));
    }

    #[test]
    fn the_wrong_type_is_refused_rather_than_coerced() {
        // A decoder that quietly returned 0 here would produce a reconciliation table full of
        // zeroes that looked like real settlements.
        assert!(i128v(&sym("not-a-number")).is_err());
        assert!(u32v(&ScVal::Void).is_err());
    }
}
