//! `ctxgrd status` — pipeline position resolution (SPEC-002).
//!
//! Sprint 1 scope: resolve the namespace DAG (declared > inferred >
//! default ladder, EARS-01.1/01.2/01.3), name the DAG source in every
//! output (EARS-01.4), and fail loudly on namespace cycles
//! (EARS-01.5). Gate evaluation (EARS-02.*), the BUG tripwire
//! (EARS-03.*), full ladder/JSON rendering (EARS-04.*) and the
//! `pipeline.conformance` rule (EARS-06.*) land with Sprints 2–4.

use std::path::Path;

use thiserror::Error;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, GatePredicate, GateQuantifier};
use crate::dag::{self, NamespaceDag};
use crate::diagnostic::Diagnostic;
use crate::document::Document;
use crate::id::DocumentId;
use crate::run::{self, LintError, LintRun};

/// Where the resolved DAG came from (EARS-01.4). Named in every
/// output — a built-in ladder is never passed off as derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DagSource {
    Declared,
    Inferred,
    Default,
}

impl DagSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Inferred => "inferred",
            Self::Default => "default",
        }
    }
}

/// The resolved namespace DAG plus its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub(crate) dag: NamespaceDag,
    pub source: DagSource,
}

/// What can go wrong answering "where is the pipeline?".
#[derive(Debug, Error)]
pub enum StatusError {
    /// Config / ingest failures — same channel as `lint`.
    #[error(transparent)]
    Lint(#[from] LintError),
    /// EARS-01.5: the lifted namespace graph contains a cycle.
    #[error("namespace dependency cycle between {}", .members.join(" ↔ "))]
    Cycle { members: Vec<String> },
}

/// Built-in default ladder (EARS-01.3), applied in this order and
/// restricted to namespaces active in the configuration.
const DEFAULT_LADDER: [&str; 4] = ["PRD", "ADR", "SPEC", "TASK"];

/// Resolve the namespace DAG for `root`: load config + documents
/// through the shared ingest pipeline (the same document set `lint`
/// sees), then apply the declared > inferred > default ladder
/// (SPEC-002 § Workflows step 1).
pub fn resolve(root: &Path) -> Result<Resolution, StatusError> {
    let run::IngestResult {
        config, documents, ..
    } = run::ingest(root)?;
    resolve_dag(&config, &documents)
}

fn resolve_dag(config: &Config, documents: &[Document]) -> Result<Resolution, StatusError> {
    // EARS-01.1: a declared [pipeline] is used verbatim.
    if let Some(pipeline) = &config.pipeline {
        return Ok(Resolution {
            dag: dag::chain_dag(&pipeline.stages),
            source: DagSource::Declared,
        });
    }

    // EARS-01.2: infer by lifting the resolved dep-edge set.
    let inferred = dag::infer_namespace_dag(documents).map_err(|cycle| StatusError::Cycle {
        members: cycle.members,
    })?;
    if !inferred.edges.is_empty() {
        return Ok(Resolution {
            dag: inferred,
            source: DagSource::Inferred,
        });
    }

    // EARS-01.3: cold start — the built-in ladder restricted to
    // active namespaces, honestly labeled `default` (EARS-01.4).
    let stages: Vec<String> = DEFAULT_LADDER
        .iter()
        .filter(|ns| config.namespaces.contains_key(**ns))
        .map(|ns| ns.to_string())
        .collect();
    Ok(Resolution {
        dag: dag::chain_dag(&stages),
        source: DagSource::Default,
    })
}

/// The gate predicate in force for `namespace` (EARS-02.4): the
/// explicit `[pipeline.gate]` entry if present, else the default —
/// `all:done` for TASK (the only built-in namespace whose terminal
/// status is `done`), `any:accepted` for everything else. `gates` is
/// empty whenever no `[pipeline]` is declared, so inferred/default
/// DAGs get all-default gates.
fn effective_gate(namespace: &str, gates: &BTreeMap<String, GatePredicate>) -> GatePredicate {
    if let Some(explicit) = gates.get(namespace) {
        return explicit.clone();
    }
    if namespace == "TASK" {
        GatePredicate {
            quantifier: GateQuantifier::All,
            status: "done".to_string(),
        }
    } else {
        GatePredicate {
            quantifier: GateQuantifier::Any,
            status: "accepted".to_string(),
        }
    }
}

/// One document's contribution to its namespace's gate (T2.2). Built
/// from the same documents and rule diagnostics `lint` produces, so
/// gate evaluation reuses the check pass rather than re-running it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocView {
    /// Author's exact id string, for naming a held document.
    id: String,
    /// Frontmatter `status`, if present.
    status: Option<String>,
    /// True when no rule diagnostic is anchored at this document.
    clean: bool,
}

/// Outcome of evaluating one stage's gate over its documents.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateOutcome {
    /// EARS-02.1: predicate satisfied AND every counted document clean.
    done: bool,
    /// EARS-02.2: documents carrying the gate's terminal status that
    /// nonetheless produced diagnostics. Name-sorted.
    held: Vec<String>,
}

/// Evaluate a gate predicate over a namespace's documents
/// (EARS-02.1/02.2/02.5).
///
/// `any:S` is satisfied by at least one document with status `S`, and
/// done when every such document is lint-clean; a dirty `S` document
/// holds the stage. `all:S` requires a non-empty namespace
/// (EARS-02.5) in which every document has status `S` and is clean.
/// Documents not carrying the gate's status are never "held" — they
/// are simply not finished yet.
fn evaluate_gate(predicate: &GatePredicate, docs: &[DocView]) -> GateOutcome {
    let matches_status = |d: &DocView| d.status.as_deref() == Some(predicate.status.as_str());
    let mut held: Vec<String> = docs
        .iter()
        .filter(|d| matches_status(d) && !d.clean)
        .map(|d| d.id.clone())
        .collect();
    held.sort();

    let done = match predicate.quantifier {
        GateQuantifier::Any => {
            let matching = docs.iter().filter(|d| matches_status(d));
            let mut any = false;
            let mut all_clean = true;
            for d in matching {
                any = true;
                all_clean &= d.clean;
            }
            any && all_clean
        }
        GateQuantifier::All => {
            // EARS-02.5: an `all:` gate over an empty namespace is
            // unsatisfied, never vacuously done.
            !docs.is_empty() && docs.iter().all(|d| matches_status(d) && d.clean)
        }
    };

    GateOutcome { done, held }
}

/// A resolved pipeline stage with its computed verdict (SPEC-002 §
/// Stage states). `Blocked` is the BUG tripwire (EARS-03.1): an open
/// BUG citing a document in the stage's namespace overrides the gate
/// verdict, so a `Done` or `Current` stage can flip to `Blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    Done,
    Current,
    Pending,
    Blocked,
}

