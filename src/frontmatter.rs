//! YAML frontmatter parsing. CORE-002 in the brief.
//!
//! A document body starts with a `---`-fenced YAML block; the parser
//! splits that block from the markdown body, deserializes the YAML, and
//! peels off the two structural keys `id` and `depends_on`. Everything
//! else flows into `metadata` verbatim.
//!
//! The parser reports only two failure modes — missing/misshapen fence,
//! and YAML that doesn't parse — both of which the kernel turns into a
//! `core.frontmatter` diagnostic. Validation of `id` itself lives in
//! [`crate::id`]; the `core.id` rule consumes [`Frontmatter::id`]
//! separately and emits on `None` or empty string.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

const FENCE: &str = "---";

/// A `pin` frontmatter block (ADR-040 § PIN-001): the green commit a
/// document was last validated against and the path globs it covers.
///
/// Parsed once at the ingest boundary alongside `id`/`depends_on`
/// (ADR-029 § PIP-001) — `pin` is a reserved frontmatter key. The git
/// query that consults this data lives entirely in the rule layer
/// (`core.commit-freshness`, ADR-040 § PIN-006); the parse stays a pure
/// function of bytes.
///
/// Invariant: `scope` is non-empty. An absent or empty `scope` on a
/// present `pin` is a parse error (PIN-001), never a whole-repo default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub commit: String,
    pub scope: Vec<String>,
}

/// Frontmatter extracted from a document body.
///
/// Construction rule: `id` holds the raw string as authored (trimmed).
/// `depends_on` collects every string item from the YAML sequence,
/// skipping non-string entries silently — the kernel enforces ID
/// well-formedness downstream via [`crate::id::DocumentId`]. `metadata`
/// contains every other top-level key in the YAML mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frontmatter {
    pub id: Option<String>,
    pub depends_on: Vec<String>,
    /// An optional `pin` block (ADR-040 § PIN-001). `None` when no `pin`
    /// key is present; a malformed `pin` is a parse error.
    pub pin: Option<Pin>,
    pub metadata: BTreeMap<String, Value>,
}

/// What went wrong trying to extract a frontmatter block.
///
/// The kernel turns both variants into a `core.frontmatter` diagnostic;
/// they're kept distinct so tests and future rule authors can tell
/// "no block at all" from "block there but YAML was broken".
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum FrontmatterError {
    /// No opening `---` fence on line 1, or no closing `---` fence found.
    #[error("missing '---' frontmatter fence")]
    MissingFence,
    /// Fence was present and delimited a region, but the YAML inside did
    /// not deserialize as a mapping.
    #[error("invalid YAML frontmatter: {0}")]
    YamlParse(String),
}

impl Frontmatter {
    /// Parse a document body's frontmatter block.
    ///
    /// Thin wrapper around [`Self::parse_with_lines`] for callers that do
    /// not need key line numbers.
    pub(crate) fn parse(body: &str) -> Result<Self, FrontmatterError> {
        let (fm, _) = Self::parse_with_lines(body)?;
        Ok(fm)
    }

    /// Parse the frontmatter block and compute the 1-indexed line number of
    /// every top-level YAML key in a single pass over the body (ADR-029
    /// § PIP-001).
    ///
    /// Expects the body to open with a line consisting solely of `---`
    /// (optionally preceded by a UTF-8 BOM). The closing fence is the
    /// next line that is exactly `---`. Everything between is passed to
    /// `serde_yaml`. Key line numbers are computed from the same YAML
    /// slice without a second walk of the full body.
    pub(crate) fn parse_with_lines(
        body: &str,
    ) -> Result<(Self, BTreeMap<String, u32>), FrontmatterError> {
        let body = strip_bom(body);
        let Some(yaml) = extract_yaml_block(body) else {
            return Err(FrontmatterError::MissingFence);
        };

        // Parse into a JSON Value so the internal representation matches
        // the rule-stdin context wire format. serde_yaml routes through
        // serde, so any YAML mapping that has string keys deserializes
        // fine.
        let value: Value =
            serde_yaml::from_str(yaml).map_err(|e| FrontmatterError::YamlParse(e.to_string()))?;

        let mut map = match value {
            Value::Object(map) => map,
            // An empty frontmatter block (e.g. `---\n---\n`) yields a
            // YAML null. Treat that as an empty mapping rather than an
            // error — the id/required-metadata rules will catch the
            // missing keys.
            Value::Null => serde_json::Map::new(),
            other => {
                return Err(FrontmatterError::YamlParse(format!(
                    "frontmatter must be a mapping, got {}",
                    value_kind(&other)
                )));
            }
        };

        let id = match map.remove("id") {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            }
            Some(Value::Null) | None => None,
            Some(other) => {
                return Err(FrontmatterError::YamlParse(format!(
                    "'id' must be a string, got {}",
                    value_kind(&other)
                )));
            }
        };

