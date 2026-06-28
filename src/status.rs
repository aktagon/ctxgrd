//! `ctxgrd status` — pipeline position resolution (SPEC-002).
//!
//! Sprint 1 scope: resolve the namespace DAG (declared-or-default,
//! ADR-039 § DAG-007; EARS-01.1/01.3), name the DAG source in the JSON
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

/// Where the resolved DAG came from (EARS-01.4). Named in the JSON
/// output (`--format json`) — a built-in ladder is never passed off as
/// derived to an agent routing on it; the human table omits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DagSource {
    Declared,
    Default,
}

impl DagSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Default => "default",
        }
    }

    /// Plain-language gloss of [`Self::as_str`], emitted as the JSON
    /// `source_hint` so a person or an LLM reading `--format json` need not
    /// decode the bare token. The const above stays the stable thing a strict
    /// parser switches on; this is the human-readable companion.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Declared => "order you set in ctxgrd.toml",
            Self::Default => "ctxgrd's default order (you haven't set one)",
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
    /// EARS-04.5: a `--lineage <ID>` selector that resolves to no document
    /// in the run.
    #[error("lineage root '{id}' is not a document in this run")]
    LineageNotFound { id: String },
}

/// Built-in default ladder (EARS-01.3), applied in this order and
/// restricted to namespaces active in the configuration.
const DEFAULT_LADDER: [&str; 4] = ["PRD", "ADR", "SPEC", "TASK"];

/// Resolve the namespace DAG for `root`: load config through the shared
/// ingest pipeline (the same config `lint` sees), then apply the
/// declared-or-default ladder (ADR-039 § DAG-007; SPEC-002 § Workflows
/// step 1).
pub fn resolve(root: &Path) -> Result<Resolution, StatusError> {
    let run::IngestResult { config, .. } = run::ingest(root)?;
    resolve_dag(&config)
}

/// ADR-039 § DAG-007 — the init-time inference seam. Lift the existing
/// documents' `depends_on` edges into per-namespace `core.dep-shape`
/// `requires` suggestions (edge `T → NS` ⇒ NS requires T), so `ctxgrd
/// init` can seed a *declared* DAG that `status` then reports as
/// `source: declared`. Returned map is keyed by namespace, values
/// name-sorted; empty when nothing lifts. This is descriptive guidance
/// for scaffolding only — runtime resolution never calls it (inference
/// no longer runs live, DAG-007).
///
/// NOTE (DAG-007): `init_cmd` does not yet thread these suggestions into
/// the generated `ctxgrd.toml` — see the TODO there. This function is the
/// retained `infer_namespace_dag` caller-side entry point that wiring will
/// build on.
pub fn infer_dep_shape_requires(root: &Path) -> Result<BTreeMap<String, Vec<String>>, StatusError> {
    let run::IngestResult { documents, .. } = run::ingest(root)?;
    let dag = dag::infer_namespace_dag(&documents).map_err(|cycle| StatusError::Cycle {
        members: cycle.members,
    })?;
    // A lifted edge `(from, to)` means `to`'s documents depend on `from`,
    // so `to` requires `from`.
    let mut requires: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (from, to) in dag.edges {
        requires.entry(to).or_default().insert(from);
    }
    Ok(requires
        .into_iter()
        .map(|(ns, set)| (ns, set.into_iter().collect()))
        .collect())
}

fn resolve_dag(config: &Config) -> Result<Resolution, StatusError> {
    // DAG-001/DAG-002/DAG-005: the declared DAG is the union of every
    // namespace's `core.dep-shape` `requires`/`allows` lifts and any
    // `[pipeline].stages` adjacency, assembled through the same
    // construction/validation as inference. A declared `[pipeline]`
    // contributes its stages as nodes even when single-stage (no edges),
    // so an isolated one-stage pipeline still resolves declared
    // (EARS-01.1, DAG-005).
    let stage_nodes: BTreeSet<String> = config
        .pipeline
        .as_ref()
        .map(|p| p.stages.iter().cloned().collect())
        .unwrap_or_default();
    let declared =
        dag::build_dag_from_edges(config.dep_shape_edges(), stage_nodes).map_err(|cycle| {
            StatusError::Cycle {
                members: cycle.members,
            }
        })?;
    if !declared.order.is_empty() {
        return Ok(Resolution {
            dag: declared,
            source: DagSource::Declared,
        });
    }

    // ADR-039 § DAG-007: runtime resolution is declared-or-default. The
    // descriptive `inferred` mode is gone — inference now belongs to
    // `ctxgrd init` (see DAG-007 wiring TODO in `init_cmd`), so nothing
    // guesses at runtime and the declaration is the only voice.

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
/// empty whenever no `[pipeline]` is declared, so default DAGs get
/// all-default gates.
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
            // EARS-03.1 (SPEC-003): `any:` over an empty (or all-non-matching)
            // namespace is unsatisfied — `any` stays false, so a pruned/empty
            // lineage stage holds rather than passing vacuously, matching the
            // `all:`-over-empty rule (EARS-02.5).
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
    /// The pipeline's ready set (ADR-039 § DAG-006): the name-sorted
    /// antichain of stages that are NOT done but whose every parent IS
    /// done. Roots (no parents) appear iff not done; independent
    /// components (multiple roots) all appear side by side. Empty when
    /// the pipeline is complete.
    pub frontier: Vec<String>,
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
    /// The lineage root id when scoped via `--lineage <ID>` (EARS-04.1), or
    /// `None` for the global view. Serialized as the JSON `lineage` field
    /// only in lineage mode — absent globally, so bare `status` output stays
    /// byte-identical (EARS-04.7).
    pub lineage: Option<String>,
    /// Per-stage shared-node disclosure (EARS-04.4, ADR-059 § LIN-005):
    /// namespace → the other lineage-root ids whose closures also contain a
    /// document counted in that stage. Empty in the global view and for any
    /// stage with no shared members; rendered as the JSON per-stage `shared`
    /// array. Name-sorted, deduped.
    pub shared: BTreeMap<String, Vec<String>>,
}

