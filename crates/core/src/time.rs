//! Timestamps. One instant, stored as seconds and nanoseconds since the Unix
//! epoch, rendered as RFC 3339 in UTC.
//!
//! The domain starts at the epoch on purpose. Every timestamp in this system is
//! produced by a running service, so a pre-1970 instant is a broken clock rather
//! than history, and refusing it keeps the calendar maths free of the negative
//! era branch that no test could reach.
//!
//! There is no timezone database and no calendar library. The two conversions
//! between days since the epoch and a civil date are the well known era
//! formulas, written here because a dependency that brings a timezone database
//! would also bring a clock.

use alloc::string::String;
use core::fmt;
use core::time::Duration;

/// Why a timestamp could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeError {
    /// The text did not have the expected shape.
    BadShape,
    /// A field was outside its range, such as month 13 or minute 61.
    OutOfRange,
    /// The offset was neither `Z` nor `+HH:MM` or `-HH:MM`.
    BadOffset,
    /// The instant is before 1970-01-01T00:00:00Z.
    BeforeEpoch,
}

impl fmt::Display for TimeError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let text = match self {
            Self::BadShape => "not an RFC 3339 timestamp",
            Self::OutOfRange => "a date or time field is out of range",
            Self::BadOffset => "offset must be Z or +HH:MM or -HH:MM",
            Self::BeforeEpoch => "the instant is before 1970-01-01T00:00:00Z",
        };
        formatter.write_str(text)
    }
}

impl core::error::Error for TimeError {}

/// An instant at or after the Unix epoch, with nanosecond precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    secs: i64,
    nanos: u32,
}

impl Timestamp {
    /// Builds an instant from Unix seconds and nanoseconds.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::BeforeEpoch`] for a negative `secs` and
    /// [`TimeError::OutOfRange`] when `nanos` is a billion or more, because a
    /// fractional second larger than one second is a caller bug that would
    /// otherwise make two different instants compare equal.
    pub const fn new(
        secs: i64,
        nanos: u32,
    ) -> Result<Self, TimeError> {
        if secs < 0 {
            return Err(TimeError::BeforeEpoch);
        }
        if nanos >= 1_000_000_000 {
            return Err(TimeError::OutOfRange);
        }
        Ok(Self { secs, nanos })
    }

    /// This instant moved forward by a duration.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::OutOfRange`] when the result would not fit or when
    /// the duration is larger than an `i64` of seconds.
    pub fn plus(
        &self,
        duration: Duration,
    ) -> Result<Self, TimeError> {
        let nanos = u64::from(self.nanos) + u64::from(duration.subsec_nanos());
        let carry = i64::try_from(nanos / 1_000_000_000).map_err(|_| TimeError::OutOfRange)?;
        let seconds = i64::try_from(duration.as_secs()).map_err(|_| TimeError::OutOfRange)?;
        let secs = self
            .secs
            .checked_add(seconds)
            .and_then(|sum| sum.checked_add(carry))
            .ok_or(TimeError::OutOfRange)?;
        let nanos = u32::try_from(nanos % 1_000_000_000).map_err(|_| TimeError::OutOfRange)?;
        Self::new(secs, nanos)
    }

    #[must_use]
    pub const fn secs(&self) -> i64 {
        self.secs
    }

    #[must_use]
    pub const fn nanos(&self) -> u32 {
        self.nanos
    }

    /// Writes the instant as RFC 3339 in UTC, with a `Z` offset.
    ///
    /// # Errors
    ///
    /// Returns the formatter's error when the writer refuses text, which a
    /// `String` never does and a fixed buffer does.
    pub fn write_rfc3339(
        &self,
        out: &mut impl fmt::Write,
    ) -> fmt::Result {
        let days = self.secs.div_euclid(86_400);
        let second_of_day = self.secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = second_of_day / 3_600;
        let minute = second_of_day % 3_600 / 60;
        let second = second_of_day % 60;
        write!(
            out,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
        )?;
        if self.nanos > 0 {
            write!(out, ".{:09}", self.nanos)?;
        }
        out.write_str("Z")
    }

