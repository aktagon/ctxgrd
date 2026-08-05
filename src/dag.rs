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

/// A node handle inside a [`DepGraph`] (ADR-064 § DAG-004). A newtype
/// over the document-slice position, so a graph node and a bare document
/// index can't be mixed by accident. One node per document:
/// `NodeIdx(i)` is `docs[i]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeIdx(usize);

/// The document-level dependency graph, built once per run
/// (ADR-064 § DAG-001).
///
/// Stores **only** the canonical pair — the node index (`id → NodeIdx`)
/// and the resolved edge adjacency — plus the unresolved entries
/// captured while resolving (DAG-003). Every other property (cycles,
/// topological order, degree) is derived on demand by a method and never
/// persisted (DAG-002). This extends ADR-029's "parse once, rules read"
/// invariant to the graph built over the documents.
pub(crate) struct DepGraph<'d> {
    /// The node table: node `NodeIdx(i)` is `docs[i]`.
    docs: &'d [Document],
    /// `id → node handle`, first occurrence winning on duplicate ids
    /// (callers care about presence, not which instance). Part of the
    /// canonical model (DAG-002). Read by [`DepGraph::index_of`] to resolve
    /// a `--lineage <ID>` selector to a node (ADR-059 § LIN-001, the first
    /// reader the field was waiting for).
    index: BTreeMap<DocumentId, NodeIdx>,
    /// Resolved dependency edges: `adjacency[i]` lists the nodes that
    /// `docs[i]` depends on. Malformed or unresolved entries are not
    /// here — they are in `unresolved`.
    adjacency: Vec<Vec<NodeIdx>>,
    /// `depends_on` entries that didn't resolve to a document in the run,
    /// in document-then-entry order (DAG-003).
    unresolved: Vec<UnresolvedRef>,
}

impl<'d> DepGraph<'d> {
    /// Build the graph in one pass over `docs`, parsing each
    /// `depends_on` entry exactly once (DAG-003): an entry that parses to
    /// an id present in the index becomes an adjacency edge, otherwise
    /// (malformed or absent) it is captured as an [`UnresolvedRef`]. An
    /// entry is therefore an edge or an unresolved reference, never both
    /// and never neither — a dangling edge stays unrepresentable.
    pub(crate) fn new(docs: &'d [Document]) -> Self {
        // Pass 1: index every id. First occurrence wins on collision —
        // presence is what matters here, not which instance.
        let mut index: BTreeMap<DocumentId, NodeIdx> = BTreeMap::new();
        for (idx, doc) in docs.iter().enumerate() {
            index.entry(doc.id.clone()).or_insert(NodeIdx(idx));
        }

        // Pass 2: partition each entry into a resolved edge or an
        // unresolved reference. The index must be complete first — a
        // document may depend on one that appears later in the slice.
        let mut adjacency: Vec<Vec<NodeIdx>> = vec![Vec::new(); docs.len()];
        let mut unresolved = Vec::new();
        for (idx, doc) in docs.iter().enumerate() {
            for entry in &doc.depends_on {
                if let Ok(id) = entry.parse::<DocumentId>() {
                    if let Some(&to) = index.get(&id) {
                        adjacency[idx].push(to);
                        continue;
                    }
                }
                unresolved.push(UnresolvedRef {
                    from_doc_idx: idx,
                    raw_entry: entry.clone(),
                });
            }
        }

        Self {
            docs,
            index,
            adjacency,
            unresolved,
        }
    }

    /// Every unresolved reference across all documents, in document-then-
    /// entry order. Self-edges (a doc depending on its own id) resolve —
    /// they surface through [`DepGraph::cycles`], not here.
    pub(crate) fn unresolved(&self) -> &[UnresolvedRef] {
        &self.unresolved
    }

