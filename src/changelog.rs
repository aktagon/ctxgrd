//! Git-derived changelog generation (ADR-084).
//!
//! `CHANGELOG.md` is treated as a *derived projection of the document
//! graph*, not a hand-maintained ledger. Each released section is built by
//! reading the whitelisted-namespace documents *at each release tag*
//! (`git show <tag>:<path>`) and attributing every document to the **first**
//! tag whose frozen tree marks it terminal (e.g. a `BUG` with
//! `status: fixed`). Released sections are immutable because they are read
//! from frozen tags — editing a document at `HEAD` changes only
//! `## [Unreleased]` (CHG-004).
//!
//! This module is a **command**, not a lint rule. It reuses the ADR-029
//! ingest frontmatter parser per `(document, ref)` and the git-subprocess
//! layer (ADR-040), and writes markdown out — the same class of operation
//! as `pin --write` / `init` / `scaffold`. It never re-parses a document
//! body; entry text comes from the `changelog:` frontmatter field, falling
//! back to `title` (CHG-003).
//!
//! Migration is by cutover (CHG-006): a literal marker line separates the
//! generated region (top) from the hand-authored history (frozen tail).
//! `--write` regenerates only the region above the marker; everything from
//! the marker down is preserved byte-for-byte.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde::Serialize;
use thiserror::Error;

use crate::config::{self, ChangelogConfig, Config};
use crate::frontmatter::Frontmatter;
use crate::id::DocumentId;

/// The cutover boundary (CHG-006). Everything from this line to EOF is
/// hand-authored history that `--write` preserves verbatim; everything
/// above it is regenerated.
pub(crate) const CUTOVER_MARKER: &str =
    "<!-- ctxgrd:cutover — hand-authored history below is not regenerated (ADR-084 § CHG-006) -->";

/// Keep-a-Changelog section categories, in canonical render order. A
/// section a namespace maps to that is not in this list still renders — it
/// is appended after these, alphabetically — so the config is not silently
/// constrained to the six standard names.
const SECTION_ORDER: [&str; 6] = [
    "Added",
    "Changed",
    "Deprecated",
    "Removed",
    "Fixed",
    "Security",
];

/// What can go wrong generating the changelog.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ChangelogError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Lint(#[from] crate::run::LintError),
    /// No `[changelog]` table is configured — there is nothing to derive.
    #[error("no `[changelog]` table in ctxgrd.toml — nothing to generate")]
    NotConfigured,
    /// A whitelisted namespace has no `paths` glob, so its documents
    /// cannot be located at a tag.
    #[error(
        "namespace `{0}` is whitelisted for the changelog but declares no `[{0}].paths` — \
         a path-claim glob is required to locate its documents at a tag"
    )]
    NamespaceNoPaths(String),
    /// `git` is unavailable or a subprocess failed.
    #[error("git command failed: {0}")]
    Git(String),
    /// A `since` cutover tag is configured but the on-disk `CHANGELOG.md`
    /// carries no cutover marker to preserve history below.
    #[error(
        "`[changelog].since` is set but CHANGELOG.md has no cutover marker — \
         seed the cutover first (insert the marker above the frozen history)"
    )]
    MissingMarker,
    /// Reading or writing `CHANGELOG.md` failed.
    #[error("failed to access {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl ChangelogError {
    /// Stable diagnostic code for the CLI's `error: [<code>] …` line.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(c) => c.code().unwrap_or("cfg.invalid"),
            Self::Lint(l) => l.code().unwrap_or("cfg.invalid"),
            Self::NotConfigured => "changelog.not-configured",
            Self::NamespaceNoPaths(_) => "changelog.namespace-no-paths",
            Self::Git(_) => "changelog.git",
            Self::MissingMarker => "changelog.missing-marker",
            Self::Io { .. } => "changelog.io",
        }
    }
}

