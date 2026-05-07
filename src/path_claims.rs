//! Per-namespace path-classification index (ADR-007 § DOC-001 plumbing).
//!
//! Aggregates `[<NS>].paths` GlobSets across all configured
//! namespaces so the markdown walker can ask, "which namespaces
//! claim this path?" in one call. Built by [`run::ingest`] after
//! config-load; consumed by [`source::markdown::parse_one`].
//!
//! ADR-007 § DOC-001 (intent-based classification) and § DOC-007
//! (cross-namespace conflict resolution) both build on this surface.
//! This module currently provides the *plumbing* only — DOC-001's
//! behavior flip in `parse_one` is a separate change. The early-
//! return on `(no id, no path match) → ParseOutcome::Skip` will
//! consume [`PathClaims::matching_namespaces`] then; today
//! `parse_one` only proves the threading.
//!
//! [`run::ingest`]: crate::run::ingest
//! [`source::markdown::parse_one`]: crate::source::markdown::parse_one

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::Config;
use crate::diagnostic::KernelMessage;

/// Namespace name → compiled glob index built from `[<NS>].paths`.
///
/// Namespaces without a configured `paths` are absent from the map
/// (they own no path-claims). Iteration order is namespace-name
/// sorted (`BTreeMap` guarantee), which makes downstream diagnostics
/// — notably `cfg.path-conflict` (DOC-007) — deterministic.
#[derive(Debug, Default, Clone)]
pub struct PathClaims {
    by_namespace: BTreeMap<String, globset::GlobSet>,
}

impl PathClaims {
    /// Build an empty index.
    ///
    /// Used by tests that exercise functions taking `&PathClaims`
    /// without needing to construct a full [`Config`]. Keeping this
    /// helper close to the type spares every test callsite from
    /// having to spell out the field by hand whenever the type
    /// gains a new internal slot.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Aggregate every `[<NS>].paths` GlobSet from `config` into a
    /// single index. Namespaces without `paths` are not entered.
    pub fn from_config(config: &Config) -> Self {
        let by_namespace = config
            .namespaces
            .iter()
            .filter_map(|(name, ns)| ns.paths.as_ref().map(|g| (name.clone(), g.clone())))
            .collect();
        Self { by_namespace }
    }

    /// `true` iff no namespace has a configured `paths` glob.
    pub fn is_empty(&self) -> bool {
        self.by_namespace.is_empty()
    }

    /// Number of namespaces with at least one configured path glob.
    pub fn len(&self) -> usize {
        self.by_namespace.len()
    }

    /// Iterate namespace names whose `paths` GlobSet matches `path`.
    ///
    /// Order is namespace-name-sorted. Multiple matches are possible
    /// — DOC-007 will collect this iterator and treat cardinality
    /// `> 1` as a `cfg.path-conflict` candidate (resolved by id-claim
    /// when present).
    pub fn matching_namespaces<'a, P: AsRef<Path> + 'a>(
        &'a self,
        path: P,
    ) -> impl Iterator<Item = &'a str> + 'a {
        let path = path.as_ref().to_path_buf();
        self.by_namespace
            .iter()
            .filter(move |(_, globs)| globs.is_match(&path))
            .map(|(name, _)| name.as_str())
    }
}

/// A file claimed by two or more namespaces' `[<NS>].paths` globs
/// without an `id` resolving the ambiguity (ADR-007 § DOC-007).
///
/// Surfaced as a `cfg.path-conflict` [`KernelMessage`] at ingest
/// time, before rule execution. Conflicting files are excluded from
/// the document list — no per-document diagnostics fire against
/// them, since classification under ambiguity would be undefined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathConflict {
    /// Lint-root-relative location of the conflicting file.
    pub location: String,
    /// Names of the namespaces whose `paths` claim the file. Sorted
    /// (BTreeMap iteration order from `PathClaims::matching_namespaces`)
    /// so message text is deterministic across runs.
    pub namespaces: Vec<String>,
}

