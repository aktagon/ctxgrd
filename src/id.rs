//! Document IDs. CORE-003 in the brief.
//!
//! An ID is `<NAMESPACE>-<NUMBER>` where namespace starts with an
//! uppercase ASCII letter and continues with uppercase ASCII letters or
//! digits, and number is one or more ASCII digits. Leading zeros on the
//! number are legal on input (e.g. `ADR-099`) but collapse to the parsed
//! integer for uniqueness checks: `ADR-099` and `ADR-99` collide.

use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

/// Parsed document identifier.
///
/// Two `DocumentId` values are equal iff they share namespace AND number,
/// regardless of how the original string padded the digit field. The
/// `raw` form used in diagnostics is whatever the source emitted — this
/// struct does not remember it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocumentId {
    pub namespace: String,
    pub number: u32,
}

impl DocumentId {
    pub fn new(namespace: impl Into<String>, number: u32) -> Self {
        Self {
            namespace: namespace.into(),
            number,
        }
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.namespace, self.number)
    }
}

/// Parse error for an ID string that doesn't satisfy the CORE-003 regex.
///
/// `input` is preserved so diagnostics can echo exactly what the author
/// wrote.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid document id {input:?}")]
pub struct ParseIdError {
    pub input: String,
}

impl FromStr for DocumentId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let re = id_regex();
        let caps = re.captures(s).ok_or_else(|| ParseIdError {
            input: s.to_owned(),
        })?;
        let namespace = caps
            .get(1)
            .expect("group 1 in id regex")
            .as_str()
            .to_owned();
        let number: u32 = caps
            .get(2)
            .expect("group 2 in id regex")
            .as_str()
            .parse()
            .map_err(|_| ParseIdError {
                input: s.to_owned(),
            })?;
        Ok(Self { namespace, number })
    }
}

fn id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([A-Z][A-Z0-9]*)-(\d+)$").expect("static regex compiles"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<DocumentId, ParseIdError> {
        s.parse()
    }

    #[test]
    fn accepts_basic_form() {
        assert_eq!(parse("ADR-001").unwrap(), DocumentId::new("ADR", 1));
        assert_eq!(parse("PRD-42").unwrap(), DocumentId::new("PRD", 42));
    }

    #[test]
    fn accepts_namespace_with_digits_after_letter() {
        assert_eq!(parse("A1B-7").unwrap(), DocumentId::new("A1B", 7));
        assert_eq!(parse("RFC9110-1").unwrap(), DocumentId::new("RFC9110", 1));
    }

    #[test]
    fn accepts_namespace_with_trailing_digit() {
        // The soc2 compliance pack (ADR-069) claims ids under `SOC2`. The
        // trailing digit is part of the namespace; the final `-001` is the
        // counter, so this is unambiguous — `SOC2` is a legal namespace.
        assert_eq!(parse("SOC2-001").unwrap(), DocumentId::new("SOC2", 1));
        assert_eq!(parse("SOC2-42").unwrap(), DocumentId::new("SOC2", 42));
    }

    #[test]
    fn single_uppercase_letter_namespace_ok() {
        assert_eq!(parse("A-1").unwrap(), DocumentId::new("A", 1));
    }

    #[test]
    fn leading_zeros_collapse_on_number() {
        assert_eq!(parse("ADR-099").unwrap(), DocumentId::new("ADR", 99));
        assert_eq!(parse("ADR-099").unwrap(), parse("ADR-99").unwrap());
    }

    #[test]
    fn empty_string_rejected() {
        assert!(parse("").is_err());
    }

    #[test]
    fn missing_dash_rejected() {
        assert!(parse("ADR001").is_err());
    }

    #[test]
    fn lowercase_namespace_rejected() {
        assert!(parse("adr-1").is_err());
    }

    #[test]
    fn namespace_starting_with_digit_rejected() {
        assert!(parse("1ADR-1").is_err());
    }

    #[test]
    fn number_with_non_digit_rejected() {
        assert!(parse("ADR-1a").is_err());
        assert!(parse("ADR-a1").is_err());
        assert!(parse("ADR--1").is_err());
    }

    #[test]
    fn trailing_whitespace_rejected() {
        assert!(parse("ADR-1 ").is_err());
        assert!(parse(" ADR-1").is_err());
    }

    #[test]
    fn newline_or_embedded_control_rejected() {
        assert!(parse("ADR-1\n").is_err());
        assert!(parse("ADR\n-1").is_err());
    }

    #[test]
    fn multi_segment_slug_rejected() {
        // An on-disk file may be named ADR-099-broken-demo.md but the id
        // itself is only the prefix; a full filename must not parse.
        assert!(parse("ADR-099-broken-demo").is_err());
    }

    #[test]
    fn number_zero_accepted() {
        assert_eq!(parse("ADR-0").unwrap(), DocumentId::new("ADR", 0));
    }

    #[test]
    fn very_large_number_rejected_when_overflows_u32() {
        // 2^32 = 4294967296; one beyond u32::MAX must fail.
        assert!(parse("ADR-4294967296").is_err());
    }

    #[test]
    fn number_at_u32_max_accepted() {
        assert_eq!(parse("ADR-4294967295").unwrap().number, u32::MAX);
    }

    #[test]
    fn display_round_trips_canonical_form() {
        let id = parse("ADR-007").unwrap();
        assert_eq!(id.to_string(), "ADR-7");
    }

    #[test]
    fn error_preserves_input_for_diagnostics() {
        let err = parse("not-an-id").unwrap_err();
        assert_eq!(err.input, "not-an-id");
        assert!(err.to_string().contains("not-an-id"));
    }

    #[test]
    fn equality_and_hashing_ignore_leading_zeros() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(parse("ADR-01").unwrap());
        assert!(set.contains(&parse("ADR-1").unwrap()));
        assert!(set.contains(&parse("ADR-001").unwrap()));
    }
}
