//! Namespace coverage gates — ADR-076 § OWN-003/OWN-004/OWN-005.
//!
//! ctxgrd's other checks all answer "is what the config declares
//! satisfied?". These two answer the mirror question: *what is missing
//! from the config?*
//!
//! - `cfg.namespace-undeclared` (OWN-004) — documents claim a namespace by
//!   id that the config never declares. Such a namespace silently resolves
//!   to [`crate::config::ZERO_CONFIG_RULES`] (the non-parameterized core
//!   rules), so every shape rule — `core.required-headings`,
//!   `core.required-metadata`, `core.allowed-values`, `core.min-docs` — is
//!   absent while the run still reports `ok`. A convention a team invents is
//!   therefore unenforced *and* reassuringly green.
//! - `cfg.namespace-unowned` (OWN-003) — a declared, document-bearing
//!   namespace names no accountable `owner` role, so "which role writes this
//!   doc type?" stays tribal knowledge rather than a checked invariant.
//!
//! Both are `cfg.*`, not `core.*`, and both are always-on. That is
//! mechanical, not stylistic: `core.*` rules are opted into by listing them
//! in `[<NS>].rules`, and the namespace that forgets `owner` is exactly the
//! one that would forget to list the rule — while for OWN-004 the `[<NS>]`
//! block that would carry the binding is precisely what does not exist.
//! Neither code appears in the rule registry (`ctxgrd rules`).
//!
//! Both are warning severity: these runs exit 0 today, and promoting the
//! finding to an error would fail previously-clean documents — a MAJOR bump
//! under the Versioning policy for no signal a warning does not carry.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::Config;
use crate::diagnostic::Diagnostic;
use crate::document::Document;

/// Result of the coverage pass: the gate diagnostics plus the count
/// OWN-005 puts on the summary line.
pub(crate) struct Coverage {
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// How many namespaces documents claim that the config does not
    /// declare — including any exempted via `[ignore].namespaces`. The
    /// exemption silences the *warning*, not the fact: those documents
    /// really are linting under the zero-config set, and a summary that hid it
    /// would recreate the false confidence OWN-005 exists to correct.
    pub(crate) namespaces_undeclared: usize,
}

impl Coverage {
    /// The "gate did not run" value — zero-config mode and scoped runs.
    pub(crate) fn none() -> Self {
        Self {
            diagnostics: Vec::new(),
            namespaces_undeclared: 0,
        }
    }
}

/// One undeclared namespace and where to anchor its diagnostic.
struct Claim<'a> {
    documents: usize,
    /// Location of the lowest-numbered claimant — the anchor. Lowest
    /// rather than first-walked so the anchor is stable when files move.
    location: &'a str,
    line: u32,
    lowest: u32,
}

/// Run both coverage gates (ADR-076 § OWN-003/OWN-004).
///
/// `file_level` is the set of namespaces whose path-claimed singletons
/// (CLAUDE.md, TODO.md) were linted; they never become id-keyed documents,
/// so a namespace holding only those would otherwise look empty to the
/// ownership gate.
///
/// Returns an empty [`Coverage`] in zero-config mode. That is the whole
/// discriminator OWN-004 turns on: with no declared namespace, *every*
/// namespace is undeclared by definition and the gate would fire on every
/// document — the `unwrap_or_else(NamespaceConfig::zero_config)` fallback
/// is correct there and only there.
///
/// The caller must not run this under an ADR-080 scope: coverage is a
/// property of the whole config, and a scoped run reports one slice
/// (`config.namespaces` has been narrowed to it, which would make every
/// namespace outside the slice look undeclared).
pub(crate) fn check(
    config: &Config,
    documents: &[Document],
    file_level: &BTreeSet<String>,
    root: &Path,
) -> Coverage {
    if config.namespaces.is_empty() {
        return Coverage::none();
    }

    let mut diagnostics = Vec::new();

    // --- OWN-004: namespaces claimed by documents but never declared. ---
    let mut undeclared: BTreeMap<&str, Claim<'_>> = BTreeMap::new();
    for doc in documents {
        let ns = doc.id.namespace.as_str();
        if config.namespaces.contains_key(ns) {
            continue;
        }
        let line = doc.frontmatter_lines.get("id").copied().unwrap_or(0);
        let entry = undeclared.entry(ns).or_insert(Claim {
            documents: 0,
            location: doc.location.as_str(),
            line,
            lowest: doc.id.number,
        });
        entry.documents += 1;
        if doc.id.number < entry.lowest {
            entry.lowest = doc.id.number;
            entry.location = doc.location.as_str();
            entry.line = line;
        }
    }
    let namespaces_undeclared = undeclared.len();

    for (ns, claim) in &undeclared {
        if config.ignore_namespaces.iter().any(|n| n == ns) {
            continue;
        }
        diagnostics.push(undeclared_diagnostic(ns, claim, root));
    }

    // --- OWN-003: declared, document-bearing namespaces with no owner. ---
    //
    // "Document-bearing" keeps the gate off namespaces a config declares
    // ahead of use: a pack you have added but not yet written to is not a
    // coverage gap, it is an empty shelf.
    let mut bearing: BTreeSet<&str> = documents
        .iter()
        .map(|d| d.id.namespace.as_str())
        .collect();
    bearing.extend(file_level.iter().map(String::as_str));

    for (ns, ns_cfg) in &config.namespaces {
        if !bearing.contains(ns.as_str()) {
            continue;
        }
        match (&ns_cfg.owner, &config.roles_allowed) {
            (None, _) => diagnostics.push(unowned_diagnostic(ns)),
            (Some(owner), Some(allowed)) if !allowed.iter().any(|r| r == owner) => {
                diagnostics.push(owner_unknown_diagnostic(ns, owner, allowed));
            }
            _ => {}
        }
    }

    Coverage {
        diagnostics,
        namespaces_undeclared,
    }
}