    /// The resolved dependency edges of `docs[doc_idx]`, as document-slice
    /// positions (ADR-106 § DPS-001). Unresolved entries are excluded —
    /// they are [`DepGraph::unresolved`]'s to report.
    ///
    /// Yields `usize` rather than the private [`NodeIdx`] handle, matching
    /// [`DepGraph::index_of`]: the node newtype guards the graph's own
    /// internals and does not cross the module boundary. An out-of-range
    /// `doc_idx` yields nothing.
    pub(crate) fn dependencies(&self, doc_idx: usize) -> impl Iterator<Item = usize> + '_ {
        self.adjacency
            .get(doc_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|n| n.0)
    }

    /// Every cycle in the graph: every self-edge plus every non-trivial
    /// SCC. Derived on demand from the adjacency, never stored (DAG-002).
    /// Unresolved entries can't appear in a cycle — they were excluded
    /// from the adjacency at construction.
    pub(crate) fn cycles(&self) -> Vec<Cycle> {
        let mut out = Vec::new();

        // Self-edges first — one per doc where id ∈ depends_on.
        for (idx, neighbours) in self.adjacency.iter().enumerate() {
            if neighbours.contains(&NodeIdx(idx)) {
                out.push(Cycle::SelfEdge { doc_idx: idx });
            }
        }

        // Tarjan's SCC over a plain-index view of the adjacency — the
        // hand-rolled algorithm is reused unchanged (DAG-005).
        let adj: Vec<Vec<usize>> = self
            .adjacency
            .iter()
            .map(|neighbours| neighbours.iter().map(|n| n.0).collect())
            .collect();
        for mut scc in tarjan_scc(&adj) {
            if scc.len() < 2 {
                // Singletons are only cycles via a self-edge, already
                // emitted above.
                continue;
            }
            scc.sort_by(|a, b| self.docs[*a].id.cmp(&self.docs[*b].id));
            out.push(Cycle::Scc { members: scc });
        }

        out
    }

    /// The documents this graph is built over — the node table. Lets the
    /// rule layer resolve a [`Cycle`]/[`UnresolvedRef`] document index
    /// back to its [`Document`] for diagnostic formatting.
    pub(crate) fn docs(&self) -> &'d [Document] {
        self.docs
    }

    /// The node index for `id`, or `None` when no document in the run has
    /// that id (EARS-04.5). The first reader of the `index` field
    /// (ADR-059 § LIN-001) — resolves a `--lineage <ID>` selector to a node.
    pub(crate) fn index_of(&self, id: &DocumentId) -> Option<usize> {
        self.index.get(id).map(|n| n.0)
    }

    /// The transitive **dependents** of the document at `root` over the
    /// **transpose** of the `depends_on` graph: every document that
    /// transitively depends on `root`, plus `root` itself (ADR-059 §
    /// LIN-001). Because adjacency stores edges child→parent, a forward
    /// walk would yield prerequisites; a feature's members are reached only
    /// over the reverse edges, computed here by a BFS that never persists a
    /// reverse adjacency (DAG-002 — derived on demand, stdlib-only per
    /// ADR-064 § DAG-005, no `petgraph`).
    pub(crate) fn dependents(&self, root: usize) -> BTreeSet<usize> {
        // Reverse adjacency on the fly: parent → the children depending on it.
        let mut reverse: Vec<Vec<usize>> = vec![Vec::new(); self.docs.len()];
        for (child, parents) in self.adjacency.iter().enumerate() {
            for p in parents {
                reverse[p.0].push(child);
            }
        }
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        seen.insert(root);
        let mut stack = vec![root];
        while let Some(u) = stack.pop() {
            for &child in &reverse[u] {
                if seen.insert(child) {
                    stack.push(child);
                }
            }
        }
        seen
    }

    /// The lineage **roots** the document at `idx` belongs to: the
    /// documents reachable by walking `idx`'s prerequisites forward (over
    /// `depends_on`) that themselves have no outgoing edge — the tops of
    /// the dependency forest (ADR-059 § LIN-005). A document reachable from
    /// more than one such root is a shared node, and those roots are
    /// disclosed beside its stage so a lineage's "done" never silently
    /// hides a document still driven by another feature. `idx` is itself a
    /// root when it has no resolved prerequisites.
    pub(crate) fn owning_roots(&self, idx: usize) -> BTreeSet<usize> {
        let mut roots: BTreeSet<usize> = BTreeSet::new();
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        seen.insert(idx);
        let mut stack = vec![idx];
        while let Some(u) = stack.pop() {
            if self.adjacency[u].is_empty() {
                roots.insert(u);
            }
            for p in &self.adjacency[u] {
                if seen.insert(p.0) {
                    stack.push(p.0);
                }
            }
        }
        roots
    }
}