/// One rendered changelog entry: a document's display id and its line text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Authored id string, e.g. `BUG-017` (used for the `(BUG-017)` suffix).
    pub id: String,
    /// The line text: the `changelog:` field, or `title` fallback (CHG-003).
    pub text: String,
}

/// A single version block — `## [Unreleased]` or `## [X.Y.Z] — date`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionSection {
    /// Version string without the `v` prefix (`0.51.0`), or `None` for
    /// `## [Unreleased]`.
    pub version: Option<String>,
    /// Tag date (`YYYY-MM-DD`), or `None` for `## [Unreleased]`.
    pub date: Option<String>,
    /// Keep-a-Changelog section → entries, rendered in [`SECTION_ORDER`].
    pub sections: BTreeMap<String, Vec<Entry>>,
}

/// The derived changelog: the generated version blocks (Unreleased first,
/// then releases version-descending). The frozen preamble is handled
/// separately by [`write`]/[`check`] — it is opaque authored text, not part
/// of the derived model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Changelog {
    pub versions: Vec<VersionSection>,
}

// ---------------------------------------------------------------------------
// Semver + tag enumeration
// ---------------------------------------------------------------------------

/// A `vMAJOR.MINOR.PATCH` release tag with its section date.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tag {
    /// Full tag name as git knows it (`v0.48.0`).
    name: String,
    /// `(major, minor, patch)` for ordering.
    version: (u64, u64, u64),
    /// Version string without the `v` (`0.48.0`).
    version_str: String,
    /// Section date `YYYY-MM-DD`.
    date: String,
}

/// Parse `vMAJOR.MINOR.PATCH` → `(major, minor, patch)` and the bare
/// version string. `None` when the name is not a release tag.
fn parse_version(name: &str) -> Option<((u64, u64, u64), String)> {
    let bare = name.strip_prefix('v')?;
    let mut parts = bare.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // more than three components — not a release tag
    }
    Some(((major, minor, patch), bare.to_string()))
}

/// Enumerate `vX.Y.Z` release tags with their section dates, sorted
/// ascending by version. `creatordate` resolves to the tag date for
/// annotated tags and the commit date for lightweight ones (CHG-004).
fn list_release_tags(root: &Path) -> Result<Vec<Tag>, ChangelogError> {
    let out = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname:short)%09%(creatordate:short)",
            "refs/tags",
        ],
    )?;
    let mut tags = Vec::new();
    for line in out.lines() {
        let mut cols = line.split('\t');
        let (Some(name), Some(date)) = (cols.next(), cols.next()) else {
            continue;
        };
        let Some((version, version_str)) = parse_version(name) else {
            continue;
        };
        tags.push(Tag {
            name: name.to_string(),
            version,
            version_str,
            date: date.to_string(),
        });
    }
    tags.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(tags)
}

// ---------------------------------------------------------------------------
// Reading documents at a ref
// ---------------------------------------------------------------------------

/// A whitelisted document as read at some git ref.
struct DocAtRef {
    id: DocumentId,
    raw_id: String,
    status: Option<String>,
    text: String,
}

/// Read every whitelisted-namespace document present at `ref` (a tag or
/// `HEAD`), parsing each blob through the ingest frontmatter parser
/// (ADR-029). Only files matching the namespace's `paths` glob whose
/// frontmatter carries a valid `<NS>-<n>` id are returned.
fn docs_at_ref(
    root: &Path,
    git_ref: &str,
    namespace: &str,
    paths: &globset::GlobSet,
) -> Result<Vec<DocAtRef>, ChangelogError> {
    let listing = git(root, &["ls-tree", "-r", "--name-only", git_ref])?;
    let mut docs = Vec::new();
    for path in listing.lines() {
        if !path.ends_with(".md") || !paths.is_match(path) {
            continue;
        }
        let blob = git(root, &["show", &format!("{git_ref}:{path}")])?;
        let Ok(fm) = Frontmatter::parse(&blob) else {
            continue;
        };
        let Some(raw_id) = fm.id.clone() else {
            continue;
        };
        let Ok(id) = raw_id.parse::<DocumentId>() else {
            continue;
        };
        if id.namespace != namespace {
            continue;
        }
        let text = entry_text(&fm, &raw_id);
        docs.push(DocAtRef {
            id,
            raw_id,
            status: string_field(&fm, "status"),
            text,
        });
    }
    Ok(docs)
}

