//! Property tests for the policy core. The property that matters for milestone 1
//! is path-rule precedence: `deny > write > read`, with reads as the broad base
//! grant. The implementation takes the most restrictive matching entry; the
//! reference model scans the entries in priority order and takes the first
//! match. The two are independent formulations of the same rule, so a
//! disagreement is a bug in one of them.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use sandbox::policy::{Access, FsEntry, FsPolicy, InvalidPolicy, Policy};

/// A small alphabet of paths that nest, so generated entries overlap.
fn a_path() -> impl Strategy<Value = PathBuf> {
    prop_oneof![
        Just(PathBuf::from("/a")),
        Just(PathBuf::from("/b")),
        Just(PathBuf::from("/a/x")),
        Just(PathBuf::from("/a/x/y")),
        Just(PathBuf::from("/ab")),
    ]
}

/// Any access mode.
fn an_access() -> impl Strategy<Value = Access> {
    prop_oneof![Just(Access::Read), Just(Access::Write), Just(Access::Deny),]
}

/// One entry over the small path alphabet.
fn an_entry() -> impl Strategy<Value = FsEntry> {
    (a_path(), an_access()).prop_map(|(path, access)| FsEntry { path, access })
}

/// The reference model: the most restrictive access among the matching entries,
/// found by trying the modes in priority order rather than by comparing ranks.
///
/// This deliberately does not reuse the implementation's `max`, so the two are
/// independent: `deny` is tried first, then `write`, then `read`, and an entry
/// that matches none leaves the base read grant.
fn reference_access(
    entries: &[FsEntry],
    target: &Path,
) -> Access {
    for access in [Access::Deny, Access::Write, Access::Read] {
        if entries
            .iter()
            .any(|entry| entry.access == access && target.starts_with(&entry.path))
        {
            return access;
        }
    }
    Access::Read
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Precedence is `deny > write > read`, and an unmatched path keeps the
    /// broad read grant.
    #[test]
    fn access_for_agrees_with_a_priority_ordered_reference(
        entries in prop::collection::vec(an_entry(), 0..8),
        target in a_path(),
    ) {
        let fs = FsPolicy { entries, protected: Vec::new() };
        prop_assert_eq!(fs.access_for(&target), reference_access(&fs.entries, &target));
    }

    /// A `deny` nested inside the broadest possible write root still wins for
    /// every path under it. This is the precedence rule in its sharpest form:
    /// `write /` matches everything, so only the `deny` can produce a denial.
    #[test]
    fn a_deny_inside_the_broadest_write_root_still_holds(
        deny_path in a_path(),
        extra in prop::collection::vec(an_entry(), 0..6),
        suffix in prop::collection::vec("[a-z]{1,3}", 0..3),
    ) {
        let mut entries = vec![FsEntry::write("/"), FsEntry::deny(deny_path.clone())];
        entries.extend(extra);
        let fs = FsPolicy { entries, protected: Vec::new() };

        let mut target = deny_path;
        for segment in suffix {
            target.push(segment);
        }
        prop_assert_eq!(fs.access_for(&target), Access::Deny);

        // A path outside the deny still gets the broad write grant, so the
        // assertion above is not vacuous.
        prop_assert_eq!(fs.access_for(&PathBuf::from("/outside")), Access::Write);
    }

    /// A `deny` that covers a `write` root is a construction error, however the
    /// two are ordered in the entry list.
    #[test]
    fn a_deny_over_a_write_root_is_always_rejected(
        deny_path in a_path(),
        suffix in prop::collection::vec("[a-z]{1,3}", 0..3),
        deny_first in any::<bool>(),
    ) {
        // Put the write root *under* the deny, so the deny covers it.
        let mut write_root = deny_path.clone();
        for segment in suffix {
            write_root.push(segment);
        }
        let deny = FsEntry::deny(deny_path);
        let write = FsEntry::write(write_root);
        let entries = if deny_first { vec![deny, write] } else { vec![write, deny] };
        let fs = FsPolicy { entries, protected: Vec::new() };

        let rejected = matches!(
            fs.validate(),
            Err(InvalidPolicy::DenyCoversWriteRoot { .. })
        );
        prop_assert!(rejected);
    }

    /// A `deny` nested strictly inside a `write` root is *accepted*: it is the
    /// case precedence exists for, and validation must not reject it.
    #[test]
    fn a_deny_nested_inside_a_write_root_is_accepted(
        suffix in prop::collection::vec("[a-z]{1,3}", 1..3),
    ) {
        let mut nested = PathBuf::from("/work");
        for segment in suffix {
            nested.push(segment);
        }
        let fs = FsPolicy {
            entries: vec![FsEntry::write("/work"), FsEntry::deny(nested)],
            protected: Vec::new(),
        };
        prop_assert!(fs.validate().is_ok());
    }

    /// Coverage is component-wise: a path is never covered by a sibling whose
    /// name merely shares a byte prefix.
    #[test]
    fn coverage_never_crosses_a_component_boundary(
        name in "[a-z]{1,4}",
        suffix in "[a-z]{1,4}",
    ) {
        let root = PathBuf::from(format!("/{name}"));
        let sibling = PathBuf::from(format!("/{name}{suffix}/file"));
        let fs = FsPolicy {
            entries: vec![FsEntry::write(root)],
            protected: Vec::new(),
        };
        prop_assert_eq!(fs.access_for(&sibling), Access::Read);
    }

    /// A policy round-trips through JSON unchanged.
    #[test]
    fn a_policy_round_trips_through_json(entries in prop::collection::vec(an_entry(), 0..6)) {
        let policy = Policy {
            fs: FsPolicy { entries, protected: vec![".git".to_owned()] },
            ..Policy::default()
        };
        let json = serde_json::to_string(&policy).expect("serializes");
        let parsed: Policy = serde_json::from_str(&json).expect("parses");
        prop_assert_eq!(parsed, policy);
    }

    /// A `write` root is writable exactly when no `deny` in the set covers it.
    #[test]
    fn a_write_root_is_writable_exactly_when_not_covered_by_a_deny(
        root in a_path(),
        extra in prop::collection::vec(an_entry(), 0..4),
    ) {
        let mut entries = vec![FsEntry::write(root.clone())];
        entries.extend(extra);
        let fs = FsPolicy { entries, protected: Vec::new() };

        let denied = fs
            .entries
            .iter()
            .any(|entry| entry.access == Access::Deny && root.starts_with(&entry.path));
        let expected = if denied { Access::Deny } else { Access::Write };
        prop_assert_eq!(fs.access_for(&root), expected);
    }

    /// A protected name inside a write root is never writable, however the
    /// policy is otherwise shaped.
    #[test]
    fn a_protected_name_is_never_writable(
        name in "[.]?[a-z]{1,4}",
        suffix in prop::collection::vec("[a-z]{1,3}", 0..3),
        extra in prop::collection::vec(an_entry(), 0..4),
    ) {
        let mut entries = vec![FsEntry::write("/work")];
        entries.extend(extra);
        let fs = FsPolicy {
            entries,
            protected: vec![name.clone()],
        };

        let mut target = PathBuf::from("/work");
        target.push(&name);
        for segment in suffix {
            target.push(segment);
        }
        prop_assert_ne!(fs.access_for(&target), Access::Write);
    }
}
