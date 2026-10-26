//! The one content hash in the system.
//!
//! Every identity in this design is BLAKE3 over bytes: an event id over its
//! canonical form, a prefix hash over a chain of segments. Keeping a single
//! function means a change of algorithm is one edit and one set of recorded
//! values, not a hunt for every place that hashed something.

use crate::ids::PrefixHash;

/// The 32 byte digest of a byte string.
#[must_use]
pub fn digest(bytes: &[u8]) -> PrefixHash {
    PrefixHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::digest;

    #[test]
    fn the_same_bytes_digest_the_same() {
        assert_eq!(digest(b"abc"), digest(b"abc"));
    }

    #[test]
    fn one_changed_byte_changes_the_digest() {
        assert_ne!(digest(b"abc"), digest(b"abd"));
    }

    #[test]
    fn length_is_part_of_the_identity() {
        assert_ne!(digest(b""), digest(b"\0"));
        assert_ne!(digest(b"ab"), digest(b"ab\0"));
    }

    #[test]
    fn the_digest_is_thirty_two_bytes() {
        assert_eq!(digest(b"abc").as_bytes().len(), 32);
    }
}
