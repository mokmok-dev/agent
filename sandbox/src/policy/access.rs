//! The three access modes a path entry can grant or remove.

use serde::{Deserialize, Serialize};

/// The access a path entry grants or removes.
///
/// The variant order **is** the precedence order, least restrictive first, so
/// the derived [`Ord`] is the precedence: [`Deny`](Self::Deny) beats
/// [`Write`](Self::Write) beats [`Read`](Self::Read). Do not reorder the
/// variants. See `docs/sandbox/filesystem.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Read-only access.
    Read,
    /// Read and write access.
    Write,
    /// No access. A deny overrides any broader grant that covers it.
    Deny,
}

#[cfg(test)]
mod tests {
    // The `deny > write > read` precedence rests on the variant declaration
    // order, which the derived `Ord` follows. This test pins that coupling, so
    // reordering the variants cannot silently invert precedence.

    use super::*;

    #[test]
    fn the_declaration_order_is_the_precedence_order() {
        assert!(Access::Deny > Access::Write);
        assert!(Access::Write > Access::Read);
        assert!(Access::Deny > Access::Read);
    }
}
