//! `ctxgrd status` — the per-document work queue (ADR-107, ADR-118).
//!
//! ADR-118 removed the namespace stage layer. What a document can be
//! worked on is now a property of the document: its own `status` and the
//! statuses of the documents its `depends_on` closure names. There is no
//! stage, no gate, no frontier, and no declared ladder — `core.dep-shape`
//! carries namespace-level edge constraints, enforced per document at lint
//! time (STG-006).
//!
//! The queue covers **every** linted document carrying an id (STG-001).
//! The previous filter admitted only staged namespaces, which dropped 91 of
//! 212 documents in this repo — including every `BUG` and every `HANDOFF`,
//! the two namespaces that most directly answer "what can I work on"
//! (`BUG-036` R1).

use std::path::Path;

use thiserror::Error;

use std::collections::BTreeSet;

use crate::dag::{self};
use crate::document::Document;
use crate::id::DocumentId;
use crate::run::{self, LintError};

/// What can go wrong answering "what is workable?".
#[derive(Debug, Error)]
pub enum StatusError {
    /// Config / ingest failures — same channel as `lint`.
    #[error(transparent)]
    Lint(#[from] LintError),
    /// A `--lineage <ID>` selector that resolves to no document in the run
    /// (EARS-04.5).
    #[error("lineage root '{id}' is not a document in this run")]
    LineageNotFound { id: String },
}

// `infer_dep_shape_requires` — the ADR-039 § DAG-007 init-time seam that
// lifted `depends_on` edges into `core.dep-shape` `requires` suggestions —
// was deleted along with the namespace DAG it was the only caller of. It
// had no callers itself, so the whole chain (and `StatusError::Cycle`,
// which only it could produce) was unreachable. Rebuild it against the
// document graph if `ctxgrd init` ever wants to seed a declared shape.

/// The class a document draws in (ADR-108 § GRN-002), read off the same
/// readiness projection the JSON publishes so the picture and the work
/// queue cannot disagree.
///
/// ADR-118 removed the `pending` and `blocked`-by-tripwire stage states
/// along with the stage layer; a document is settled, workable, or waiting
/// on a dependency, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocState {
    /// Terminal `status` — finished, nothing to do.
    Done,
    /// Not terminal, and every dependency is. Workable now.
    Current,
    /// Not terminal, and at least one dependency is not either.
    Blocked,
}

impl DocState {
    /// The stable token used as the Mermaid `classDef` name and in the
    /// text report. `Current` renders as `current` (the ADR-038 palette
    /// name) rather than `ready`, so the surviving diagram output is
    /// unchanged by ADR-118.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Current => "current",
            Self::Blocked => "blocked",
        }
    }
}

/// One document's readiness (ADR-107 § RDY-001): the work-queue row a
/// consumer filters on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocReadiness {
    /// The document's id as written (`ADR-107`). JSON `id`.
    pub id: String,
    /// The namespace that claims it. JSON `namespace`.
    pub namespace: String,
    /// The document's `title:` (or `name:`) frontmatter value, empty when it
    /// declares neither. JSON `title`, whole; the text report fits it to
    /// [`TITLE_WIDTH`].
    ///
    /// `BUG-046`: an id identifies a document without saying anything about
    /// it, so a queue keyed only by id is unreadable to anyone not already
    /// holding the corpus in their head — and, with no `title` on the wire,
    /// unrecoverable by an agent without a second pass over `ctxgrd list`.
    pub title: String,
    /// The `status` frontmatter value verbatim, or `None` when absent —
    /// which serializes as JSON `null` and counts as non-terminal.
    pub status: Option<String>,
    /// True iff `status` is NOT terminal AND every resolved `depends_on`
    /// target's status IS terminal. A settled document is `false` with an
    /// empty `blocked_by`: not workable because it is done.
    ///
    /// That combination is deliberate and is not a defect — `BUG-036` R1
    /// was filed against it in error and corrected. `status` is in the row
    /// so a consumer can tell "done" from "stuck" without a second query.
    pub ready: bool,
    /// The id-sorted resolved dependency targets whose status is not
    /// terminal. Resolved against the full graph, so a blocker outside a
    /// `--lineage` scope is still named (RDY-003).
    pub blocked_by: Vec<String>,
    /// Every resolved `depends_on` target, id-sorted — the superset of
    /// `blocked_by`. The edge set the diagrams draw (ADR-108 § GRN-002);
    /// not serialized. Unresolved entries are absent, as in `blocked_by`.
    pub deps: Vec<String>,
}

impl DocReadiness {
    /// This row's diagram/report class.
    ///
    /// Keyed on the **settled** set, not the arming one: a `rejected` or
    /// `deferred` document is [`DocState::Done`] for the census even though
    /// nothing may rest on it (ADR-121 § SPL-001). `ready` is computed against
    /// the same predicate, so the three states stay a partition.
    pub fn state(&self) -> DocState {
        if is_settled_status(self.status.as_deref()) {
            DocState::Done
        } else if self.ready {
            DocState::Current
        } else {
            DocState::Blocked
        }
    }
}

/// The full `ctxgrd status` answer (ADR-118): the per-document work queue,
/// plus the lineage scope when one is selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The lineage root id when scoped via `--lineage <ID>` (EARS-04.1), or
    /// `None` for the global view. Serialized only in lineage mode.
    pub lineage: Option<String>,
    /// Shared-node disclosure (ADR-059 § LIN-005): the other lineage-root
    /// ids whose closures also contain a document counted here. Name-sorted
    /// and deduped; empty in the global view.
    ///
    /// ADR-118 re-keyed this from per-stage to a flat list — the per-stage
    /// array went with the stages. The disclosure itself survives: LIN-005
    /// requires shared members be named rather than silently folded.
    pub shared: Vec<String>,
    /// One row per counted document, id-sorted (ADR-107 § RDY-001).
    pub documents: Vec<DocReadiness>,
}

impl Report {
    /// Documents workable right now.
    ///
    /// Keyed on [`DocReadiness::state`], not on the raw `ready` field. The
    /// two agree for every row [`project`] builds, but the field is public
    /// and a directly-constructed `DocReadiness` can set `ready: true`
    /// beside a terminal `status` — which would put one row in both this
    /// bucket and `settled`, breaking the partition that
    /// `tests/reporting_properties.rs` asserts. Deriving all three buckets
    /// from the same 3-way-exclusive function makes the partition hold by
    /// construction rather than by the constructor's discipline.
    pub fn ready(&self) -> impl Iterator<Item = &DocReadiness> {
        self.documents
            .iter()
            .filter(|d| d.state() == DocState::Current)
    }

