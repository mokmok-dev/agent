#[must_use]
pub const fn add(
    left: u64,
    right: u64,
) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn add_returns_the_sum(left in any::<u64>(), right in any::<u64>()) {
            prop_assume!(left <= u64::MAX - right);
            prop_assert_eq!(add(left, right), left + right);
        }
    }
}