/// `cfg.namespace-undeclared`, anchored at the lowest-numbered claimant's
/// `id:` frontmatter line.
///
/// The help follows the ADR-025 pack-discoverability precedent: when a
/// discoverable pack ships the namespace, name the `pack add` that installs
/// it rather than making the reader hunt for it.
fn undeclared_diagnostic(namespace: &str, claim: &Claim<'_>, root: &Path) -> Diagnostic {
    let providers = crate::pack::providers_of_namespace(root, namespace);
    let help = match providers.as_slice() {
        [] => format!(
            "add a [{namespace}] block to ctxgrd.toml, or add '{namespace}' to \
             [ignore].namespaces if the id belongs to another repo"
        ),
        [one] => format!(
            "run `ctxgrd pack add {one}` — it ships [{namespace}] — or add a \
             [{namespace}] block to ctxgrd.toml by hand"
        ),
        many => format!(
            "run `ctxgrd pack add <pack>` — {} ship [{namespace}] — or add a \
             [{namespace}] block to ctxgrd.toml by hand",
            many.join(" and ")
        ),
    };
    Diagnostic::warning(
        "cfg.namespace-undeclared",
        claim.location,
        claim.line,
        0,
        format!(
            "{} {} namespace '{namespace}', which ctxgrd.toml does not declare",
            crate::reporter::plural(claim.documents, "document"),
            if claim.documents == 1 { "claims" } else { "claim" },
        ),
    )
    .with_help(help)
    .with_note(
        "an undeclared namespace lints with the 6 zero-config core rules only — no \
         required-headings, required-metadata, allowed-values, or min-docs",
    )
}

/// `cfg.namespace-unowned`, anchored at `ctxgrd.toml` — the file that would
/// carry the fix — like every other whole-config `cfg.*` finding.
fn unowned_diagnostic(namespace: &str) -> Diagnostic {
    Diagnostic::warning(
        "cfg.namespace-unowned",
        "ctxgrd.toml",
        0,
        0,
        format!("[{namespace}] declares no owning role"),
    )
    .with_help(format!(
        "add `owner = \"<role>\"` to [{namespace}] — a role such as `developer` or \
         `writer`, not a leaf skill name"
    ))
    .with_note(
        "a document type no role will claim is a coverage gap: the pack enforces a \
         contract no agent is guided toward",
    )
}