    /// Documents held by at least one non-terminal dependency **and not
    /// themselves finished**.
    ///
    /// The second clause is the `HANDOFF-036` § 1 fix. This filtered on
    /// `!blocked_by.is_empty()` alone, ignoring state, so an `accepted`
    /// document pointing at a `draft` one was counted here *and* in
    /// `settled` — three buckets over two documents, with a finished ADR
    /// printed under `blocked:` as though someone were waiting on it.
    ///
    /// The edge is not discarded: [`settled_on_open_work`] names it, and
    /// the JSON wire shape carries `status` and `blocked_by` per row, so a
    /// consumer can still derive it. Whether such an edge is a *defect* is
    /// `core.dep-status`'s question, and that rule is opt-in (ADR-106
    /// § DPS-004) — the census must not answer it by arithmetic.
    pub fn blocked(&self) -> impl Iterator<Item = &DocReadiness> {
        self.documents
            .iter()
            .filter(|d| d.state() == DocState::Blocked)
    }

    /// Terminal documents that still point at open work, each paired with
    /// the non-terminal targets holding the edge — the disclosure behind
    /// the narrowed [`blocked`](Self::blocked).
    ///
    /// Named rather than counted, following `ADR-059` § LIN-005's `shared:`
    /// disclosure: a report that drops a row from a bucket owes the reader
    /// the row, not a tally.
    pub fn settled_on_open_work(&self) -> impl Iterator<Item = &DocReadiness> {
        self.documents
            .iter()
            .filter(|d| d.state() == DocState::Done && !d.blocked_by.is_empty())
    }

    /// STG-004: the `--exit-code` done-signal. True when no document in
    /// scope is [`DocState::Blocked`].
    ///
    /// Note this asks whether anything is *stuck*, not whether everything
    /// is *finished* — a repo with open work that nothing blocks is a
    /// healthy repo, and `ADR-056`'s signal is meant to certify a feature
    /// is unobstructed. Under the stage layer this could not pass at all
    /// (`BUG-036` R2).
    ///
    /// It follows [`blocked`](Self::blocked) rather than testing
    /// `blocked_by` directly, so a corpus whose only "block" is a finished
    /// document pointing at open work now returns 0. That document is done;
    /// nothing is waiting on it, and there is no action the signal could be
    /// asking for. Gating that edge is `core.dep-status`'s job — a rule a
    /// project opts into — not the done-signal's.
    pub fn unblocked(&self) -> bool {
        self.documents
            .iter()
            .all(|d| d.state() != DocState::Blocked)
    }
}

/// The global `ctxgrd status` report: every document, no lineage scope.
pub fn report(root: &Path) -> Result<Report, StatusError> {
    report_scoped(root, None)
}

/// Ingest, optionally scope the document set to a lineage, and project
/// per-document readiness (ADR-107 § RDY-001; ADR-118 § STG-001).
///
/// WHERE `lineage` is `Some(id)`, the counted set is restricted to the
/// transitive **dependents** of `id` over the transpose of the `depends_on`
/// graph (EARS-04.1). Shared members (reachable from another lineage root)
/// are disclosed, not folded (EARS-04.3/04.4). An `id` that resolves to no
/// document is [`StatusError::LineageNotFound`] (EARS-04.5).
///
/// This runs [`run::ingest`], not the full lint pass. ADR-118 removed the
/// only consumer of rule diagnostics here — the stage gate's
/// satisfied-**and-clean** test — so `status` no longer executes 116 rules
/// over the corpus to answer a question about frontmatter.
pub fn report_scoped(root: &Path, lineage: Option<&str>) -> Result<Report, StatusError> {
    let run::IngestResult { documents, .. } = run::ingest(root)?;
    // The one `depends_on` graph this run needs: it scopes the lineage
    // (EARS-04.1) and projects readiness (ADR-107 § RDY-001). Built once.
    let graph = dag::DepGraph::new(&documents);

    let mut lineage_id: Option<String> = None;
    let mut members: Option<BTreeSet<DocumentId>> = None;
    let mut shared: Vec<String> = Vec::new();
    if let Some(raw) = lineage {
        let id: DocumentId = raw
            .parse()
            .map_err(|_| StatusError::LineageNotFound { id: raw.to_string() })?;
        let Some(root_idx) = graph.index_of(&id) else {
            return Err(StatusError::LineageNotFound { id: raw.to_string() });
        };
        let member_idxs = graph.dependents(root_idx);
        members = Some(
            member_idxs
                .iter()
                .map(|&i| documents[i].id.clone())
                .collect(),
        );
        lineage_id = Some(documents[root_idx].raw_id.clone());

        // Disclose shared members (EARS-04.4): a member reachable from a
        // lineage root other than the queried one is counted here AND named.
        for &m in &member_idxs {
            shared.extend(
                graph
                    .owning_roots(m)
                    .into_iter()
                    .filter(|&r| r != root_idx)
                    .map(|r| documents[r].raw_id.clone()),
            );
        }
        shared.sort();
        shared.dedup();
    }

    let documents = compute_documents(&documents, &graph, members.as_ref());

    Ok(Report {
        lineage: lineage_id,
        shared,
        documents,
    })
}

/// A `status` value is **arming** when it is in the shared vocabulary
/// [`crate::agent_guide::DEFAULT_TERMINAL_STATUSES`], compared
/// case-insensitively — the same test `core.dep-status` applies (ADR-106 §
/// DPS-003). An absent status is non-arming: a document that never
/// declares itself finished has not finished (ADR-107 § RDY-002).
///
/// Answers *may a document rest on this one?*, so it is the right test for
/// `blocked_by` and the wrong one for the census. Use [`is_settled_status`]
/// there (ADR-121 § SPL-001).
fn is_arming_status(status: Option<&str>) -> bool {
    match status {
        Some(s) => {
            crate::agent_guide::DEFAULT_TERMINAL_STATUSES.contains(&s.to_lowercase().as_str())
        }
        None => false,
    }
}

/// A `status` value is **settled** when the document has no work left —
/// every arming status, plus `rejected` and `deferred`, which are finished
/// without settling anything ([`crate::agent_guide::is_settled_status`]).
/// An absent status is unsettled, for `is_arming_status`'s reason.
///
/// This is the census predicate: `ready`, [`DocState`], and every count
/// derived from them. Reading the arming set here reported 34 finished
/// documents as workable and diluted the queue by 46% (`BUG-037`).
fn is_settled_status(status: Option<&str>) -> bool {
    match status {
        Some(s) => crate::agent_guide::is_settled_status(s),
        None => false,
    }
}