impl StageState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Current => "current",
            Self::Pending => "pending",
            Self::Blocked => "blocked",
        }
    }
}

/// One stage in the resolved ladder: its namespace, computed state, the
/// documents it counts, and a representative verdict (SPEC-002 § Data
/// model — the JSON stage shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub namespace: String,
    pub state: StageState,
    /// Document ids in this namespace, name-sorted. JSON `docs`.
    pub docs: Vec<String>,
    /// Representative status token (JSON `verdict`): `empty`, the gate's
    /// terminal status when satisfied, `held`, or the status of the next
    /// document to advance. A fixed vocabulary — never body text.
    pub verdict: String,
    /// This stage's own gate result (ADR-037 § WIRE-002): predicate
    /// satisfied AND every counted document clean, independent of
    /// parent-stage state (EARS-02.6) and the BUG tripwire. JSON
    /// `gate_met` — the field to read beside `verdict`, which can render
    /// a non-terminal status when the gate is met but a parent is not.
    pub gate_met: bool,
    /// Documents carrying the gate's terminal status that nonetheless
    /// produced diagnostics (EARS-02.2). Drives the "held by" text and
    /// the JSON `hold` array.
    pub held: Vec<String>,
    /// The document `next_action` targets for this stage, if any.
    /// Internal — feeds the fixed next-action template, not serialized.
    actionable: Option<String>,
}

/// The full `ctxgrd status` answer: the resolved DAG, its source, the
/// per-stage verdicts, and the BUG tripwire (EARS-03.*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub source: DagSource,
    pub(crate) dag: NamespaceDag,
    pub stages: Vec<Stage>,
    /// The pipeline's actionable position: the first blocked stage if
    /// any (a blocker takes precedence), else the first not-done stage,
    /// else `None` when the pipeline is complete.
    pub current: Option<String>,
    /// `open` BUG documents citing a document in the active lineage
    /// (EARS-03.1). Name-sorted; empty when the tripwire is clear.
    pub blockers: Vec<String>,
    /// Per-blocker attribution (ADR-037 § WIRE-004): each id in
    /// `blockers` mapped to the name-sorted namespaces it blocks. Empty
    /// keys match `blockers`; JSON `blocker_stages`.
    pub blocker_stages: BTreeMap<String, Vec<String>>,
    /// The single next action, from a fixed template keyed by (state,
    /// verdict) — never document body text (EARS-04.3/04.4).
    pub next_action: String,
}

/// Resolve the DAG, run the shared lint pass, compute per-stage
/// verdicts, then sweep the BUG tripwire (SPEC-002 § Workflows steps
/// 1–4).
pub fn report(root: &Path) -> Result<Report, StatusError> {
    let LintRun {
        outcome,
        config,
        documents,
    } = run::lint_run(root)?;
    let resolution = resolve_dag(&config, &documents)?;
    let mut stages = compute_stages(&resolution.dag, &config, &documents, &outcome.diagnostics);

    // Step 4 — tripwire sweep (EARS-03.1): an `open` BUG citing a
    // document in the active lineage blocks the stage(s) it cites,
    // overriding the gate verdict. Cleared automatically once no
    // citing BUG is `open` (EARS-03.2), since the sweep only counts
    // `open` BUGs.
    let tripwire = sweep_tripwire(&resolution.dag, &documents);
    for stage in &mut stages {
        if tripwire.blocked_namespaces.contains(&stage.namespace) {
            stage.state = StageState::Blocked;
        }
    }

    // Position: a blocked stage is the pipeline's actionable focus
    // (resolve the blocker first), else the first not-done stage.
    let current = stages
        .iter()
        .find(|s| s.state == StageState::Blocked)
        .or_else(|| stages.iter().find(|s| s.state == StageState::Current))
        .map(|s| s.namespace.clone());

    let next_action = next_action(&stages, current.as_deref(), &tripwire.blockers, &config);

    Ok(Report {
        source: resolution.source,
        dag: resolution.dag,
        stages,
        current,
        blockers: tripwire.blockers,
        blocker_stages: tripwire.blocker_stages,
        next_action,
    })
}

/// The single next action, from a FIXED template keyed by (state,
/// verdict) — never document body text (EARS-04.3/04.4). A live blocker
/// dominates; otherwise the verb comes from the current stage's gate
/// terminal status and the object is the stage's actionable document.
fn next_action(
    stages: &[Stage],
    current: Option<&str>,
    blockers: &[String],
    config: &Config,
) -> String {
    if let Some(bug) = blockers.first() {
        return format!("resolve {bug}");
    }
    let Some(current_ns) = current else {
        return "pipeline complete".to_string();
    };
    let Some(stage) = stages.iter().find(|s| s.namespace == current_ns) else {
        return "pipeline complete".to_string();
    };
    let empty = BTreeMap::new();
    let gates = config.pipeline.as_ref().map(|p| &p.gates).unwrap_or(&empty);
    let gate = effective_gate(current_ns, gates);
    match stage.verdict.as_str() {
        "empty" => format!("create the first {current_ns} document"),
        "held" => match &stage.actionable {
            Some(id) => format!("fix {id}"),
            None => format!("fix the held {current_ns} document"),
        },
        _ => {
            let verb = advance_verb(&gate.status);
            match &stage.actionable {
                Some(id) => format!("{verb} {id}"),
                None => format!("{verb} the {current_ns} document"),
            }
        }
    }
}

/// The verb that moves a document to a gate's terminal status. TASK's
/// terminal status is `done` ("complete"); everything else advances by
/// acceptance.
fn advance_verb(terminal_status: &str) -> &'static str {
    match terminal_status {
        "done" => "complete",
        _ => "accept",
    }
}

/// Outcome of the BUG tripwire sweep (EARS-03.1/03.2).
struct Tripwire {
    /// `open` BUG ids citing the active lineage. Name-sorted, deduped.
    blockers: Vec<String>,
    /// Namespaces (within the DAG) that have a document cited by an
    /// `open` BUG — each such stage is overridden to `Blocked`.
    blocked_namespaces: BTreeSet<String>,
    /// Per-blocker attribution (ADR-037 § WIRE-004): each blocking BUG id
    /// mapped to the name-sorted lineage namespaces it cites. The union
    /// of the values equals `blocked_namespaces`; the map keeps which BUG
    /// holds which stage, which the flat sets discard.
    blocker_stages: BTreeMap<String, Vec<String>>,
}

