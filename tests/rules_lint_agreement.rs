//! BUG-049: `ctxgrd rules` and `ctxgrd lint` must not contradict each
//! other about the same tree.
//!
//! Before the fix, `rules` derived its answer from `ctxgrd.toml` while
//! `lint` derived its rule count from the *documents* — so in a
//! config-less tree `lint` reported `12 rules` and `rules --format json`
//! reported `[]`. An agent branching on `len(rules) == 0` concluded
//! ctxgrd did not govern the tree while ctxgrd was governing it.
//!
//! These tests pin the two commands **to each other**, not to a literal.
//! Asserting `rules` returns six entries in a config-less tree passes
//! just as well if someone hardcodes six; asserting it returns exactly
//! what `lint` dispatched cannot (BUG-049 § Fix, ADR-112 § CLR-007).
//!
//! The two sets are deliberately *not* equal in general. `lint` counts
//! only namespaces that have documents; `rules` also reports a namespace
//! the config declares but nothing populates yet, because "what am I
//! held to" must stay answerable in a project that wrote its config
//! before its first document. `declared_but_unpopulated_namespace_is_
//! still_reported` pins that gap open on purpose — without it, a later
//! "simplification" to strict equality would silently re-break the
//! greenfield case.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use ctxgrd::config;
use ctxgrd::introspect::{self, BINDING_CONFIGURED, BINDING_ZERO_CONFIG};
use ctxgrd::run;

/// Point `HOME` at an empty directory for the whole test binary.
///
/// These tests call `config::load` in-process, and `config::load` resolves
/// `~/.ctxgrd` for global packs and `~/.ctxgrd/namespaces/<NS>.toml` for
/// global namespace config. Without this a developer who has global
/// namespaces gets failures here, and — worse — a developer who does not
/// gets passes that never exercised the fallback path the assertions are
/// about. Every other integration test in this repo pins `HOME` for the
/// same reason; these were the exception.
///
/// `OnceLock` makes the target directory identical for every caller, so
/// the concurrent `set_var` calls all write the same value and the race is
/// benign. The `TempDir` is held in the static so it outlives the tests.
fn isolate_home() {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = HOME.get_or_init(|| tempfile::tempdir().expect("tempdir"));
    std::env::set_var("HOME", dir.path());
}

fn doc(dir: &Path, name: &str, id: &str) {
    fs::create_dir_all(dir).expect("mkdir");
    fs::write(
        dir.join(name),
        format!("---\nid: {id}\ntitle: t\nstatus: accepted\n---\n\n# t\n"),
    )
    .expect("write doc");
}

/// Every `(namespace, rule)` pair `rules` would print for `root`.
fn rules_entries(root: &Path) -> Vec<introspect::RuleEntry> {
    let config = config::load(root).expect("config loads");
    let discovered = config::discover_external_rules(root).expect("rule discovery");
    let governed = run::governed_namespaces(root, &config).expect("namespace scan");
    introspect::list_rules(&config, &discovered, None, &governed)
}

/// The namespaces that actually hold documents — the ones `lint`'s
/// `rules_active` sums over.
fn populated(root: &Path) -> BTreeSet<String> {
    let config = config::load(root).expect("config loads");
    let path_claims = ctxgrd::path_claims::PathClaims::from_config(&config);
    ctxgrd::source::markdown::scan(root, config.ignore.as_ref(), Some(&path_claims))
        .expect("scan")
        .documents
        .into_iter()
        .map(|d| d.id.namespace)
        .collect()
}

#[test]
fn config_less_tree_reports_the_rules_lint_actually_applied() {
    isolate_home();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    doc(&root.join("docs"), "001-a.md", "ADR-001");
    doc(&root.join("docs"), "002-s.md", "SPEC-001");

    let outcome = run::lint(root).expect("lint runs");
    let entries = rules_entries(root);

    // The defect, stated as an assertion: `rules` was empty here.
    assert!(
        !entries.is_empty(),
        "rules must not report an empty set for a tree lint is governing"
    );
    // Two namespaces x the six zero-config rules. Pinned to lint's own
    // count rather than to `12`, so a change to ZERO_CONFIG_RULES moves
    // both sides together or fails.
    assert_eq!(
        entries.len(),
        outcome.rules_active,
        "rules entry count must equal the rule count lint reports applying"
    );

    let namespaces: BTreeSet<&str> = entries.iter().map(|e| e.namespace.as_str()).collect();
    assert_eq!(
        namespaces,
        BTreeSet::from(["ADR", "SPEC"]),
        "both id-claimed namespaces must appear"
    );
    assert!(
        entries.iter().all(|e| e.binding == BINDING_ZERO_CONFIG),
        "no config declares these namespaces, so every row is zero-config"
    );
}

