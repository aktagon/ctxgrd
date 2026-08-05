//! Dogfood self-lint (ADR-095 § PDOC-002): every config param a builtin
//! rule accepts in a shipped pack manifest — or in the repo's own
//! `ctxgrd.toml` — must be documented in that rule's PDOC-001 `params`
//! metadata (`src/builtin_rules.rs`).
//!
//! A capability with no discoverable record is a build failure, not a
//! shipped surprise — the same class of drift `tests/dogfood_pack_drift.rs`
//! guards for pack/config shape. Scope is config-params only (the
//! mechanically-visible path); frontmatter-attribute association is the
//! documented residual per the ADR.
//!
//! Pure `core.*` rules (`core.required-metadata`, `core.allowed-values`,
//! …) are not builtin-compiled, so `builtin_param_names` returns `None`
//! and they are out of scope here — as the ADR intends.

use std::fs;
use std::path::{Path, PathBuf};

use ctxgrd::builtin_param_names;
use toml::Value;

/// Every TOML manifest the check reads: each shipped pack plus the
/// repo's own dogfood config.
fn manifests() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    let packs_dir = root.join("packs");
    let mut entries: Vec<PathBuf> = fs::read_dir(&packs_dir)
        .expect("packs/ directory is readable")
        .map(|e| e.expect("readable dir entry").path())
        .collect();
    entries.sort();
    for dir in entries {
        let manifest = dir.join("pack.toml");
        if manifest.is_file() {
            paths.push(manifest);
        }
    }
    paths.push(root.join("ctxgrd.toml"));
    paths
}

#[test]
fn every_config_param_used_in_a_pack_is_documented_in_rule_metadata() {
    let mut violations: Vec<String> = Vec::new();

    for manifest in manifests() {
        let text = fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest.display()));
        let doc: Value = text
            .parse()
            .unwrap_or_else(|e| panic!("{} is invalid TOML: {e}", manifest.display()));
        let Value::Table(top) = doc else {
            continue;
        };

        // Each top-level table is a candidate namespace; each of its
        // sub-tables keyed by a builtin rule code is that rule's param
        // block, and every key inside must be a documented config param.
        for (namespace, ns_body) in &top {
            let Value::Table(ns_table) = ns_body else {
                continue;
            };
            for (code, rule_body) in ns_table {
                let Value::Table(param_table) = rule_body else {
                    continue;
                };
                let Some(documented) = builtin_param_names(code) else {
                    // Not a builtin-compiled rule (pure core, external,
                    // or a non-rule table like `pin`) — out of scope.
                    continue;
                };
                for key in param_table.keys() {
                    let ok = documented
                        .iter()
                        .any(|(name, is_config)| *is_config && name == key);
                    if !ok {
                        violations.push(format!(
                            "{} [{namespace}.\"{code}\"] uses config param `{key}`, but it is \
                             not a documented ConfigParam in that rule's `params` metadata \
                             (src/builtin_rules.rs)",
                            manifest.display()
                        ));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "undocumented config params (ADR-095 § PDOC-002):\n{}",
        violations.join("\n")
    );
}