fn owner_unknown_diagnostic(namespace: &str, owner: &str, allowed: &[String]) -> Diagnostic {
    Diagnostic::warning(
        "cfg.namespace-unowned",
        "ctxgrd.toml",
        0,
        0,
        format!("[{namespace}] owner '{owner}' is not in [roles].allowed"),
    )
    .with_help(format!(
        "use one of {}, or add '{owner}' to [roles].allowed",
        allowed
            .iter()
            .map(|r| format!("'{r}'"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .with_note("`owner` names a role, not a leaf skill — leaf skills are renamed and split")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamespaceConfig;
    use crate::id::DocumentId;

    fn doc(raw_id: &str, location: &str) -> Document {
        let id: DocumentId = raw_id.parse().expect("test id parses");
        Document {
            id,
            raw_id: raw_id.to_string(),
            location: location.to_string(),
            file: None,
            depends_on: Vec::new(),
            frontmatter_lines: [("id".to_string(), 2u32)].into_iter().collect(),
            metadata: Default::default(),
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    fn config_with(namespaces: &[(&str, Option<&str>)]) -> Config {
        let mut config = Config::default();
        for (name, owner) in namespaces {
            config.namespaces.insert(
                (*name).to_string(),
                NamespaceConfig {
                    owner: owner.map(str::to_string),
                    ..NamespaceConfig::zero_config()
                },
            );
        }
        config
    }

    fn run(config: &Config, documents: &[Document]) -> Coverage {
        check(config, documents, &BTreeSet::new(), Path::new("."))
    }

    #[test]
    fn zero_config_is_silent() {
        let coverage = run(&Config::default(), &[doc("REPORT-1", "docs/reports/001.md")]);
        assert_eq!(coverage.diagnostics.len(), 0);
        assert_eq!(coverage.namespaces_undeclared, 0);
    }

    #[test]
    fn undeclared_namespace_warns_once_per_namespace() {
        let config = config_with(&[("ADR", Some("developer"))]);
        let coverage = run(
            &config,
            &[
                doc("ADR-1", "docs/adrs/001-real.md"),
                doc("REPORT-2", "docs/reports/002-second.md"),
                doc("REPORT-1", "docs/reports/001-first.md"),
            ],
        );
        assert_eq!(coverage.namespaces_undeclared, 1);
        let codes: Vec<&str> = coverage
            .diagnostics
            .iter()
            .map(|d| d.code.as_str())
            .collect();
        assert_eq!(codes, vec!["cfg.namespace-undeclared"]);
        let d = &coverage.diagnostics[0];
        assert_eq!(
            d.message,
            "2 documents claim namespace 'REPORT', which ctxgrd.toml does not declare"
        );
        assert!(d
            .help
            .as_deref()
            .expect("undeclared carries a fix")
            .contains("[REPORT]"));
        // Anchored at the lowest-numbered claimant, not the first walked.
        assert_eq!(d.location, "docs/reports/001-first.md");
        assert_eq!(d.line, Some(2));
        assert_eq!(d.severity, crate::diagnostic::Severity::Warning);
    }

    #[test]
    fn ignored_namespace_silences_the_warning_but_still_counts() {
        let mut config = config_with(&[("ADR", Some("developer"))]);
        config.ignore_namespaces = vec!["REPORT".to_string()];
        let coverage = run(
            &config,
            &[
                doc("ADR-1", "docs/adrs/001-real.md"),
                doc("REPORT-1", "docs/reports/001-first.md"),
            ],
        );
        assert_eq!(coverage.diagnostics.len(), 0);
        assert_eq!(coverage.namespaces_undeclared, 1);
    }

    #[test]
    fn declared_namespace_without_owner_warns() {
        let config = config_with(&[("ADR", None)]);
        let coverage = run(&config, &[doc("ADR-1", "docs/adrs/001-real.md")]);
        let d = &coverage.diagnostics[0];
        assert_eq!(d.code, "cfg.namespace-unowned");
        assert_eq!(d.message, "[ADR] declares no owning role");
        assert_eq!(d.location, "ctxgrd.toml");
    }

    #[test]
    fn declared_namespace_holding_no_document_is_not_gated() {
        let config = config_with(&[("ADR", Some("developer")), ("PRD", None)]);
        let coverage = run(&config, &[doc("ADR-1", "docs/adrs/001-real.md")]);
        assert_eq!(coverage.diagnostics.len(), 0);
    }

    #[test]
    fn file_level_namespace_counts_as_document_bearing() {
        let config = config_with(&[("CLAUDE", None)]);
        let coverage = check(
            &config,
            &[],
            &["CLAUDE".to_string()].into_iter().collect(),
            Path::new("."),
        );
        assert_eq!(coverage.diagnostics.len(), 1);
        assert_eq!(coverage.diagnostics[0].code, "cfg.namespace-unowned");
    }

    #[test]
    fn owner_outside_declared_vocabulary_warns() {
        let mut config = config_with(&[("ADR", Some("docs-requirements"))]);
        config.roles_allowed = Some(vec!["developer".to_string(), "writer".to_string()]);
        let coverage = run(&config, &[doc("ADR-1", "docs/adrs/001-real.md")]);
        let d = &coverage.diagnostics[0];
        assert_eq!(d.code, "cfg.namespace-unowned");
        assert_eq!(
            d.message,
            "[ADR] owner 'docs-requirements' is not in [roles].allowed"
        );
    }

    #[test]
    fn owner_without_a_roles_table_is_declare_only() {
        let config = config_with(&[("ADR", Some("anything-at-all"))]);
        let coverage = run(&config, &[doc("ADR-1", "docs/adrs/001-real.md")]);
        assert_eq!(coverage.diagnostics.len(), 0);
    }
}