/// A frontmatter string field, if present and a string.
fn string_field(fm: &Frontmatter, key: &str) -> Option<String> {
    fm.metadata.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// Entry text for a document: `changelog:` field, then `title`, then the
/// raw id as a last resort (CHG-003 — never parse the body).
fn entry_text(fm: &Frontmatter, raw_id: &str) -> String {
    string_field(fm, "changelog")
        .or_else(|| string_field(fm, "title"))
        .unwrap_or_else(|| raw_id.to_string())
}

// ---------------------------------------------------------------------------
// Build the derived model
// ---------------------------------------------------------------------------

/// A whitelisted namespace resolved for generation: its name, the compiled
/// `paths` globset that locates its documents, its terminal `when` status,
/// and the `section` its entries render under.
struct Whitelisted {
    namespace: String,
    paths: globset::GlobSet,
    when: String,
    section: String,
}

/// Resolve the compiled `paths` globset for every whitelisted namespace,
/// erroring on any that declares none.
fn whitelist_globs(
    config: &Config,
    cfg: &ChangelogConfig,
) -> Result<Vec<Whitelisted>, ChangelogError> {
    let mut out = Vec::new();
    for ns in &cfg.namespaces {
        let entry = &cfg.entries[ns]; // presence guaranteed by config validation
        let paths = config
            .namespaces
            .get(ns)
            .and_then(|c| c.paths.clone())
            .ok_or_else(|| ChangelogError::NamespaceNoPaths(ns.clone()))?;
        out.push(Whitelisted {
            namespace: ns.clone(),
            paths,
            when: entry.when.clone(),
            section: entry.section.clone(),
        });
    }
    Ok(out)
}

/// Insert an entry into a section map, keeping entries sorted by document id.
fn push_entry(sections: &mut BTreeMap<String, Vec<Entry>>, section: &str, id: &DocumentId, entry: Entry) {
    let bucket = sections.entry(section.to_string()).or_default();
    // Binary-insert by DocumentId so the render is deterministic without a
    // trailing sort pass. We reconstruct the id from the display string for
    // comparison; entries in a bucket are unique by id.
    let pos = bucket
        .binary_search_by(|e| entry_id(&e.id).cmp(id))
        .unwrap_or_else(|p| p);
    bucket.insert(pos, entry);
}

/// Parse an `Entry.id` string back to a `DocumentId` for ordering. A
/// well-formed entry id always parses; fall back to a sentinel otherwise.
fn entry_id(raw: &str) -> DocumentId {
    raw.parse().unwrap_or_else(|_| DocumentId::new("ZZZ", u32::MAX))
}

/// Build the derived [`Changelog`] model from the tag history and the
/// working tree (CHG-004/CHG-005).
pub fn build(root: &Path) -> Result<Changelog, ChangelogError> {
    let config = config::load(root)?;
    let cfg = config
        .changelog
        .clone()
        .ok_or(ChangelogError::NotConfigured)?;
    let whitelist = whitelist_globs(&config, &cfg)?;

    let tags = list_release_tags(root)?;
    let since_ver = match &cfg.since {
        Some(name) => Some(parse_version(name).map(|(v, _)| v).ok_or_else(|| {
            ChangelogError::Git(format!("`since` tag {name:?} is not a vMAJOR.MINOR.PATCH tag"))
        })?),
        None => None,
    };

    // Already-shipped ids: those terminal in the cutover tag's tree. Reading
    // just the `since` tree keeps this O(documents) — status is effectively
    // monotonic (a fixed bug stays fixed), so anything terminal before the
    // cutover is terminal in its tree (CHG-005). With no cutover, empty.
    let mut attributed: BTreeSet<DocumentId> = BTreeSet::new();
    if let Some(since_name) = &cfg.since {
        for w in &whitelist {
            for doc in docs_at_ref(root, since_name, &w.namespace, &w.paths)? {
                if doc.status.as_deref() == Some(w.when.as_str()) {
                    attributed.insert(doc.id);
                }
            }
        }
    }

    // Released blocks: tags strictly after the cutover (or all tags when
    // uncut), ascending so first-terminal-tag attribution is stable.
    let mut released: Vec<VersionSection> = Vec::new();
    for tag in &tags {
        if let Some(since) = since_ver {
            if tag.version <= since {
                continue;
            }
        }
        let mut sections: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
        for w in &whitelist {
            for doc in docs_at_ref(root, &tag.name, &w.namespace, &w.paths)? {
                if doc.status.as_deref() != Some(w.when.as_str()) {
                    continue;
                }
                if !attributed.insert(doc.id.clone()) {
                    continue; // already attributed to an earlier tag
                }
                push_entry(&mut sections, &w.section, &doc.id, Entry { id: doc.raw_id, text: doc.text });
            }
        }
        // A tagged release with no new whitelisted document earns no
        // synthetic section (CHG-007 non-goal).
        if !sections.is_empty() {
            released.push(VersionSection {
                version: Some(tag.version_str.clone()),
                date: Some(tag.date.clone()),
                sections,
            });
        }
    }

    // `## [Unreleased]`: terminal-at-HEAD (the working tree, via the ADR-029
    // ingest pipeline) minus every already-shipped/attributed id (CHG-004).
    let ingest = crate::run::ingest(root)?;
    let mut unreleased: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for w in &whitelist {
        for doc in &ingest.documents {
            if doc.id.namespace != w.namespace {
                continue;
            }
            if doc.metadata.get("status").and_then(|v| v.as_str()) != Some(w.when.as_str()) {
                continue;
            }
            if attributed.contains(&doc.id) {
                continue;
            }
            let text = doc
                .metadata
                .get("changelog")
                .and_then(|v| v.as_str())
                .or_else(|| doc.metadata.get("title").and_then(|v| v.as_str()))
                .unwrap_or(&doc.raw_id)
                .to_string();
            push_entry(&mut unreleased, &w.section, &doc.id, Entry { id: doc.raw_id.clone(), text });
        }
    }

    // Unreleased first, then releases version-descending (CHG-005). The
    // version is the tiebreak for same-date releases; a back-ported patch
    // sorts below a newer-numbered release by design.
    released.sort_by(|a, b| {
        let av = a.version.as_deref().and_then(|s| parse_version(&format!("v{s}")).map(|(v, _)| v));
        let bv = b.version.as_deref().and_then(|s| parse_version(&format!("v{s}")).map(|(v, _)| v));
        bv.cmp(&av)
    });

    let mut versions = Vec::with_capacity(released.len() + 1);
    versions.push(VersionSection {
        version: None,
        date: None,
        sections: unreleased,
    });
    versions.extend(released);

    Ok(Changelog { versions })
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the generated region (header + version blocks), ending with a
/// trailing blank line so a following cutover marker is visually separated.
fn render_head(changelog: &Changelog) -> String {
    let mut out = String::new();
    out.push_str("# Changelog\n\n");
    out.push_str(
        "All notable changes to this project are documented in this file. The sections above the \
         cutover marker are generated by `ctxgrd changelog` from the document graph (ADR-084); the \
         history below the marker is hand-authored and is not regenerated.\n\n",
    );
    out.push_str(
        "The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this \
         project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).\n\n",
    );

    for v in &changelog.versions {
        match (&v.version, &v.date) {
            (Some(ver), Some(date)) => {
                out.push_str(&format!("## [{ver}] — {date}\n\n"));
            }
            _ => {
                out.push_str("## [Unreleased]\n\n");
            }
        }
        for section in section_order(&v.sections) {
            let entries = &v.sections[&section];
            out.push_str(&format!("### {section}\n\n"));
            for e in entries {
                out.push_str(&format!("- {} ({})\n", e.text, e.id));
            }
            out.push('\n');
        }
    }
    out
}

/// Section names present in `sections`, ordered by [`SECTION_ORDER`] then
/// any non-standard names alphabetically.
fn section_order(sections: &BTreeMap<String, Vec<Entry>>) -> Vec<String> {
    let mut ordered: Vec<String> = SECTION_ORDER
        .iter()
        .filter(|s| sections.contains_key(**s))
        .map(|s| s.to_string())
        .collect();
    for key in sections.keys() {
        if !SECTION_ORDER.contains(&key.as_str()) {
            ordered.push(key.clone());
        }
    }
    ordered
}

/// Everything from the cutover marker line to EOF, verbatim. `None` when
/// the content carries no marker.
fn frozen_tail(content: &str) -> Option<&str> {
    let idx = content.find(CUTOVER_MARKER)?;
    // Back up to the start of the marker's line.
    let line_start = content[..idx].rfind('\n').map(|n| n + 1).unwrap_or(0);
    Some(&content[line_start..])
}

/// Render the full `CHANGELOG.md` for `root`: the generated head, plus the
/// preserved frozen tail below the cutover marker (CHG-006).
pub fn generate(root: &Path) -> Result<String, ChangelogError> {
    let changelog = build(root)?;
    let head = render_head(&changelog);
    let path = root.join("CHANGELOG.md");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(ChangelogError::Io {
                path: "CHANGELOG.md".to_string(),
                source,
            })
        }
    };

    let has_since = build_since_configured(root)?;
    match existing.as_deref().and_then(frozen_tail) {
        Some(tail) => Ok(format!("{head}{tail}")),
        None if has_since => Err(ChangelogError::MissingMarker),
        None => Ok(head),
    }
}

