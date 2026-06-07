//! Minimal duration string parser for scenario timeouts and waits.
//!
//! Accepts forms like `"2s"`, `"500ms"`, `"1500ms"`, `"3s"`. We parse this by
//! hand instead of pulling in `humantime`: the grammar we support is tiny
//! (an integer followed by a `ms` or `s` unit), so a dedicated dependency
//! would add surface area without buying us anything.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Deserialize, Deserializer, Visitor};

/// A `Duration` newtype that deserializes from compact human strings such as
/// `"2s"` or `"500ms"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationStr(pub Duration);

impl DurationStr {
    /// Borrow the inner [`Duration`].
    pub fn as_duration(&self) -> Duration {
        self.0
    }
}

impl From<DurationStr> for Duration {
    fn from(d: DurationStr) -> Self {
        d.0
    }
}

/// Parse a duration string into a [`Duration`].
///
/// The input must be a non-negative integer immediately followed by a unit:
/// `ms` for milliseconds or `s` for seconds. Surrounding whitespace is
/// trimmed. Anything else is rejected with a human-readable message.
pub fn parse_duration(input: &str) -> std::result::Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }

    // Split the trailing unit from the leading numeric magnitude by scanning
    // from the front while characters are ASCII digits. We check `ms` before
    // `s` because `ms` is the longer suffix and would otherwise be misread as
    // a bare `s` preceded by a stray `m`.
    let digit_end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());

    if digit_end == 0 {
        return Err(format!("duration '{s}' must start with a number"));
    }

    let (num_part, unit_part) = s.split_at(digit_end);
    let magnitude: u64 = num_part
        .parse()
        .map_err(|_| format!("invalid duration magnitude in '{s}'"))?;

    match unit_part {
        "ms" => Ok(Duration::from_millis(magnitude)),
        "s" => Ok(Duration::from_secs(magnitude)),
        other => Err(format!(
            "unknown duration unit '{other}' in '{s}' (expected 'ms' or 's')"
        )),
    }
}

impl<'de> Deserialize<'de> for DurationStr {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DurationVisitor;

        impl Visitor<'_> for DurationVisitor {
            type Value = DurationStr;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a duration string like \"2s\" or \"500ms\"")
            }

            fn visit_str<E>(self, v: &str) -> std::result::Result<DurationStr, E>
            where
                E: de::Error,
            {
                parse_duration(v)
                    .map(DurationStr)
                    .map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(DurationVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds() {
        // "2s" must yield exactly two seconds.
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
    }

    #[test]
    fn parses_milliseconds() {
        // "500ms" must yield 500 milliseconds, not 500 seconds.
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(
            parse_duration("1500ms").unwrap(),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn trims_surrounding_whitespace() {
        // Leading/trailing whitespace must not change the parsed value.
        assert_eq!(parse_duration("  3s ").unwrap(), Duration::from_secs(3));
    }

    #[test]
    fn accepts_zero_durations() {
        // "0s" and "0ms" are valid zero durations (used by expect timeouts to
        // mean "check once, do not wait").
        assert_eq!(parse_duration("0s").unwrap(), Duration::from_secs(0));
        assert_eq!(parse_duration("0ms").unwrap(), Duration::from_millis(0));
    }

    #[test]
    fn rejects_missing_unit() {
        // A bare number is ambiguous and must be rejected rather than guessed.
        assert!(parse_duration("100").is_err());
    }

    #[test]
    fn rejects_unknown_unit() {
        // Units we do not support (e.g. minutes) must fail loudly.
        assert!(parse_duration("5m").is_err());
        assert!(parse_duration("5min").is_err());
    }

    #[test]
    fn rejects_missing_number() {
        // A unit with no magnitude is invalid.
        assert!(parse_duration("ms").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn deserializes_from_yaml_scalar() {
        // The serde path must round-trip the same grammar as parse_duration.
        let d: DurationStr = serde_norway::from_str("\"750ms\"").unwrap();
        assert_eq!(d.as_duration(), Duration::from_millis(750));
    }
}
