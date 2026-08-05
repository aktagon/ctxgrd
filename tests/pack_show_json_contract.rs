//! `pack show --format json` emits the complete namespace contract
//! (ADR-113, CR-008).
//!
//! The load-bearing test here is `every_declared_param_table_appears_in_json`:
//! it derives the expected key set from each `pack.toml` rather than restating
//! it, so a pack that grows a param table cannot silently stop being emitted.
//! A test listing the params it expects would pass forever while the packs
//! moved underneath it — the same transcription-decay failure CR-008 exists to
//! close, reproduced in the test suite.

use std::path::Path;
use std::process::Command;

fn run(args: &[&str]) -> String {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(env!("CARGO_BIN_EXE_ctxgrd"))
        .args(args)
        .current_dir(manifest)
        .output()
        .expect("ctxgrd runs");
    assert!(
        out.status.success(),
        "ctxgrd {args:?} exits 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

fn show_json(pack: &str) -> serde_json::Value {
    serde_json::from_str(&run(&["pack", "show", pack, "--format", "json"]))
        .expect("pack show --format json emits valid JSON")
}

/// Namespace blocks and their declared sub-tables, read straight from the
/// committed `pack.toml`. `[NS]` is a namespace; `[NS."rule.code"]` is one of
/// its param tables.
fn declared_param_tables(pack_toml: &str) -> Vec<(String, Vec<String>)> {
    let table: toml::Table = pack_toml.parse().expect("pack.toml parses");
    let mut out = Vec::new();
    for (ns, value) in &table {
        let Some(ns_tbl) = value.as_table() else {
            continue;
        };
        let mut codes: Vec<String> = ns_tbl
            .iter()
            .filter(|(_, v)| v.is_table())
            .map(|(k, _)| k.clone())
            .collect();
        codes.sort();
        out.push((ns.clone(), codes));
    }
    out.sort();
    out
}

/// Every pack whose `pack.toml` is committed in this repo. Discovered from
/// disk, not listed — a new pack is covered the day it lands.
fn committed_packs() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("packs");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("packs/ exists")
        .filter_map(Result::ok)
        .filter(|e| e.path().join("pack.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(
        names.len() >= 5,
        "expected the committed pack set, found {names:?}"
    );
    names
}

/// PKJ-001: nothing is withheld. For every committed pack, the set of param
/// tables in the JSON equals the set declared in `pack.toml` — both
/// directions, so neither a dropped table nor an invented one passes.
#[test]
fn every_declared_param_table_appears_in_json() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("packs");
    for pack in committed_packs() {
        let toml_text = std::fs::read_to_string(dir.join(&pack).join("pack.toml")).unwrap();
        let declared = declared_param_tables(&toml_text);
        let json = show_json(&pack);

        let mut emitted: Vec<(String, Vec<String>)> = json["namespaces"]
            .as_array()
            .expect("namespaces is an array")
            .iter()
            .map(|ns| {
                let name = ns["namespace"].as_str().unwrap().to_owned();
                let mut codes: Vec<String> = ns["params"]
                    .as_object()
                    .expect("every namespace carries a params object")
                    .keys()
                    .cloned()
                    .collect();
                codes.sort();
                (name, codes)
            })
            .collect();
        emitted.sort();

        assert_eq!(
            emitted, declared,
            "pack '{pack}': `pack show --format json` params must match the param tables \
             declared in pack.toml exactly — a withheld param is one a consumer must \
             transcribe (CR-008)"
        );
    }
}

/// PKJ-001: the vocabularies CR-008 measured as withheld are present with
/// their full contents, not truncated or summarised. Spot-checks the four
/// values the CR tabulated, so the fix is pinned to the report that motivated
/// it rather than only to the generic set-equality above.
#[test]
fn soc2_json_carries_the_vocabularies_cr_008_measured_as_withheld() {
    let json = show_json("soc2");
    let ns = &json["namespaces"][0];
    assert_eq!(ns["namespace"], "SOC2");
    let params = &ns["params"];

    let criteria = params["core.allowed-values"]["criterion"]
        .as_array()
        .expect("the Trust Services criteria vocabulary");
    assert_eq!(
        criteria.len(),
        61,
        "the full closed catalog, not a sample — 33 Common Criteria plus the 28 optional"
    );
    assert_eq!(criteria.first().unwrap(), "CC1.1");
    assert_eq!(criteria.last().unwrap(), "P8.1");

    assert_eq!(
        params["core.allowed-values"]["category"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    // CR-008 measured this as `review_date`; ADR-114 § FLD-001 renamed it to
    // `reviewed_date` so the whole compliance family spells it one way. Asserted
    // at the current value, not the quoted historical one — this is a live
    // contract check, not a transcript of the report.
    assert_eq!(params["core.calendar-freshness"]["field"], "reviewed_date");
    assert_eq!(params["core.calendar-freshness"]["stale_days"], 365);
    assert_eq!(
        params["soc2.control-evidence"]["evidence-fields"][0],
        "evidence_link"
    );
    assert_eq!(
        params["soc2.control-evidence"]["out-of-scope-status"][0],
        "not-applicable"
    );
}

/// PKJ-002: the serialisation is canonical, so a consumer can pin a digest it
/// computes itself. Two invocations must be byte-identical, and object keys
/// must be sorted — a digest over an unstable serialisation fails on
/// reserialisation and gets ignored within a week (CR-008).
#[test]
fn json_output_is_byte_stable_across_invocations() {
    for pack in ["soc2", "gdpr", "security", "project-docs"] {
        let a = run(&["pack", "show", pack, "--format", "json"]);
        let b = run(&["pack", "show", pack, "--format", "json"]);
        assert_eq!(a, b, "pack '{pack}': output must be byte-stable");

        // Round-tripping through a sorted-map parser must not move a byte —
        // that is what "keys are sorted" means operationally.
        let parsed: serde_json::Value = serde_json::from_str(&a).unwrap();
        let reserialised = serde_json::to_string_pretty(&parsed).unwrap();
        assert_eq!(
            a.trim_end(),
            reserialised.trim_end(),
            "pack '{pack}': emitted form must already be the canonical sorted form"
        );
    }
}

/// PKJ-003: the output says which artifact it describes. Without this a
/// consumer cannot tell the pack definition from a project's materialised
/// `ctxgrd.toml`, and comparing against the wrong one is a false green.
#[test]
fn json_declares_its_scope_and_dependencies() {
    let soc2 = show_json("soc2");
    assert_eq!(soc2["scope"], "pack-definition");
    assert_eq!(
        soc2["depends"].as_array().unwrap(),
        &[serde_json::Value::String("security".into())],
        "soc2 layers over the security base (ADR-068 § PKD-001)"
    );

    // A base pack declares no dependency — an empty array, never a missing key.
    let security = show_json("security");
    assert_eq!(security["scope"], "pack-definition");
    assert_eq!(security["depends"].as_array().unwrap().len(), 0);
}

/// The pre-ADR-113 shape keeps working: `pack show --format json` already
/// shipped `name`/`path`/`summary`/`namespaces`/`external_rules`/`rules` and
/// per-namespace `required_metadata` (ADR-096 § CMD-002). ADR-113 is additive,
/// which is what makes it MINOR — this test is what that claim rests on.
#[test]
fn adr_096_json_shape_is_preserved() {
    let json = show_json("soc2");
    for key in [
        "name",
        "path",
        "summary",
        "namespaces",
        "external_rules",
        "rules",
    ] {
        assert!(
            json.get(key).is_some(),
            "ADR-096 § CMD-002 key '{key}' must still be emitted"
        );
    }
    let ns = &json["namespaces"][0];
    for key in ["namespace", "rules", "paths", "required_metadata"] {
        assert!(
            ns.get(key).is_some(),
            "ADR-096 § CMD-002 namespace key '{key}' must still be emitted"
        );
    }
    // The hoisted field and the params table agree — the duplication is
    // deliberate, so assert it is consistent rather than merely present.
    assert_eq!(
        ns["required_metadata"],
        ns["params"]["core.required-metadata"]["keys"]
    );
}
