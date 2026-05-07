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

use std::collections::BTreeMap;

use crate::document::Document;
use crate::id::DocumentId;

/// A reference from one document to another that the graph couldn't
/// satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedRef {
    /// Index of the document whose `depends_on` list contained the
    /// unresolved entry.
    pub from_doc_idx: usize,
    /// The raw string from `depends_on` — echoed verbatim into the
    /// diagnostic.
    pub raw_entry: String,
}

/// A cycle detected in the `depends_on` graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cycle {
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
pub fn unresolved_refs(docs: &[Document]) -> Vec<UnresolvedRef> {
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
pub fn cycles(docs: &[Document]) -> Vec<Cycle> {
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
}
