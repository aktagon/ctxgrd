//! Dependency graph over documents. CORE-004 in the brief.
//!
//! Documents reference each other via `depends_on: [<ID>, ...]`.
//! Two kinds of integrity violation show up:
//!
//! 1. **Unresolved** — the ID string either doesn't parse or doesn't
//!    match any document present in the run → `core.dep-resolved`.
//! 2. **Cyclic** — a self-edge or a non-trivial strongly-connected
//!    component of two or more nodes → `core.dep-cycle` (one per
//!    self-edge, one per SCC).
//!
//! This module is pure graph: it returns structured results, not
//! [`Diagnostic`]s, so the rule layer can format messages and attach
//! locations.

use std::collections::{BTreeMap, BTreeSet};

use crate::document::Document;
use crate::id::DocumentId;

/// A reference from one document to another that the graph couldn't
/// satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnresolvedRef {
    /// Index of the document whose `depends_on` list contained the
    /// unresolved entry.
    pub from_doc_idx: usize,
    /// The raw string from `depends_on` — echoed verbatim into the
    /// diagnostic.
    pub raw_entry: String,
}

/// A cycle detected in the `depends_on` graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cycle {
    /// `doc_idx` depends on itself directly.
    SelfEdge { doc_idx: usize },
    /// Two or more documents form a non-trivial strongly-connected
    /// component. Members are sorted by `DocumentId` for stable
    /// diagnostic output.
    Scc { members: Vec<usize> },
}

/// Every unresolved reference across all documents, in document-then-
/// entry order. Self-edges (a doc depending on its own id) are
/// resolved (they find themselves) — they only surface through
/// [`cycles`] below.
pub(crate) fn unresolved_refs(docs: &[Document]) -> Vec<UnresolvedRef> {
    let index = build_index(docs);
    let mut out = Vec::new();
    for (idx, doc) in docs.iter().enumerate() {
        for entry in &doc.depends_on {
            let parsed: Result<DocumentId, _> = entry.parse();
            let resolved = parsed.is_ok_and(|id| index.contains_key(&id));
            if !resolved {
                out.push(UnresolvedRef {
                    from_doc_idx: idx,
                    raw_entry: entry.clone(),
                });
            }
        }
    }
    out
}

/// Every cycle in the `depends_on` graph: every self-edge + every
/// non-trivial SCC. Unresolved entries are silently ignored because
/// they're reported by [`unresolved_refs`] already.
pub(crate) fn cycles(docs: &[Document]) -> Vec<Cycle> {
    let index = build_index(docs);
    let adj = adjacency_list(docs, &index);
    let mut out = Vec::new();

    // Self-edges first — one per doc where id ∈ depends_on.
    for (idx, neighbours) in adj.iter().enumerate() {
        if neighbours.contains(&idx) {
            out.push(Cycle::SelfEdge { doc_idx: idx });
        }
    }

    // Tarjan's SCC for the rest.
    let sccs = tarjan_scc(&adj);
    for mut scc in sccs {
        if scc.len() < 2 {
            // Singletons are only cycles if they have a self-edge, and
            // those were already emitted above.
            continue;
        }
        scc.sort_by(|a, b| docs[*a].id.cmp(&docs[*b].id));
        out.push(Cycle::Scc { members: scc });
    }

    out
}

// -- Namespace-level DAG (SPEC-002 § DAG resolution) --------------------
//
// `ctxgrd status` folds the document-level dep edges into a
// namespace-level DAG. Same module because it is the same pure-graph
// concern: structured results in, no `Diagnostic`s out.

/// Namespace-level DAG resolved for `ctxgrd status` (SPEC-002
/// EARS-01.*). Built from a declared `[pipeline].stages` chain
/// ([`chain_dag`]) or by lifting resolved document dep edges
/// ([`infer_namespace_dag`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamespaceDag {
    /// Namespaces in topological order, sibling ties broken by
    /// namespace name (EARS-01.6).
    pub(crate) order: Vec<String>,
    /// `(from, to)` edges after transitive reduction. Deterministic:
    /// name-sorted for inferred DAGs, declaration order for chains.
    pub(crate) edges: Vec<(String, String)>,
}

