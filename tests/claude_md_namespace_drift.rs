//! `CLAUDE.md`'s namespace table must match `ctxgrd.toml`'s bound
//! namespaces (`HANDOFF-037` § A8).
//!
//! The table is the always-loaded orientation an agent reads before
//! touching this repo, and it had drifted three ways at once: `RFC` was
//! retired 2026-06-07 but still listed, `BOUNDEDCONTEXT` and `CONTEXTMAP`
//! were listed but never bound, and `ARC42`, `CR`, `DESIGN`, `FEEDBACK`,
//! `SPEC` and `TASK` were bound but unlisted. 20 rows against 23
//! namespaces, with only 17 in common.
//!
//! Drift in either direction costs something real. A listed-but-unbound
//! row sends an agent to author documents nothing lints; a bound-but-
//! unlisted namespace is invisible to the agent that should be maintaining
//! it. Neither is visible to `ctxgrd` itself — the table is prose inside a
//! file the `[CLAUDE]` namespace claims for entirely different rules.
//!
//! Third instance of the genre, after `tests/dogfood_pack_drift.rs` and
//! `tests/adr_citation_status.rs`: a fact stated in two places, pinned by a
//! test that names the divergence rather than trusting discipline.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Top-level `ctxgrd.toml` tables that are configuration blocks rather
/// than namespaces. Namespace names are uppercase by construction
/// (`cfg.namespace-name-invalid` enforces `^[A-Z][A-Z0-9]*$` for id-claim
/// namespaces), so the case test carries most of the weight; this set
/// exists so a future lowercase-adjacent block cannot sneak through.
const CONFIG_TABLES: &[&str] = &[
    "ignore",
    "references",
    "roles",
    "changelog",
    "sources",
    "packs",
];

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Every namespace `ctxgrd.toml` binds.
fn bound_namespaces() -> BTreeSet<String> {
    let text = fs::read_to_string(repo_root().join("ctxgrd.toml")).expect("ctxgrd.toml is readable");
    let value: toml::Value = text.parse().expect("ctxgrd.toml parses");
    value
        .as_table()
        .expect("ctxgrd.toml is a table")
        .iter()
        .filter(|(name, body)| {
            body.is_table()
                && !CONFIG_TABLES.contains(&name.as_str())
                && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Every namespace named in the leading cell of a `CLAUDE.md` table row.
///
/// The rows look like `` | `ADR` | id | … | `` — a backticked uppercase
/// name in the first column. Only the namespace table uses that shape.
fn tabled_namespaces() -> BTreeSet<String> {
    let text = fs::read_to_string(repo_root().join("CLAUDE.md")).expect("CLAUDE.md is readable");
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix('|')?;
            let cell = rest.split('|').next()?.trim();
            let name = cell.strip_prefix('`')?.strip_suffix('`')?;
            let uppercase = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
            uppercase.then(|| name.to_string())
        })
        .collect()
}

#[test]
fn claude_md_namespace_table_matches_the_config() {
    let bound = bound_namespaces();
    let tabled = tabled_namespaces();

    assert!(
        bound.len() >= 20,
        "the extractor found only {} namespaces in ctxgrd.toml — it has probably \
         stopped matching the file's shape rather than the config having shrunk",
        bound.len()
    );

    let unlisted: Vec<&String> = bound.difference(&tabled).collect();
    let unbound: Vec<&String> = tabled.difference(&bound).collect();

    assert!(
        unlisted.is_empty() && unbound.is_empty(),
        "CLAUDE.md's namespace table has drifted from ctxgrd.toml.\n\
         bound but not in the table (an agent cannot see these): {unlisted:?}\n\
         in the table but not bound (an agent will author documents nothing lints): {unbound:?}"
    );
}
