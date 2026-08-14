//! Document envelope — the wire format external sources emit.
//!
//! Each line of an external source's stdout is one [`Envelope`]
//! serialised as JSON. The kernel parses, validates, and converts
//! envelopes to in-memory [`Document`]s using the same metadata-merge
//! semantics (CORE-002) that apply to the built-in `markdown-file`
//! source.
//!
//! `markdown-file` does NOT go through this module — that source
//! produces `Document`s directly in-process for efficiency. Envelopes
//! exist only to give subprocess sources a typed wire contract.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::ast::Ast;
use crate::document::Document;
use crate::frontmatter::{self, Frontmatter, FrontmatterError};
use crate::id::{DocumentId, ParseIdError};

/// The JSON shape published by external sources on stdout.
///
/// Field defaults let a source emit the minimal `{"id","body","location"}`
/// trio and have the kernel fill in the rest — one less thing for shell
/// script authors to get right.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Envelope {
    pub id: String,
    pub body: String,
    pub location: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, Value>,
    #[serde(default)]
    pub ast: Option<Ast>,
}

/// Why an envelope didn't make it into the doc set.
///
/// Both variants map to user-visible diagnostics — `IdMalformed` to
/// `core.id`, `Frontmatter` to `core.frontmatter`. The kernel decides
/// the routing; this enum just names what went wrong.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub(crate) enum EnvelopeError {
    #[error("envelope id {raw_id:?} does not match the required pattern")]
    IdMalformed { raw_id: String },
    /// Body frontmatter was present but failed to parse.
    /// Missing-fence is NOT an error here — external source bodies
    /// routinely have no frontmatter (JIRA tickets, plain prose).
    #[error(transparent)]
    Frontmatter(#[from] FrontmatterError),
}

