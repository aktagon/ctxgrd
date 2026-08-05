//! Integration tests for the ADR-112 resolution threading — the seam
//! between `run.rs` (which computes each document's *resolving* references)
//! and the conditional-link rules in `agent_guide.rs` (which read them
//! through the synthesized `resolved-refs` param).
//!
//! **Why this file exists.** Every unit test for those rules builds its
//! candidate set through the `with_refs` helper, which calls the production
//! matcher directly. That covers the matcher thoroughly and covers the
//! dispatch not at all: replacing the per-document threading in `run.rs`
//! with the whole corpus id set — the permissive behaviour BUG-030/BUG-031
//! describe — leaves the entire unit suite green. Verified by doing exactly
//! that: 30 test binaries, 0 failures, while a mitigated finding citing
//! nothing linted clean.
//!
//! Each fixture below is built so the two behaviours *disagree*. The corpus
//! always contains a document that would satisfy the rule if candidacy were
//! corpus-wide, and the document under test never cites it. A test that
//! passes under both threadings would be worthless here, so the
//! `..._is_not_satisfied_by_an_unrelated_document` cases are the load-bearing
//! ones and the green companions exist to prove the rules are not simply
//! always-erroring.

use std::fs;
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;

fn run(root: &Path) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap(), "lint"])
        .output()
        .expect("ctxgrd executes")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const CONFIG: &str = r#"
[VULN]
paths = ["docs/security/findings/**"]
rules = ["security.remediation-link"]

[VULN."security.remediation-link"]
require-when-status = "mitigated"
accepted-namespaces = ["ADR"]

[SOC2]
paths = ["docs/compliance/soc2/**"]
rules = ["soc2.control-evidence"]

[SOC2."soc2.control-evidence"]
evidence-fields = ["evidence_link"]
out-of-scope-status = ["not-applicable"]

[ADR]
paths = ["docs/adrs/**"]
rules = ["core.id"]

[POLICY]
paths = ["docs/policies/**"]
rules = ["core.id"]
"#;

/// A tree holding one ADR and one POLICY that the documents under test do
/// **not** cite. Their only job is to exist: under corpus-wide candidacy
/// they would satisfy both rules.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("ctxgrd.toml"), CONFIG).unwrap();
    for dir in [
        "docs/security/findings",
        "docs/compliance/soc2",
        "docs/adrs",
        "docs/policies",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(
        root.join("docs/adrs/001-unrelated.md"),
        "---\nid: ADR-001\ntitle: An unrelated decision\n---\n\n# ADR-001: An unrelated decision\n\nNothing to do with the finding.\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/policies/001-unrelated.md"),
        "---\nid: POLICY-001\ntitle: An unrelated policy\n---\n\n# POLICY-001: An unrelated policy\n\nNothing to do with the control.\n",
    )
    .unwrap();
    tmp
}

fn write_vuln(root: &Path, body: &str, depends_on: &str) {
    fs::write(
        root.join("docs/security/findings/001-finding.md"),
        format!(
            "---\nid: VULN-001\ntitle: Example finding\nstatus: mitigated\n{depends_on}---\n\n{body}\n"
        ),
    )
    .unwrap();
}

fn write_soc2(root: &Path, body: &str, depends_on: &str) {
    fs::write(
        root.join("docs/compliance/soc2/001-control.md"),
        format!(
            "---\nid: SOC2-001\ntitle: Logical access\ncriterion: CC6.1\nstatus: implemented\n{depends_on}---\n\n{body}\n"
        ),
    )
    .unwrap();
}

#[test]
fn remediation_link_is_not_satisfied_by_an_unrelated_document() {
    // The corpus contains ADR-001; this finding cites nothing. Corpus-wide
    // candidacy would pass it — per-document candidacy must not.
    let tmp = fixture();
    write_vuln(tmp.path(), "# VULN-001: Example finding\n\nFixed.", "");
    let out = run(tmp.path());
    let text = stdout(&out);
    assert!(
        text.contains("security.remediation-link"),
        "a mitigated finding citing nothing must be flagged even though an \
         unrelated ADR exists in the corpus: {text}"
    );
}

#[test]
fn remediation_link_is_satisfied_by_a_citation_it_actually_makes() {
    // The positive control for the test above. Same tree, same rule; the
    // only change is that the finding now cites the ADR.
    let tmp = fixture();
    write_vuln(
        tmp.path(),
        "# VULN-001: Example finding\n\nFixed by ADR-001.",
        "depends_on: [ADR-001]\n",
    );
    let out = run(tmp.path());
    let text = stdout(&out);
    assert!(
        !text.contains("security.remediation-link"),
        "a finding citing a resolving ADR must pass: {text}"
    );
}

#[test]
fn control_evidence_is_not_satisfied_by_an_unrelated_policy() {
    // BUG-030 end to end: POLICY-999 does not exist, POLICY-001 does and is
    // not cited. Neither may satisfy the control.
    let tmp = fixture();
    write_soc2(
        tmp.path(),
        "# SOC2-001: Logical access\n\nEvidence pending.",
        "depends_on: [POLICY-999]\n",
    );
    let out = run(tmp.path());
    let text = stdout(&out);
    assert!(
        text.contains("soc2.control-evidence"),
        "a control citing a POLICY nobody wrote must be flagged: {text}"
    );
}

#[test]
fn control_evidence_is_satisfied_by_a_resolving_policy() {
    let tmp = fixture();
    write_soc2(
        tmp.path(),
        "# SOC2-001: Logical access\n\nImplemented per POLICY-001.",
        "depends_on: [POLICY-001]\n",
    );
    let out = run(tmp.path());
    let text = stdout(&out);
    assert!(
        !text.contains("soc2.control-evidence"),
        "a control citing a resolving POLICY must pass: {text}"
    );
}

#[test]
fn remediation_link_is_not_satisfied_by_the_documents_own_id() {
    // BUG-031 end to end. `ctxgrd new VULN` scaffolds the id into the body
    // H1, so this is the shape every scaffolded finding starts in. The
    // accepted namespace is widened to VULN precisely so the own-id
    // exclusion is what is under test rather than the namespace filter.
    let tmp = fixture();
    let cfg = CONFIG.replace(
        r#"accepted-namespaces = ["ADR"]"#,
        r#"accepted-namespaces = ["ADR", "VULN"]"#,
    );
    fs::write(tmp.path().join("ctxgrd.toml"), cfg).unwrap();
    write_vuln(tmp.path(), "# VULN-001: Example finding\n\nFixed.", "");
    let out = run(tmp.path());
    let text = stdout(&out);
    assert!(
        text.contains("security.remediation-link"),
        "the document's own id must not satisfy its remediation link: {text}"
    );
}