/// A cycle in the lifted namespace graph (EARS-01.5). `members` is
/// sorted by namespace name for stable output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamespaceCycle {
    pub(crate) members: Vec<String>,
}

/// Infer the namespace DAG from resolved document dep edges
/// (EARS-01.2): lift each cross-namespace edge `A deps B` to
/// `ns(B) → ns(A)`, fail loudly on a namespace cycle (EARS-01.5),
/// then take the transitive reduction.
///
/// Intra-namespace edges (an ADR superseding another ADR) carry no
/// pipeline-order information and are skipped — lifting them would
/// self-loop every such namespace into a false EARS-01.5 cycle.
/// Unresolved or malformed entries are skipped too: `core.dep-resolved`
/// already reports them, and the lift only folds over edges that
/// resolve. No documents/edges at all yields an empty DAG — the caller
/// falls back to the default ladder (EARS-01.3).
pub(crate) fn infer_namespace_dag(docs: &[Document]) -> Result<NamespaceDag, NamespaceCycle> {
    let index = build_index(docs);

    let mut lifted: BTreeSet<(String, String)> = BTreeSet::new();
    for doc in docs {
        for entry in &doc.depends_on {
            let Ok(id) = entry.parse::<DocumentId>() else {
                continue;
            };
            if !index.contains_key(&id) || id.namespace == doc.id.namespace {
                continue;
            }
            lifted.insert((id.namespace.clone(), doc.id.namespace.clone()));
        }
    }

    // Index the namespaces. `nodes` is name-sorted, so node-index
    // order IS the EARS-01.6 tie-break order.
    let nodes: Vec<String> = lifted
        .iter()
        .flat_map(|(from, to)| [from.clone(), to.clone()])
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let idx: BTreeMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let edge_idx: Vec<(usize, usize)> = lifted
        .iter()
        .map(|(from, to)| (idx[from.as_str()], idx[to.as_str()]))
        .collect();
    for &(u, v) in &edge_idx {
        adj[u].push(v);
    }

    // Cycle check (EARS-01.5). Self-loops are impossible here (intra-
    // namespace edges were skipped), so any SCC of 2+ is a cycle.
    for mut scc in tarjan_scc(&adj) {
        if scc.len() >= 2 {
            scc.sort_unstable(); // index order == name order
            return Err(NamespaceCycle {
                members: scc.into_iter().map(|i| nodes[i].clone()).collect(),
            });
        }
    }

    // Transitive reduction: a direct edge implied by a longer path is
    // redundant. Edge occurrence counts are deliberately unused
    // (SPEC-002 § Out of scope).
    let kept: Vec<(usize, usize)> = edge_idx
        .iter()
        .copied()
        .filter(|&(u, v)| !reachable_avoiding(&adj, u, v))
        .collect();

    // Topological order via Kahn's algorithm; the ready set is a
    // BTreeSet over name-ordered indices, so siblings pop in name
    // order (EARS-01.6).
    let mut indegree = vec![0usize; nodes.len()];
    let mut reduced_adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for &(u, v) in &kept {
        indegree[v] += 1;
        reduced_adj[u].push(v);
    }
    let mut ready: BTreeSet<usize> = (0..nodes.len()).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(&u) = ready.iter().next() {
        ready.remove(&u);
        order.push(nodes[u].clone());
        for &v in &reduced_adj[u] {
            indegree[v] -= 1;
            if indegree[v] == 0 {
                ready.insert(v);
            }
        }
    }

    let edges = kept
        .into_iter()
        .map(|(u, v)| (nodes[u].clone(), nodes[v].clone()))
        .collect();
    Ok(NamespaceDag { order, edges })
}