/// Whether the resolved config sets `[changelog].since` (a cutover tag).
fn build_since_configured(root: &Path) -> Result<bool, ChangelogError> {
    Ok(config::load(root)?
        .changelog
        .map(|c| c.since.is_some())
        .unwrap_or(false))
}

/// Outcome of `--write`: whether the on-disk file changed.
pub struct WriteOutcome {
    pub changed: bool,
}

/// Regenerate `CHANGELOG.md` in place (CHG-001). Returns whether the file
/// changed, so callers can report a no-op distinctly.
pub fn write(root: &Path) -> Result<WriteOutcome, ChangelogError> {
    let new_content = generate(root)?;
    let path = root.join("CHANGELOG.md");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let changed = old != new_content;
    if changed {
        std::fs::write(&path, &new_content).map_err(|source| ChangelogError::Io {
            path: "CHANGELOG.md".to_string(),
            source,
        })?;
    }
    Ok(WriteOutcome { changed })
}

/// Outcome of `--check`: whether the committed file is fresh.
pub struct CheckOutcome {
    pub fresh: bool,
}

/// Compare the regenerated changelog against the file on disk (CHG-001,
/// the `cargo fmt --check` contract). `fresh` is true when they are
/// byte-identical.
pub fn check(root: &Path) -> Result<CheckOutcome, ChangelogError> {
    let new_content = generate(root)?;
    let path = root.join("CHANGELOG.md");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(CheckOutcome {
        fresh: old == new_content,
    })
}

