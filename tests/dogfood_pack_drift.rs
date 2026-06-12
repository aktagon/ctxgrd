//! Drift guard: the repo's own `ctxgrd.toml` mirrors the built-in doc
//! packs for the namespaces both define (`project-docs`: ADR, PRD;
//! `ops`: RUN, PMR).
//!
//! Packs are applied by copy, not by reference (ADR-013 PACK-001), so
//! nothing at lint time reconciles the dogfood config with the pack.
//! This test is that reconciliation: edit one shape without the other
//! and it fails, naming the diverged table.

use toml::Value;

const REPO_CONFIG: &str = include_str!("../ctxgrd.toml");
const PROJECT_DOCS_PACK: &str = include_str!("../packs/project-docs/pack.toml");
const OPS_PACK: &str = include_str!("../packs/ops/pack.toml");

/// The param tables that define a namespace's document shape.
const SHAPE_TABLES: &[&str] = &[
    "core.required-headings",
    "core.required-metadata",
    "core.allowed-values",
];

fn assert_shapes_match(pack_toml: &str, pack_label: &str, namespaces: &[&str]) {
    let repo: Value = REPO_CONFIG.parse().expect("repo ctxgrd.toml is valid TOML");
    let pack: Value = pack_toml
        .parse()
        .unwrap_or_else(|e| panic!("{pack_label} pack.toml is invalid TOML: {e}"));

    for ns in namespaces {
        for table in SHAPE_TABLES {
            let repo_table = repo
                .get(ns)
                .and_then(|v| v.get(table))
                .unwrap_or_else(|| panic!("ctxgrd.toml [{ns}.\"{table}\"] missing"));
            let pack_table = pack
                .get(ns)
                .and_then(|v| v.get(table))
                .unwrap_or_else(|| panic!("{pack_label} pack.toml [{ns}.\"{table}\"] missing"));
            assert_eq!(
                repo_table, pack_table,
                "[{ns}.\"{table}\"] drifted between ctxgrd.toml and \
                 packs/{pack_label}/pack.toml — update both together"
            );
        }
    }
}

#[test]
fn repo_config_adr_prd_shapes_match_project_docs_pack() {
    assert_shapes_match(PROJECT_DOCS_PACK, "project-docs", &["ADR", "PRD"]);
}

#[test]
fn repo_config_run_pmr_shapes_match_ops_pack() {
    assert_shapes_match(OPS_PACK, "ops", &["RUN", "PMR"]);
}