/// The global `ctxgrd status` report (SPEC-002): the whole document set,
/// no lineage scope. Thin wrapper over [`report_scoped`] with no selector.
pub fn report(root: &Path) -> Result<Report, StatusError> {
    report_scoped(root, None)
}

/// Resolve the DAG, run the shared lint pass, optionally scope the document
/// set to a lineage, compute per-stage verdicts, then sweep the BUG
/// tripwire (SPEC-002 § Workflows steps 1–4; SPEC-003 § Workflows step 3).
///
/// WHERE `lineage` is `Some(id)`, the evaluated document set is restricted
/// to the transitive **dependents** of `id` over the transpose of the
/// `depends_on` graph (EARS-04.1) — the engine is unchanged, only the
/// counted documents differ (EARS-04.2). Shared members (reachable from
/// another lineage root) are disclosed, not folded (EARS-04.3/04.4). An
/// `id` that resolves to no document is [`StatusError::LineageNotFound`]
/// (EARS-04.5).
pub fn report_scoped(root: &Path, lineage: Option<&str>) -> Result<Report, StatusError> {
    let LintRun {
        outcome,
        config,
        documents,
    } = run::lint_run(root)?;
    let resolution = resolve_dag(&config)?;

    // Step 3 — scope the document set (EARS-04.*). `members` is the lineage
    // id-set the gate/tripwire count over; `None` is the global view, which
    // counts every document and discloses nothing (EARS-04.7 byte-identity).
    let mut lineage_id: Option<String> = None;
    let mut members: Option<BTreeSet<DocumentId>> = None;
    let mut shared: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(raw) = lineage {
        let id: DocumentId = raw
            .parse()
            .map_err(|_| StatusError::LineageNotFound { id: raw.to_string() })?;
        let graph = dag::DepGraph::new(&documents);
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
            let others: Vec<String> = graph
                .owning_roots(m)
                .into_iter()
                .filter(|&r| r != root_idx)
                .map(|r| documents[r].raw_id.clone())
                .collect();
            if others.is_empty() {
                continue;
            }
            shared
                .entry(documents[m].id.namespace.clone())
                .or_default()
                .extend(others);
        }
        for ids in shared.values_mut() {
            ids.sort();
            ids.dedup();
        }
    }

    let mut stages = compute_stages(
        &resolution.dag,
        &config,
        &documents,
        &outcome.diagnostics,
        members.as_ref(),
    );

    // Step 4/5 — tripwire sweep (EARS-03.1), restricted to the scoped
    // lineage (SPEC-003 § Workflows 5): an `open` BUG citing a counted
    // document blocks the stage(s) it cites, overriding the gate verdict.
    // Cleared automatically once no citing BUG is `open` (EARS-03.2).
    let tripwire = sweep_tripwire(&resolution.dag, &documents, members.as_ref());
    for stage in &mut stages {
        if tripwire.blocked_namespaces.contains(&stage.namespace) {
            stage.state = StageState::Blocked;
        }
    }

    // Position: the frontier — the name-sorted ready set (ADR-039 §
    // DAG-006). A stage is ready iff it is not done AND every parent is
    // done; roots appear iff not done.
    let frontier = compute_frontier(&resolution.dag, &stages);

    let next_action = next_action(&stages, frontier.first().map(String::as_str), &tripwire.blockers, &config);

    Ok(Report {
        source: resolution.source,
        dag: resolution.dag,
        stages,
        frontier,
        blockers: tripwire.blockers,
        blocker_stages: tripwire.blocker_stages,
        next_action,
        lineage: lineage_id,
        shared,
    })
}