// -- Tarjan's SCC ------------------------------------------------------
//
// Textbook *recursive* Tarjan over the adjacency list. Output: every
// SCC the graph contains (including singletons — callers filter by
// length).
//
// Stack depth equals the longest `depends_on` chain, so a pathological
// corpus (a chain ~10^5 documents deep) would overflow the thread
// stack. Acceptable for one-shot CLI runs at realistic corpus sizes;
// convert `strongconnect` to the explicit-stack iterative form before
// running this in-process in a long-lived server (the LSP brief), where
// a crash takes the whole session down (BUG-027).

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
            pin: None,
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
        assert!(DepGraph::new(&docs).unresolved().is_empty());
    }

    #[test]
    fn unresolved_entry_reported_with_raw_string() {
        let docs = vec![
            doc("ADR-099", "c.md", vec!["PRD-999"]),
            doc("PRD-001", "p.md", vec![]),
        ];
        let graph = DepGraph::new(&docs);
        let refs = graph.unresolved();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].from_doc_idx, 0);
        assert_eq!(refs[0].raw_entry, "PRD-999");
    }

    #[test]
    fn malformed_entry_reported_as_unresolved() {
        let docs = vec![doc("ADR-001", "a.md", vec!["not-an-id"])];
        let graph = DepGraph::new(&docs);
        let refs = graph.unresolved();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].raw_entry, "not-an-id");
    }

    #[test]
    fn self_edge_reported_as_cycle() {
        let docs = vec![doc("ADR-001", "a.md", vec!["ADR-001"])];
        let c = DepGraph::new(&docs).cycles();
        assert_eq!(c, vec![Cycle::SelfEdge { doc_idx: 0 }]);
    }

    #[test]
    fn two_node_cycle_reported_as_scc() {
        let docs = vec![
            doc("ADR-001", "a.md", vec!["ADR-002"]),
            doc("ADR-002", "b.md", vec!["ADR-001"]),
        ];
        let c = DepGraph::new(&docs).cycles();
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
        let c = DepGraph::new(&docs).cycles();
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
        assert!(DepGraph::new(&docs).cycles().is_empty());
    }

    #[test]
    fn disjoint_cycle_and_chain_both_reported_once() {
        // ADR-001 <-> ADR-002, PRD-001 linear.
        let docs = vec![
            doc("ADR-001", "a.md", vec!["ADR-002"]),
            doc("ADR-002", "b.md", vec!["ADR-001"]),
            doc("PRD-001", "p.md", vec![]),
        ];
        let c = DepGraph::new(&docs).cycles();
        assert_eq!(c.len(), 1);
    }

    // -- ADR-059 § LIN-001/LIN-005: lineage closure over the transpose ----

    #[test]
    fn dependents_walks_the_transpose_not_the_forward_edges() {
        // PRD-3 ← SPEC-9 ← TASK-4 (edges stored child→parent: SPEC-9 deps
        // PRD-3, TASK-4 deps SPEC-9). A feature's members are the transitive
        // DEPENDENTS of its root, reachable only over the transpose.
        let docs = vec![
            doc("PRD-003", "p.md", vec![]),
            doc("SPEC-009", "s.md", vec!["PRD-003"]),
            doc("TASK-004", "t.md", vec!["SPEC-009"]),
        ];
        let graph = DepGraph::new(&docs);
        let prd = graph.index_of(&"PRD-003".parse().unwrap()).unwrap();
        // Dependents of PRD-3 = the whole feature.
        assert_eq!(graph.dependents(prd), BTreeSet::from([0, 1, 2]));
    }

    #[test]
    fn dependents_of_a_leaf_returns_only_itself() {
        // Fixture 3: the lineage of a leaf TASK is just that TASK — no
        // forward walk into its prerequisites (EARS-04.1).
        let docs = vec![
            doc("PRD-003", "p.md", vec![]),
            doc("SPEC-009", "s.md", vec!["PRD-003"]),
            doc("TASK-004", "t.md", vec!["SPEC-009"]),
        ];
        let graph = DepGraph::new(&docs);
        let task = graph.index_of(&"TASK-004".parse().unwrap()).unwrap();
        assert_eq!(graph.dependents(task), BTreeSet::from([2]));
    }

    #[test]
    fn owning_roots_discloses_a_shared_nodes_two_roots() {
        // SPEC-9 depended on by PRD-3 and PRD-7 → it belongs to both
        // lineage roots (LIN-005). owning_roots walks prerequisites forward.
        let docs = vec![
            doc("PRD-003", "p3.md", vec![]),
            doc("PRD-007", "p7.md", vec![]),
            doc("SPEC-009", "s.md", vec!["PRD-003", "PRD-007"]),
        ];
        let graph = DepGraph::new(&docs);
        let spec = graph.index_of(&"SPEC-009".parse().unwrap()).unwrap();
        assert_eq!(graph.owning_roots(spec), BTreeSet::from([0, 1]));
    }

    #[test]
    fn owning_roots_of_a_root_is_itself() {
        let docs = vec![doc("PRD-003", "p.md", vec![])];
        let graph = DepGraph::new(&docs);
        assert_eq!(graph.owning_roots(0), BTreeSet::from([0]));
    }

    #[test]
    fn index_of_resolves_present_and_misses_absent() {
        let docs = vec![doc("PRD-003", "p.md", vec![])];
        let graph = DepGraph::new(&docs);
        assert_eq!(graph.index_of(&"PRD-003".parse().unwrap()), Some(0));
        assert_eq!(graph.index_of(&"PRD-999".parse().unwrap()), None);
    }

}
