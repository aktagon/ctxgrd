//! Integration tests for `core.acceptance-complete` on `[TASK]` (ADR-122 §
//! ACC-001) — the anti-self-declaration rule on the one namespace whose
//! terminal status genuinely means the work is delivered.
//!
//! The three fixture roots are the same document differing by **one
//! character**: `done-open` has an unchecked acceptance box, `done-checked`
//! flips it to `[x]`, `in-flight` walks `status` back to `doing`. Isolating the
//! variable that way is what makes the silent cases evidence — a test that only
//! asserts "fires on a terminal doc" passes just as well if the rule fires on
//! everything.
//!
//! The pack-binding tests pin ADR-122's *negative* decision. The rule is
//! deliberately NOT bound on `[SPEC]` or `[PRD]`, whose terminal status
//! (`accepted`) means the design or plan is agreed rather than delivered. That
//! is a conclusion from measurement, not an oversight, so it needs a test — an
//! absent binding leaves nothing else to notice if someone re-adds it.

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;

const WORKFLOW_PACK: &str = include_str!("../packs/workflow/pack.toml");
const PROJECT_DOCS_PACK: &str = include_str!("../packs/project-docs/pack.toml");
const REPO_CONFIG: &str = include_str!("../ctxgrd.toml");

const RULE: &str = "core.acceptance-complete";

fn fixture(scenario: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/acceptance-complete-task")
        .join(scenario)
}

fn run(root: &Path) -> Output {
    Command::cargo_bin("ctxgrd")
        .expect("binary built")
        .args(["--root", root.to_str().unwrap()])
        .output()
        .expect("ctxgrd executes")
}

/// Reads a namespace's `rules` array out of a pack or config.
fn rules_of(toml_src: &str, namespace: &str) -> Vec<String> {
    let v: toml::Value = toml_src.parse().expect("valid TOML");
    v.get(namespace)
        .and_then(|n| n.get("rules"))
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn done_task_with_an_unchecked_acceptance_item_errors() {
    let out = run(&fixture("done-open"));
    let stdout = String::from_utf8(out.stdout).unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "a TASK at `done` with an unmet acceptance criterion must fail\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("error[{RULE}]")),
        "expected a {RULE} error:\n{stdout}"
    );
    // Exactly one: the fixture carries a second open box under `Out of scope`,
    // which is deferred work and must never fire. If the heading window ever
    // widens, this count is what catches it.
    assert!(
        !stdout.contains("this line is YAML/TOML noise"),
        "a `- ` inside a fenced code block is an example, not a question:\n{stdout}"
    );
    assert!(
        stdout.contains("found: 1 error"),
        "exactly one finding — the `Out of scope` box must stay silent:\n{stdout}"
    );
}