/// Reverse-edge query (SPEC-002 § Workflows step 4): find every `open`
/// BUG document citing a document in the active lineage — the set of
/// documents whose namespace appears in the resolved DAG. A BUG cites a
/// document through its `depends_on` edges or a non-suppressed body
/// cross-ref token (the same two pointer kinds [`run::find_references`]
/// counts), reusing the already-ingested document set rather than
/// re-walking the tree.
fn sweep_tripwire(dag: &NamespaceDag, documents: &[Document]) -> Tripwire {
    let lineage_ns: BTreeSet<&str> = dag.order.iter().map(String::as_str).collect();
    let lineage: BTreeSet<DocumentId> = documents
        .iter()
        .filter(|d| lineage_ns.contains(d.id.namespace.as_str()))
        .map(|d| d.id.clone())
        .collect();

    let mut blockers: Vec<String> = Vec::new();
    let mut blocked_namespaces: BTreeSet<String> = BTreeSet::new();
    let mut blocker_stages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for bug in documents.iter().filter(|d| d.id.namespace == "BUG") {
        let open = bug
            .metadata
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == "open");
        if !open {
            continue;
        }
        let cited = cited_lineage_namespaces(bug, &lineage);
        if cited.is_empty() {
            continue;
        }
        blockers.push(bug.raw_id.clone());
        // Record this BUG's own cited namespaces before draining `cited`
        // into the flat set (BTreeSet → already name-sorted).
        blocker_stages.insert(bug.raw_id.clone(), cited.iter().cloned().collect());
        blocked_namespaces.extend(cited);
    }
    blockers.sort();
    blockers.dedup();
    Tripwire {
        blockers,
        blocked_namespaces,
        blocker_stages,
    }
}

/// The lineage namespaces `bug` cites, via `depends_on` entries or
/// non-suppressed body cross-ref tokens. Empty when the BUG cites
/// nothing in the active lineage.
fn cited_lineage_namespaces(bug: &Document, lineage: &BTreeSet<DocumentId>) -> BTreeSet<String> {
    let mut cited: BTreeSet<String> = BTreeSet::new();
    for entry in &bug.depends_on {
        if let Ok(id) = entry.parse::<DocumentId>() {
            if lineage.contains(&id) {
                cited.insert(id.namespace);
            }
        }
    }
    if let Some(ast) = bug.ast.as_ref() {
        for tok in &ast.cross_ref_tokens {
            if tok.in_code || tok.in_strikethrough {
                continue;
            }
            let id = DocumentId::new(tok.namespace.as_str(), tok.number);
            if lineage.contains(&id) {
                cited.insert(id.namespace);
            }
        }
    }
    cited
}

/// Assign each stage a verdict (SPEC-002 § Stage states). A stage is
/// `done` when its own gate is satisfied-and-clean AND every parent is
/// done (EARS-02.6 — a join gate waits on all parents); the first
/// not-done stage in DAG order is `current`, the rest `pending`.
///
/// `dag.order` is already topologically sorted (name-order tie-break,
/// EARS-01.6), so every parent's `done` verdict is settled before the
/// stage that depends on it is processed.
fn compute_stages(
    dag: &NamespaceDag,
    config: &Config,
    documents: &[Document],
    diagnostics: &[Diagnostic],
) -> Vec<Stage> {
    let gates = config
        .pipeline
        .as_ref()
        .map(|p| p.gates.clone())
        .unwrap_or_default();
    let dirty: BTreeSet<&str> = diagnostics.iter().map(|d| d.location.as_str()).collect();

    let position: BTreeMap<&str, usize> = dag
        .order
        .iter()
        .enumerate()
        .map(|(i, ns)| (ns.as_str(), i))
        .collect();

    /// Per-stage data accumulated in DAG order before the single
    /// current stage is known.
    struct Acc {
        held: Vec<String>,
        docs: Vec<String>,
        verdict: String,
        gate_met: bool,
        actionable: Option<String>,
    }

    let mut done = vec![false; dag.order.len()];
    let mut acc: Vec<Acc> = Vec::with_capacity(dag.order.len());
    for (i, namespace) in dag.order.iter().enumerate() {
        let mut views: Vec<DocView> = documents
            .iter()
            .filter(|d| d.id.namespace == *namespace)
            .map(|d| DocView {
                id: d.raw_id.clone(),
                status: d
                    .metadata
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                clean: !dirty.contains(d.location.as_str()),
            })
            .collect();
        views.sort_by(|a, b| a.id.cmp(&b.id));
        let gate = effective_gate(namespace, &gates);
        let outcome = evaluate_gate(&gate, &views);
        // EARS-02.6: a stage cannot be done while any parent is not.
        let parents_done = dag
            .edges
            .iter()
            .filter(|(_, to)| to == namespace)
            .all(|(from, _)| done[position[from.as_str()]]);
        let stage_done = outcome.done && parents_done;
        done[i] = stage_done;

        let docs: Vec<String> = views.iter().map(|v| v.id.clone()).collect();
        let (verdict, actionable) = summarize_stage(&gate, &views, stage_done, &outcome);
        acc.push(Acc {
            held: outcome.held,
            docs,
            verdict,
            // The stage's own gate, before parent-gating (EARS-02.6) and
            // the tripwire override (ADR-037 § WIRE-002).
            gate_met: outcome.done,
            actionable,
        });
    }

    let current_idx = done.iter().position(|d| !d);
    acc.into_iter()
        .enumerate()
        .map(|(i, a)| {
            let state = if done[i] {
                StageState::Done
            } else if Some(i) == current_idx {
                StageState::Current
            } else {
                StageState::Pending
            };
            Stage {
                namespace: dag.order[i].clone(),
                state,
                docs: a.docs,
                verdict: a.verdict,
                gate_met: a.gate_met,
                held: a.held,
                actionable: a.actionable,
            }
        })
        .collect()
}

/// The stage's representative verdict token and the document
/// `next_action` should target. Verdict tokens are a fixed vocabulary
/// (`empty`, `held`, a terminal status, or the next document's status)
/// — never document body text (EARS-04.4). `views` must be name-sorted
/// so `first`/`find` pick deterministically.
fn summarize_stage(
    gate: &GatePredicate,
    views: &[DocView],
    stage_done: bool,
    outcome: &GateOutcome,
) -> (String, Option<String>) {
    if views.is_empty() {
        return ("empty".to_string(), None);
    }
    if stage_done {
        // Gate satisfied and clean — the terminal status was reached.
        return (gate.status.clone(), None);
    }
    // A terminal-but-dirty document holds the stage (EARS-02.2).
    if let Some(first_held) = outcome.held.first() {
        return ("held".to_string(), Some(first_held.clone()));
    }
    // Otherwise the first name-sorted document short of the terminal
    // status is the one to advance; its status is the verdict.
    if let Some(next) = views
        .iter()
        .find(|d| d.status.as_deref() != Some(gate.status.as_str()))
    {
        let verdict = next
            .status
            .clone()
            .unwrap_or_else(|| "no-status".to_string());
        return (verdict, Some(next.id.clone()));
    }
    // Locally satisfied but not done — waiting on a parent (EARS-02.6).
    (gate.status.clone(), None)
}