/// Serialize the derived model as pretty JSON (CHG-001 `--format json`).
pub fn render_json(root: &Path) -> Result<String, ChangelogError> {
    let changelog = build(root)?;
    Ok(serde_json::to_string_pretty(&changelog).unwrap_or_else(|_| "{}".to_string()))
}

// ---------------------------------------------------------------------------
// git subprocess
// ---------------------------------------------------------------------------

/// Run `git -C <root> <args…>` and return trimmed stdout. A non-zero exit
/// or spawn failure becomes [`ChangelogError::Git`].
fn git(root: &Path, args: &[&str]) -> Result<String, ChangelogError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| ChangelogError::Git(format!("could not run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ChangelogError::Git(format!(
            "`git {}` failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_release_tags() {
        assert_eq!(parse_version("v0.48.0"), Some(((0, 48, 0), "0.48.0".to_string())));
        assert_eq!(parse_version("v1.2.3"), Some(((1, 2, 3), "1.2.3".to_string())));
    }

    #[test]
    fn parse_version_rejects_non_release_tags() {
        assert_eq!(parse_version("0.48.0"), None); // no `v`
        assert_eq!(parse_version("v0.48"), None); // two components
        assert_eq!(parse_version("v0.48.0.1"), None); // four components
        assert_eq!(parse_version("release"), None);
    }

    #[test]
    fn frozen_tail_returns_from_marker_line() {
        let content = format!("# Changelog\n\n## [Unreleased]\n\n{CUTOVER_MARKER}\n\n## [0.1.0] — 2026-01-01\n");
        let tail = frozen_tail(&content).unwrap();
        assert!(tail.starts_with(CUTOVER_MARKER));
        assert!(tail.contains("## [0.1.0] — 2026-01-01"));
        assert!(!tail.contains("# Changelog"));
    }

    #[test]
    fn frozen_tail_none_without_marker() {
        assert_eq!(frozen_tail("# Changelog\n\n## [Unreleased]\n"), None);
    }

    #[test]
    fn section_order_puts_standard_first_then_custom() {
        let mut sections: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
        sections.insert("Security".to_string(), vec![]);
        sections.insert("Added".to_string(), vec![]);
        sections.insert("Custom".to_string(), vec![]);
        assert_eq!(section_order(&sections), vec!["Added", "Security", "Custom"]);
    }

    #[test]
    fn render_head_emits_unreleased_and_release() {
        let changelog = Changelog {
            versions: vec![
                VersionSection {
                    version: None,
                    date: None,
                    sections: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "Fixed".to_string(),
                            vec![Entry {
                                id: "BUG-017".to_string(),
                                text: "A nested config is silently ignored".to_string(),
                            }],
                        );
                        m
                    },
                },
                VersionSection {
                    version: Some("0.49.0".to_string()),
                    date: Some("2026-07-01".to_string()),
                    sections: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "Fixed".to_string(),
                            vec![Entry {
                                id: "BUG-016".to_string(),
                                text: "EINTR during walk aborts whole lint".to_string(),
                            }],
                        );
                        m
                    },
                },
            ],
        };
        let out = render_head(&changelog);
        assert!(out.contains("## [Unreleased]\n\n### Fixed\n\n- A nested config is silently ignored (BUG-017)\n"));
        assert!(out.contains("## [0.49.0] — 2026-07-01\n\n### Fixed\n\n- EINTR during walk aborts whole lint (BUG-016)\n"));
        // Unreleased precedes the release block.
        let unrel = out.find("[Unreleased]").unwrap();
        let rel = out.find("[0.49.0]").unwrap();
        assert!(unrel < rel);
    }

    #[test]
    fn push_entry_keeps_entries_sorted_by_id() {
        let mut sections: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
        push_entry(&mut sections, "Fixed", &DocumentId::new("BUG", 17), Entry { id: "BUG-017".into(), text: "b".into() });
        push_entry(&mut sections, "Fixed", &DocumentId::new("BUG", 4), Entry { id: "BUG-004".into(), text: "a".into() });
        push_entry(&mut sections, "Fixed", &DocumentId::new("BUG", 9), Entry { id: "BUG-009".into(), text: "c".into() });
        let ids: Vec<&str> = sections["Fixed"].iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["BUG-004", "BUG-009", "BUG-017"]);
    }
}