impl Envelope {
    /// Materialise this envelope as a [`Document`].
    ///
    /// Steps:
    /// 1. Parse `id` into a [`DocumentId`].
    /// 2. Try to parse the body's frontmatter. A missing fence is fine
    ///    — not every external source produces markdown with
    ///    frontmatter. A fence that exists but doesn't parse is a
    ///    real error; it surfaces as `core.frontmatter`.
    /// 3. Merge `extra ⊕ body.frontmatter` with frontmatter winning
    ///    on key conflict (CORE-002).
    /// 4. Copy the AST through unchanged.
    /// 5. Resolve `file` by probing `root.join(&location)` exactly once
    ///    (ADR-123 § LOC-003). A source-supplied `location` is only
    ///    *sometimes* a path — ADR-005 § the envelope schema allows any
    ///    identifier — so the filesystem is the only arbiter, and the
    ///    ingest boundary is where that impurity belongs (ADR-040 §
    ///    PIN-006). Downstream readers are then pure.
    pub(crate) fn into_document(self, root: &Path) -> Result<Document, EnvelopeError> {
        let id: DocumentId =
            self.id
                .parse()
                .map_err(|_: ParseIdError| EnvelopeError::IdMalformed {
                    raw_id: self.id.clone(),
                })?;

        let (metadata, frontmatter_lines, pin) = match Frontmatter::parse_with_lines(&self.body) {
            Ok((fm, lines)) => {
                let merged = frontmatter::merge_metadata(&self.extra, &fm.metadata);
                (merged, lines, fm.pin)
            }
            Err(FrontmatterError::MissingFence) => (self.extra.clone(), BTreeMap::new(), None),
            Err(e) => return Err(EnvelopeError::Frontmatter(e)),
        };

        let on_disk = root.join(&self.location);
        let file = on_disk.is_file().then_some(on_disk);

        Ok(Document {
            id,
            raw_id: self.id,
            location: self.location,
            file,
            depends_on: self.depends_on,
            frontmatter_lines,
            metadata,
            pin,
            ast: self.ast,
            body: self.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_through_json_serde() {
        let env = Envelope {
            id: "ADR-001".into(),
            body: "---\nid: ADR-001\n---\n# Heading\n".into(),
            location: "adrs/ADR-001.md".into(),
            depends_on: vec!["PRD-001".into()],
            extra: [("status".to_string(), json!("accepted"))].into(),
            ast: None,
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "ADR-001");
        assert_eq!(back.depends_on, vec!["PRD-001"]);
        assert_eq!(back.extra.get("status"), Some(&json!("accepted")));
    }

    #[test]
    fn missing_optional_fields_default() {
        // A minimal envelope — just the three required fields.
        let json = r#"{"id":"JIRA-1","body":"body text","location":"x"}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        assert!(env.depends_on.is_empty());
        assert!(env.extra.is_empty());
        assert!(env.ast.is_none());
    }

    #[test]
    fn into_document_merges_extra_and_frontmatter() {
        let env = Envelope {
            id: "ADR-001".into(),
            body: "---\nid: ADR-001\ntitle: Doc\n---\n".into(),
            location: "x.md".into(),
            depends_on: vec![],
            extra: [
                ("status".to_string(), json!("from-extra")),
                ("priority".to_string(), json!("P1")),
            ]
            .into(),
            ast: None,
        };
        let doc = env
            .into_document(Path::new("/nonexistent-ctxgrd-test-root"))
            .unwrap();
        assert_eq!(doc.id, DocumentId::new("ADR", 1));
        // frontmatter key → comes from body
        assert_eq!(doc.metadata.get("title"), Some(&json!("Doc")));
        // extra-only key → preserved
        assert_eq!(doc.metadata.get("priority"), Some(&json!("P1")));
        // body has no `status` → extra value survives
        assert_eq!(doc.metadata.get("status"), Some(&json!("from-extra")));
    }

    #[test]
    fn into_document_frontmatter_overrides_extra() {
        let env = Envelope {
            id: "ADR-001".into(),
            body: "---\nid: ADR-001\nstatus: from-frontmatter\n---\n".into(),
            location: "x.md".into(),
            depends_on: vec![],
            extra: [("status".to_string(), json!("from-extra"))].into(),
            ast: None,
        };
        let doc = env
            .into_document(Path::new("/nonexistent-ctxgrd-test-root"))
            .unwrap();
        assert_eq!(doc.metadata.get("status"), Some(&json!("from-frontmatter")));
    }

    #[test]
    fn into_document_accepts_body_without_frontmatter() {
        // JIRA-style body with a plain heading, no frontmatter.
        let env = Envelope {
            id: "JIRA-100".into(),
            body: "# A plain heading\n\nSome text.\n".into(),
            location: "https://jira.example.com/browse/JIRA-100".into(),
            depends_on: vec!["PRD-001".into()],
            extra: [("status".to_string(), json!("Open"))].into(),
            ast: None,
        };
        let doc = env
            .into_document(Path::new("/nonexistent-ctxgrd-test-root"))
            .unwrap();
        assert_eq!(doc.id.namespace, "JIRA");
        assert_eq!(doc.id.number, 100);
        assert_eq!(doc.metadata.get("status"), Some(&json!("Open")));
        assert!(doc.frontmatter_lines.is_empty());
    }

    #[test]
    fn malformed_id_surfaces() {
        let env = Envelope {
            id: "not-an-id".into(),
            body: "".into(),
            location: "x".into(),
            depends_on: vec![],
            extra: BTreeMap::new(),
            ast: None,
        };
        match env.into_document(Path::new("/nonexistent-ctxgrd-test-root")) {
            Err(EnvelopeError::IdMalformed { raw_id }) => assert_eq!(raw_id, "not-an-id"),
            other => panic!("expected IdMalformed, got {other:?}"),
        }
    }

    #[test]
    fn broken_frontmatter_bubbles_up() {
        // Fence present, YAML broken → returned as Frontmatter error.
        let env = Envelope {
            id: "ADR-001".into(),
            body: "---\ntags: [unterminated\n---\n".into(),
            location: "x.md".into(),
            depends_on: vec![],
            extra: BTreeMap::new(),
            ast: None,
        };
        assert!(matches!(
            env.into_document(Path::new("/nonexistent-ctxgrd-test-root")),
            Err(EnvelopeError::Frontmatter(_))
        ));
    }

    fn env_at(location: &str) -> Envelope {
        Envelope {
            id: "STATUTE-1".into(),
            body: "body".into(),
            location: location.into(),
            depends_on: vec![],
            extra: BTreeMap::new(),
            ast: None,
        }
    }

    #[test]
    fn into_document_resolves_no_file_for_a_non_path_location() {
        // ADR-123 § LOC-003. ADR-005 lets a source emit any identifier;
        // a statute citation names no file, so `file` stays `None`.
        let root = tempfile::tempdir().unwrap();
        let doc = env_at("Työttömyysturvalaki 1290/2002 §6(1)")
            .into_document(root.path())
            .unwrap();
        assert_eq!(doc.file, None);
    }

    #[test]
    fn into_document_resolves_a_file_for_a_location_that_names_one() {
        // A source may legitimately emit a file-backed document — which
        // is why ADR-123 rejected provenance as the predicate.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("docs")).unwrap();
        std::fs::write(root.path().join("docs/001-x.md"), "body").unwrap();
        let doc = env_at("docs/001-x.md").into_document(root.path()).unwrap();
        assert_eq!(doc.file, Some(root.path().join("docs/001-x.md")));
    }

    #[test]
    fn into_document_resolves_no_file_for_a_directory_location() {
        // `is_file()`, not `exists()` — a directory is not a document body.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("docs")).unwrap();
        let doc = env_at("docs").into_document(root.path()).unwrap();
        assert_eq!(doc.file, None);
    }
}