/// Render the resolved DAG: the source token first (EARS-01.4), then
/// the shape — one arrow line for chains, one edge per line for
/// branching shapes.
fn render_dag(source: DagSource, dag: &NamespaceDag) -> String {
    let mut out = format!("source: {}\n", source.as_str());
    if dag.order.is_empty() {
        out.push_str("(no stages)\n");
        return out;
    }
    if is_chain(dag) {
        out.push_str(&dag.order.join(" → "));
        out.push('\n');
    } else {
        for (from, to) in &dag.edges {
            out.push_str(from);
            out.push_str(" → ");
            out.push_str(to);
            out.push('\n');
        }
    }
    out
}

/// Sprint-1 DAG-only text output. Retained as a building block; the
/// CLI now renders the full [`Report`] via [`render_report`].
pub fn render_text(resolution: &Resolution) -> String {
    render_dag(resolution.source, &resolution.dag)
}

/// Full `ctxgrd status` text ladder (EARS-04.1): the resolved DAG, a
/// per-stage line carrying the state and verdict (and any held
/// documents, EARS-02.2), then a footer naming the current position,
/// any blockers, and the next action. No document body content reaches
/// the output (EARS-04.4) — verdicts and the next action are fixed
/// tokens, ids are author-chosen frontmatter, never prose.
pub fn render_report(report: &Report) -> String {
    let mut out = render_dag(report.source, &report.dag);
    if !report.stages.is_empty() {
        out.push('\n');
    }
    for stage in &report.stages {
        out.push_str(&stage.namespace);
        out.push_str(": ");
        out.push_str(stage.state.as_str());
        out.push_str(" (");
        out.push_str(&stage.verdict);
        out.push(')');
        if !stage.held.is_empty() {
            out.push_str(" — held by ");
            out.push_str(&stage.held.join(", "));
        }
        out.push('\n');
    }
    if !report.stages.is_empty() {
        out.push('\n');
    }
    if let Some(current) = &report.current {
        out.push_str("current: ");
        out.push_str(current);
        out.push('\n');
    }
    if !report.blockers.is_empty() {
        out.push_str("blocked by: ");
        out.push_str(&report.blockers.join(", "));
        out.push('\n');
    }
    out.push_str("next: ");
    out.push_str(&report.next_action);
    out.push('\n');
    // Point tools and agents at the structured projection — the text
    // ladder is the human view; `--format json` is the machine contract.
    out.push('\n');
    out.push_str("tip: run with --format json for machine-readable output\n");
    out
}

/// `ctxgrd status --format json` (EARS-04.2): a single JSON object
/// conforming to SPEC-002 § Data model — `source`, `edges`, `stages`
/// (`namespace`/`state`/`docs`/`verdict`/`gate_met`/`hold`), `current`,
/// `blockers`, `blocker_stages`, `next_action`. The wire shape is a
/// dedicated struct so the JSON contract is pinned independently of the
/// in-memory [`Report`]: the internal `dag` field is projected to
/// `edges`, `held` to `hold`, and `actionable` stays out entirely
/// (ADR-037 § WIRE-005).
pub fn render_json(report: &Report) -> String {
    #[derive(serde::Serialize)]
    struct WireEdge<'a> {
        from: &'a str,
        to: &'a str,
    }
    #[derive(serde::Serialize)]
    struct WireStage<'a> {
        namespace: &'a str,
        state: &'a str,
        docs: &'a [String],
        verdict: &'a str,
        gate_met: bool,
        hold: &'a [String],
    }
    #[derive(serde::Serialize)]
    struct Wire<'a> {
        source: &'a str,
        edges: Vec<WireEdge<'a>>,
        stages: Vec<WireStage<'a>>,
        current: Option<&'a str>,
        blockers: &'a [String],
        blocker_stages: &'a BTreeMap<String, Vec<String>>,
        next_action: &'a str,
    }
    let wire = Wire {
        source: report.source.as_str(),
        edges: report
            .dag
            .edges
            .iter()
            .map(|(from, to)| WireEdge { from, to })
            .collect(),
        stages: report
            .stages
            .iter()
            .map(|s| WireStage {
                namespace: &s.namespace,
                state: s.state.as_str(),
                docs: &s.docs,
                verdict: &s.verdict,
                gate_met: s.gate_met,
                hold: &s.held,
            })
            .collect(),
        current: report.current.as_deref(),
        blockers: &report.blockers,
        blocker_stages: &report.blocker_stages,
        next_action: &report.next_action,
    };
    serde_json::to_string_pretty(&wire).unwrap_or_else(|_| "{}".to_string())
}