/// The frontier (ADR-039 § DAG-006): the name-sorted set of stages that
/// are NOT done and whose every parent IS done. `dag.order` is name-sorted
/// within topological tiers, but the frontier is sorted by name directly
/// so the output is a stable antichain regardless of `order` tie-breaks.
fn compute_frontier(dag: &NamespaceDag, stages: &[Stage]) -> Vec<String> {
    let done: BTreeSet<&str> = stages
        .iter()
        .filter(|s| s.state == StageState::Done)
        .map(|s| s.namespace.as_str())
        .collect();
    let mut frontier: Vec<String> = stages
        .iter()
        .filter(|s| s.state != StageState::Done)
        .filter(|s| {
            dag.edges
                .iter()
                .filter(|(_, to)| to == &s.namespace)
                .all(|(from, _)| done.contains(from.as_str()))
        })
        .map(|s| s.namespace.clone())
        .collect();
    frontier.sort();
    frontier
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
fn sweep_tripwire(
    dag: &NamespaceDag,
    documents: &[Document],
    members: Option<&BTreeSet<DocumentId>>,
) -> Tripwire {
    let lineage_ns: BTreeSet<&str> = dag.order.iter().map(String::as_str).collect();
    // The cited-document set is every staged-namespace document, narrowed to
    // the scoped lineage members when one is selected (SPEC-003 § Workflows
    // 5). BUGs are still scanned across the whole corpus below — a BUG
    // outside the lineage that cites a member still blocks its stage.
    let lineage: BTreeSet<DocumentId> = documents
        .iter()
        .filter(|d| lineage_ns.contains(d.id.namespace.as_str()))
        .filter(|d| members.is_none_or(|m| m.contains(&d.id)))
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
    members: Option<&BTreeSet<DocumentId>>,
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
            // EARS-04.2: in lineage mode, count only the scoped members; the
            // engine is unchanged, the document set is filtered.
            .filter(|d| members.is_none_or(|m| m.contains(&d.id)))
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

/// Full `ctxgrd status` text output (EARS-04.1): a KISS table — one
/// aligned row per stage carrying its state, document count, upstream
/// `needs`, and any hold/block — then a `ready:` line (the frontier) and
/// the next action. The DAG shape lives in the per-row `needs` column,
/// not a separate graph header: one column renders chains, diamonds, and
/// disconnected roots identically (a root simply has no `needs`). No
/// document body content reaches the output (EARS-04.4) — the next action
/// is a fixed token, ids are author-chosen frontmatter, never prose.
pub fn render_report(report: &Report) -> String {
    // The human table does not print the DAG source — `declared` vs `default`
    // is provenance for routing, not something a person reading the terminal
    // needs decoded (EARS-01.4 now lives on the `--format json` contract,
    // where agents consume it). `report.source` is still rendered by
    // `render_json`.
    let mut out = String::new();
    if !report.stages.is_empty() {
        // Column widths from the data so namespaces, states, and counts
        // line up regardless of length (48 docs vs 0).
        let ns_w = report.stages.iter().map(|s| s.namespace.len()).max().unwrap_or(0);
        let state_w = report
            .stages
            .iter()
            .map(|s| s.state.as_str().len())
            .max()
            .unwrap_or(0);
        let count_w = report
            .stages
            .iter()
            .map(|s| s.docs.len().to_string().len())
            .max()
            .unwrap_or(1);

        for stage in &report.stages {
            // Upstream namespaces (the `needs` column): every edge whose
            // head is this stage. Name-sorted; empty for a root.
            let mut needs: Vec<&str> = report
                .dag
                .edges
                .iter()
                .filter(|(_, to)| to == &stage.namespace)
                .map(|(from, _)| from.as_str())
                .collect();
            needs.sort_unstable();
            needs.dedup();

            // The `open` BUGs blocking this stage (ADR-037 § WIRE-004:
            // blocker_stages maps BUG → blocked namespaces; invert here).
            let mut blocked_by: Vec<&str> = report
                .blocker_stages
                .iter()
                .filter(|(_, nss)| nss.iter().any(|n| n == &stage.namespace))
                .map(|(bug, _)| bug.as_str())
                .collect();
            blocked_by.sort_unstable();

            let n = stage.docs.len();
            let unit = if n == 1 { "doc" } else { "docs" };
            out.push_str(&format!(
                "{:<ns_w$}  {:<state_w$}  {:>count_w$} {}",
                stage.namespace,
                stage.state.as_str(),
                n,
                unit,
            ));
            if !needs.is_empty() {
                out.push_str("   needs ");
                out.push_str(&needs.join(", "));
            }
            // A dirty terminal document holds the stage (EARS-02.2); an
            // open BUG blocks it (EARS-03.1). Distinct verbs, distinct
            // pointer kinds — docs vs BUG ids.
            if !stage.held.is_empty() {
                out.push_str("   held by ");
                out.push_str(&stage.held.join(", "));
            }
            if !blocked_by.is_empty() {
                out.push_str("   blocked by ");
                out.push_str(&blocked_by.join(", "));
            }
            // EARS-04.4: disclose shared members in text — a stage counting a
            // document another lineage also drives carries the other roots.
            // Empty (and silent) in the global view, so EARS-04.7 holds.
            if let Some(roots) = report.shared.get(&stage.namespace) {
                if !roots.is_empty() {
                    out.push_str("   shared with ");
                    out.push_str(&roots.join(", "));
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }
    // The ready set (the JSON `frontier`): stages workable right now —
    // labeled `ready:` in the human view, `frontier` on the wire.
    if !report.frontier.is_empty() {
        out.push_str("ready: ");
        out.push_str(&report.frontier.join(", "));
        out.push('\n');
    }
    out.push_str("next: ");
    out.push_str(&report.next_action);
    out.push('\n');
    // Point tools and agents at the structured projection — this table is
    // the human view; `--format json` is the machine contract.
    out.push('\n');
    out.push_str("tip: --format json for agents, mermaid/dot for diagrams\n");
    out
}

/// `ctxgrd status --format json` (EARS-04.2): a single JSON object
/// conforming to SPEC-002 § Data model — `source`, `edges`, `stages`
/// (`namespace`/`state`/`docs`/`verdict`/`gate_met`/`hold`), `frontier`,
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
        /// Other lineage roots a stage's documents also belong to
        /// (EARS-04.4). Omitted when empty — so the global view stays
        /// byte-identical to pre-SPEC-003 (EARS-04.7).
        #[serde(skip_serializing_if = "Vec::is_empty")]
        shared: Vec<&'a str>,
    }
    #[derive(serde::Serialize)]
    struct Wire<'a> {
        source: &'a str,
        source_hint: &'a str,
        /// The lineage root id in `--lineage` mode (EARS-04.1). Omitted in
        /// the global view (EARS-04.7 byte-identity).
        #[serde(skip_serializing_if = "Option::is_none")]
        lineage: Option<&'a str>,
        edges: Vec<WireEdge<'a>>,
        stages: Vec<WireStage<'a>>,
        frontier: &'a [String],
        blockers: &'a [String],
        blocker_stages: &'a BTreeMap<String, Vec<String>>,
        next_action: &'a str,
    }
    let wire = Wire {
        source: report.source.as_str(),
        source_hint: report.source.hint(),
        lineage: report.lineage.as_deref(),
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
                shared: report
                    .shared
                    .get(&s.namespace)
                    .map(|ids| ids.iter().map(String::as_str).collect())
                    .unwrap_or_default(),
            })
            .collect(),
        frontier: &report.frontier,
        blockers: &report.blockers,
        blocker_stages: &report.blocker_stages,
        next_action: &report.next_action,
    };
    serde_json::to_string_pretty(&wire).unwrap_or_else(|_| "{}".to_string())
}