/// Build the trivial chain DAG for an ordered stage list — the shape
/// of a declared `[pipeline].stages` (EARS-01.1) and of the built-in
/// default ladder (EARS-01.3).
pub(crate) fn chain_dag(stages: &[String]) -> NamespaceDag {
    let edges = stages
        .windows(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect();
    NamespaceDag {
        order: stages.to_vec(),
        edges,
    }
}

/// Is `to` reachable from `from` without walking the direct edge
/// `from → to`? True means that direct edge is transitively redundant.
fn reachable_avoiding(adj: &[Vec<usize>], from: usize, to: usize) -> bool {
    let mut seen = vec![false; adj.len()];
    seen[from] = true;
    let mut stack = vec![from];
    while let Some(u) = stack.pop() {
        for &w in &adj[u] {
            if u == from && w == to {
                continue; // the edge under test
            }
            if w == to {
                return true;
            }
            if !seen[w] {
                seen[w] = true;
                stack.push(w);
            }
        }
    }
    false
}

fn build_index(docs: &[Document]) -> BTreeMap<DocumentId, usize> {
    // When duplicate ids collide we keep the first — callers care
    // about presence here, not which instance.
    let mut map = BTreeMap::new();
    for (idx, doc) in docs.iter().enumerate() {
        map.entry(doc.id.clone()).or_insert(idx);
    }
    map
}

fn adjacency_list(docs: &[Document], index: &BTreeMap<DocumentId, usize>) -> Vec<Vec<usize>> {
    docs.iter()
        .map(|doc| {
            doc.depends_on
                .iter()
                .filter_map(|entry| entry.parse::<DocumentId>().ok())
                .filter_map(|id| index.get(&id).copied())
                .collect()
        })
        .collect()
}

// -- Tarjan's SCC ------------------------------------------------------
//
// Straightforward iterative Tarjan over the adjacency list. Output:
// every SCC the graph contains (including singletons — callers filter
// by length).

fn tarjan_scc(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut state = TarjanState {
        index: vec![usize::MAX; n],
        lowlink: vec![0usize; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        next_index: 0,
        sccs: Vec::new(),
    };
    for v in 0..n {
        if state.index[v] == usize::MAX {
            strongconnect(v, adj, &mut state);
        }
    }
    state.sccs
}

struct TarjanState {
    index: Vec<usize>,
    lowlink: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    next_index: usize,
    sccs: Vec<Vec<usize>>,
}

fn strongconnect(v: usize, adj: &[Vec<usize>], s: &mut TarjanState) {
    s.index[v] = s.next_index;
    s.lowlink[v] = s.next_index;
    s.next_index += 1;
    s.stack.push(v);
    s.on_stack[v] = true;

    for &w in &adj[v] {
        if s.index[w] == usize::MAX {
            strongconnect(w, adj, s);
            s.lowlink[v] = s.lowlink[v].min(s.lowlink[w]);
        } else if s.on_stack[w] {
            s.lowlink[v] = s.lowlink[v].min(s.index[w]);
        }
    }

    if s.lowlink[v] == s.index[v] {
        let mut scc = Vec::new();
        loop {
            let w = s.stack.pop().expect("stack non-empty during SCC close");
            s.on_stack[w] = false;
            scc.push(w);
            if w == v {
                break;
            }
        }
        s.sccs.push(scc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Ast;

    fn doc(raw_id: &str, location: &str, depends_on: Vec<&str>) -> Document {
        Document {
            id: raw_id.parse().expect("valid id"),
            raw_id: raw_id.to_owned(),
            location: location.to_owned(),
            depends_on: depends_on.into_iter().map(String::from).collect(),
            frontmatter_lines: Default::default(),
            metadata: Default::default(),
            ast: Some(Ast::default()),
            body: String::new(),
        }
    }

    #[test]
    fn no_unresolved_when_every_dep_is_present() {
        let docs = vec![
            doc("ADR-001", "a.md", vec!["PRD-001"]),
            doc("PRD-001", "b.md", vec![]),
        ];
        assert!(unresolved_refs(&docs).is_empty());
    }

    #[test]
    fn unresolved_entry_reported_with_raw_string() {
        let docs = vec![
            doc("ADR-099", "c.md", vec!["PRD-999"]),
            doc("PRD-001", "p.md", vec![]),
        ];
        let refs = unresolved_refs(&docs);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].from_doc_idx, 0);
        assert_eq!(refs[0].raw_entry, "PRD-999");
    }

    #[test]
    fn malformed_entry_reported_as_unresolved() {
        let docs = vec![doc("ADR-001", "a.md", vec!["not-an-id"])];
        let refs = unresolved_refs(&docs);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].raw_entry, "not-an-id");
    }

    #[test]
    fn self_edge_reported_as_cycle() {
        let docs = vec![doc("ADR-001", "a.md", vec!["ADR-001"])];
        let c = cycles(&docs);
        assert_eq!(c, vec![Cycle::SelfEdge { doc_idx: 0 }]);
    }

    #[test]
    fn two_node_cycle_reported_as_scc() {
        let docs = vec![
            doc("ADR-001", "a.md", vec!["ADR-002"]),
            doc("ADR-002", "b.md", vec!["ADR-001"]),
        ];
        let c = cycles(&docs);
        assert_eq!(c.len(), 1);
        match &c[0] {
            Cycle::Scc { members } => assert_eq!(members, &vec![0, 1]),
            other => panic!("expected Scc, got {:?}", other),
        }
    }

    #[test]
    fn three_node_cycle_reported_as_scc_sorted_by_id() {
        // Create a 3-cycle with ids out of insertion order to test sort.
        let docs = vec![
            doc("ADR-003", "c.md", vec!["ADR-001"]),
            doc("ADR-001", "a.md", vec!["ADR-002"]),
            doc("ADR-002", "b.md", vec!["ADR-003"]),
        ];
        let c = cycles(&docs);
        assert_eq!(c.len(), 1);
        match &c[0] {
            Cycle::Scc { members } => {
                let ids: Vec<&DocumentId> = members.iter().map(|i| &docs[*i].id).collect();
                assert_eq!(
                    ids,
                    vec![
                        &DocumentId::new("ADR", 1),
                        &DocumentId::new("ADR", 2),
                        &DocumentId::new("ADR", 3),
                    ]
                );
            }
            other => panic!("expected Scc, got {:?}", other),
        }
    }

    #[test]
    fn acyclic_dag_produces_no_cycles() {
        let docs = vec![
            doc("ADR-001", "a.md", vec!["PRD-001"]),
            doc("PRD-001", "p.md", vec!["SPR-001"]),
            doc("SPR-001", "s.md", vec![]),
        ];
        assert!(cycles(&docs).is_empty());
    }

    #[test]
    fn disjoint_cycle_and_chain_both_reported_once() {
        // ADR-001 <-> ADR-002, PRD-001 linear.
        let docs = vec![
            doc("ADR-001", "a.md", vec!["ADR-002"]),
            doc("ADR-002", "b.md", vec!["ADR-001"]),
            doc("PRD-001", "p.md", vec![]),
        ];
        let c = cycles(&docs);
        assert_eq!(c.len(), 1);
    }

    // -- SPEC-002 T1.2: namespace DAG inference (EARS-01.2/01.5/01.6) ----

    fn edge(from: &str, to: &str) -> (String, String) {
        (from.to_string(), to.to_string())
    }

    #[test]
    fn lift_dedups_cross_namespace_edges() {
        // Two ADRs each depending on the same PRD lift to ONE
        // namespace edge PRD → ADR (EARS-01.2).
        let docs = vec![
            doc("PRD-001", "p.md", vec![]),
            doc("ADR-001", "a.md", vec!["PRD-001"]),
            doc("ADR-002", "b.md", vec!["PRD-001"]),
        ];
        let dag = infer_namespace_dag(&docs).unwrap();
        assert_eq!(dag.edges, vec![edge("PRD", "ADR")]);
        assert_eq!(dag.order, vec!["PRD", "ADR"]);
    }

    #[test]
    fn lift_skips_intra_namespace_edges() {
        // An ADR superseding another ADR carries no pipeline-order
        // information — it must not lift to an ADR → ADR self-loop
        // (which EARS-01.5 would then misreport as a cycle).
        let docs = vec![
            doc("ADR-001", "a.md", vec![]),
            doc("ADR-002", "b.md", vec!["ADR-001"]),
        ];
        let dag = infer_namespace_dag(&docs).unwrap();
        assert!(dag.edges.is_empty());
        assert!(dag.order.is_empty());
    }

    #[test]
    fn lift_ignores_unresolved_and_malformed_deps() {
        // core.dep-resolved already reports these; the lift only
        // folds over edges that actually resolve.
        let docs = vec![doc("ADR-001", "a.md", vec!["PRD-999", "not-an-id"])];
        let dag = infer_namespace_dag(&docs).unwrap();
        assert!(dag.edges.is_empty());
    }

    #[test]
    fn transitive_reduction_removes_direct_edge() {
        // SPEC-002 § Acceptance scenario 2 shape: SPEC depends on both
        // ADR and PRD directly; PRD → SPEC is implied by PRD → ADR →
        // SPEC and must be reduced away (EARS-01.2).
        let docs = vec![
            doc("PRD-001", "p.md", vec![]),
            doc("ADR-001", "a.md", vec!["PRD-001"]),
            doc("SPEC-001", "s.md", vec!["ADR-001", "PRD-001"]),
        ];
        let dag = infer_namespace_dag(&docs).unwrap();
        assert_eq!(
            dag.edges,
            vec![edge("ADR", "SPEC"), edge("PRD", "ADR")],
            "the direct PRD → SPEC edge must be reduced away"
        );
        assert_eq!(dag.order, vec!["PRD", "ADR", "SPEC"]);
    }

    #[test]
    fn namespace_cycle_is_loud_and_sorted() {
        // SPEC-001 depends on ADR-001 while ADR-002 depends on
        // SPEC-001 → ADR ↔ SPEC at namespace level (EARS-01.5).
        let docs = vec![
            doc("ADR-001", "a.md", vec![]),
            doc("SPEC-001", "s.md", vec!["ADR-001"]),
            doc("ADR-002", "b.md", vec!["SPEC-001"]),
        ];
        let err = infer_namespace_dag(&docs).unwrap_err();
        assert_eq!(err.members, vec!["ADR", "SPEC"]);
    }

    #[test]
    fn order_breaks_sibling_ties_by_name() {
        // PRD → {DESIGN, ADR} → SPEC: ADR and DESIGN are unordered
        // siblings; EARS-01.6 says name order, so ADR first.
        let docs = vec![
            doc("PRD-001", "p.md", vec![]),
            doc("DESIGN-001", "d.md", vec!["PRD-001"]),
            doc("ADR-001", "a.md", vec!["PRD-001"]),
            doc("SPEC-001", "s.md", vec!["ADR-001", "DESIGN-001"]),
        ];
        let dag = infer_namespace_dag(&docs).unwrap();
        assert_eq!(dag.order, vec!["PRD", "ADR", "DESIGN", "SPEC"]);
        assert_eq!(
            dag.edges,
            vec![
                edge("ADR", "SPEC"),
                edge("DESIGN", "SPEC"),
                edge("PRD", "ADR"),
                edge("PRD", "DESIGN"),
            ]
        );
    }

    #[test]
    fn chain_dag_builds_consecutive_edges() {
        let stages: Vec<String> = ["PRD", "ADR", "SPEC", "TASK"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let dag = chain_dag(&stages);
        assert_eq!(dag.order, stages);
        assert_eq!(
            dag.edges,
            vec![
                edge("PRD", "ADR"),
                edge("ADR", "SPEC"),
                edge("SPEC", "TASK"),
            ]
        );
    }

    #[test]
    fn chain_dag_single_stage_has_no_edges() {
        let dag = chain_dag(&["ADR".to_string()]);
        assert_eq!(dag.order, vec!["ADR"]);
        assert!(dag.edges.is_empty());
    }
}
