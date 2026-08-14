//! Reporting invariants, as properties rather than examples
//! (`HANDOFF-037` § A10, phase 1).
//!
//! Chosen by measurement, not taste. Of 41 bugs filed against this
//! project, exactly **one** is in the dependency graph (`BUG-027`, a
//! recursion depth that random inputs would never have surfaced), while
//! **18** are in claim/dispatch and reporting. The reporting defects share
//! a shape that example tests are structurally bad at catching: they are
//! arithmetic relationships *between* numbers the report prints, so a
//! hand-written case only fails if the author already suspected the
//! relationship could break. `HANDOFF-036` § 1 found the census defect by
//! hand after it had shipped in every release since `ADR-118`; the
//! partition property below fails on the first generated corpus.
//!
//! Phase 2 — claim uniqueness and rule reachability, the 11-bug cluster —
//! is deliberately not here.

use ctxgrd::diagnostic::{Diagnostic, KernelMessage, Severity};
use ctxgrd::reporter;
use ctxgrd::status::{DocReadiness, DocState, Report};
use proptest::prelude::*;
use std::path::Path;

/// Statuses spanning the three cases the report distinguishes: terminal,
/// non-terminal, and absent. Real vocabulary, never `foo`/`bar` — a
/// generator that cannot produce `consumed` would not have caught A3.
fn status_strategy() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some("accepted".to_string())),
        Just(Some("consumed".to_string())),
        Just(Some("superseded".to_string())),
        Just(Some("fixed".to_string())),
        Just(Some("draft".to_string())),
        Just(Some("open".to_string())),
        Just(Some("rejected".to_string())),
        Just(Some("deferred".to_string())),
        Just(Some("pending".to_string())),
    ]
}

/// A `DocReadiness` built **directly**, not through `status`'s projection.
///
/// That is the point of the exercise. The projection maintains
/// `ready == !terminal && blocked_by.is_empty()` at its construction site,
/// so a generator routed through it could never produce a row that lands
/// in two buckets — and the partition would look sound while resting on
/// one line of a private function. Constructing rows independently tests
/// the accessors instead of the constructor, which is why `Report::ready`
/// had to stop reading the raw `ready` field.
fn doc_strategy() -> impl Strategy<Value = DocReadiness> {
    (
        1u32..40,
        prop::sample::select(vec!["ADR", "BUG", "HANDOFF", "SPEC", "PRD"]),
        status_strategy(),
        any::<bool>(),
        prop::collection::vec(1u32..40, 0..3),
    )
        .prop_map(|(number, namespace, status, ready, blockers)| {
            let mut blocked_by: Vec<String> =
                blockers.iter().map(|n| format!("{namespace}-{n:03}")).collect();
            blocked_by.sort();
            blocked_by.dedup();
            DocReadiness {
                id: format!("{namespace}-{number:03}"),
                namespace: namespace.to_string(),
                // Over 60 characters for every namespace and number, so
                // the `BUG-046` title column is *fitted* on every generated
                // report — the properties below then hold over truncated
                // rows and not only over short ones.
                title: format!(
                    "Reconcile the {namespace} ledger for period {number} and reissue the statement"
                ),
                status,
                ready,
                blocked_by,
                deps: Vec::new(),
            }
        })
}

fn report_strategy() -> impl Strategy<Value = Report> {
    prop::collection::vec(doc_strategy(), 0..25).prop_map(|mut documents| {
        documents.sort_by(|a, b| a.id.cmp(&b.id));
        documents.dedup_by(|a, b| a.id == b.id);
        Report {
            lineage: None,
            shared: Vec::new(),
            documents,
        }
    })
}

fn severity_strategy() -> impl Strategy<Value = Severity> {
    prop_oneof![
        Just(Severity::Error),
        Just(Severity::Warning),
        Just(Severity::Info),
    ]
}