/// `ctxgrd status --format mermaid`: the resolved DAG as Mermaid
/// `flowchart LR` *source* (output only — never rendered). Stage nodes
/// are styled by state via `classDef`, dependency edges are solid, and
/// each open BUG is a node with dashed `blocks` edges to the stage(s) it
/// holds (`blocker_stages`). Labels are fixed tokens + author ids
/// (EARS-04.4) — no document body text. Node ids are sanitized to
/// `[A-Za-z0-9_]` so a hyphenated id like `BUG-008` stays valid Mermaid.
pub fn render_mermaid(report: &Report) -> String {
    let mut out = String::from("flowchart LR\n");
    out.push_str(&format!(
        "  %% ctxgrd status · source: {} · next: {}\n",
        report.source.as_str(),
        report.next_action,
    ));
    for stage in &report.stages {
        let n = stage.docs.len();
        let unit = if n == 1 { "doc" } else { "docs" };
        let state = stage.state.as_str();
        out.push_str(&format!(
            "  {id}[\"{ns}: {state} ({n} {unit})\"]:::{state}\n",
            id = mermaid_id(&stage.namespace),
            ns = stage.namespace,
        ));
    }
    for (from, to) in &report.dag.edges {
        out.push_str(&format!("  {} --> {}\n", mermaid_id(from), mermaid_id(to)));
    }
    for (bug, blocked) in &report.blocker_stages {
        let bid = mermaid_id(bug);
        out.push_str(&format!("  {bid}[\"{bug} (open)\"]:::bug\n"));
        for ns in blocked {
            out.push_str(&format!("  {bid} -. blocks .-> {}\n", mermaid_id(ns)));
        }
    }
    out.push_str("  classDef done fill:#cde6c5,stroke:#33aa77;\n");
    out.push_str("  classDef current fill:#cfe3ff,stroke:#3377aa;\n");
    out.push_str("  classDef pending fill:#eeeeee,stroke:#999999;\n");
    out.push_str("  classDef blocked fill:#f6cccc,stroke:#cc3333;\n");
    out.push_str("  classDef bug fill:#ffe0a3,stroke:#ee9900;\n");
    out
}

