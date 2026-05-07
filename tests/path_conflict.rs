//! End-to-end integration test for ADR-007 § DOC-007 — cross-
//! namespace path-conflict surfacing. The fixture configures two
//! namespaces (`[ADR]` and `[PRD]`) with the same `paths = ["docs/**"]`,
//! places one ambiguous file (no `id`) and one resolvable file
//! (`id: ADR-001`) under `docs/`. The test asserts:
//!
//! - the ambiguous file produces exactly one `cfg.path-conflict`
//!   `KernelMessage` (not a per-document `Diagnostic`);
//! - that message names both conflicting namespaces and the file path;
//! - the ambiguous file is excluded from the document list so per-
//!   document rules don't fire against it;
//! - the resolvable file is classified as ADR via id-claim and lints
//!   cleanly.

use std::fs;
use std::path::Path;

use ctxgrd::run;

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

fn fixture_into_tempdir(name: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    copy_dir_recursive(&src, tmp.path());
    tmp
}

#[test]
fn cross_namespace_path_conflict_emits_kernel_message() {
    let tmp = fixture_into_tempdir("path-conflict");
    let outcome = run::lint(tmp.path()).expect("lint runs");

    // Exactly one cfg.path-conflict KernelMessage, anchored on the
    // ambiguous file. (We assert containment rather than equality
    // because other kernel-channel messages — e.g. cfg.reserved-source
    // — could legitimately appear in future without breaking the
    // DOC-007 contract. But there must be EXACTLY ONE path-conflict.)
    let conflicts: Vec<_> = outcome
        .kernel_messages
        .iter()
        .filter(|m| m.code == "cfg.path-conflict")
        .collect();
    assert_eq!(
        conflicts.len(),
        1,
        "expected exactly one cfg.path-conflict message; got {} in:\n{:#?}",
        conflicts.len(),
        outcome.kernel_messages
    );

    let msg = conflicts[0];
    assert!(
        msg.message.contains("ambiguous.md"),
        "message must name the conflicting file: {:?}",
        msg.message
    );
    assert!(
        msg.message.contains("ADR") && msg.message.contains("PRD"),
        "message must name both conflicting namespaces: {:?}",
        msg.message
    );
    assert!(
        msg.help.is_some(),
        "DOC-007 help text must be populated for resolution guidance"
    );

    // The ambiguous file must NOT have produced any per-document
    // Diagnostic — it is excluded from rule execution under DOC-007.
    let ambiguous_diags: Vec<_> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.location.contains("ambiguous.md"))
        .collect();
    assert!(
        ambiguous_diags.is_empty(),
        "conflicting file must not generate per-document diagnostics; got:\n{:#?}",
        ambiguous_diags
    );
}

#[test]
fn id_claim_resolves_overlapping_paths_without_conflict() {
    let tmp = fixture_into_tempdir("path-conflict");
    let outcome = run::lint(tmp.path()).expect("lint runs");

    // ADR-001.md has `id: ADR-001` — id-claim resolves the overlap to
    // ADR. It must NOT trigger cfg.path-conflict, and it must produce
    // zero per-document diagnostics (the file is well-formed).
    for msg in &outcome.kernel_messages {
        assert!(
            !(msg.code == "cfg.path-conflict" && msg.message.contains("ADR-001.md")),
            "id-claimed file must not appear in path-conflict messages: {:?}",
            msg
        );
    }
    let adr_001_diags: Vec<_> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.location.contains("ADR-001.md"))
        .collect();
    assert!(
        adr_001_diags.is_empty(),
        "id-claimed well-formed file must lint clean; got:\n{:#?}",
        adr_001_diags
    );
}
