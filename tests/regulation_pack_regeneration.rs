//! Every extract-backed pack regenerates from its committed `regulation.json`
//! byte-for-byte (ADR-066 § CMP-005).
//!
//! **Derived from disk, not listed.** `tests/pack.rs` carries one hand-written
//! `<name>_pack_regenerates_byte_for_byte` per pack; that pattern covered five
//! packs and silently covered none of the three added by ADR-115/116/117 until
//! someone remembered to copy it. This walks `packs/*/regulation.json` instead,
//! so a pack is covered the day its extract lands.
//!
//! What it protects: these `pack.toml` files are **compiled output**. A
//! hand-edit survives until the next regeneration reverts it. That is not
//! hypothetical — it happened while implementing ADR-112, where the fix was
//! first written into `pack.toml` and this gate caught it in one run.

use std::path::{Path, PathBuf};
use std::{fs, process::Command};

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Every pack directory carrying a `regulation.json` — the extract-backed
/// packs, as opposed to hand-authored ones like `security`.
fn extract_backed_packs() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(manifest().join("packs"))
        .expect("packs/ exists")
        .filter_map(Result::ok)
        .filter(|e| e.path().join("regulation.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn every_extract_backed_pack_regenerates_byte_for_byte() {
    let packs = extract_backed_packs();
    assert!(
        packs.len() >= 8,
        "expected at least the eight extract-backed packs (gdpr, hipaa, soc2, iso-27001, \
         nist-800-53, nis2, eu-ai-act, ccpa), found {packs:?}"
    );

    for pack in &packs {
        let committed = fs::read_to_string(manifest().join("packs").join(pack).join("pack.toml"))
            .unwrap_or_else(|e| panic!("committed {pack}/pack.toml exists: {e}"));

        // Stage an isolated tree so the generator cannot overwrite the
        // committed file, and pass it as the generator's root override.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("packs").join(pack);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(
            manifest().join("packs").join(pack).join("regulation.json"),
            dir.join("regulation.json"),
        )
        .unwrap();

        let status = Command::new(env!("CARGO"))
            .args([
                "run",
                "--quiet",
                "--example",
                "gen_compliance_pack",
                "--",
                pack,
                tmp.path().to_str().unwrap(),
            ])
            .current_dir(manifest())
            .status()
            .expect("generator runs");
        assert!(status.success(), "generator exits 0 for {pack}");

        let regenerated = fs::read_to_string(dir.join("pack.toml"))
            .unwrap_or_else(|e| panic!("generator wrote {pack}/pack.toml: {e}"));
        assert_eq!(
            regenerated, committed,
            "pack '{pack}': regenerating from the unchanged extract must reproduce pack.toml \
             byte-for-byte — a hand-edit here is reverted by the next regeneration"
        );
    }
}

/// The extract is the provenance record, so it must actually cite its source.
/// Every regulation figure in these packs is read off a published authority;
/// an extract with no `_note` and no `edition` is one whose numbers cannot be
/// traced, which is the failure mode "name the authority instead of recalling
/// the number" exists to prevent.
#[test]
fn every_extract_records_its_provenance() {
    for pack in extract_backed_packs() {
        let raw =
            fs::read_to_string(manifest().join("packs").join(&pack).join("regulation.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{pack}/regulation.json parses: {e}"));

        let note = json.get("_note").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            note.len() > 80,
            "pack '{pack}': `_note` must record where the catalog was read from — \
             found {} chars",
            note.len()
        );

        // Every closed vocabulary carries a citation. This is the field that
        // makes a value auditable rather than asserted.
        let vocabs = json
            .get("vocabularies")
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("pack '{pack}': extract declares `vocabularies`"));
        for (name, vocab) in vocabs {
            let cite = vocab.get("cite").and_then(|v| v.as_str()).unwrap_or("");
            assert!(
                cite.len() > 10,
                "pack '{pack}': vocabulary `{name}` must carry a `cite` naming the authority \
                 its values come from"
            );
        }
    }
}

/// PKJ-001 interaction: a namespace binding the regime-neutral
/// `core.evidence-link` must declare the `field` param. The rule reports a
/// misconfiguration rather than going silently inert (ADR-115 § REG-001), but a
/// shipped pack should never be the thing that triggers it.
#[test]
fn packs_binding_core_evidence_link_declare_its_field() {
    let mut checked = 0;
    for pack in extract_backed_packs() {
        let toml_text =
            fs::read_to_string(manifest().join("packs").join(&pack).join("pack.toml")).unwrap();
        let table: toml::Table = toml_text.parse().expect("pack.toml parses");
        for (ns, value) in &table {
            let Some(ns_tbl) = value.as_table() else {
                continue;
            };
            if !ns_tbl
                .get("rules")
                .and_then(|r| r.as_array())
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("core.evidence-link")))
            {
                continue;
            }
            let params = ns_tbl
                .get("core.evidence-link")
                .and_then(|v| v.as_table())
                .unwrap_or_else(|| {
                    panic!("pack '{pack}' namespace [{ns}] binds core.evidence-link but declares no params block")
                });
            assert!(
                params.get("field").and_then(|v| v.as_str()).is_some(),
                "pack '{pack}' namespace [{ns}]: core.evidence-link needs `field` naming the \
                 obligation identifier"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 2,
        "expected the nis2 and eu-ai-act registers to bind core.evidence-link, checked {checked}"
    );
}

/// The three packs added by ADR-115/116/117 are discoverable and layer over
/// `security`, so `pack add <name>` installs the base first (ADR-068 § PKD-002).
#[test]
fn new_regulation_packs_are_listed_and_depend_on_security() {
    let out = Command::new(env!("CARGO_BIN_EXE_ctxgrd"))
        .args(["pack", "list"])
        .current_dir(manifest())
        .output()
        .expect("ctxgrd runs");
    let stdout = String::from_utf8(out.stdout).unwrap();
    for pack in ["nis2", "eu-ai-act", "ccpa"] {
        assert!(stdout.contains(pack), "pack list includes {pack}:\n{stdout}");

        let show = Command::new(env!("CARGO_BIN_EXE_ctxgrd"))
            .args(["pack", "show", pack, "--format", "json"])
            .current_dir(manifest())
            .output()
            .expect("ctxgrd runs");
        let json: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
        assert_eq!(
            json["depends"].as_array().unwrap(),
            &[serde_json::Value::String("security".into())],
            "{pack} layers over the security base"
        );
        assert_eq!(
            json["namespaces"].as_array().unwrap().len(),
            2,
            "{pack} defines two namespaces — these are laws, not single-register frameworks"
        );
    }
}

/// A guard against the pack-vs-binary drift the `include_str!` registration
/// makes possible: a `pack.toml` on disk that was never wired into
/// `builtin_packs()` is invisible to every consumer while looking committed.
#[test]
fn every_committed_pack_is_registered_as_builtin() {
    let out = Command::new(env!("CARGO_BIN_EXE_ctxgrd"))
        .args(["pack", "list"])
        .current_dir(manifest())
        .output()
        .expect("ctxgrd runs");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let mut missing: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(manifest().join("packs")).unwrap().flatten() {
        if !entry.path().join("pack.toml").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !stdout.lines().any(|l| l.split_whitespace().next() == Some(name.as_str())) {
            missing.push(entry.path());
        }
    }
    assert!(
        missing.is_empty(),
        "these packs are committed but not registered in builtin_packs(), so no consumer can \
         reach them: {missing:?}"
    );
}
