//! Tamper-evident hash chain for the durable log.
//!
//! Each log line carries two `CloudEvents` extension attributes: `prevhash`,
//! the previous line's hash, and `chainhash`, this line's hash. The hash is
//! SHA-256 over the previous hash followed by the canonical JSON of the event
//! with the chain attributes removed, so any edit to a line (or a reordering)
//! breaks the chain from that position on and [`verify_chain`] reports it.
//!
//! The attributes are lowercase alphanumeric, which is what `CloudEvents`
//! requires of extension attribute names, so the line stays a valid event and
//! readers that ignore extensions are unaffected.
//!
//! [verify_chain]: crate::log::verify_chain

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::Event;

/// The `prevhash` of the first chained record: sixty-four zeroes.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The `CloudEvents` extension attribute carrying the previous record's hash.
pub const PREV_HASH_ATTR: &str = "prevhash";
/// The `CloudEvents` extension attribute carrying this record's hash.
pub const HASH_ATTR: &str = "chainhash";

/// Appends `event` to the chain, returning its JSON line and hash.
pub(crate) fn seal(
    event: &Event,
    prev_hash: &str,
) -> Result<(String, String), serde_json::Error> {
    let mut value = serde_json::to_value(event)?;
    let canonical = canonical(&value)?;
    let hash = hash(prev_hash, &canonical);
    if let Value::Object(object) = &mut value {
        object.insert(
            String::from(PREV_HASH_ATTR),
            Value::String(prev_hash.to_string()),
        );
        object.insert(String::from(HASH_ATTR), Value::String(hash.clone()));
    }
    let line = serde_json::to_string(&value)?;
    Ok((line, hash))
}

/// The hash of one record: SHA-256 over the previous hash and the canonical
/// event JSON, as lowercase hex.
pub(crate) fn hash(
    prev_hash: &str,
    canonical: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(canonical.as_bytes());
    to_hex(&hasher.finalize())
}

/// The canonical bytes of an event: its JSON with the chain attributes removed.
///
/// The value is re-serialized from a parsed [`Value`], so it is stable
/// regardless of the whitespace in the line and the order the writer used.
pub(crate) fn canonical(value: &Value) -> Result<String, serde_json::Error> {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove(HASH_ATTR);
        object.remove(PREV_HASH_ATTR);
    }
    serde_json::to_string(&value)
}

/// Reads the `(prevhash, chainhash)` attributes from a parsed line, or `None`
/// when either is absent.
pub(crate) fn attributes(value: &Value) -> Option<(String, String)> {
    let object = value.as_object()?;
    let prev = object.get(PREV_HASH_ATTR)?.as_str()?;
    let hash = object.get(HASH_ATTR)?.as_str()?;
    Some((prev.to_string(), hash.to_string()))
}

/// Encodes bytes as lowercase hex without an extra dependency.
fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::{GENESIS_HASH, HASH_ATTR, PREV_HASH_ATTR, canonical, hash, seal};
    use crate::Event;
    use serde_json::{Value, json};

    fn event() -> Event {
        Event::new("test.event", json!({ "value": 1 }))
    }

    #[test]
    fn seal_adds_both_attributes_and_hashes_the_canonical_event() {
        let (line, digest) = seal(&event(), GENESIS_HASH).expect("seal");
        let value: Value = serde_json::from_str(&line).expect("parse");

        assert_eq!(value[PREV_HASH_ATTR], GENESIS_HASH);
        assert_eq!(value[HASH_ATTR], digest);
        let expected = hash(GENESIS_HASH, &canonical(&value).expect("canonical"));
        assert_eq!(digest, expected);
    }

    #[test]
    fn the_canonical_form_is_independent_of_attribute_order_and_whitespace() {
        let event = event();
        let (line, _) = seal(&event, GENESIS_HASH).expect("seal");
        let parsed: Value = serde_json::from_str(&line).expect("parse");

        // The same event with the chain attributes inserted in the other order
        // and the line re-serialized must canonicalize identically.
        let mut reordered = serde_json::to_value(&event).expect("value");
        let object = reordered.as_object_mut().expect("an object");
        object.insert(String::from(HASH_ATTR), Value::String(String::from("x")));
        object.insert(
            String::from(PREV_HASH_ATTR),
            Value::String(String::from("y")),
        );

        assert_eq!(
            canonical(&parsed).expect("canonical"),
            canonical(&reordered).expect("canonical")
        );
    }

    #[test]
    fn a_different_previous_hash_changes_the_digest() {
        let event = event();
        let (_, first) = seal(&event, GENESIS_HASH).expect("seal");
        let (_, second) = seal(&event, &"a".repeat(64)).expect("seal");

        assert_ne!(first, second);
    }
}