impl PathConflict {
    /// Render the conflict as a `cfg.path-conflict` `KernelMessage`.
    /// Channel choice locked in ADR-007 OQ-8: configuration error,
    /// no per-document anchor, sits alongside `cfg.reserved-source`.
    pub fn to_kernel_message(&self) -> KernelMessage {
        let names = self.namespaces.join(", ");
        KernelMessage::error(
            "cfg.path-conflict",
            format!(
                "{:?} is claimed by multiple namespaces' [<NS>].paths globs: {names}",
                self.location
            ),
        )
        .with_help(
            "add an `id: <NS>-<number>` field whose namespace matches one of the conflicting \
             namespaces, or narrow one of the `[<NS>].paths` globs so it no longer covers \
             this file.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamespaceConfig;
    use globset::{Glob, GlobSetBuilder};

    fn glob_set(patterns: &[&str]) -> globset::GlobSet {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(Glob::new(p).expect("test glob compiles"));
        }
        b.build().expect("globset builds")
    }

    fn ns_with_paths(patterns: &[&str]) -> NamespaceConfig {
        NamespaceConfig {
            rules: Vec::new(),
            params: BTreeMap::new(),
            paths: Some(glob_set(patterns)),
            path_patterns: patterns.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn ns_without_paths() -> NamespaceConfig {
        NamespaceConfig {
            rules: Vec::new(),
            params: BTreeMap::new(),
            paths: None,
            path_patterns: Vec::new(),
        }
    }

    fn config_with(namespaces: Vec<(&str, NamespaceConfig)>) -> Config {
        let mut cfg = Config::default();
        for (name, ns) in namespaces {
            cfg.namespaces.insert(name.to_owned(), ns);
        }
        cfg
    }

    #[test]
    fn empty_helper_has_zero_namespaces() {
        let claims = PathClaims::empty();
        assert!(claims.is_empty());
        assert_eq!(claims.len(), 0);
        assert_eq!(
            claims
                .matching_namespaces("docs/adrs/ADR-001.md")
                .collect::<Vec<_>>(),
            Vec::<&str>::new(),
        );
    }

    #[test]
    fn from_config_indexes_only_namespaces_with_paths() {
        let cfg = config_with(vec![
            ("ADR", ns_with_paths(&["docs/adrs/**"])),
            ("PRD", ns_without_paths()),
            ("RFC", ns_with_paths(&["docs/rfcs/**"])),
        ]);
        let claims = PathClaims::from_config(&cfg);

        assert_eq!(claims.len(), 2);
        assert!(!claims.is_empty());
    }

    #[test]
    fn matching_namespaces_returns_only_globs_that_hit() {
        let cfg = config_with(vec![
            ("ADR", ns_with_paths(&["docs/adrs/**"])),
            ("PRD", ns_with_paths(&["docs/prds/**"])),
        ]);
        let claims = PathClaims::from_config(&cfg);

        assert_eq!(
            claims
                .matching_namespaces("docs/adrs/ADR-001.md")
                .collect::<Vec<_>>(),
            vec!["ADR"],
        );
        assert_eq!(
            claims
                .matching_namespaces("docs/prds/PRD-001.md")
                .collect::<Vec<_>>(),
            vec!["PRD"],
        );
        assert_eq!(
            claims.matching_namespaces("README.md").collect::<Vec<_>>(),
            Vec::<&str>::new(),
        );
    }

    #[test]
    fn overlapping_globs_yield_multiple_namespaces() {
        // DOC-007 precondition: a path matched by two globs returns
        // both namespaces, sorted by name. The conflict resolution
        // (id-claim wins, else `cfg.path-conflict`) is DOC-007's job;
        // this layer only reports membership.
        let cfg = config_with(vec![
            ("ADR", ns_with_paths(&["docs/**"])),
            ("PRD", ns_with_paths(&["docs/**"])),
        ]);
        let claims = PathClaims::from_config(&cfg);

        let hits: Vec<&str> = claims.matching_namespaces("docs/something.md").collect();
        assert_eq!(hits, vec!["ADR", "PRD"]);
    }

    #[test]
    fn iteration_order_is_namespace_sorted() {
        // Insert in non-alphabetical order; expect alphabetical out.
        let cfg = config_with(vec![
            ("ZED", ns_with_paths(&["**/*.md"])),
            ("ADR", ns_with_paths(&["**/*.md"])),
            ("MED", ns_with_paths(&["**/*.md"])),
        ]);
        let claims = PathClaims::from_config(&cfg);

        let hits: Vec<&str> = claims.matching_namespaces("any.md").collect();
        assert_eq!(hits, vec!["ADR", "MED", "ZED"]);
    }
}