proptest! {
    /// The census line says `N documents · R ready · B blocked · S settled`.
    /// If those three buckets do not partition the corpus, at least one
    /// number on that line is a lie — either a document is counted twice or
    /// one is invisible. This is `HANDOFF-036` § 1 stated generally.
    #[test]
    fn buckets_partition_the_corpus(report in report_strategy()) {
        let ready = report.ready().count();
        let blocked = report.blocked().count();
        let settled = report
            .documents
            .iter()
            .filter(|d| d.state() == DocState::Done)
            .count();
        prop_assert_eq!(
            ready + blocked + settled,
            report.documents.len(),
            "ready={} blocked={} settled={} total={}",
            ready,
            blocked,
            settled,
            report.documents.len()
        );
    }

    /// Every document lands in exactly one bucket — the same fact from the
    /// other side. The sum could hold by two errors cancelling; membership
    /// cannot.
    #[test]
    fn every_document_lands_in_exactly_one_bucket(report in report_strategy()) {
        for doc in &report.documents {
            let in_ready = report.ready().any(|d| d.id == doc.id);
            let in_blocked = report.blocked().any(|d| d.id == doc.id);
            let in_settled = doc.state() == DocState::Done;
            let memberships = [in_ready, in_blocked, in_settled]
                .iter()
                .filter(|m| **m)
                .count();
            prop_assert_eq!(
                memberships, 1,
                "{} is in {} buckets (ready={}, blocked={}, settled={}), status={:?}, ready_field={}",
                doc.id, memberships, in_ready, in_blocked, in_settled, doc.status, doc.ready
            );
        }
    }

    /// `--exit-code` must agree with what the report printed. A run that
    /// lists nothing under `blocked:` and then exits 1 is the `BUG-036` R2
    /// complaint; the inverse would be worse.
    #[test]
    fn the_done_signal_agrees_with_the_blocked_bucket(report in report_strategy()) {
        prop_assert_eq!(report.unblocked(), report.blocked().count() == 0);
    }

    /// The text census and the JSON payload are two renderings of one
    /// answer. An agent branching on JSON and a human reading the text
    /// must not be told different things (the ADR-086 output contract).
    #[test]
    fn text_and_json_agree_on_the_counts(report in report_strategy()) {
        let text = ctxgrd::status::render_report(&report, true);
        let json: serde_json::Value =
            serde_json::from_str(&ctxgrd::status::render_json(&report, true)).expect("valid JSON");

        let rows = json["documents"].as_array().expect("documents array");
        prop_assert_eq!(rows.len(), report.documents.len());

        let census = text.lines().next().unwrap_or_default();
        prop_assert!(
            census.starts_with(&format!("{} document", report.documents.len())),
            "census line {:?} disagrees with {} documents",
            census,
            report.documents.len()
        );

        // The JSON `ready` flag is the raw field; the text bucket is
        // `state()`. They may legitimately differ on a hand-built row, but
        // every row the text calls ready must be flagged ready in JSON —
        // `Current` is a strict narrowing of the field, never a widening.
        for doc in report.ready() {
            let row = rows
                .iter()
                .find(|r| r["id"].as_str() == Some(doc.id.as_str()))
                .expect("every text row has a JSON row");
            prop_assert_eq!(row["ready"].as_bool(), Some(true), "{}", doc.id);
        }
    }

    /// Every document named under `ready:` or `blocked:` appears in the
    /// text, and nothing is quietly truncated — `render_report`'s doc
    /// comment promises exactly this ("Nothing is truncated ... the output
    /// never implies coverage it does not have").
    #[test]
    fn the_text_report_names_every_document_it_counts(report in report_strategy()) {
        let text = ctxgrd::status::render_report(&report, true);
        for doc in report.ready().chain(report.blocked()) {
            prop_assert!(
                text.contains(doc.id.as_str()),
                "{} is counted but never named:\n{}",
                doc.id,
                text
            );
        }
    }

    /// The `found:` trailer is the run's tally. It must equal the
    /// severities actually rendered, across both channels — `BUG-039` was
    /// this property failing whenever the kernel channel was non-empty.
    #[test]
    fn the_found_trailer_matches_the_rendered_severities(
        diag_severities in prop::collection::vec(severity_strategy(), 0..8),
        kernel_severities in prop::collection::vec(severity_strategy(), 0..5),
    ) {
        let diagnostics: Vec<Diagnostic> = diag_severities
            .iter()
            .enumerate()
            .map(|(i, sev)| {
                let d = Diagnostic::error("core.id", "docs/adrs/001-ledger.md", 0, 0, "bad id");
                Diagnostic { severity: *sev, message: format!("finding {i}"), ..d }
            })
            .collect();
        let kernel: Vec<KernelMessage> = kernel_severities
            .iter()
            .enumerate()
            .map(|(i, sev)| {
                let m = KernelMessage::error("src.runtime-error", format!("source {i} failed"));
                KernelMessage { severity: *sev, ..m }
            })
            .collect();

        let expected_errors = diag_severities.iter().chain(kernel_severities.iter())
            .filter(|s| **s == Severity::Error).count();
        let expected_warnings = diag_severities.iter().chain(kernel_severities.iter())
            .filter(|s| **s == Severity::Warning).count();

        let out = reporter::render_rich(&diagnostics, &kernel, Path::new("."));

        if diagnostics.is_empty() && kernel.is_empty() {
            prop_assert_eq!(out, String::new());
        } else {
            let expected = format!(
                "found: {} · {}",
                if expected_errors == 1 { "1 error".to_string() }
                    else { format!("{expected_errors} errors") },
                if expected_warnings == 1 { "1 warning".to_string() }
                    else { format!("{expected_warnings} warnings") },
            );
            prop_assert!(
                out.contains(&expected),
                "expected {:?} in:\n{}",
                expected,
                out
            );
        }
    }
}