/// Sanitize an id to a Mermaid-safe node id (`[A-Za-z0-9_]`). Namespaces
/// are already alphanumeric; hyphenated document ids (`BUG-008`) become
/// `BUG_008`. The original id is preserved in the node label.
fn mermaid_id(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// `ctxgrd status --format dot`: the resolved DAG as Graphviz DOT
/// *source* (output only — never rendered, never shelling out to `dot`).
/// Same model as [`render_mermaid`]: state-filled stage nodes, solid
/// dependency edges, and dashed `blocks` edges from each open BUG. All
/// node ids are quoted strings, so hyphenated ids need no sanitizing.
pub fn render_dot(report: &Report) -> String {
    let mut out = String::from("digraph pipeline {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str("  labelloc=t;\n");
    out.push_str(&format!(
        "  label=\"ctxgrd status — source: {} — next: {}\";\n",
        report.source.as_str(),
        report.next_action,
    ));
    out.push_str("  node [shape=box, style=\"rounded,filled\"];\n");
    for stage in &report.stages {
        let n = stage.docs.len();
        let unit = if n == 1 { "doc" } else { "docs" };
        out.push_str(&format!(
            "  \"{ns}\" [label=\"{ns}\\n{state} ({n} {unit})\", fillcolor=\"{color}\"];\n",
            ns = stage.namespace,
            state = stage.state.as_str(),
            color = dot_fill(stage.state),
        ));
    }
    for (from, to) in &report.dag.edges {
        out.push_str(&format!("  \"{from}\" -> \"{to}\";\n"));
    }
    for (bug, blocked) in &report.blocker_stages {
        out.push_str(&format!(
            "  \"{bug}\" [label=\"{bug}\\n(open)\", shape=note, fillcolor=\"#ffe0a3\"];\n"
        ));
        for ns in blocked {
            out.push_str(&format!(
                "  \"{bug}\" -> \"{ns}\" [style=dashed, label=\"blocks\"];\n"
            ));
        }
    }
    out.push_str("}\n");
    out
}

/// Graphviz fill colour for a stage state — mirrors the Mermaid
/// `classDef` palette so both formats read the same.
fn dot_fill(state: StageState) -> &'static str {
    match state {
        StageState::Done => "#cde6c5",
        StageState::Current => "#cfe3ff",
        StageState::Pending => "#eeeeee",
        StageState::Blocked => "#f6cccc",
    }
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
            pin: None,
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
        let resolution = resolve_dag(&config).unwrap();
        assert_eq!(resolution.source, DagSource::Declared);
        assert_eq!(resolution.dag.order, vec!["SPEC", "ADR"]);
    }

    /// Add a `core.dep-shape` param table (and the rule) to a namespace.
    fn with_dep_shape(config: &mut Config, ns: &str, requires: &[&str], allows: &[&str]) {
        let entry = config.namespaces.entry(ns.to_string()).or_default();
        entry.rules.push("core.dep-shape".to_string());
        let mut params = serde_json::Map::new();
        if !requires.is_empty() {
            params.insert("requires".to_string(), serde_json::json!(requires));
        }
        if !allows.is_empty() {
            params.insert("allows".to_string(), serde_json::json!(allows));
        }
        entry
            .params
            .insert("core.dep-shape".to_string(), serde_json::Value::Object(params));
    }

    #[test]
    fn dep_shape_requires_assembles_declared_dag() {
        // ADR-039 § DAG-002: `[SPEC."core.dep-shape"] requires = ["PRD"]`
        // declares doc-edge SPEC → PRD, lifting to ordering edge
        // PRD → SPEC (PRD first). The assembled DAG is `source: declared`.
        let mut config = config_with_namespaces(&["PRD", "ADR", "SPEC", "TASK"]);
        with_dep_shape(&mut config, "SPEC", &["PRD"], &[]);
        let resolution = resolve_dag(&config).unwrap();
        assert_eq!(resolution.source, DagSource::Declared);
        assert!(
            resolution.dag.edges.contains(&("PRD".to_string(), "SPEC".to_string())),
            "requires=[PRD] on SPEC must produce ordering edge PRD → SPEC; got {:?}",
            resolution.dag.edges
        );
    }

    #[test]
    fn declared_diamond_resolves_without_linearity_error() {
        // ADR-039 § DAG-001: a declared diamond (PRD → ADR, PRD → SPEC,
        // ADR → SPEC) must resolve — no linearity assumption. Expressed
        // via dep-shape: ADR requires PRD; SPEC requires PRD and ADR.
        let mut config = config_with_namespaces(&["PRD", "ADR", "SPEC"]);
        with_dep_shape(&mut config, "ADR", &["PRD"], &[]);
        with_dep_shape(&mut config, "SPEC", &["PRD", "ADR"], &[]);
        let resolution = resolve_dag(&config).unwrap();
        assert_eq!(resolution.source, DagSource::Declared);
        // PRD → SPEC is implied by PRD → ADR → SPEC and reduced away.
        assert_eq!(
            resolution.dag.edges,
            vec![
                ("ADR".to_string(), "SPEC".to_string()),
                ("PRD".to_string(), "ADR".to_string()),
            ]
        );
        assert_eq!(resolution.dag.order, vec!["PRD", "ADR", "SPEC"]);
    }

    #[test]
    fn no_declaration_falls_back_to_default() {
        // ADR-039 § DAG-007: runtime resolution is declared-or-default.
        // A config with active namespaces but no `core.dep-shape` and no
        // `[pipeline]` resolves to the default ladder — never `inferred`,
        // regardless of any document edges (which runtime no longer reads).
        let config = config_with_namespaces(&["ADR", "SPEC"]);
        let resolution = resolve_dag(&config).unwrap();
        assert_eq!(resolution.source, DagSource::Default);
        assert_eq!(resolution.dag.order, vec!["ADR", "SPEC"]);
    }

    #[test]
    fn default_ladder_when_no_edges_restricted_to_active() {
        // EARS-01.3: TASK is not active → ladder is PRD → ADR → SPEC.
        let config = config_with_namespaces(&["ADR", "PRD", "SPEC"]);
        let resolution = resolve_dag(&config).unwrap();
        assert_eq!(resolution.source, DagSource::Default);
        assert_eq!(resolution.dag.order, vec!["PRD", "ADR", "SPEC"]);
    }

    #[test]
    fn namespace_cycle_surfaces_as_status_error() {
        // ADR-039 § DAG-002/DAG-007: a cycle declared through dep-shape
        // (ADR requires SPEC and SPEC requires ADR) surfaces as a status
        // error at resolution time.
        let mut config = config_with_namespaces(&["ADR", "SPEC"]);
        with_dep_shape(&mut config, "ADR", &["SPEC"], &[]);
        with_dep_shape(&mut config, "SPEC", &["ADR"], &[]);
        let err = resolve_dag(&config).unwrap_err();
        match err {
            StatusError::Cycle { members } => assert_eq!(members, vec!["ADR", "SPEC"]),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // -- DAG-007: infer_dep_shape_requires lift-direction seam ----------

    /// Write `contents` to `path`, creating parent dirs. Mirrors the
    /// integration harness so the fixture style matches `tests/status.rs`.
    fn write_fixture(path: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn infer_dep_shape_requires_lifts_dependency_into_requires_with_correct_direction() {
        // ADR-039 § DAG-007: the init-scaffolding seam. A SPEC document
        // depending on a PRD document must lift to `SPEC requires PRD`
        // (the downstream namespace requires the upstream one), and a TASK
        // depending on the SPEC must lift to `TASK requires SPEC`. This
        // pins the lift DIRECTION: the depended-ON namespace lands in the
        // depending namespace's `requires` list, never the reverse.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_fixture(
            &tmp.path().join("ctxgrd.toml"),
            "[PRD]\nrules = []\n\n[SPEC]\nrules = []\n\n[TASK]\nrules = []\n",
        );
        write_fixture(
            &tmp.path().join("docs/prds/001-billing-reconciliation.md"),
            "---\nid: PRD-001\ntitle: Billing reconciliation\nstatus: accepted\n---\n\n# PRD-001\n",
        );
        write_fixture(
            &tmp.path().join("docs/specs/001-reconciliation-engine.md"),
            "---\nid: SPEC-001\ntitle: Reconciliation engine\nstatus: draft\ndepends_on: [PRD-001]\n---\n\n# SPEC-001\n",
        );
        write_fixture(
            &tmp.path().join("docs/tasks/001-wire-the-engine.md"),
            "---\nid: TASK-001\ntitle: Wire the engine\nstatus: doing\ndepends_on: [SPEC-001]\n---\n\n# TASK-001\n",
        );

        let requires = infer_dep_shape_requires(tmp.path()).expect("inference succeeds");

        // SPEC depends on PRD ⇒ SPEC requires PRD (direction guard).
        // TASK depends on SPEC ⇒ TASK requires SPEC.
        // PRD depends on nothing ⇒ it has no requires entry at all.
        let expected: BTreeMap<String, Vec<String>> = BTreeMap::from([
            ("SPEC".to_string(), vec!["PRD".to_string()]),
            ("TASK".to_string(), vec!["SPEC".to_string()]),
        ]);
        assert_eq!(requires, expected);
        assert!(
            !requires.contains_key("PRD"),
            "the upstream PRD must NOT acquire a requires entry — that would invert the lift; got {requires:?}"
        );
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
    fn any_gate_over_empty_namespace_is_unsatisfied() {
        // SPEC-003 EARS-03.1 (the pin): `any:` over a namespace with zero
        // counted documents is unsatisfied too — a pruned/empty lineage stage
        // holds rather than passing vacuously, matching `all:`-over-empty.
        let eval = evaluate_gate(&any_accepted(), &[]);
        assert!(!eval.done);
        assert!(eval.held.is_empty());
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
        let resolution = resolve_dag(&config).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[], None);
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
        let stages = compute_stages(&resolution_dag(&config), &config, &docs, &[diag], None);
        assert_eq!(stages[0].state, StageState::Current);
        assert_eq!(stages[0].held, vec!["SPEC-001"]);
    }

    fn resolution_dag(config: &Config) -> NamespaceDag {
        resolve_dag(config).unwrap().dag
    }

    #[test]
    fn join_stage_waits_on_unfinished_parent() {
        // EARS-02.6: SPEC's own gate is satisfied (accepted) but the
        // DESIGN parent is a draft → SPEC pending, DESIGN current. The
        // diamond is declared via dep-shape (SPEC requires ADR + DESIGN).
        let mut config = config_with_namespaces(&["ADR", "DESIGN", "SPEC"]);
        with_dep_shape(&mut config, "SPEC", &["ADR", "DESIGN"], &[]);
        let docs = vec![
            doc_with_status("ADR-001", "accepted", vec![]),
            doc_with_status("DESIGN-001", "draft", vec![]),
            doc_with_status("SPEC-001", "accepted", vec!["ADR-001", "DESIGN-001"]),
        ];
        let resolution = resolve_dag(&config).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[], None);
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
        let mut config = config_with_namespaces(&["ADR", "DESIGN", "SPEC"]);
        with_dep_shape(&mut config, "SPEC", &["ADR", "DESIGN"], &[]);
        let docs = vec![
            doc_with_status("ADR-001", "accepted", vec![]),
            doc_with_status("DESIGN-001", "accepted", vec![]),
            doc_with_status("SPEC-001", "accepted", vec!["ADR-001", "DESIGN-001"]),
        ];
        let resolution = resolve_dag(&config).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[], None);
        assert!(stages.iter().all(|s| s.state == StageState::Done));
    }

    #[test]
    fn cold_start_first_empty_stage_is_current() {
        // Default ladder PRD → ADR → SPEC → TASK with only an accepted
        // PRD: PRD done, ADR current (no accepted ADR), rest pending.
        let config = config_with_namespaces(&["PRD", "ADR", "SPEC", "TASK"]);
        let docs = vec![doc_with_status("PRD-001", "accepted", vec![])];
        let resolution = resolve_dag(&config).unwrap();
        let stages = compute_stages(&resolution.dag, &config, &docs, &[], None);
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
        let trip = sweep_tripwire(&dag, &docs, None);
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
        let trip = sweep_tripwire(&dag, &docs, None);
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
        let trip = sweep_tripwire(&dag, &docs, None);
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
        let trip = sweep_tripwire(&dag, &docs, None);
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
        let trip = sweep_tripwire(&dag, &docs, None);
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
            frontier: vec!["SPEC".to_string()],
            blockers: vec!["BUG-001".to_string()],
            blocker_stages: BTreeMap::from([("BUG-001".to_string(), vec!["SPEC".to_string()])]),
            next_action: "resolve BUG-001".to_string(),
            lineage: None,
            shared: BTreeMap::new(),
        };
        let out = render_report(&report);
        assert!(out.contains("SPEC"), "out:\n{out}");
        assert!(out.contains("blocked"), "out:\n{out}");
        assert!(out.contains("blocked by BUG-001"), "out:\n{out}");
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
        // EARS-04.2 + ADR-037 + ADR-039 § DAG-006: the JSON object carries
        // source / edges / stages (namespace, state, docs, verdict,
        // gate_met, hold) / frontier / blockers / blocker_stages /
        // next_action and nothing from the internal Report under its
        // internal field names.
        let report = Report {
            source: DagSource::Declared,
            dag: dag::chain_dag(&["ADR".to_string(), "SPEC".to_string()]),
            stages: vec![
                stage("ADR", StageState::Done, "accepted", None),
                stage("SPEC", StageState::Current, "draft", Some("SPEC-001")),
            ],
            frontier: vec!["SPEC".to_string()],
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "accept SPEC-001".to_string(),
            lineage: None,
            shared: BTreeMap::new(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
        assert_eq!(parsed["source"], "declared");
        assert_eq!(parsed["source_hint"], "order you set in ctxgrd.toml");
        // ADR-039 § DAG-006: `frontier` is a sorted array; `current` is gone.
        assert_eq!(parsed["frontier"], serde_json::json!(["SPEC"]));
        assert!(parsed.get("current").is_none(), "the `current` field is removed");
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
    fn dag_source_hint_glosses_both_variants() {
        // EARS-01.4: `source_hint` is the plain-language companion to the
        // stable `source` enum, for a person or LLM reading `--format json`.
        assert_eq!(DagSource::Declared.hint(), "order you set in ctxgrd.toml");
        assert_eq!(
            DagSource::Default.hint(),
            "ctxgrd's default order (you haven't set one)"
        );
    }

    #[test]
    fn render_json_gate_met_is_true_on_a_parent_gated_pending_stage() {
        // ADR-037 § WIRE-002: the load-bearing distinction. SPEC's own
        // gate is met (accepted+clean) but it is `pending` because a
        // parent is unfinished (EARS-02.6); `verdict` renders a
        // non-terminal token, so `gate_met` is the field that tells the
        // truth about SPEC's own gate.
        let report = Report {
            source: DagSource::Declared,
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
            frontier: vec!["ADR".to_string()],
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "accept ADR-001".to_string(),
            lineage: None,
            shared: BTreeMap::new(),
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
            frontier: vec!["ADR".to_string(), "SPEC".to_string()],
            blockers: vec!["BUG-001".to_string(), "BUG-002".to_string()],
            blocker_stages: BTreeMap::from([
                ("BUG-001".to_string(), vec!["ADR".to_string()]),
                ("BUG-002".to_string(), vec!["ADR".to_string(), "SPEC".to_string()]),
            ]),
            next_action: "resolve BUG-001".to_string(),
            lineage: None,
            shared: BTreeMap::new(),
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
    fn render_json_frontier_is_empty_when_complete() {
        // ADR-039 § DAG-006: a complete pipeline reports an empty frontier
        // (the ready set is empty); the `current` field no longer exists.
        let report = Report {
            source: DagSource::Default,
            dag: dag::chain_dag(&["ADR".to_string()]),
            stages: vec![stage("ADR", StageState::Done, "accepted", None)],
            frontier: Vec::new(),
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "pipeline complete".to_string(),
            lineage: None,
            shared: BTreeMap::new(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
        assert_eq!(parsed["frontier"], serde_json::json!([]));
        assert!(parsed.get("current").is_none());
    }

    /// A two-stage chain with one open-BUG blocker — shared fixture for
    /// the diagram renderers.
    fn diagram_report() -> Report {
        Report {
            source: DagSource::Declared,
            dag: dag::chain_dag(&["ADR".to_string(), "SPEC".to_string()]),
            stages: vec![
                stage("ADR", StageState::Blocked, "accepted", None),
                stage("SPEC", StageState::Current, "draft", Some("SPEC-001")),
            ],
            frontier: vec!["SPEC".to_string()],
            blockers: vec!["BUG-008".to_string()],
            blocker_stages: BTreeMap::from([("BUG-008".to_string(), vec!["ADR".to_string()])]),
            next_action: "resolve BUG-008".to_string(),
            lineage: None,
            shared: BTreeMap::new(),
        }
    }

    #[test]
    fn render_mermaid_emits_flowchart_with_state_classes_and_blocker() {
        let out = render_mermaid(&diagram_report());
        assert!(out.starts_with("flowchart LR\n"), "out:\n{out}");
        // Stage nodes carry state class + doc count; SPEC has 1 doc.
        assert!(
            out.contains("ADR[\"ADR: blocked (0 docs)\"]:::blocked"),
            "out:\n{out}"
        );
        assert!(
            out.contains("SPEC[\"SPEC: current (1 doc)\"]:::current"),
            "out:\n{out}"
        );
        assert!(out.contains("ADR --> SPEC"), "out:\n{out}");
        // The open BUG is a node with a sanitized id (BUG-008 -> BUG_008)
        // and a dashed `blocks` edge to the stage it cites.
        assert!(out.contains("BUG_008[\"BUG-008 (open)\"]:::bug"), "out:\n{out}");
        assert!(out.contains("BUG_008 -. blocks .-> ADR"), "out:\n{out}");
        assert!(out.contains("classDef blocked"), "out:\n{out}");
    }

    #[test]
    fn render_dot_emits_digraph_with_fill_colors_and_blocker() {
        let out = render_dot(&diagram_report());
        assert!(out.starts_with("digraph pipeline {\n"), "out:\n{out}");
        assert!(out.contains("rankdir=LR;"), "out:\n{out}");
        // Blocked ADR is filled with the blocked palette colour.
        assert!(
            out.contains("\"ADR\" [label=\"ADR\\nblocked (0 docs)\", fillcolor=\"#f6cccc\"];"),
            "out:\n{out}"
        );
        assert!(out.contains("\"ADR\" -> \"SPEC\";"), "out:\n{out}");
        // BUG nodes keep their hyphen (DOT ids are quoted) and dash-block.
        assert!(
            out.contains("\"BUG-008\" [label=\"BUG-008\\n(open)\", shape=note"),
            "out:\n{out}"
        );
        assert!(
            out.contains("\"BUG-008\" -> \"ADR\" [style=dashed, label=\"blocks\"];"),
            "out:\n{out}"
        );
        assert!(out.ends_with("}\n"), "out:\n{out}");
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
            frontier: vec!["ADR".to_string()],
            blockers: Vec::new(),
            blocker_stages: BTreeMap::new(),
            next_action: "create the first ADR document".to_string(),
            lineage: None,
            shared: BTreeMap::new(),
        };
        // No `source:` line — provenance moved to the JSON contract; the human
        // table leads straight with the per-stage rows (EARS-01.4).
        insta::assert_snapshot!(render_report(&report), @r"
        PRD   done     0 docs
        ADR   current  0 docs   needs PRD
        SPEC  pending  0 docs   needs ADR
        TASK  pending  0 docs   needs SPEC

        ready: ADR
        next: create the first ADR document

        tip: --format json for agents, mermaid/dot for diagrams
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
        // A declared diamond (ADR/DESIGN require PRD; SPEC requires both)
        // renders one edge per line — the branching shape is not a chain.
        let mut config = config_with_namespaces(&["ADR", "DESIGN", "PRD", "SPEC"]);
        with_dep_shape(&mut config, "ADR", &["PRD"], &[]);
        with_dep_shape(&mut config, "DESIGN", &["PRD"], &[]);
        with_dep_shape(&mut config, "SPEC", &["ADR", "DESIGN"], &[]);
        let resolution = resolve_dag(&config).unwrap();
        assert_eq!(
            render_text(&resolution),
            "source: declared\n\
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