#[test]
fn done_task_with_every_box_checked_is_silent() {
    let out = run(&fixture("done-checked"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "all criteria met — nothing to report\n{stdout}"
    );
    assert!(
        !stdout.contains(RULE),
        "no {RULE} diagnostic when every box is checked:\n{stdout}"
    );
}

#[test]
fn in_flight_task_with_an_open_box_is_silent() {
    let out = run(&fixture("in-flight"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a TASK still at `doing` may legitimately carry open boxes\n{stdout}"
    );
    assert!(
        !stdout.contains(RULE),
        "the rule is gated on a terminal status:\n{stdout}"
    );
}

/// `require_checkboxes` (ADR-122 § ACC-006). Without it the scan only sees GFM
/// task items, so a section of prose bullets is invisible and the document
/// reports clean because nothing is *checkable* — the ADR-119 invariant at the
/// item level. 94 of this repo's 119 terminal ADRs are in exactly that state.
#[test]
fn require_checkboxes_flags_a_prose_open_question() {
    let out = run(&fixture("oq-prose-strict"));
    let stdout = String::from_utf8(out.stdout).unwrap();

    assert_eq!(out.status.code(), Some(1), "a prose item fails\n{stdout}");
    assert!(
        stdout.contains("is a prose bullet, not a checkbox"),
        "expected the prose-bullet diagnostic:\n{stdout}"
    );
    // Exactly one. The fixture deliberately surrounds that single prose item
    // with four shapes that must all stay silent:
    //   - a checked `- [x]` item (already conforming)
    //   - a nested elaboration bullet under it (detail, not a question)
    //   - a `- ` line inside a fenced ```toml block (an example, and the
    //     author's only remedy would be to delete it — unfixable)
    //   - a `- - -` thematic break (CommonMark rule, not a list item)
    // The count is what proves it: a rule that flagged every bullet-shaped
    // line would report four.
    assert!(
        stdout.contains("found: 1 error"),
        "the `[x]` item and the nested bullet must not fire:\n{stdout}"
    );
}

/// The same document, same rule, with the flag absent. Off by default means no
/// existing config tightens when this ships — the two fixture roots differ by
/// exactly one line.
#[test]
fn require_checkboxes_is_off_by_default() {
    let out = run(&fixture("oq-prose-default"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "prose items are silent unless the namespace opts in\n{stdout}"
    );
    assert!(!stdout.contains("prose bullet"), "no diagnostic:\n{stdout}");
}

#[test]
fn workflow_pack_binds_the_rule_on_task() {
    assert!(
        rules_of(WORKFLOW_PACK, "TASK").iter().any(|r| r == RULE),
        "the workflow pack must bind {RULE} on [TASK] (ADR-122 § ACC-001)"
    );
}

/// ADR-122 § ACC-001, the rejected half. `[SPEC]`'s terminal status is
/// `accepted` — the design is agreed, not delivered — so a freshly-accepted
/// SPEC correctly carries unchecked boxes. Measured across the local fleet, the
/// binding fired on 9 of the 9 terminal SPECs that use checkboxes at all.
#[test]
fn workflow_pack_does_not_bind_the_rule_on_spec() {
    assert!(
        !rules_of(WORKFLOW_PACK, "SPEC").iter().any(|r| r == RULE),
        "{RULE} must NOT be bound on [SPEC] — `accepted` is not a delivery \
         state, and binding it over-fires on healthy documents (ADR-122)"
    );
}

/// Same reasoning for `[PRD]`, which terminates at `accepted` too. 21 of 50
/// terminal PRDs in the local fleet have no matching heading at all, so the
/// binding would be latent as well as unsound.
#[test]
fn project_docs_pack_does_not_bind_the_rule_on_prd() {
    assert!(
        !rules_of(PROJECT_DOCS_PACK, "PRD").iter().any(|r| r == RULE),
        "{RULE} must NOT be bound on [PRD] — see ADR-122 § ACC-002"
    );
}

/// The `[ADR]` binding (ADR-099) is untouched by ADR-122 and must stay: it
/// scans `Open Questions`, where ADR `accepted` = decision settled makes the
/// question fair.
#[test]
fn project_docs_pack_still_binds_the_rule_on_adr() {
    assert!(
        rules_of(PROJECT_DOCS_PACK, "ADR").iter().any(|r| r == RULE),
        "ADR-122 must not disturb the ADR-099 binding on [ADR]"
    );
}

/// This repo tracks its own packs. `[TASK]` carries the rule; `[SPEC]` must
/// not, having had a hand-added binding removed by ADR-122.
#[test]
fn repo_config_tracks_the_pack_decision() {
    assert!(
        rules_of(REPO_CONFIG, "TASK").iter().any(|r| r == RULE),
        "ctxgrd.toml [TASK] must carry {RULE}, matching the workflow pack"
    );
    assert!(
        !rules_of(REPO_CONFIG, "SPEC").iter().any(|r| r == RULE),
        "ctxgrd.toml [SPEC] must not carry {RULE} — removed by ADR-122; it was \
         inert here anyway, since this repo's SPECs use numbered lists"
    );
}