/// A DAG is a chain when consecutive `order` pairs account for every
/// edge.
fn is_chain(dag: &NamespaceDag) -> bool {
    dag.edges.len() + 1 == dag.order.len()
        && dag
            .order
            .windows(2)
            .all(|pair| dag.edges.iter().any(|(f, t)| f == &pair[0] && t == &pair[1]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Ast;
    use crate::config::{GatePredicate, GateQuantifier, NamespaceConfig, PipelineConfig};
    use std::collections::BTreeMap;

    fn doc(raw_id: &str, depends_on: Vec<&str>) -> Document {
        Document {
            id: raw_id.parse().expect("valid id"),
            raw_id: raw_id.to_owned(),
            location: format!("{}.md", raw_id.to_lowercase()),
            depends_on: depends_on.into_iter().map(String::from).collect(),
            frontmatter_lines: Default::default(),
            metadata: Default::default(),
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    fn config_with_namespaces(names: &[&str]) -> Config {
        let mut config = Config::default();
        for name in names {
            config
                .namespaces
                .insert(name.to_string(), NamespaceConfig::default());
        }
        config
    }

    #[test]
    fn declared_pipeline_wins_over_edges() {
        // EARS-01.1: declared order is used verbatim even when dep
        // edges imply the opposite order.
        let mut config = config_with_namespaces(&["ADR", "SPEC"]);
        config.pipeline = Some(PipelineConfig {
            stages: vec!["SPEC".to_string(), "ADR".to_string()],
            gates: BTreeMap::new(),
        });
        let docs = vec![doc("ADR-001", vec![]), doc("SPEC-001", vec!["ADR-001"])];
        let resolution = resolve_dag(&config, &docs).unwrap();
        assert_eq!(resolution.source, DagSource::Declared);
        assert_eq!(resolution.dag.order, vec!["SPEC", "ADR"]);
    }

    #[test]
    fn inferred_when_cross_namespace_edges_exist() {
        let config = config_with_namespaces(&["ADR", "SPEC"]);
        let docs = vec![doc("ADR-001", vec![]), doc("SPEC-001", vec!["ADR-001"])];
        let resolution = resolve_dag(&config, &docs).unwrap();
        assert_eq!(resolution.source, DagSource::Inferred);
        assert_eq!(resolution.dag.order, vec!["ADR", "SPEC"]);
    }

    #[test]
    fn default_ladder_when_no_edges_restricted_to_active() {
        // EARS-01.3: TASK is not active → ladder is PRD → ADR → SPEC.
        let config = config_with_namespaces(&["ADR", "PRD", "SPEC"]);
        let docs = vec![doc("PRD-001", vec![])];
        let resolution = resolve_dag(&config, &docs).unwrap();
        assert_eq!(resolution.source, DagSource::Default);
        assert_eq!(resolution.dag.order, vec!["PRD", "ADR", "SPEC"]);
    }

    #[test]
    fn intra_namespace_edges_alone_fall_back_to_default() {
        // An ADR-supersedes-ADR edge lifts to nothing — that is a
        // cold start for inference purposes, not an inferred DAG.
        let config = config_with_namespaces(&["ADR"]);
        let docs = vec![doc("ADR-001", vec![]), doc("ADR-002", vec!["ADR-001"])];
        let resolution = resolve_dag(&config, &docs).unwrap();
        assert_eq!(resolution.source, DagSource::Default);
        assert_eq!(resolution.dag.order, vec!["ADR"]);
    }

    #[test]
    fn namespace_cycle_surfaces_as_status_error() {
        let config = config_with_namespaces(&["ADR", "SPEC"]);
        let docs = vec![
            doc("ADR-001", vec!["SPEC-001"]),
            doc("SPEC-001", vec![]),
            doc("SPEC-002", vec!["ADR-001"]),
        ];
        let err = resolve_dag(&config, &docs).unwrap_err();
        match err {
            StatusError::Cycle { members } => assert_eq!(members, vec!["ADR", "SPEC"]),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // -- T2.1: effective gate resolution (EARS-02.4) --------------------

    #[test]
    fn explicit_gate_is_used_when_present() {
        let mut gates = BTreeMap::new();
        gates.insert(
            "ADR".to_string(),
            GatePredicate {
                quantifier: GateQuantifier::All,
                status: "superseded".to_string(),
            },
        );
        let gate = effective_gate("ADR", &gates);
        assert_eq!(gate.quantifier, GateQuantifier::All);
        assert_eq!(gate.status, "superseded");
    }

    #[test]
    fn default_gate_is_any_accepted_for_non_task() {
        let gate = effective_gate("ADR", &BTreeMap::new());
        assert_eq!(gate.quantifier, GateQuantifier::Any);
        assert_eq!(gate.status, "accepted");
    }

    #[test]
    fn default_gate_is_all_done_for_task() {
        // TASK is the only built-in namespace whose terminal status is
        // `done` (SPEC-002 § Data model, EARS-02.4).
        let gate = effective_gate("TASK", &BTreeMap::new());
        assert_eq!(gate.quantifier, GateQuantifier::All);
        assert_eq!(gate.status, "done");
    }

    // -- T2.2: gate evaluation (EARS-02.1/02.2/02.5) --------------------

    fn view(id: &str, status: Option<&str>, clean: bool) -> DocView {
        DocView {
            id: id.to_string(),
            status: status.map(str::to_string),
            clean,
        }
    }

    fn any_accepted() -> GatePredicate {
        GatePredicate {
            quantifier: GateQuantifier::Any,
            status: "accepted".to_string(),
        }
    }

    fn all_done() -> GatePredicate {
        GatePredicate {
            quantifier: GateQuantifier::All,
            status: "done".to_string(),
        }
    }

    #[test]
    fn any_gate_done_when_one_matching_clean_doc() {
        let docs = [view("ADR-001", Some("accepted"), true), view("ADR-002", Some("draft"), true)];
        let eval = evaluate_gate(&any_accepted(), &docs);
        assert!(eval.done);
        assert!(eval.held.is_empty());
    }

    #[test]
    fn any_gate_unsatisfied_when_no_matching_doc() {
        let docs = [view("ADR-001", Some("draft"), true)];
        let eval = evaluate_gate(&any_accepted(), &docs);
        assert!(!eval.done);
        assert!(eval.held.is_empty(), "a draft is not a held terminal doc");
    }

    #[test]
    fn any_gate_held_when_matching_doc_is_dirty() {
        // EARS-02.2: the accepted doc carries the terminal status but
        // has a diagnostic → not done, and it is named as held.
        let docs = [view("SPEC-001", Some("accepted"), false)];
        let eval = evaluate_gate(&any_accepted(), &docs);
        assert!(!eval.done);
        assert_eq!(eval.held, vec!["SPEC-001"]);
    }

    #[test]
    fn any_gate_ignores_dirty_non_matching_docs() {
        // A dirty draft does not hold an any:accepted gate — only docs
        // carrying the gate's status are counted (EARS-02.1).
        let docs = [view("ADR-001", Some("accepted"), true), view("ADR-002", Some("draft"), false)];
        let eval = evaluate_gate(&any_accepted(), &docs);
        assert!(eval.done);
        assert!(eval.held.is_empty());
    }

    #[test]
    fn all_gate_done_when_every_doc_matches_and_clean() {
        let docs = [view("TASK-001", Some("done"), true), view("TASK-002", Some("done"), true)];
        let eval = evaluate_gate(&all_done(), &docs);
        assert!(eval.done);
    }

    #[test]
    fn all_gate_not_done_when_one_doc_unfinished() {
        let docs = [view("TASK-001", Some("done"), true), view("TASK-002", Some("doing"), true)];
        let eval = evaluate_gate(&all_done(), &docs);
        assert!(!eval.done);
        assert!(eval.held.is_empty(), "an unfinished task is not held");
    }

    #[test]
    fn all_gate_held_names_dirty_terminal_doc() {
        let docs = [view("TASK-001", Some("done"), false), view("TASK-002", Some("done"), true)];
        let eval = evaluate_gate(&all_done(), &docs);
        assert!(!eval.done);
        assert_eq!(eval.held, vec!["TASK-001"]);
    }

    #[test]
    fn all_gate_over_empty_namespace_is_unsatisfied() {
        // EARS-02.5: an `all:` gate with no documents is unsatisfied —
        // not vacuously true.
        let eval = evaluate_gate(&all_done(), &[]);
        assert!(!eval.done);
    }

    #[test]
    fn held_docs_are_name_sorted() {
        let docs = [view("ADR-009", Some("accepted"), false), view("ADR-002", Some("accepted"), false)];
        let eval = evaluate_gate(&any_accepted(), &docs);
        assert_eq!(eval.held, vec!["ADR-002", "ADR-009"]);
    }

    // -- T2.2: stage report (no parent gating yet — that is T2.3) --------

    fn doc_with_status(raw_id: &str, status: &str, depends_on: Vec<&str>) -> Document {
        let mut d = doc(raw_id, depends_on);
        d.metadata
            .insert("status".to_string(), serde_json::json!(status));
        d
    }

    #[test]
    fn single_stage_accepted_clean_is_done() {
        let config = config_with_namespaces(&["SPEC"]);
        let docs = vec![doc_with_status("SPEC-001", "accepted", vec![])];
        let resolution = resolve_dag(&config, &docs).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[]);
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].namespace, "SPEC");
        assert_eq!(stages[0].state, StageState::Done);
        assert!(stages[0].held.is_empty());
    }

    #[test]
    fn single_stage_accepted_dirty_is_current_and_held() {
        // Scenario 4 at the unit level: the SPEC doc is dirty (a
        // diagnostic anchored at its location), so the stage is the
        // current, held stage.
        let config = config_with_namespaces(&["SPEC"]);
        let docs = vec![doc_with_status("SPEC-001", "accepted", vec![])];
        let diag = crate::diagnostic::Diagnostic::error(
            "core.required-headings",
            "spec-001.md", // matches doc().location = "<lower-id>.md"
            1,
            0,
            "missing heading",
        );
        let stages = compute_stages(&resolution_dag(&config, &docs), &config, &docs, &[diag]);
        assert_eq!(stages[0].state, StageState::Current);
        assert_eq!(stages[0].held, vec!["SPEC-001"]);
    }

    fn resolution_dag(config: &Config, docs: &[Document]) -> NamespaceDag {
        resolve_dag(config, docs).unwrap().dag
    }

    #[test]
    fn join_stage_waits_on_unfinished_parent() {
        // EARS-02.6: SPEC's own gate is satisfied (accepted) but the
        // DESIGN parent is a draft → SPEC pending, DESIGN current.
        let config = config_with_namespaces(&["ADR", "DESIGN", "SPEC"]);
        let docs = vec![
            doc_with_status("ADR-001", "accepted", vec![]),
            doc_with_status("DESIGN-001", "draft", vec![]),
            doc_with_status("SPEC-001", "accepted", vec!["ADR-001", "DESIGN-001"]),
        ];
        let resolution = resolve_dag(&config, &docs).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[]);
        let by_ns: std::collections::BTreeMap<&str, StageState> =
            stages.iter().map(|s| (s.namespace.as_str(), s.state)).collect();
        assert_eq!(by_ns["ADR"], StageState::Done);
        assert_eq!(by_ns["DESIGN"], StageState::Current);
        assert_eq!(
            by_ns["SPEC"],
            StageState::Pending,
            "a join stage with one unfinished parent must not be done"
        );
    }

    #[test]
    fn join_stage_done_when_all_parents_done() {
        let config = config_with_namespaces(&["ADR", "DESIGN", "SPEC"]);
        let docs = vec![
            doc_with_status("ADR-001", "accepted", vec![]),
            doc_with_status("DESIGN-001", "accepted", vec![]),
            doc_with_status("SPEC-001", "accepted", vec!["ADR-001", "DESIGN-001"]),
        ];
        let resolution = resolve_dag(&config, &docs).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[]);
        assert!(stages.iter().all(|s| s.state == StageState::Done));
    }

    #[test]
    fn cold_start_first_empty_stage_is_current() {
        // Default ladder PRD → ADR → SPEC → TASK with only an accepted
        // PRD: PRD done, ADR current (no accepted ADR), rest pending.
        let config = config_with_namespaces(&["PRD", "ADR", "SPEC", "TASK"]);
        let docs = vec![doc_with_status("PRD-001", "accepted", vec![])];
        let resolution = resolve_dag(&config, &docs).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[]);
        let by_ns: std::collections::BTreeMap<&str, StageState> =
            stages.iter().map(|s| (s.namespace.as_str(), s.state)).collect();
        assert_eq!(by_ns["PRD"], StageState::Done);
        assert_eq!(by_ns["ADR"], StageState::Current);
        assert_eq!(by_ns["SPEC"], StageState::Pending);
        assert_eq!(by_ns["TASK"], StageState::Pending);
    }

    // -- T3.1: BUG tripwire (EARS-03.1/03.2) ----------------------------

    fn ast_with_cross_ref(namespace: &str, number: u32) -> Ast {
        let mut ast = Ast::default();
        ast.cross_ref_tokens.push(crate::ast::CrossRefToken {
            token: format!("{namespace}-{number}"),
            namespace: namespace.to_string(),
            number,
            line: 1,
            col: 0,
            in_code: false,
            in_strikethrough: false,
        });
        ast
    }

    #[test]
    fn open_bug_citing_lineage_via_depends_on_blocks_the_stage() {
        // EARS-03.1: an `open` BUG whose depends_on points at a lineage
        // SPEC blocks the SPEC stage and is named as a blocker.
        let dag = dag::chain_dag(&["SPEC".to_string()]);
        let docs = vec![
            doc_with_status("SPEC-001", "draft", vec![]),
            doc_with_status("BUG-001", "open", vec!["SPEC-001"]),
        ];
        let trip = sweep_tripwire(&dag, &docs);
        assert_eq!(trip.blockers, vec!["BUG-001".to_string()]);
        assert!(trip.blocked_namespaces.contains("SPEC"));
        // ADR-037 § WIRE-004: the sweep retains per-BUG attribution.
        assert_eq!(
            trip.blocker_stages,
            BTreeMap::from([("BUG-001".to_string(), vec!["SPEC".to_string()])])
        );
    }

    #[test]
    fn open_bug_citing_lineage_via_body_token_blocks() {
        // The other pointer kind: a non-suppressed body cross-ref token
        // to a lineage document counts as a citation too.
        let dag = dag::chain_dag(&["SPEC".to_string()]);
        let spec = doc_with_status("SPEC-001", "draft", vec![]);
        let mut bug = doc_with_status("BUG-001", "open", vec![]);
        bug.ast = Some(ast_with_cross_ref("SPEC", 1));
        let docs = vec![spec, bug];
        let trip = sweep_tripwire(&dag, &docs);
        assert_eq!(trip.blockers, vec!["BUG-001".to_string()]);
        assert!(trip.blocked_namespaces.contains("SPEC"));
    }

    #[test]
    fn non_open_bug_does_not_block() {
        // EARS-03.2: a BUG that has left `open` no longer blocks — the
        // sweep only counts `open` BUGs, so the state clears for free.
        let dag = dag::chain_dag(&["SPEC".to_string()]);
        let docs = vec![
            doc_with_status("SPEC-001", "draft", vec![]),
            doc_with_status("BUG-001", "fixed", vec!["SPEC-001"]),
        ];
        let trip = sweep_tripwire(&dag, &docs);
        assert!(trip.blockers.is_empty());
        assert!(trip.blocked_namespaces.is_empty());
    }

    #[test]
    fn open_bug_citing_outside_the_lineage_does_not_block() {
        // ADR is not a staged namespace here, so a BUG citing ADR-001
        // touches nothing in the active lineage and must not block.
        let dag = dag::chain_dag(&["SPEC".to_string()]);
        let docs = vec![
            doc_with_status("SPEC-001", "draft", vec![]),
            doc_with_status("ADR-001", "accepted", vec![]),
            doc_with_status("BUG-001", "open", vec!["ADR-001"]),
        ];
        let trip = sweep_tripwire(&dag, &docs);
        assert!(trip.blockers.is_empty());
        assert!(trip.blocked_namespaces.is_empty());
    }

    #[test]
    fn suppressed_body_token_does_not_block() {
        // A cross-ref token inside code/strikethrough is suppressed —
        // it must not count as a citation (mirrors find_references).
        let dag = dag::chain_dag(&["SPEC".to_string()]);
        let spec = doc_with_status("SPEC-001", "draft", vec![]);
        let mut bug = doc_with_status("BUG-001", "open", vec![]);
        let mut ast = Ast::default();
        ast.cross_ref_tokens.push(crate::ast::CrossRefToken {
            token: "SPEC-1".to_string(),
            namespace: "SPEC".to_string(),
            number: 1,
            line: 1,
            col: 0,
            in_code: true,
            in_strikethrough: false,
        });
        bug.ast = Some(ast);
        let docs = vec![spec, bug];
        let trip = sweep_tripwire(&dag, &docs);
        assert!(trip.blockers.is_empty());
    }

    #[test]
    fn render_report_lists_blockers() {
        let report = Report {
            source: DagSource::Declared,
            dag: dag::chain_dag(&["SPEC".to_string()]),
            stages: vec![Stage {
                namespace: "SPEC".to_string(),
                state: StageState::Blocked,
                docs: vec!["SPEC-001".to_string()],
                verdict: "draft".to_string(),
                gate_met: false,
                held: vec![],
                actionable: Some("SPEC-001".to_string()),
            }],
            current: Some("SPEC".to_string()),
            blockers: vec!["BUG-001".to_string()],
            blocker_stages: BTreeMap::from([("BUG-001".to_string(), vec!["SPEC".to_string()])]),
            next_action: "resolve BUG-001".to_string(),
        };
        let out = render_report(&report);
        assert!(out.contains("SPEC: blocked"), "out:\n{out}");
        assert!(out.contains("blocked by: BUG-001"), "out:\n{out}");
        assert!(out.contains("next: resolve BUG-001"), "out:\n{out}");
    }

    // -- T3.2: next_action fixed template (EARS-04.3/04.4) ---------------

    #[test]
    fn next_action_resolves_blocker_first() {
        let stages = vec![stage("SPEC", StageState::Blocked, "draft", Some("SPEC-001"))];
        let action = next_action(
            &stages,
            Some("SPEC"),
            &["BUG-001".to_string()],
            &Config::default(),
        );
        assert_eq!(action, "resolve BUG-001");
    }

    #[test]
    fn next_action_accepts_the_draft_in_the_current_stage() {
        let stages = vec![stage("ADR", StageState::Current, "draft", Some("ADR-001"))];
        let action = next_action(&stages, Some("ADR"), &[], &Config::default());
        assert_eq!(action, "accept ADR-001");
    }

    #[test]
    fn next_action_completes_a_task_stage() {
        // TASK's terminal status is `done`, so the verb is `complete`.
        let mut config = Config::default();
        config.pipeline = Some(PipelineConfig {
            stages: vec!["TASK".to_string()],
            gates: BTreeMap::new(),
        });
        let stages = vec![stage("TASK", StageState::Current, "doing", Some("TASK-001"))];
        let action = next_action(&stages, Some("TASK"), &[], &config);
        assert_eq!(action, "complete TASK-001");
    }

    #[test]
    fn next_action_fixes_a_held_stage() {
        let stages = vec![stage("SPEC", StageState::Current, "held", Some("SPEC-001"))];
        let action = next_action(&stages, Some("SPEC"), &[], &Config::default());
        assert_eq!(action, "fix SPEC-001");
    }

    #[test]
    fn next_action_creates_the_first_doc_in_an_empty_stage() {
        let stages = vec![stage("ADR", StageState::Current, "empty", None)];
        let action = next_action(&stages, Some("ADR"), &[], &Config::default());
        assert_eq!(action, "create the first ADR document");
    }

    #[test]
    fn next_action_complete_when_pipeline_done() {
        let stages = vec![stage("ADR", StageState::Done, "accepted", None)];
        let action = next_action(&stages, None, &[], &Config::default());
        assert_eq!(action, "pipeline complete");
    }

    fn stage(ns: &str, state: StageState, verdict: &str, actionable: Option<&str>) -> Stage {
        Stage {
            namespace: ns.to_string(),
            state,
            docs: actionable.iter().map(|s| s.to_string()).collect(),
            verdict: verdict.to_string(),
            // Default: a done stage has its gate met. Tests needing the
            // gate-met-but-not-done case build a Stage literal directly.
            gate_met: matches!(state, StageState::Done),
            held: Vec::new(),
            actionable: actionable.map(str::to_string),
        }
    }

    #[test]
    fn render_json_emits_the_data_model_schema() {
        // EARS-04.2 + ADR-037: the JSON object carries source / edges /
        // stages (namespace, state, docs, verdict, gate_met, hold) /
        // current / blockers / blocker_stages / next_action and nothing
        // from the internal Report under its internal field names.
        let report = Report {
            source: DagSource::Inferred,
            dag: dag::chain_dag(&["ADR".to_string(), "SPEC".to_string()]),
            stages: vec![
                stage("ADR", StageState::Done, "accepted", None),
                stage("SPEC", StageState::Current, "draft", Some("SPEC-001")),
            ],
            current: Some("SPEC".to_string()),
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "accept SPEC-001".to_string(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
        assert_eq!(parsed["source"], "inferred");
        assert_eq!(parsed["current"], "SPEC");
        assert_eq!(parsed["next_action"], "accept SPEC-001");
        assert!(parsed["blockers"].as_array().unwrap().is_empty());
        let stages = parsed["stages"].as_array().unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[1]["namespace"], "SPEC");
        assert_eq!(stages[1]["state"], "current");
        assert_eq!(stages[1]["verdict"], "draft");
        assert_eq!(stages[1]["docs"][0], "SPEC-001");
        // ADR-037 § WIRE-001: edges mirror the resolved DAG as {from,to}.
        assert_eq!(parsed["edges"], serde_json::json!([{"from": "ADR", "to": "SPEC"}]));
        // ADR-037 § WIRE-002/003: per-stage gate result and hold list.
        assert_eq!(stages[0]["gate_met"], true, "ADR is done → its gate is met");
        assert_eq!(stages[1]["gate_met"], false, "SPEC draft → gate not met");
        assert!(stages[1]["hold"].as_array().unwrap().is_empty());
        // ADR-037 § WIRE-004: blocker_stages is a (here empty) object.
        assert_eq!(parsed["blocker_stages"], serde_json::json!({}));
        // Internal field names never leak to the wire (WIRE-005).
        assert!(parsed.get("dag").is_none());
        assert!(stages[1].get("held").is_none());
        assert!(stages[1].get("actionable").is_none());
    }

    #[test]
    fn render_json_gate_met_is_true_on_a_parent_gated_pending_stage() {
        // ADR-037 § WIRE-002: the load-bearing distinction. SPEC's own
        // gate is met (accepted+clean) but it is `pending` because a
        // parent is unfinished (EARS-02.6); `verdict` renders a
        // non-terminal token, so `gate_met` is the field that tells the
        // truth about SPEC's own gate.
        let report = Report {
            source: DagSource::Inferred,
            dag: dag::chain_dag(&["ADR".to_string(), "SPEC".to_string()]),
            stages: vec![
                stage("ADR", StageState::Current, "draft", Some("ADR-001")),
                Stage {
                    namespace: "SPEC".to_string(),
                    state: StageState::Pending,
                    docs: vec!["SPEC-001".to_string()],
                    verdict: "superseded".to_string(),
                    gate_met: true,
                    held: Vec::new(),
                    actionable: None,
                },
            ],
            current: Some("ADR".to_string()),
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "accept ADR-001".to_string(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
        let spec = &parsed["stages"][1];
        assert_eq!(spec["state"], "pending");
        assert_eq!(spec["verdict"], "superseded");
        assert_eq!(spec["gate_met"], true, "own gate is met despite pending state");
    }

    #[test]
    fn render_json_blocker_stages_attributes_each_bug_to_its_stages() {
        // ADR-037 § WIRE-004: the map keys are the blockers and the
        // values name the stages each BUG blocks.
        let report = Report {
            source: DagSource::Declared,
            dag: dag::chain_dag(&["ADR".to_string(), "SPEC".to_string()]),
            stages: vec![
                stage("ADR", StageState::Blocked, "accepted", None),
                stage("SPEC", StageState::Blocked, "draft", Some("SPEC-001")),
            ],
            current: Some("ADR".to_string()),
            blockers: vec!["BUG-001".to_string(), "BUG-002".to_string()],
            blocker_stages: BTreeMap::from([
                ("BUG-001".to_string(), vec!["ADR".to_string()]),
                ("BUG-002".to_string(), vec!["ADR".to_string(), "SPEC".to_string()]),
            ]),
            next_action: "resolve BUG-001".to_string(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
        assert_eq!(
            parsed["blocker_stages"],
            serde_json::json!({"BUG-001": ["ADR"], "BUG-002": ["ADR", "SPEC"]})
        );
        // The flat v1 list is still present, unchanged (WIRE-005).
        assert_eq!(parsed["blockers"], serde_json::json!(["BUG-001", "BUG-002"]));
    }

    #[test]
    fn render_json_current_is_null_when_complete() {
        let report = Report {
            source: DagSource::Default,
            dag: dag::chain_dag(&["ADR".to_string()]),
            stages: vec![stage("ADR", StageState::Done, "accepted", None)],
            current: None,
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "pipeline complete".to_string(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
        assert!(parsed["current"].is_null());
    }

    #[test]
    fn render_report_full_ladder_snapshot() {
        // Cold-start ladder: PRD done, ADR current (empty), the rest
        // pending. Pins the whole text rendering (EARS-04.1).
        let report = Report {
            source: DagSource::Default,
            dag: dag::chain_dag(&[
                "PRD".to_string(),
                "ADR".to_string(),
                "SPEC".to_string(),
                "TASK".to_string(),
            ]),
            stages: vec![
                stage("PRD", StageState::Done, "accepted", None),
                stage("ADR", StageState::Current, "empty", None),
                stage("SPEC", StageState::Pending, "empty", None),
                stage("TASK", StageState::Pending, "empty", None),
            ],
            current: Some("ADR".to_string()),
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "create the first ADR document".to_string(),
        };
        insta::assert_snapshot!(render_report(&report), @r"
        source: default
        PRD → ADR → SPEC → TASK

        PRD: done (accepted)
        ADR: current (empty)
        SPEC: pending (empty)
        TASK: pending (empty)

        current: ADR
        next: create the first ADR document

        tip: run with --format json for machine-readable output
        ");
    }

    #[test]
    fn render_names_the_source_and_chains_on_one_line() {
        let resolution = Resolution {
            dag: dag::chain_dag(&["PRD".to_string(), "ADR".to_string()]),
            source: DagSource::Default,
        };
        assert_eq!(render_text(&resolution), "source: default\nPRD → ADR\n");
    }

    #[test]
    fn render_branching_dag_prints_one_edge_per_line() {
        let config = config_with_namespaces(&["ADR", "DESIGN", "PRD", "SPEC"]);
        let docs = vec![
            doc("PRD-001", vec![]),
            doc("ADR-001", vec!["PRD-001"]),
            doc("DESIGN-001", vec!["PRD-001"]),
            doc("SPEC-001", vec!["ADR-001", "DESIGN-001"]),
        ];
        let resolution = resolve_dag(&config, &docs).unwrap();
        assert_eq!(
            render_text(&resolution),
            "source: inferred\n\
             ADR → SPEC\n\
             DESIGN → SPEC\n\
             PRD → ADR\n\
             PRD → DESIGN\n"
        );
    }

    #[test]
    fn render_empty_dag_says_no_stages() {
        let resolution = Resolution {
            dag: dag::chain_dag(&[]),
            source: DagSource::Default,
        };
        assert_eq!(render_text(&resolution), "source: default\n(no stages)\n");
    }
}
