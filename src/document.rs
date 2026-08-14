//! Document assembly + ID uniqueness. CORE-002/003 in the brief.
//!
//! A [`Document`] is the in-memory record the kernel passes around once
//! a source has produced its envelope and the kernel has reconciled it
//! with the body frontmatter. Sources populate the typed [`Ast`] on
//! their envelopes per CORE-006; the AST rides through this struct as
//! an `Option`, because external sources are free to omit it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::ast::Ast;
use crate::frontmatter::Pin;
use crate::id::DocumentId;

/// In-memory representation of a single document the kernel has ingested.
///
/// `raw_id` preserves the exact string the source authored — diagnostics
/// echo this, not the canonicalized `DocumentId::to_string()`. `location`
/// is rendered relative to the lint root where possible (REP-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: DocumentId,
    pub raw_id: String,
    pub location: String,
    /// The file this document was read from, resolved once at ingest
    /// (ADR-123 § LOC-001). `Some(path)` when the document is backed by
    /// a readable file under the lint root — the walker sets it from the
    /// path it already opened (LOC-002), envelope conversion probes
    /// `root.join(&location)` exactly once (LOC-003). `None` when
    /// `location` is a label rather than a path, which ADR-005 §
    /// the envelope schema explicitly permits ("URL, path, or other
    /// identifier").
    ///
    /// Consumers that need a real path MUST read this field rather than
    /// re-deriving one from `location`; doing the latter is what produced
    /// BUG-059 and BUG-060.
    pub file: Option<PathBuf>,
    pub depends_on: Vec<String>,
    /// 1-indexed line of every top-level YAML key in the frontmatter
    /// block, keyed by key name. Rules (`core.dep-resolved`,
    /// `core.allowed-values`) use this map to anchor diagnostics at
    /// the offending YAML key.
    pub frontmatter_lines: BTreeMap<String, u32>,
    pub metadata: BTreeMap<String, Value>,
    /// The parsed `pin` block (ADR-040 § PIN-001), or `None` when the
    /// document declares no pin. Carried on the shared `Document` so the
    /// `core.commit-freshness` rule reads it without re-parsing the body
    /// — the git query stays in the rule layer (PIN-006), this is just
    /// the declarative data parsed once at ingest (PIP-001).
    pub pin: Option<Pin>,
    /// Typed AST produced by the source (CORE-006). `None` when the
    /// source did not populate `ast`; in that case the structural
    /// rules (`core.cross-ref`, `core.required-headings`) silently
    /// no-op per CORE-005.
    pub ast: Option<Ast>,
    /// Full document body. Needed later to materialize a temp file
    /// for external rules on non-filesystem sources (EXT-002 step 2).
    pub body: String,
}

/// One collision group detected by [`find_id_collisions`].
///
/// Every entry in `locations` is the `location` field of a colliding
/// document. The vector is sorted so the reporter output is stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdCollision {
    pub id: DocumentId,
    pub locations: Vec<String>,
}

/// Group documents by their parsed `(namespace, number)` pair and
/// return only the groups with two or more members.
///
/// Output is sorted by `DocumentId` (namespace then number) so the
/// caller's diagnostic output is byte-reproducible across runs.
pub(crate) fn find_id_collisions(docs: &[Document]) -> Vec<IdCollision> {
    let mut buckets: BTreeMap<DocumentId, Vec<String>> = BTreeMap::new();
    for d in docs {
        buckets
            .entry(d.id.clone())
            .or_default()
            .push(d.location.clone());
    }
    buckets
        .into_iter()
        .filter(|(_, locs)| locs.len() >= 2)
        .map(|(id, mut locations)| {
            locations.sort();
            IdCollision { id, locations }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(raw_id: &str, location: &str) -> Document {
        let id: DocumentId = raw_id.parse().expect("valid id in test fixture");
        Document {
            id,
            raw_id: raw_id.to_owned(),
            location: location.to_owned(),
            file: None,
            depends_on: Vec::new(),
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn no_collisions_when_all_ids_unique() {
        let docs = vec![
            doc("ADR-001", "adrs/ADR-001-a.md"),
            doc("ADR-002", "adrs/ADR-002-b.md"),
            doc("PRD-001", "prds/PRD-001-c.md"),
        ];
        assert!(find_id_collisions(&docs).is_empty());
    }

    #[test]
    fn simple_duplicate_reported_once_with_all_locations() {
        let docs = vec![
            doc("ADR-001", "adrs/ADR-001-first.md"),
            doc("ADR-001", "attic/ADR-001-second.md"),
        ];
        let cols = find_id_collisions(&docs);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].id, DocumentId::new("ADR", 1));
        assert_eq!(
            cols[0].locations,
            vec!["adrs/ADR-001-first.md", "attic/ADR-001-second.md"]
        );
    }

    #[test]
    fn leading_zero_variants_count_as_same_id() {
        let docs = vec![doc("ADR-01", "adrs/one.md"), doc("ADR-001", "adrs/two.md")];
        let cols = find_id_collisions(&docs);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].id, DocumentId::new("ADR", 1));
        assert_eq!(cols[0].locations.len(), 2);
    }

    #[test]
    fn three_way_collision_lists_all_three_sorted() {
        let docs = vec![
            doc("ADR-5", "c.md"),
            doc("ADR-5", "a.md"),
            doc("ADR-5", "b.md"),
        ];
        let cols = find_id_collisions(&docs);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].locations, vec!["a.md", "b.md", "c.md"]);
    }

    #[test]
    fn different_namespaces_same_number_do_not_collide() {
        let docs = vec![doc("ADR-1", "adrs/a.md"), doc("PRD-1", "prds/b.md")];
        assert!(find_id_collisions(&docs).is_empty());
    }

    #[test]
    fn multiple_collision_groups_sorted_by_id() {
        let docs = vec![
            doc("PRD-1", "prds/a.md"),
            doc("ADR-2", "adrs/x.md"),
            doc("ADR-2", "adrs/y.md"),
            doc("PRD-1", "prds/b.md"),
        ];
        let cols = find_id_collisions(&docs);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].id, DocumentId::new("ADR", 2));
        assert_eq!(cols[1].id, DocumentId::new("PRD", 1));
    }

    #[test]
    fn document_ast_is_optional_passthrough() {
        let mut d = doc("ADR-1", "a.md");
        d.ast = Some(Ast::default());
        assert!(d.ast.is_some());
        assert!(d.metadata.is_empty());
        assert!(d.depends_on.is_empty());
    }
}