        let depends_on = match map.remove("depends_on") {
            Some(Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
            Some(Value::Null) | None => Vec::new(),
            Some(other) => {
                return Err(FrontmatterError::YamlParse(format!(
                    "'depends_on' must be a sequence, got {}",
                    value_kind(&other)
                )));
            }
        };

        // `pin` is a reserved frontmatter key (ADR-040 § PIN-001): peel
        // it off here in the single pass so it never leaks into
        // `metadata`. A present `pin` must carry a `commit` string and a
        // non-empty `scope` list of path globs; anything else is a parse
        // error, not a silent skip (PIN-001 — an empty scope is never a
        // whole-repo default).
        let pin = match map.remove("pin") {
            Some(Value::Object(pin_map)) => Some(parse_pin(pin_map)?),
            Some(Value::Null) | None => None,
            Some(other) => {
                return Err(FrontmatterError::YamlParse(format!(
                    "'pin' must be a mapping with `commit` and `scope`, got {}",
                    value_kind(&other)
                )));
            }
        };

        let metadata: BTreeMap<String, Value> = map.into_iter().collect();

        // Key line numbers: walk the YAML slice (already located above)
        // to find top-level keys. The opening "---" is line 1, so the
        // first YAML line (index 0) maps to body line 2.
        let mut key_lines = BTreeMap::new();
        for (idx, line) in yaml.lines().enumerate() {
            if let Some(key) = top_level_key(line) {
                key_lines.insert(key, (idx + 2) as u32);
            }
        }

        Ok((
            Self {
                id,
                depends_on,
                pin,
                metadata,
            },
            key_lines,
        ))
    }
}

/// Merge source-provided `extra` with frontmatter metadata.
///
/// Implements the CORE-002 rule `source.extra ⊕ body.frontmatter` with
/// frontmatter winning on key conflict. Neither input is mutated.
pub(crate) fn merge_metadata(
    source_extra: &BTreeMap<String, Value>,
    frontmatter: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    let mut out = source_extra.clone();
    for (k, v) in frontmatter {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Byte offset of the first post-frontmatter character in `body`.
///
/// Returns `0` when there's no frontmatter block, so callers can always
/// pass `&body[offset..]` to the markdown parser. A partial block (fence
/// opens but never closes) also returns `0`, because the source won't
/// treat the body's leading `---` as a horizontal rule in that case — it
/// treats it as a broken frontmatter, and the `core.frontmatter` rule
/// will flag it separately.
pub(crate) fn body_start_offset(body: &str) -> usize {
    let stripped = strip_bom(body);
    let bom_len = body.len() - stripped.len();
    let Some(after_fence) = stripped.strip_prefix(FENCE) else {
        return 0;
    };
    let rest = if let Some(r) = after_fence.strip_prefix('\n') {
        r
    } else if let Some(r) = after_fence.strip_prefix("\r\n") {
        r
    } else {
        return 0;
    };
    let rest_start = body.len() - rest.len();
    let mut cursor = rest_start;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        cursor += line.len();
        if trimmed == FENCE {
            return cursor;
        }
    }
    // No closing fence — treat as no frontmatter so the caller parses
    // the whole body. The rule layer catches the missing-fence case.
    let _ = bom_len;
    0
}

/// Deserialize a `pin` mapping into a [`Pin`], enforcing PIN-001: a
/// non-empty `commit` string and a non-empty `scope` list of strings.
fn parse_pin(map: serde_json::Map<String, Value>) -> Result<Pin, FrontmatterError> {
    let commit = match map.get("commit") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
        Some(Value::String(_)) | None => {
            return Err(FrontmatterError::YamlParse(
                "'pin.commit' must be a non-empty git revision string".to_owned(),
            ));
        }
        Some(other) => {
            return Err(FrontmatterError::YamlParse(format!(
                "'pin.commit' must be a string, got {}",
                value_kind(other)
            )));
        }
    };

    let scope = match map.get("scope") {
        Some(Value::Array(items)) => {
            let globs: Vec<String> = items
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
                    _ => None,
                })
                .collect();
            if globs.is_empty() {
                return Err(FrontmatterError::YamlParse(
                    "'pin.scope' must be a non-empty list of path globs — an empty scope is \
                     not a whole-repo default (ADR-040 § PIN-001)"
                        .to_owned(),
                ));
            }
            globs
        }
        Some(other) => {
            return Err(FrontmatterError::YamlParse(format!(
                "'pin.scope' must be a sequence of path globs, got {}",
                value_kind(other)
            )));
        }
        None => {
            return Err(FrontmatterError::YamlParse(
                "'pin.scope' is required and must be a non-empty list of path globs \
                 (ADR-040 § PIN-001)"
                    .to_owned(),
            ));
        }
    };

    Ok(Pin { commit, scope })
}