#[test]
fn undeclared_namespace_in_a_configured_tree_is_reported_as_zero_config() {
    isolate_home();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("ctxgrd.toml"),
        "[ADR]\npaths = [\"docs/adrs/**\"]\nrules = [\"core.frontmatter\", \"core.id\"]\n",
    )
    .expect("write config");
    doc(&root.join("docs/adrs"), "001-a.md", "ADR-001");
    doc(&root.join("docs/specs"), "001-s.md", "SPEC-001");

    let entries = rules_entries(root);

    // The half of the bug the report did not claim: `rules` under-reports
    // in a *configured* tree too, whenever a document claims a namespace
    // the config never declared. `lint` governs it with the zero-config
    // six and warns `cfg.namespace-undeclared`; `rules` showed nothing.
    let spec: Vec<_> = entries.iter().filter(|e| e.namespace == "SPEC").collect();
    assert_eq!(
        spec.len(),
        config::ZERO_CONFIG_RULES.len(),
        "an undeclared namespace must report the zero-config fallback set"
    );
    assert!(
        spec.iter().all(|e| e.binding == BINDING_ZERO_CONFIG),
        "SPEC is not declared, so its rows must say so"
    );

    let adr: Vec<_> = entries.iter().filter(|e| e.namespace == "ADR").collect();
    assert_eq!(adr.len(), 2, "ADR reports exactly what the config lists");
    assert!(
        adr.iter().all(|e| e.binding == BINDING_CONFIGURED),
        "ADR is declared, so its rows must say configured"
    );

    // The pairing, restricted to namespaces that have documents — which
    // is the set lint sums over.
    let outcome = run::lint(root).expect("lint runs");
    let populated = populated(root);
    let paired = entries
        .iter()
        .filter(|e| populated.contains(&e.namespace))
        .count();
    assert_eq!(
        paired, outcome.rules_active,
        "for every namespace holding documents, rules and lint must agree"
    );
}

#[test]
fn declared_but_unpopulated_namespace_is_still_reported() {
    isolate_home();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    // A project that wrote its config before its first document — the
    // greenfield case. PRD is declared and empty.
    fs::write(
        root.join("ctxgrd.toml"),
        "[ADR]\npaths = [\"docs/adrs/**\"]\nrules = [\"core.frontmatter\"]\n\
         \n[PRD]\npaths = [\"docs/prds/**\"]\nrules = [\"core.frontmatter\", \"core.id\"]\n",
    )
    .expect("write config");
    doc(&root.join("docs/adrs"), "001-a.md", "ADR-001");

    let entries = rules_entries(root);
    let prd: Vec<_> = entries.iter().filter(|e| e.namespace == "PRD").collect();

    assert_eq!(
        prd.len(),
        2,
        "a declared namespace with no documents yet must still answer \
         'what am I held to' — reporting only populated namespaces would \
         hand a greenfield project an empty rule set"
    );
    assert!(prd.iter().all(|e| e.binding == BINDING_CONFIGURED));

    // And therefore rules is strictly larger than lint's count here.
    // Pinned so the containment direction stays deliberate.
    let outcome = run::lint(root).expect("lint runs");
    assert!(
        entries.len() > outcome.rules_active,
        "rules reports the declared-but-empty namespace that lint does not count"
    );
}

#[test]
fn binding_column_appears_only_when_a_row_is_zero_config() {
    isolate_home();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("ctxgrd.toml"),
        "[ADR]\npaths = [\"docs/adrs/**\"]\nrules = [\"core.frontmatter\"]\n",
    )
    .expect("write config");
    doc(&root.join("docs/adrs"), "001-a.md", "ADR-001");

    // Fully configured: the column would read `configured` on every row,
    // which is width spent on no information.
    let table = introspect::render_table(&rules_entries(root));
    assert!(
        !table.contains("binding"),
        "no zero-config row, so no binding column: {table}"
    );

    // Add an undeclared namespace and the column earns its place.
    doc(&root.join("docs/specs"), "001-s.md", "SPEC-001");
    let table = introspect::render_table(&rules_entries(root));
    assert!(
        table.contains("binding") && table.contains(BINDING_ZERO_CONFIG),
        "an undeclared namespace must be visible in the human view: {table}"
    );
}

#[test]
fn namespace_filter_still_narrows_to_one_namespace() {
    isolate_home();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    doc(&root.join("docs"), "001-a.md", "ADR-001");
    doc(&root.join("docs"), "002-s.md", "SPEC-001");

    let config = config::load(root).expect("config loads");
    let discovered = config::discover_external_rules(root).expect("rule discovery");
    let governed = run::governed_namespaces(root, &config).expect("namespace scan");

    // The filter has to keep working now that it runs against the
    // governed set rather than the config's keys — a zero-config
    // namespace was previously unreachable by name at all.
    let entries = introspect::list_rules(&config, &discovered, Some("SPEC"), &governed);
    assert_eq!(entries.len(), config::ZERO_CONFIG_RULES.len());
    assert!(entries.iter().all(|e| e.namespace == "SPEC"));

    let absent = introspect::list_rules(&config, &discovered, Some("NOPE"), &governed);
    assert!(absent.is_empty(), "a namespace with no claim reports nothing");
}