    /// Renders the instant as an owned RFC 3339 string.
    ///
    /// # Panics
    ///
    /// Panics only if writing into a `String` fails, which cannot happen.
    #[must_use]
    #[allow(
        clippy::expect_used,
        reason = "writing into a String cannot fail, and threading a Result through every caller buys nothing"
    )]
    pub fn rfc3339(&self) -> String {
        let mut out = String::new();
        self.write_rfc3339(&mut out)
            .expect("a String accepts every write");
        out
    }

    /// Reads RFC 3339 text with a `Z` offset or a `+HH:MM` or `-HH:MM` offset.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::BadShape`] for text that is not a timestamp,
    /// [`TimeError::OutOfRange`] for a field value that does not exist,
    /// [`TimeError::BadOffset`] for an unusable offset, and
    /// [`TimeError::BeforeEpoch`] for an instant before 1970.
    pub fn parse(text: &str) -> Result<Self, TimeError> {
        let bytes = text.as_bytes();
        if bytes.len() < 19 {
            return Err(TimeError::BadShape);
        }
        if bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes[10] != b'T'
            || bytes[13] != b':'
            || bytes[16] != b':'
        {
            return Err(TimeError::BadShape);
        }
        let year = i64::from(read_number(bytes, 0, 4)?);
        let month = read_number(bytes, 5, 2)?;
        let day = read_number(bytes, 8, 2)?;
        let hour = read_number(bytes, 11, 2)?;
        let minute = read_number(bytes, 14, 2)?;
        let second = read_number(bytes, 17, 2)?;

        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err(TimeError::OutOfRange);
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(TimeError::OutOfRange);
        }

        let mut cursor = 19;
        let mut nanos = 0_u32;
        if bytes.get(cursor) == Some(&b'.') {
            cursor += 1;
            let digits = bytes[cursor..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits == 0 || digits > 9 {
                return Err(TimeError::BadShape);
            }
            let value = read_number(bytes, cursor, digits)?;
            nanos = value * 10_u32.pow(9 - u32::try_from(digits).map_err(|_| TimeError::BadShape)?);
            cursor += digits;
        }
        if cursor >= bytes.len() {
            return Err(TimeError::BadOffset);
        }
        let offset = match bytes[cursor] {
            b'Z' if cursor + 1 == bytes.len() => 0,
            sign @ (b'+' | b'-') if cursor + 6 == bytes.len() && bytes[cursor + 3] == b':' => {
                let hours = i64::from(read_number(bytes, cursor + 1, 2)?);
                let minutes = i64::from(read_number(bytes, cursor + 4, 2)?);
                if hours > 23 || minutes > 59 {
                    return Err(TimeError::BadOffset);
                }
                let magnitude = hours * 3_600 + minutes * 60;
                if sign == b'-' { -magnitude } else { magnitude }
            },
            _ => return Err(TimeError::BadOffset),
        };

        let days = days_from_civil(year, month, day);
        let secs =
            days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)
                - offset;
        Self::new(secs, nanos)
    }
}

fn read_number(
    bytes: &[u8],
    start: usize,
    length: usize,
) -> Result<u32, TimeError> {
    let mut value: u32 = 0;
    for index in start..start + length {
        let byte = *bytes.get(index).ok_or(TimeError::BadShape)?;
        if !byte.is_ascii_digit() {
            return Err(TimeError::BadShape);
        }
        value = value * 10 + u32::from(byte - b'0');
    }
    Ok(value)
}

const fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const fn days_in_month(
    year: i64,
    month: u32,
) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        _ => 28,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "month is 1..=12 and day is 1..=31 by construction"
)]
const fn days_from_civil(
    year: i64,
    month: u32,
    day: u32,
) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_index = (month as i64 + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the month index is 0..=11 and the day of month is 1..=31 by construction"
)]
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::{TimeError, Timestamp};
    use alloc::format;
    use alloc::string::{String, ToString};

    fn render(timestamp: &Timestamp) -> String {
        let mut out = String::new();
        timestamp.write_rfc3339(&mut out).expect("writes");
        out
    }

    fn round_trip(text: &str) {
        let parsed = Timestamp::parse(text).expect("parses");
        assert_eq!(render(&parsed), text);
    }

    #[test]
    fn the_epoch_renders_as_1970() {
        let epoch = Timestamp::new(0, 0).expect("valid");
        assert_eq!(render(&epoch), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_known_instant_round_trips() {
        round_trip("2026-10-25T12:57:16Z");
    }

    #[test]
    fn a_positive_offset_is_subtracted() {
        let parsed = Timestamp::parse("2026-10-25T21:57:16+09:00").expect("parses");
        assert_eq!(
            parsed.secs(),
            Timestamp::parse("2026-10-25T12:57:16Z")
                .expect("parses")
                .secs()
        );
    }

    #[test]
    fn a_negative_offset_is_added() {
        let parsed = Timestamp::parse("2026-10-25T07:57:16-05:00").expect("parses");
        assert_eq!(
            parsed.secs(),
            Timestamp::parse("2026-10-25T12:57:16Z")
                .expect("parses")
                .secs()
        );
    }

    #[test]
    fn an_offset_with_minutes_moves_the_instant() {
        let parsed = Timestamp::parse("2026-10-25T18:42:16+05:45").expect("parses");
        assert_eq!(
            parsed.secs(),
            Timestamp::parse("2026-10-25T12:57:16Z")
                .expect("parses")
                .secs()
        );
    }

    #[test]
    fn the_largest_offset_is_accepted() {
        assert!(Timestamp::parse("2026-10-25T12:57:16+23:59").is_ok());
    }

    #[test]
    fn a_fraction_becomes_nanoseconds() {
        let parsed = Timestamp::parse("2026-10-25T12:57:16.5Z").expect("parses");
        assert_eq!(parsed.nanos(), 500_000_000);
        assert_eq!(render(&parsed), "2026-10-25T12:57:16.500000000Z");
    }

    #[test]
    fn nine_fraction_digits_are_accepted() {
        let parsed = Timestamp::parse("2026-01-01T00:00:00.123456789Z").expect("parses");
        assert_eq!(parsed.nanos(), 123_456_789);
    }

    #[test]
    fn a_leap_day_is_accepted() {
        assert!(Timestamp::parse("2024-02-29T00:00:00Z").is_ok());
    }

    #[test]
    fn a_leap_day_in_a_common_year_is_rejected() {
        assert_eq!(
            Timestamp::parse("2023-02-29T00:00:00Z"),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn the_century_rule_holds_in_both_directions() {
        assert!(Timestamp::parse("2000-02-29T00:00:00Z").is_ok());
        assert_eq!(
            Timestamp::parse("2100-02-29T00:00:00Z"),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Timestamp::parse("1900-02-28T00:00:00Z"),
            Err(TimeError::BeforeEpoch)
        );
    }

    #[test]
    fn a_thirty_day_month_accepts_day_thirty() {
        for month in [4, 6, 9, 11] {
            round_trip(&format!("2026-{month:02}-30T00:00:00Z"));
        }
    }

    #[test]
    fn a_thirty_one_day_month_accepts_day_thirty_one() {
        for month in [1, 3, 5, 7, 8, 10, 12] {
            round_trip(&format!("2026-{month:02}-31T00:00:00Z"));
        }
    }

    #[test]
    fn a_thirty_day_month_rejects_day_thirty_one() {
        for month in [4, 6, 9, 11] {
            assert_eq!(
                Timestamp::parse(&format!("2026-{month:02}-31T00:00:00Z")),
                Err(TimeError::OutOfRange)
            );
        }
    }

    #[test]
    fn month_and_day_bounds_are_enforced() {
        assert_eq!(
            Timestamp::parse("2026-13-01T00:00:00Z"),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Timestamp::parse("2026-01-00T00:00:00Z"),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn time_of_day_bounds_are_enforced() {
        assert_eq!(
            Timestamp::parse("2026-01-01T24:00:00Z"),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01T00:60:00Z"),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:60Z"),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn a_missing_offset_is_an_offset_error() {
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00"),
            Err(TimeError::BadOffset)
        );
    }

    #[test]
    fn a_fraction_at_the_end_without_an_offset_is_an_offset_error() {
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00.5"),
            Err(TimeError::BadOffset)
        );
    }

    #[test]
    fn a_malformed_offset_is_an_offset_error() {
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00+0900"),
            Err(TimeError::BadOffset)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00+24:00"),
            Err(TimeError::BadOffset)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00+09:60"),
            Err(TimeError::BadOffset)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00Zz"),
            Err(TimeError::BadOffset)
        );
    }

    #[test]
    fn an_offset_missing_its_colon_is_an_offset_error() {
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00+09-00"),
            Err(TimeError::BadOffset)
        );
    }

    #[test]
    fn an_offset_with_a_trailing_digit_is_an_offset_error() {
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00+09:001"),
            Err(TimeError::BadOffset)
        );
    }

    #[test]
    fn a_wrong_separator_is_a_shape_error() {
        assert_eq!(
            Timestamp::parse("2026/01/01T00:00:00Z"),
            Err(TimeError::BadShape)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01 00:00:00Z"),
            Err(TimeError::BadShape)
        );
        assert_eq!(Timestamp::parse("short"), Err(TimeError::BadShape));
    }

    #[test]
    fn a_single_wrong_separator_is_a_shape_error() {
        assert_eq!(
            Timestamp::parse("2026/01-01T00:00:00Z"),
            Err(TimeError::BadShape)
        );
        assert_eq!(
            Timestamp::parse("2026-01-01T00-00:00Z"),
            Err(TimeError::BadShape)
        );
    }

    #[test]
    fn a_fraction_without_digits_is_a_shape_error() {
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00.Z"),
            Err(TimeError::BadShape)
        );
    }

    #[test]
    fn a_fraction_longer_than_nanoseconds_is_rejected() {
        assert_eq!(
            Timestamp::parse("2026-01-01T00:00:00.1234567890Z"),
            Err(TimeError::BadShape)
        );
    }

    #[test]
    fn a_nanosecond_field_of_a_billion_is_rejected() {
        assert_eq!(Timestamp::new(0, 1_000_000_000), Err(TimeError::OutOfRange));
    }

    #[test]
    fn an_instant_before_the_epoch_is_rejected() {
        assert_eq!(Timestamp::new(-1, 0), Err(TimeError::BeforeEpoch));
        assert_eq!(
            Timestamp::parse("1969-12-31T23:59:59Z"),
            Err(TimeError::BeforeEpoch)
        );
    }

    #[test]
    fn an_era_boundary_round_trips() {
        round_trip("2399-12-31T23:59:59Z");
        round_trip("2400-01-01T00:00:00Z");
        round_trip("2400-02-29T00:00:00Z");
    }

    #[test]
    fn a_century_boundary_round_trips() {
        round_trip("2100-02-28T23:59:59Z");
        round_trip("2100-03-01T00:00:00Z");
        round_trip("2000-03-01T00:00:00Z");
    }

    #[test]
    fn every_month_boundary_round_trips() {
        for month in 1..=12 {
            round_trip(&format!("2026-{month:02}-01T00:00:00Z"));
        }
    }

    #[test]
    fn a_far_future_year_round_trips() {
        round_trip("9999-12-31T23:59:59Z");
    }

    #[test]
    fn the_last_second_of_a_day_does_not_roll_over() {
        let text = "2026-03-01T00:00:00Z";
        let previous =
            Timestamp::new(Timestamp::parse(text).expect("parses").secs() - 1, 0).expect("valid");
        assert_eq!(render(&previous), "2026-02-28T23:59:59Z");
    }

    #[test]
    fn every_hour_of_a_leap_year_round_trips() {
        let start = Timestamp::parse("2024-01-01T00:00:00Z")
            .expect("parses")
            .secs();
        let mut secs = start;
        while secs < start + 366 * 86_400 {
            let timestamp = Timestamp::new(secs, 0).expect("valid");
            let text = render(&timestamp);
            assert_eq!(Timestamp::parse(&text), Ok(timestamp));
            secs += 3_600;
        }
    }

    #[test]
    fn an_error_says_what_went_wrong() {
        assert_eq!(TimeError::BadShape.to_string(), "not an RFC 3339 timestamp");
        assert_eq!(
            TimeError::OutOfRange.to_string(),
            "a date or time field is out of range"
        );
        assert_eq!(
            TimeError::BadOffset.to_string(),
            "offset must be Z or +HH:MM or -HH:MM"
        );
        assert_eq!(
            TimeError::BeforeEpoch.to_string(),
            "the instant is before 1970-01-01T00:00:00Z"
        );
    }
}

#[cfg(test)]
mod plus_tests {
    use super::Timestamp;
    use core::time::Duration;

    fn at() -> Timestamp {
        Timestamp::new(1_790_000_000, 500_000_000).expect("valid")
    }

    #[test]
    fn a_whole_second_moves_the_seconds() {
        let later = at().plus(Duration::from_secs(2)).expect("fits");
        assert_eq!(later.secs(), 1_790_000_002);
        assert_eq!(later.nanos(), 500_000_000);
    }

    #[test]
    fn nanoseconds_carry_into_the_second() {
        let later = at().plus(Duration::from_millis(700)).expect("fits");
        assert_eq!(later.secs(), 1_790_000_001);
        assert_eq!(later.nanos(), 200_000_000);
    }

    #[test]
    fn a_zero_duration_changes_nothing() {
        assert_eq!(at().plus(Duration::ZERO).expect("fits"), at());
    }

    #[test]
    fn a_duration_that_would_overflow_is_refused() {
        let huge = Duration::from_secs(u64::MAX);
        assert!(at().plus(huge).is_err());
    }
}