fn top_level_key(line: &str) -> Option<String> {
    if line.starts_with([' ', '\t', '-', '#']) || line.is_empty() {
        return None;
    }
    let colon = line.find(':')?;
    let key = &line[..colon];
    // Reject keys that look like URLs or quoted strings — real YAML
    // mapping keys are plain identifiers in the kinds of frontmatter
    // we care about. If this turns out to be too strict we'll expand.
    if key.is_empty() || key.contains(|c: char| c.is_whitespace()) {
        return None;
    }
    Some(key.to_owned())
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Return the YAML text between an opening `---` line and the next
/// `---` line, or `None` if either fence is missing.
///
/// The opening fence MUST be on line 1 with no leading whitespace other
/// than an optional BOM (already stripped upstream). A closing fence is
/// any line that trims to exactly `---`.
fn extract_yaml_block(body: &str) -> Option<&str> {
    let after_fence = body.strip_prefix(FENCE)?;
    // Require the opening fence to be its own line: allow `\n` or `\r\n`
    // immediately after `---`. Anything else (e.g. `---foo`) disqualifies.
    let rest = if let Some(r) = after_fence.strip_prefix('\n') {
        r
    } else if let Some(r) = after_fence.strip_prefix("\r\n") {
        r
    } else {
        return None;
    };

    // Walk lines looking for the closing fence.
    let mut end_of_yaml = None;
    let mut cursor = 0usize;
    for line in rest.split_inclusive('\n') {
        let line_content = line.trim_end_matches('\n').trim_end_matches('\r');
        if line_content == FENCE {
            end_of_yaml = Some(cursor);
            break;
        }
        cursor += line.len();
    }
    // If the body ends without a closing fence but the last line is
    // exactly `---` (no trailing newline), still accept it.
    if end_of_yaml.is_none() && rest.trim_end_matches(['\n', '\r']).ends_with(FENCE) {
        let trimmed = rest.trim_end_matches(['\n', '\r']);
        if trimmed == FENCE || trimmed.ends_with(&format!("\n{FENCE}")) {
            end_of_yaml = Some(trimmed.len() - FENCE.len());
        }
    }

    end_of_yaml.map(|end| &rest[..end])
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "sequence",
        Value::Object(_) => "mapping",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(body: &str) -> Result<Frontmatter, FrontmatterError> {
        Frontmatter::parse(body)
    }

    #[test]
    fn minimal_frontmatter_parses() {
        let body = "---\nid: ADR-001\n---\n# Title\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id.as_deref(), Some("ADR-001"));
        assert!(fm.depends_on.is_empty());
        assert!(fm.metadata.is_empty());
    }

    #[test]
    fn missing_fence_is_error() {
        assert_eq!(
            parse("id: ADR-001\n").unwrap_err(),
            FrontmatterError::MissingFence
        );
        assert_eq!(parse("").unwrap_err(), FrontmatterError::MissingFence);
        assert_eq!(
            parse("# just a heading\n").unwrap_err(),
            FrontmatterError::MissingFence
        );
    }

    #[test]
    fn fence_without_closing_is_error() {
        let body = "---\nid: ADR-001\nno closing fence here\n";
        assert_eq!(parse(body).unwrap_err(), FrontmatterError::MissingFence);
    }

    #[test]
    fn fence_not_on_first_line_is_error() {
        let body = "\n---\nid: ADR-001\n---\n";
        assert_eq!(parse(body).unwrap_err(), FrontmatterError::MissingFence);
    }

    #[test]
    fn body_header_adr_shape_yields_no_extraction() {
        // EXT-001 verification (ADR 006 § EXT-001). The adr-tools
        // convention encodes the ID in the H1 and status in a bold-prefix
        // body line. The kernel MUST refuse to extract from either —
        // frontmatter is the only metadata surface. If a future contributor
        // adds a body-header fallback, this test fails first.
        let body = "# ADR-001: title\n**Status:** accepted\n";
        assert_eq!(parse(body).unwrap_err(), FrontmatterError::MissingFence);
    }

    #[test]
    fn opening_fence_with_trailing_chars_rejected() {
        // `---foo` on line 1 is NOT a fence.
        let body = "---foo\nid: ADR-001\n---\n";
        assert_eq!(parse(body).unwrap_err(), FrontmatterError::MissingFence);
    }

    #[test]
    fn crlf_line_endings_accepted() {
        let body = "---\r\nid: ADR-001\r\n---\r\n# body\r\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id.as_deref(), Some("ADR-001"));
    }

    #[test]
    fn utf8_bom_is_stripped_before_fence_check() {
        let body = "\u{feff}---\nid: ADR-001\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id.as_deref(), Some("ADR-001"));
    }

    #[test]
    fn missing_id_yields_none() {
        let body = "---\ntitle: Untitled\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id, None);
        assert_eq!(fm.metadata.get("title"), Some(&json!("Untitled")));
    }

    #[test]
    fn empty_id_yields_none() {
        let body = "---\nid: ''\ntitle: T\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id, None);
    }

    #[test]
    fn null_id_yields_none() {
        let body = "---\nid: ~\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id, None);
    }

    #[test]
    fn whitespace_only_id_yields_none() {
        let body = "---\nid: '   '\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id, None);
    }

    #[test]
    fn non_string_id_is_parse_error() {
        let body = "---\nid: 42\n---\n";
        let err = parse(body).unwrap_err();
        assert!(matches!(err, FrontmatterError::YamlParse(_)));
    }

    #[test]
    fn depends_on_strings_collected() {
        let body = "---\nid: ADR-001\ndepends_on:\n  - PRD-001\n  - PRD-002\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.depends_on, vec!["PRD-001", "PRD-002"]);
    }

    #[test]
    fn depends_on_missing_is_empty_vec() {
        let body = "---\nid: ADR-001\n---\n";
        let fm = parse(body).unwrap();
        assert!(fm.depends_on.is_empty());
    }

    #[test]
    fn depends_on_null_is_empty_vec() {
        let body = "---\nid: ADR-001\ndepends_on: ~\n---\n";
        let fm = parse(body).unwrap();
        assert!(fm.depends_on.is_empty());
    }

    #[test]
    fn depends_on_non_sequence_is_parse_error() {
        let body = "---\nid: ADR-001\ndepends_on: PRD-001\n---\n";
        assert!(matches!(
            parse(body).unwrap_err(),
            FrontmatterError::YamlParse(_)
        ));
    }

    #[test]
    fn pin_block_parses_commit_and_scope() {
        let body = "---\nid: ADR-041\npin:\n  commit: a1b2c3d4\n  scope:\n    - src/auth/**\n    - Cargo.lock\n---\n";
        let fm = parse(body).unwrap();
        let pin = fm.pin.expect("pin present");
        assert_eq!(pin.commit, "a1b2c3d4");
        assert_eq!(pin.scope, vec!["src/auth/**", "Cargo.lock"]);
        // `pin` must not leak into metadata.
        assert!(!fm.metadata.contains_key("pin"));
    }

    #[test]
    fn pin_absent_is_none() {
        let body = "---\nid: ADR-041\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.pin, None);
    }

    #[test]
    fn pin_with_empty_scope_is_parse_error() {
        let body = "---\nid: ADR-041\npin:\n  commit: a1b2c3d4\n  scope: []\n---\n";
        let err = parse(body).unwrap_err();
        assert!(matches!(err, FrontmatterError::YamlParse(_)));
    }

    #[test]
    fn pin_with_missing_scope_is_parse_error() {
        let body = "---\nid: ADR-041\npin:\n  commit: a1b2c3d4\n---\n";
        let err = parse(body).unwrap_err();
        assert!(matches!(err, FrontmatterError::YamlParse(_)));
    }

    #[test]
    fn pin_with_missing_commit_is_parse_error() {
        let body = "---\nid: ADR-041\npin:\n  scope:\n    - src/auth/**\n---\n";
        let err = parse(body).unwrap_err();
        assert!(matches!(err, FrontmatterError::YamlParse(_)));
    }

    #[test]
    fn pin_non_mapping_is_parse_error() {
        let body = "---\nid: ADR-041\npin: a1b2c3d4\n---\n";
        let err = parse(body).unwrap_err();
        assert!(matches!(err, FrontmatterError::YamlParse(_)));
    }

    #[test]
    fn metadata_captures_unknown_keys() {
        let body = "---\nid: ADR-001\ntitle: A title\nstatus: accepted\ntags:\n  - security\n  - audit\n---\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id.as_deref(), Some("ADR-001"));
        assert_eq!(fm.metadata.get("title"), Some(&json!("A title")));
        assert_eq!(fm.metadata.get("status"), Some(&json!("accepted")));
        assert_eq!(fm.metadata.get("tags"), Some(&json!(["security", "audit"])));
        // id and depends_on must not leak into metadata.
        assert!(!fm.metadata.contains_key("id"));
        assert!(!fm.metadata.contains_key("depends_on"));
    }

    #[test]
    fn metadata_keys_sorted_for_stable_iteration() {
        let body = "---\nid: ADR-001\nzeta: 1\nalpha: 2\nmu: 3\n---\n";
        let fm = parse(body).unwrap();
        let keys: Vec<&String> = fm.metadata.keys().collect();
        assert_eq!(keys, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn empty_frontmatter_block_parses_as_all_none() {
        let body = "---\n---\n# body\n";
        let fm = parse(body).unwrap();
        assert_eq!(fm.id, None);
        assert!(fm.depends_on.is_empty());
        assert!(fm.metadata.is_empty());
    }

    #[test]
    fn invalid_yaml_fails_with_yamlparse() {
        // Unterminated flow sequence — no closing `]`.
        let body = "---\nid: ADR-001\ntags: [security, audit\n---\n";
        let err = parse(body).unwrap_err();
        assert!(matches!(err, FrontmatterError::YamlParse(_)));
    }

    #[test]
    fn scalar_frontmatter_rejected() {
        // A frontmatter block containing just a scalar (not a mapping)
        // is not a legal document header.
        let body = "---\njust a string\n---\n";
        let err = parse(body).unwrap_err();
        assert!(matches!(err, FrontmatterError::YamlParse(_)));
    }

    #[test]
    fn merge_metadata_frontmatter_wins_on_conflict() {
        let source_extra: BTreeMap<String, Value> = [
            ("status".to_string(), json!("from-source")),
            ("external_id".to_string(), json!("XYZ-7")),
        ]
        .into();
        let frontmatter: BTreeMap<String, Value> = [
            ("status".to_string(), json!("from-frontmatter")),
            ("title".to_string(), json!("Doc title")),
        ]
        .into();

        let merged = merge_metadata(&source_extra, &frontmatter);

        assert_eq!(merged.get("status"), Some(&json!("from-frontmatter")));
        assert_eq!(merged.get("external_id"), Some(&json!("XYZ-7")));
        assert_eq!(merged.get("title"), Some(&json!("Doc title")));
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn merge_metadata_preserves_inputs() {
        let source_extra: BTreeMap<String, Value> = [("a".to_string(), json!(1))].into();
        let frontmatter: BTreeMap<String, Value> = [("a".to_string(), json!(2))].into();

        let _ = merge_metadata(&source_extra, &frontmatter);

        assert_eq!(source_extra.get("a"), Some(&json!(1)));
        assert_eq!(frontmatter.get("a"), Some(&json!(2)));
    }

    #[test]
    fn merge_metadata_empty_inputs() {
        let empty: BTreeMap<String, Value> = BTreeMap::new();
        assert!(merge_metadata(&empty, &empty).is_empty());
    }
}
