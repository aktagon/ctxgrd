//! A rule description may not cite a `draft` ADR (HANDOFF-037 § A7).
//!
//! Every registry rule carries its originating ADR in its description by
//! convention — `"… constrains the successor to one namespace (ADR-073)."`.
//! That citation is a claim: *this rule is the decision that ADR records*.
//! A shipped rule whose ADR still reads `draft` breaks the claim in the
//! direction that matters — the decision is live in the registry and in
//! consumers' configs, while the record still says it is being proposed.
//!
//! This supersedes `HANDOFF-034` § V10's proposed `core.status-implemented`,
//! which keyed on "every requirement says implemented" and would have missed
//! `ADR-073` (whose requirements were never restated after the rule shipped
//! in `v0.48.0`). Keying on the citation instead asks the question from the
//! side that can actually go stale.
//!
//! Third instance of the drift genre, after `tests/dogfood_pack_drift.rs`
//! and `tests/dogfood_param_docs.rs`: a fact stated in two places, pinned by
//! a test that names the divergence.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ctxgrd::introspect::rule_descriptions;

/// Statuses that mean the decision is still being proposed. Deliberately
/// narrow: `rejected` and `superseded` are legitimate citations (a rule may
/// outlive the ADR that introduced it), and only `draft` contradicts a
/// shipped rule.
const UNSHIPPED: &[&str] = &["draft"];

/// Every `ADR-<number>` token in `text`, deduped, in first-seen order of
/// number. Matches the citation form the descriptions actually use; the
/// prose-only `ADR 0NN § XXX-YYY` notation does not appear in them.
fn adr_citations(text: &str) -> BTreeSet<u32> {
    let mut found = BTreeSet::new();
    let bytes = text.as_bytes();
    for (i, _) in text.match_indices("ADR-") {
        let digits: String = bytes[i + 4..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .map(|b| *b as char)
            .collect();
        if !digits.is_empty() {
            if let Ok(n) = digits.parse::<u32>() {
                found.insert(n);
            }
        }
    }
    found
}

/// `docs/adrs/<NNN>-*.md` for a cited number, or `None` when no such ADR
/// exists.
fn adr_path(number: u32) -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/adrs");
    let prefix = format!("{number:03}-");
    fs::read_dir(dir)
        .expect("docs/adrs is readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".md"))
        })
}

/// The `status:` value from a document's YAML frontmatter, lowercased.
fn frontmatter_status(path: &Path) -> Option<String> {
    let body = fs::read_to_string(path).expect("ADR is readable");
    let mut lines = body.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        if line.trim() == "---" {
            return None;
        }
        if let Some(value) = line.strip_prefix("status:") {
            return Some(value.trim().to_lowercase());
        }
    }
    None
}

#[test]
fn no_registry_rule_cites_a_draft_adr() {
    let mut offenders: Vec<String> = Vec::new();

    for (code, description) in rule_descriptions() {
        for number in adr_citations(description) {
            let Some(path) = adr_path(number) else {
                offenders.push(format!(
                    "{code} cites ADR-{number:03}, which has no document under docs/adrs/"
                ));
                continue;
            };
            let status = frontmatter_status(&path).unwrap_or_else(|| {
                panic!("{} declares no frontmatter status", path.display())
            });
            if UNSHIPPED.contains(&status.as_str()) {
                offenders.push(format!(
                    "{code} cites ADR-{number:03}, whose status is `{status}` — \
                     the rule ships, so the decision is made"
                ));
            }
        }
    }

    assert_eq!(
        offenders,
        Vec::<String>::new(),
        "shipped rules citing an unshipped ADR:\n  {}",
        offenders.join("\n  ")
    );
}