/// The document's `status` frontmatter value, or `None` when absent.
fn doc_status(doc: &Document) -> Option<&str> {
    doc.metadata.get("status").and_then(|v| v.as_str())
}

/// Project per-document readiness (ADR-107 § RDY-001/002/003) off the
/// `DepGraph` already built for this run — no second graph, no re-parse.
///
/// ADR-118 § STG-001: **every** document carrying an id gets a row. The
/// removed `counted` parameter filtered to namespaces the resolved DAG
/// staged, which silently omitted every unstaged namespace. `members`
/// still scopes to a lineage (RDY-003), while `blocked_by` is resolved
/// against the full graph so an out-of-lineage blocker is named.
/// Unresolved `depends_on` entries are ignored — that defect is
/// `core.dep-resolved`'s to report (ADR-106 § DPS-002).
fn compute_documents(
    documents: &[Document],
    graph: &dag::DepGraph<'_>,
    members: Option<&BTreeSet<DocumentId>>,
) -> Vec<DocReadiness> {
    let mut rows: Vec<DocReadiness> = documents
        .iter()
        .enumerate()
        .filter(|(_, d)| members.is_none_or(|m| m.contains(&d.id)))
        .map(|(i, d)| {
            // One walk of this document's resolved dependencies feeds both
            // the readiness predicate (RDY-002) and the document edge set
            // (ADR-108 § GRN-002) — no second graph pass.
            let mut deps: Vec<String> = Vec::new();
            let mut blocked_by: Vec<String> = Vec::new();
            // A dependency holds this document unless it *arms* it. The
            // wider settled set is deliberately not used here: resting on a
            // `rejected` decision must stay visible (ADR-121 § SPL-002).
            for t in graph.dependencies(i) {
                deps.push(documents[t].raw_id.clone());
                if !is_arming_status(doc_status(&documents[t])) {
                    blocked_by.push(documents[t].raw_id.clone());
                }
            }
            deps.sort();
            deps.dedup();
            blocked_by.sort();
            blocked_by.dedup();
            let status = doc_status(d);
            DocReadiness {
                ready: !is_settled_status(status) && blocked_by.is_empty(),
                id: d.raw_id.clone(),
                namespace: d.id.namespace.clone(),
                // Already parsed into `Document.metadata` by the single
                // ingest pass — the row was simply never given it
                // (`BUG-046`). Read through `list`'s accessor so the two
                // commands cannot name the same document differently.
                title: crate::list::title_of(d),
                status: status.map(str::to_string),
                blocked_by,
                deps,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// The text report's title column, in characters (`BUG-046`).
///
/// Measured on this repo: 35 ready rows cost +627 tokens with titles whole
/// and +272 fitted to 60. The bound is there for legibility, not for the
/// tokens — this project writes titles as full sentences and the longest is
/// 202 characters, so untruncated a handful of rows would dominate an
/// injected report. The JSON field is never cut.
const TITLE_WIDTH: usize = 60;

/// `title` fitted to [`TITLE_WIDTH`], with an ellipsis standing in for what
/// was cut so a shortened title never reads as the whole one.
///
/// Counts `char`s rather than bytes: a multi-byte title must be cut on a
/// character boundary, not panic on one.
fn fit_title(title: &str) -> String {
    if title.chars().count() <= TITLE_WIDTH {
        return title.to_string();
    }
    let kept: String = title.chars().take(TITLE_WIDTH - 1).collect();
    format!("{kept}…")
}

/// One queue line up to the title: `  <id>  <status>`, then the fitted
/// title. Both buckets share it so they cannot drift into different column
/// orders.
///
/// A `tw` of 0 means *there is no title column* — under `--no-titles`, and
/// equally in a bucket where no document declares one. The column is then
/// absent rather than empty, which is what lets the opt-out reproduce the
/// pre-`BUG-046` output byte for byte rather than approximately.
fn queue_line(d: &DocReadiness, w: usize, sw: usize, tw: usize) -> String {
    let mut line = format!("  {:<w$}  {:<sw$}", d.id, doc_label_status(d));
    if tw > 0 {
        line.push_str(&format!("  {:<tw$}", fit_title(&d.title)));
    }
    line
}

/// The widest id, status and fitted title in a bucket, so its columns line
/// up within it.
///
/// Status is padded only when a title column follows it — otherwise the
/// padding is invisible trailing space in the `ready` bucket and a shifted
/// `←` in the `blocked` one, for no content. So a corpus whose documents
/// declare no titles renders identically with the flag and without it.
fn column_widths(rows: &[&DocReadiness], titles: bool) -> (usize, usize, usize) {
    let w = rows.iter().map(|d| d.id.len()).max().unwrap_or(0);
    let tw = if titles {
        rows.iter()
            .map(|d| fit_title(&d.title).chars().count())
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    if tw == 0 {
        return (w, 0, 0);
    }
    let sw = rows
        .iter()
        .map(|d| doc_label_status(d).len())
        .max()
        .unwrap_or(0);
    (w, sw, tw)
}

/// Full `ctxgrd status` text output: a one-line census, then the workable
/// documents, then the blocked ones with what holds each.
///
/// The census leads because the queue is now the whole corpus — 212 rows
/// is a wall of text, and the two lists a person acts on are short. No
/// *row* is truncated: every ready and every blocked document is printed,
/// so the output never implies coverage it does not have. The title *cell*
/// is fitted to [`TITLE_WIDTH`] (`BUG-046`), which is a bound on one column
/// and not on the census — a shortened title marks itself with an ellipsis,
/// where a dropped row could not mark itself at all.
///
/// WHERE `titles` is false (`--no-titles`), the title column is absent and
/// the output is byte-identical to the pre-`BUG-046` report — the opt-out
/// `ADR-074`'s per-session-start inject would need.
pub fn render_report(report: &Report, titles: bool) -> String {
    let mut out = String::new();

    if let Some(root) = &report.lineage {
        out.push_str(&format!("lineage: {root}\n"));
        if !report.shared.is_empty() {
            out.push_str(&format!("shared with: {}\n", report.shared.join(", ")));
        }
        out.push('\n');
    }

    let ready: Vec<&DocReadiness> = report.ready().collect();
    let blocked: Vec<&DocReadiness> = report.blocked().collect();
    let settled = report
        .documents
        .iter()
        .filter(|d| d.state() == DocState::Done)
        .count();
    out.push_str(&format!(
        "{} documents · {} ready · {} blocked · {} settled\n",
        report.documents.len(),
        ready.len(),
        blocked.len(),
        settled,
    ));

    if !ready.is_empty() {
        let (w, sw, tw) = column_widths(&ready, titles);
        out.push_str("\nready:\n");
        for d in &ready {
            // The title ends the line here, so its padding is trimmed off:
            // a row whose document declares no title must not be wider
            // than one that does.
            out.push_str(queue_line(d, w, sw, tw).trim_end());
            out.push('\n');
        }
    }

    if !blocked.is_empty() {
        let (w, sw, tw) = column_widths(&blocked, titles);
        out.push_str("\nblocked:\n");
        for d in &blocked {
            // Here the padding stays: `←` and its blockers follow, and a
            // ragged arrow costs the reader more than the spaces do.
            out.push_str(&format!(
                "{}  ← {}\n",
                queue_line(d, w, sw, tw),
                d.blocked_by.join(", "),
            ));
        }
    }

    // The disclosure behind the narrowed `blocked:` bucket. These rows are
    // finished, so they are not work — but they were visible before, and
    // dropping them silently would trade one false report for another.
    // Named, not counted (`ADR-059` § LIN-005).
    let settled_open: Vec<&DocReadiness> = report.settled_on_open_work().collect();
    if !settled_open.is_empty() {
        out.push_str("\nsettled on open work:\n");
        for d in &settled_open {
            out.push_str(&format!("  {} ← {}\n", d.id, d.blocked_by.join(", ")));
        }
        out.push_str("  (enable core.dep-status to gate this)\n");
    }

    // Gated on the two work buckets only: a settled-on-open-work edge is
    // a disclosure, not an open item, so it must not make the queue claim
    // to hold something.
    if ready.is_empty() && blocked.is_empty() {
        out.push_str("\nnothing open.\n");
    }

    out.push_str("\ntip: --format json for agents, mermaid/dot for diagrams\n");
    out
}

/// `ctxgrd status --format json` (ADR-107 § RDY-001; ADR-118 § STG-003).
///
/// The payload is the work queue and nothing else. `stages`, `edges`,
/// `frontier`, `blockers`, `blocker_stages` and `next_action` were removed
/// with the stage layer, and `source`/`source_hint` went with them — both
/// described the provenance of a namespace DAG that is no longer resolved
/// or emitted, so keeping them would have named a structure the output no
/// longer contains.
///
/// The wire shape is a dedicated struct so the JSON contract is pinned
/// independently of the in-memory [`Report`] (ADR-037 § WIRE-005): `deps`
/// stays internal, and `status` serializes as `null` when absent rather
/// than being omitted.
///
/// WHERE `titles` is false (`--no-titles`), `title` is omitted entirely and
/// the payload is byte-identical to the pre-`BUG-046` one. `title` is never
/// fitted to [`TITLE_WIDTH`] here: truncating data in a machine format
/// destroys what a consumer already paid structure to receive, and the
/// reason the text report cuts it — line width — does not exist on the wire.
pub fn render_json(report: &Report, titles: bool) -> String {
    /// One work-queue row (ADR-107 § RDY-001).
    #[derive(serde::Serialize)]
    struct WireDocument<'a> {
        id: &'a str,
        namespace: &'a str,
        /// `None` means *suppressed by `--no-titles`* and is omitted;
        /// `Some("")` means the document declares no title, and is emitted
        /// as an empty string — the distinction a consumer needs to tell
        /// "you asked me not to" from "there is none", and the same empty
        /// string `ctxgrd list --format json` publishes for that document.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<&'a str>,
        status: Option<&'a str>,
        ready: bool,
        blocked_by: &'a [String],
    }
    #[derive(serde::Serialize)]
    struct Wire<'a> {
        /// The lineage root id in `--lineage` mode (EARS-04.1). Omitted in
        /// the global view.
        #[serde(skip_serializing_if = "Option::is_none")]
        lineage: Option<&'a str>,
        /// ADR-059 § LIN-005 disclosure. Omitted when empty, so the global
        /// view carries no lineage machinery at all.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        shared: Vec<&'a str>,
        documents: Vec<WireDocument<'a>>,
    }
    let wire = Wire {
        lineage: report.lineage.as_deref(),
        shared: report.shared.iter().map(String::as_str).collect(),
        documents: report
            .documents
            .iter()
            .map(|d| WireDocument {
                id: &d.id,
                namespace: &d.namespace,
                title: titles.then_some(d.title.as_str()),
                status: d.status.as_deref(),
                ready: d.ready,
                blocked_by: &d.blocked_by,
            })
            .collect(),
    };
    serde_json::to_string_pretty(&wire).unwrap_or_else(|_| "{}".to_string())
}

/// The shared Mermaid palette (ADR-038). ADR-118 dropped the `pending` and
/// `bug` classes with the stage layer: `pending` was a stage state, and the
/// `bug` class styled the namespace-only tripwire overlay (GRN-004). A
/// `classDef` for a state that can no longer occur is a claim the output
/// cannot honour.
fn push_class_defs(out: &mut String) {
    out.push_str("  classDef done fill:#cde6c5,stroke:#33aa77;\n");
    out.push_str("  classDef current fill:#cfe3ff,stroke:#3377aa;\n");
    out.push_str("  classDef blocked fill:#f6cccc,stroke:#cc3333;\n");
}

/// The document label shown in both diagram formats: the id as written
/// plus its `status`, or `none` when the document declares none.
fn doc_label_status(doc: &DocReadiness) -> &str {
    doc.status.as_deref().unwrap_or("none")
}

/// The one-line census both diagram formats carry as a caption.
fn census(report: &Report) -> String {
    format!(
        "{} documents · {} ready · {} blocked",
        report.documents.len(),
        report.ready().count(),
        report.blocked().count(),
    )
}

/// The `depends_on` edges to draw: one per resolved relation whose *both*
/// ends are counted documents, rendered dependency → dependent. Sorted by
/// (dependency, dependent) for deterministic output.
fn doc_edges(report: &Report) -> Vec<(&str, &str)> {
    let nodes: BTreeSet<&str> = report.documents.iter().map(|d| d.id.as_str()).collect();
    let mut edges: Vec<(&str, &str)> = report
        .documents
        .iter()
        .flat_map(|doc| {
            doc.deps
                .iter()
                .filter(|dep| nodes.contains(dep.as_str()))
                .map(move |dep| (dep.as_str(), doc.id.as_str()))
        })
        .collect();
    edges.sort_unstable();
    edges
}

/// `ctxgrd status --format mermaid` (ADR-108 § GRN-002, ADR-118 § STG-005):
/// the **document** `depends_on` graph as Mermaid `flowchart LR` source —
/// one node per counted document, labelled with its id and `status` and
/// classed by ADR-107 readiness, and one solid edge per resolved relation
/// between counted documents.
///
/// This is the only granularity. The namespace view drew the stage DAG and
/// was removed with it.
pub fn render_mermaid(report: &Report) -> String {
    let mut out = String::from("flowchart LR\n");
    out.push_str(&format!("  %% ctxgrd status · {}\n", census(report)));
    for doc in &report.documents {
        out.push_str(&format!(
            "  {node}[\"{id}: {status}\"]:::{state}\n",
            node = mermaid_id(&doc.id),
            id = doc.id,
            status = doc_label_status(doc),
            state = doc.state().as_str(),
        ));
    }
    for (from, to) in doc_edges(report) {
        out.push_str(&format!("  {} --> {}\n", mermaid_id(from), mermaid_id(to)));
    }
    push_class_defs(&mut out);
    out
}

/// Sanitize an id to a Mermaid-safe node id (`[A-Za-z0-9_]`). Hyphenated
/// document ids (`BUG-008`) become `BUG_008`; the original id is preserved
/// in the node label.
fn mermaid_id(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// `ctxgrd status --format dot` (ADR-108 § GRN-002, ADR-118 § STG-005): the
/// same document graph [`render_mermaid`] draws, as Graphviz DOT *source*
/// (output only — never rendered, never shelling out to `dot`). Fill
/// colours come from [`dot_fill`], so the two formats stay comparable.
pub fn render_dot(report: &Report) -> String {
    let mut out = String::from("digraph documents {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str("  labelloc=t;\n");
    out.push_str(&format!(
        "  label=\"ctxgrd status — {}\";\n",
        census(report)
    ));
    out.push_str("  node [shape=box, style=\"rounded,filled\"];\n");
    for doc in &report.documents {
        out.push_str(&format!(
            "  \"{id}\" [label=\"{id}\\n{status}\", fillcolor=\"{color}\"];\n",
            id = doc.id,
            status = doc_label_status(doc),
            color = dot_fill(doc.state()),
        ));
    }
    for (from, to) in doc_edges(report) {
        out.push_str(&format!("  \"{from}\" -> \"{to}\";\n"));
    }
    out.push_str("}\n");
    out
}

/// Graphviz fill colour for a document state — mirrors the Mermaid
/// `classDef` palette so both formats read the same.
fn dot_fill(state: DocState) -> &'static str {
    match state {
        DocState::Done => "#cde6c5",
        DocState::Current => "#cfe3ff",
        DocState::Blocked => "#f6cccc",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Ast;
    use serde_json::json;

    fn doc(raw_id: &str, depends_on: Vec<&str>) -> Document {
        Document {
            id: raw_id.parse().expect("valid id"),
            raw_id: raw_id.to_owned(),
            location: format!("{}.md", raw_id.to_lowercase()),
            file: None,
            depends_on: depends_on.into_iter().map(String::from).collect(),
            frontmatter_lines: Default::default(),
            metadata: Default::default(),
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    fn doc_with_status(raw_id: &str, status: &str, depends_on: Vec<&str>) -> Document {
        let mut d = doc(raw_id, depends_on);
        d.metadata.insert(
            "status".to_string(),
            serde_json::Value::String(status.to_string()),
        );
        d
    }

    /// The readiness rows for a document set, global (unscoped) view.
    fn readiness(documents: &[Document]) -> Vec<DocReadiness> {
        let graph = dag::DepGraph::new(documents);
        compute_documents(documents, &graph, None)
    }

    fn row(id: &str, status: Option<&str>, ready: bool, blocked_by: &[&str], deps: &[&str]) -> DocReadiness {
        DocReadiness {
            id: id.to_string(),
            namespace: id.split('-').next().unwrap_or(id).to_string(),
            title: String::new(),
            status: status.map(str::to_string),
            ready,
            blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// [`row`] with a title — the `BUG-046` column. Kept separate so the
    /// rows every other test builds stay title-less, which is also the
    /// case that proves an untitled document does not widen its line.
    fn titled_row(id: &str, title: &str, status: Option<&str>, blocked_by: &[&str]) -> DocReadiness {
        DocReadiness {
            title: title.to_string(),
            ..row(id, status, blocked_by.is_empty(), blocked_by, blocked_by)
        }
    }

    // --- ADR-107 § RDY-002: the readiness predicate ---------------------

    #[test]
    fn readiness_is_true_when_every_dependency_is_terminal() {
        let documents = vec![
            doc_with_status("ADR-001", "accepted", vec![]),
            doc_with_status("SPEC-001", "draft", vec!["ADR-001"]),
        ];
        let rows = readiness(&documents);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id, "SPEC-001");
        assert_eq!(rows[1].namespace, "SPEC");
        assert_eq!(rows[1].status, Some("draft".to_string()));
        assert!(rows[1].ready);
        assert_eq!(rows[1].blocked_by, Vec::<String>::new());
    }

    #[test]
    fn readiness_is_false_and_names_the_non_terminal_dependency() {
        let documents = vec![
            doc_with_status("ADR-001", "draft", vec![]),
            doc_with_status("SPEC-001", "draft", vec!["ADR-001"]),
        ];
        let rows = readiness(&documents);
        assert_eq!(rows[1].id, "SPEC-001");
        assert!(!rows[1].ready);
        assert_eq!(rows[1].blocked_by, vec!["ADR-001".to_string()]);
    }

    #[test]
    fn a_settled_document_is_not_ready_and_not_blocked() {
        // RDY-002, and the shape `BUG-036` R1 mistook for a defect: done is
        // `ready: false` with an empty `blocked_by`, and `status` is in the
        // row precisely so a consumer can tell that from stuck.
        let documents = vec![doc_with_status("ADR-001", "accepted", vec![])];
        let rows = readiness(&documents);
        assert!(!rows[0].ready);
        assert_eq!(rows[0].blocked_by, Vec::<String>::new());
        assert_eq!(rows[0].status, Some("accepted".to_string()));
        assert_eq!(rows[0].state(), DocState::Done);
    }

    #[test]
    fn a_document_without_a_status_is_ready_and_reports_none() {
        let documents = vec![doc("ADR-002", vec![])];
        let rows = readiness(&documents);
        assert_eq!(rows[0].status, None);
        assert!(rows[0].ready);
        assert_eq!(rows[0].blocked_by, Vec::<String>::new());
    }

    #[test]
    fn a_dependency_without_a_status_blocks() {
        let documents = vec![
            doc("ADR-002", vec![]),
            doc_with_status("SPEC-002", "draft", vec!["ADR-002"]),
        ];
        let rows = readiness(&documents);
        assert_eq!(rows[1].id, "SPEC-002");
        assert!(!rows[1].ready);
        assert_eq!(rows[1].blocked_by, vec!["ADR-002".to_string()]);
    }

    #[test]
    fn unresolved_dependencies_are_ignored() {
        // A dangling `depends_on` is `core.dep-resolved`'s diagnostic to
        // emit, not a second report of the same defect here.
        let documents = vec![doc_with_status("SPEC-003", "draft", vec!["ADR-404"])];
        let rows = readiness(&documents);
        assert_eq!(rows[0].blocked_by, Vec::<String>::new());
        assert!(rows[0].ready);
    }

    #[test]
    fn terminal_comparison_is_case_insensitive() {
        let documents = vec![
            doc_with_status("ADR-003", "Accepted", vec![]),
            doc_with_status("SPEC-004", "draft", vec!["ADR-003"]),
        ];
        let rows = readiness(&documents);
        assert!(!rows[0].ready);
        assert!(rows[1].ready);
        assert_eq!(rows[1].blocked_by, Vec::<String>::new());
    }

    #[test]
    fn blocked_by_is_id_sorted() {
        let documents = vec![
            doc_with_status("ADR-004", "draft", vec![]),
            doc_with_status("ADR-005", "draft", vec![]),
            doc_with_status("SPEC-005", "draft", vec!["ADR-005", "ADR-004"]),
        ];
        let rows = readiness(&documents);
        assert_eq!(rows[2].id, "SPEC-005");
        assert_eq!(
            rows[2].blocked_by,
            vec!["ADR-004".to_string(), "ADR-005".to_string()]
        );
    }

    // --- ADR-118 § STG-001: the queue covers every namespace ------------

    #[test]
    fn stg001_an_unstaged_namespace_gets_a_row() {
        // The defect half of the pair. Before ADR-118 the queue was filtered
        // to `resolution.dag.order` — the staged namespaces — so BUG-001
        // produced no row at all and an agent asking what was open was told
        // the bug did not exist (`BUG-036` R1).
        let documents = vec![
            doc_with_status("ADR-006", "accepted", vec![]),
            doc_with_status("BUG-001", "open", vec![]),
            doc_with_status("HANDOFF-001", "pending", vec![]),
        ];
        let ids: Vec<String> = readiness(&documents).into_iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![
                "ADR-006".to_string(),
                "BUG-001".to_string(),
                "HANDOFF-001".to_string()
            ]
        );
    }

    #[test]
    fn stg001_an_unstaged_namespace_row_carries_real_readiness() {
        // The other half: presence alone is not the fix. The new rows must
        // carry a computed verdict, not a placeholder — an open BUG with a
        // settled dependency is workable, and one with a draft dependency
        // is not.
        let documents = vec![
            doc_with_status("ADR-007", "accepted", vec![]),
            doc_with_status("ADR-008", "draft", vec![]),
            doc_with_status("BUG-002", "open", vec!["ADR-007"]),
            doc_with_status("BUG-003", "open", vec!["ADR-008"]),
        ];
        let rows = readiness(&documents);
        let by_id = |id: &str| rows.iter().find(|r| r.id == id).expect("row present").clone();
        assert!(by_id("BUG-002").ready);
        assert_eq!(by_id("BUG-002").blocked_by, Vec::<String>::new());
        assert!(!by_id("BUG-003").ready);
        assert_eq!(by_id("BUG-003").blocked_by, vec!["ADR-008".to_string()]);
    }

    #[test]
    fn lineage_scopes_the_rows_but_blocked_by_reads_the_full_graph() {
        // RDY-003: a blocker outside the lineage is still named — omitting
        // it would report a document as unblocked when it is not.
        let documents = vec![
            doc_with_status("ADR-007", "accepted", vec![]),
            doc_with_status("ADR-008", "draft", vec![]),
            doc_with_status("SPEC-006", "draft", vec!["ADR-007", "ADR-008"]),
        ];
        let graph = dag::DepGraph::new(&documents);
        let members: BTreeSet<DocumentId> = ["ADR-007", "SPEC-006"]
            .iter()
            .map(|id| id.parse().expect("valid id"))
            .collect();
        let rows = compute_documents(&documents, &graph, Some(&members));
        let ids: Vec<String> = rows.iter().map(|d| d.id.clone()).collect();
        assert_eq!(ids, vec!["ADR-007".to_string(), "SPEC-006".to_string()]);
        assert!(!rows[1].ready);
        assert_eq!(rows[1].blocked_by, vec!["ADR-008".to_string()]);
    }

    // --- ADR-118 § STG-004: the done-signal ------------------------------

    #[test]
    fn stg004_unblocked_is_true_when_nothing_is_held() {
        // `BUG-036` R2: a feature that is accepted, shipped, and depended on
        // by nothing must be able to certify. Under the stage layer this
        // exited 1 both globally and for its own lineage.
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![
                row("ADR-117", Some("accepted"), false, &[], &[]),
                row("BUG-001", Some("open"), true, &[], &[]),
            ],
        };
        assert!(report.unblocked());
    }

    #[test]
    fn stg004_unblocked_is_false_when_a_document_is_held() {
        // The paired half — the signal must still be able to fail, or it
        // certifies everything and means nothing.
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![
                row("ADR-008", Some("draft"), true, &[], &[]),
                row("SPEC-006", Some("draft"), false, &["ADR-008"], &["ADR-008"]),
            ],
        };
        assert!(!report.unblocked());
    }

    // --- ADR-118 § STG-003: the JSON contract ----------------------------

    fn queue_report() -> Report {
        Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![
                row("ADR-001", Some("accepted"), false, &[], &[]),
                row("SPEC-001", None, false, &["ADR-002"], &["ADR-002"]),
            ],
        }
    }

    #[test]
    fn stg003_json_is_the_work_queue_and_nothing_else() {
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&queue_report(), true)).unwrap();
        assert_eq!(
            parsed,
            // `title: ""` — these rows are built title-less, and an absent
            // title is an empty string on the wire, never `null`
            // (`BUG-046`; `null` is reserved for `status`).
            json!({
                "documents": [
                    {"id": "ADR-001", "namespace": "ADR", "title": "", "status": "accepted",
                     "ready": false, "blocked_by": []},
                    {"id": "SPEC-001", "namespace": "SPEC", "title": "", "status": null,
                     "ready": false, "blocked_by": ["ADR-002"]}
                ]
            })
        );
    }

    #[test]
    fn stg003_the_removed_stage_fields_are_absent() {
        // Named individually rather than asserted as "only `documents`
        // remains", so a future field that reintroduces one of them by name
        // fails here and not merely in a whole-object comparison somebody
        // might update by hand.
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&queue_report(), true)).unwrap();
        for gone in [
            "stages",
            "edges",
            "frontier",
            "blockers",
            "blocker_stages",
            "next_action",
            "source",
            "source_hint",
        ] {
            assert!(parsed.get(gone).is_none(), "`{gone}` survived STG-003");
        }
    }

    #[test]
    fn lineage_and_shared_appear_only_when_scoped() {
        let global: serde_json::Value =
            serde_json::from_str(&render_json(&queue_report(), true)).unwrap();
        assert!(global.get("lineage").is_none());
        assert!(global.get("shared").is_none());

        let mut scoped = queue_report();
        scoped.lineage = Some("ADR-001".to_string());
        scoped.shared = vec!["ADR-042".to_string()];
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&scoped, true)).unwrap();
        assert_eq!(parsed["lineage"], json!("ADR-001"));
        assert_eq!(parsed["shared"], json!(["ADR-042"]));
    }

    // --- ADR-118 § STG-005: the document graph is the only graph --------

    fn diagram_report(adr_status: &str) -> Report {
        let settled = adr_status == "accepted";
        Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![
                row("ADR-001", Some(adr_status), !settled, &[], &[]),
                row(
                    "SPEC-001",
                    Some("draft"),
                    settled,
                    if settled { &[] } else { &["ADR-001"] },
                    &["ADR-001"],
                ),
            ],
        }
    }

    #[test]
    fn render_mermaid_draws_one_node_per_document_and_one_edge_per_dep() {
        let expected = "flowchart LR\n\
             \x20 %% ctxgrd status · 2 documents · 1 ready · 0 blocked\n\
             \x20 ADR_001[\"ADR-001: accepted\"]:::done\n\
             \x20 SPEC_001[\"SPEC-001: draft\"]:::current\n\
             \x20 ADR_001 --> SPEC_001\n\
             \x20 classDef done fill:#cde6c5,stroke:#33aa77;\n\
             \x20 classDef current fill:#cfe3ff,stroke:#3377aa;\n\
             \x20 classDef blocked fill:#f6cccc,stroke:#cc3333;\n";
        assert_eq!(render_mermaid(&diagram_report("accepted")), expected);
    }

    #[test]
    fn render_mermaid_blocks_a_dependent_when_its_dependency_is_unsettled() {
        let out = render_mermaid(&diagram_report("draft"));
        assert!(out.contains("  ADR_001[\"ADR-001: draft\"]:::current\n"), "out:\n{out}");
        assert!(
            out.contains("  SPEC_001[\"SPEC-001: draft\"]:::blocked\n"),
            "out:\n{out}"
        );
        assert!(out.contains("  ADR_001 --> SPEC_001\n"), "out:\n{out}");
    }

    #[test]
    fn render_dot_draws_the_same_graph_with_the_shared_palette() {
        let expected = "digraph documents {\n\
             \x20 rankdir=LR;\n\
             \x20 labelloc=t;\n\
             \x20 label=\"ctxgrd status — 2 documents · 1 ready · 0 blocked\";\n\
             \x20 node [shape=box, style=\"rounded,filled\"];\n\
             \x20 \"ADR-001\" [label=\"ADR-001\\naccepted\", fillcolor=\"#cde6c5\"];\n\
             \x20 \"SPEC-001\" [label=\"SPEC-001\\ndraft\", fillcolor=\"#cfe3ff\"];\n\
             \x20 \"ADR-001\" -> \"SPEC-001\";\n\
             }\n";
        assert_eq!(render_dot(&diagram_report("accepted")), expected);
    }

    #[test]
    fn render_dot_fills_a_blocked_dependent_with_the_blocked_colour() {
        let out = render_dot(&diagram_report("draft"));
        assert!(
            out.contains("\"SPEC-001\" [label=\"SPEC-001\\ndraft\", fillcolor=\"#f6cccc\"];"),
            "out:\n{out}"
        );
    }

    #[test]
    fn an_absent_status_is_labelled_none() {
        let mut report = diagram_report("accepted");
        report.documents[1].status = None;
        let out = render_mermaid(&report);
        assert!(out.contains("  SPEC_001[\"SPEC-001: none\"]:::current\n"), "out:\n{out}");
    }

    #[test]
    fn edges_to_uncounted_documents_are_dropped() {
        // One edge per resolved dep *between counted documents*: a
        // `--lineage` scope that excludes ADR-001 leaves SPEC-001 alone.
        let mut report = diagram_report("accepted");
        report.documents.remove(0);
        let out = render_mermaid(&report);
        assert!(out.contains("  SPEC_001[\"SPEC-001: draft\"]:::current\n"), "out:\n{out}");
        assert!(!out.contains("-->"), "out:\n{out}");
    }

    #[test]
    fn stg005_a_bug_is_an_ordinary_node_with_no_dashed_overlay() {
        // GRN-004 held that the dashed `blocks` overlay was namespace-only.
        // With the namespace view gone the overlay has no view to live in,
        // and a BUG draws exactly like any other document.
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![row("BUG-008", Some("open"), true, &[], &[])],
        };
        let out = render_mermaid(&report);
        assert!(out.contains("  BUG_008[\"BUG-008: open\"]:::current\n"), "out:\n{out}");
        assert!(!out.contains("blocks"), "out:\n{out}");
        assert!(!render_dot(&report).contains("style=dashed"));
    }

    // --- the text report -------------------------------------------------

    #[test]
    fn render_report_leads_with_a_census_then_lists_both_queues() {
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![
                row("ADR-001", Some("accepted"), false, &[], &[]),
                row("BUG-003", Some("open"), true, &[], &[]),
                row("SPEC-006", Some("draft"), false, &["ADR-008"], &["ADR-008"]),
            ],
        };
        insta::assert_snapshot!(render_report(&report, true), @r"
        3 documents · 1 ready · 1 blocked · 1 settled

        ready:
          BUG-003  open

        blocked:
          SPEC-006  draft  ← ADR-008

        tip: --format json for agents, mermaid/dot for diagrams
        ");
    }

    #[test]
    fn render_report_says_so_when_nothing_is_open() {
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![row("ADR-001", Some("accepted"), false, &[], &[])],
        };
        let out = render_report(&report, true);
        assert!(out.contains("1 documents · 0 ready · 0 blocked · 1 settled"), "out:\n{out}");
        assert!(out.contains("nothing open."), "out:\n{out}");
    }

    #[test]
    fn render_report_names_the_lineage_and_its_shared_roots() {
        let report = Report {
            lineage: Some("ADR-117".to_string()),
            shared: vec!["ADR-115".to_string()],
            documents: vec![row("ADR-117", Some("accepted"), false, &[], &[])],
        };
        let out = render_report(&report, true);
        assert!(out.contains("lineage: ADR-117"), "out:\n{out}");
        assert!(out.contains("shared with: ADR-115"), "out:\n{out}");
    }

    // --- BUG-046: the row names its document -----------------------------

    /// Two workable rows and one held one, all titled. `RUN-001` is the
    /// document the bug was filed over: twelve `status` invocations across
    /// one session read it as queue noise while its title was the answer
    /// being searched for.
    fn titled_report() -> Report {
        Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![
                titled_row("BUG-003", "Frontmatter parse drops a key", Some("open"), &[]),
                titled_row(
                    "RUN-001",
                    "Publish a release to the public mirror",
                    Some("active"),
                    &[],
                ),
                titled_row("SPEC-006", "Ledger reconciliation", Some("draft"), &["ADR-008"]),
            ],
        }
    }

    #[test]
    fn bug046_the_text_report_names_each_row_and_still_reports_readiness() {
        // Asserted as a pair (`ADR-112` § CLR-007). A change that printed
        // the title *instead of* the status would satisfy "titles appear",
        // so the columns it must not displace are pinned in the same
        // snapshot: status on both rows, `←` and its blockers on the
        // blocked one.
        insta::assert_snapshot!(render_report(&titled_report(), true), @r"
        3 documents · 2 ready · 1 blocked · 0 settled

        ready:
          BUG-003  open    Frontmatter parse drops a key
          RUN-001  active  Publish a release to the public mirror

        blocked:
          SPEC-006  draft  Ledger reconciliation  ← ADR-008

        tip: --format json for agents, mermaid/dot for diagrams
        ");
    }

    #[test]
    fn bug046_no_titles_reproduces_the_previous_report_byte_for_byte() {
        // The other half of the pair, and `ADR-074`'s lever: the opt-out
        // must be the old output exactly, not a near-miss with stray
        // padding where the column used to be.
        let expected = "3 documents · 2 ready · 1 blocked · 0 settled\n\
                        \nready:\n\
                        \x20 BUG-003  open\n\
                        \x20 RUN-001  active\n\
                        \nblocked:\n\
                        \x20 SPEC-006  draft  ← ADR-008\n\
                        \ntip: --format json for agents, mermaid/dot for diagrams\n";
        assert_eq!(render_report(&titled_report(), false), expected);
    }

    #[test]
    fn bug046_an_untitled_row_does_not_carry_the_column_as_whitespace() {
        // The ready bucket's title is the last column, so a document that
        // declares no title must end at its status — trailing spaces would
        // be an invisible cost on every injected report.
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![
                titled_row("BUG-003", "Frontmatter parse drops a key", Some("open"), &[]),
                titled_row("BUG-004", "", Some("open"), &[]),
            ],
        };
        let out = render_report(&report, true);
        assert!(out.contains("  BUG-004  open\n"), "out:\n{out}");
    }

    #[test]
    fn bug046_a_long_title_is_fitted_and_says_it_was_cut() {
        // This repo writes titles as full sentences; the longest is 202
        // characters. Untruncated, one row would dominate the report.
        let long = "a".repeat(80);
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![titled_row("BUG-003", &long, Some("open"), &[])],
        };
        let out = render_report(&report, true);
        let fitted = format!("{}…", "a".repeat(TITLE_WIDTH - 1));
        assert!(out.contains(&fitted), "out:\n{out}");
        assert!(!out.contains(&long), "the whole title survived:\n{out}");
    }

    #[test]
    fn bug046_fit_title_cuts_on_a_character_boundary() {
        // `chars`, not bytes: a multi-byte title must be shortened, not
        // panic. Slicing `&title[..59]` here would be a crash on a corpus
        // that is not ASCII.
        let title = "é".repeat(80);
        let fitted = fit_title(&title);
        assert_eq!(fitted.chars().count(), TITLE_WIDTH);
        assert!(fitted.ends_with('…'), "fitted: {fitted}");
    }

    #[test]
    fn bug046_a_title_at_the_limit_is_left_whole() {
        let exact = "b".repeat(TITLE_WIDTH);
        assert_eq!(fit_title(&exact), exact);
    }

    #[test]
    fn bug046_json_carries_the_title_untruncated() {
        // A machine format has no line to overflow, and a consumer that
        // asked for structure has already paid for it — so the text
        // report's bound must not follow the title onto the wire.
        let long = "a".repeat(80);
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![titled_row("BUG-003", &long, Some("open"), &[])],
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&render_json(&report, true)).unwrap();
        assert_eq!(parsed["documents"][0]["title"], json!(long));
        // The paired half: adding the field displaced nothing.
        assert_eq!(parsed["documents"][0]["status"], json!("open"));
        assert_eq!(parsed["documents"][0]["ready"], json!(true));
    }

    #[test]
    fn bug046_json_omits_the_title_field_under_no_titles() {
        let parsed: serde_json::Value =
            serde_json::from_str(&render_json(&titled_report(), false)).unwrap();
        assert!(
            parsed["documents"][0].get("title").is_none(),
            "parsed:\n{parsed:#}"
        );
        assert_eq!(parsed["documents"][0]["id"], json!("BUG-003"));
    }

    #[test]
    fn bug046_a_titleless_document_serializes_an_empty_string_not_null() {
        // `null` would collide with "suppressed"; the empty string is what
        // `ctxgrd list --format json` publishes for the same document.
        let report = Report {
            lineage: None,
            shared: Vec::new(),
            documents: vec![titled_row("BUG-004", "", Some("open"), &[])],
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&render_json(&report, true)).unwrap();
        assert_eq!(parsed["documents"][0]["title"], json!(""));
    }

    #[test]
    fn bug046_the_projection_reads_the_title_from_frontmatter() {
        // End to end through `compute_documents`, so the row is fed by the
        // same ingest the report runs on rather than by a test constructor.
        let mut d = doc_with_status("RUN-001", "active", vec![]);
        d.metadata.insert(
            "title".to_string(),
            serde_json::Value::String("Publish a release to the public mirror".to_string()),
        );
        let rows = readiness(&[d]);
        assert_eq!(rows[0].title, "Publish a release to the public mirror");
    }

    #[test]
    fn bug046_the_title_falls_back_to_name_as_list_does() {
        // Shared with `list::title_of`, so the `name:` convention some
        // packs use is not a blank column in one command and a title in
        // the other.
        let mut d = doc_with_status("GUIDE-001", "active", vec![]);
        d.metadata.insert(
            "name".to_string(),
            serde_json::Value::String("Getting started".to_string()),
        );
        let rows = readiness(&[d]);
        assert_eq!(rows[0].title, "Getting started");
    }
}
