//! `ctxgrd list` document inventory. ADR-015.
//!
//! Where [`crate::introspect`] catalogs the resolved *rule set*, this
//! module catalogs the ingested *documents* — one row per `<NS>-<n>`
//! the kernel saw. It reuses [`crate::run::ingest`] so the inventory
//! reflects exactly what `lint` would lint (markdown walk + external
//! sources), then projects each [`Document`] onto the four columns
//! ADR-015 § LST-002 fixes: id, title, status, depends_on.
//!
//! Three renderings, mirroring `ctxgrd rules`:
//!
//! - [`render_table`] — column-aligned terminal table, the default.
//! - [`render_markdown`] — an H2 heading per namespace plus a GFM
//!   pipe table, for pasting into docs or an LLM prompt.
//! - [`render_json`] — the full [`DocEntry`] array for tooling.

use std::path::Path;

use serde_json::Value;

use crate::document::Document;
use crate::run::{self, LintError};

/// One row of `ctxgrd list` output.
///
/// `id` preserves the author's exact string (`raw_id`) so the table
/// matches what appears in filenames and frontmatter, the same
/// faithfulness `ctxgrd refs` applies to diagnostics. `title` and
/// `status` are pulled from frontmatter and may be empty — they are
/// conventions, not guarantees of the document model.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DocEntry {
    pub namespace: String,
    pub id: String,
    pub title: String,
    pub status: String,
    pub depends_on: Vec<String>,
}

/// Collect the document inventory under `root`, optionally restricted
/// to one namespace.
///
/// Runs the full ingest pipeline (config load, external sources,
/// markdown walk) so the inventory matches `lint`'s view of the tree.
/// Propagates [`LintError`] unchanged so the CLI renders config and
/// scan failures exactly as `lint` and `refs` do.
pub fn inventory(root: &Path, namespace: Option<&str>) -> Result<Vec<DocEntry>, LintError> {
    let result = run::ingest(root)?;
    Ok(entries(&result.documents, namespace))
}

/// Pure projection of ingested documents onto sorted [`DocEntry`]
/// rows. Split from [`inventory`] so it is testable without touching
/// the filesystem.
///
/// Sorted by `(namespace, number)` so output is byte-reproducible
/// across runs. `filter` restricts to one namespace when `Some`.
pub fn entries(documents: &[Document], filter: Option<&str>) -> Vec<DocEntry> {
    let mut docs: Vec<&Document> = documents
        .iter()
        .filter(|d| filter.is_none_or(|ns| ns == d.id.namespace))
        .collect();
    // Sort on the parsed `(namespace, number)` — not the string id —
    // so leading zeros don't push `ADR-10` ahead of `ADR-9`.
    docs.sort_by(|a, b| {
        (a.id.namespace.as_str(), a.id.number).cmp(&(b.id.namespace.as_str(), b.id.number))
    });
    docs.into_iter()
        .map(|d| DocEntry {
            namespace: d.id.namespace.clone(),
            id: d.raw_id.clone(),
            title: title_of(d),
            status: metadata_str(d, "status"),
            depends_on: d.depends_on.clone(),
        })
        .collect()
}

/// The document's display title, falling back across the two
/// conventional keys. Nygard-style ADRs use `title:`; ctxgrd's own
/// `llm-agents` pack and some ADR conventions use `name:`. Trying both
/// keeps the column populated without per-project config — the
/// first-touch-works property `list` inherits from the rest of ctxgrd.
fn title_of(doc: &Document) -> String {
    ["title", "name"]
        .iter()
        .map(|k| metadata_str(doc, k))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Read a frontmatter key as a display string. String values pass
/// through verbatim; other scalars render via their JSON form so a
/// numeric or boolean field is still legible rather than blanked.
fn metadata_str(doc: &Document, key: &str) -> String {
    match doc.metadata.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// Em-dash placeholder for an empty cell, matching the convention
/// `ctxgrd rules` uses for absent params — a blank cell reads as
/// "missing?" rather than "none".
fn cell(s: &str) -> String {
    if s.is_empty() {
        "—".to_string()
    } else {
        s.to_string()
    }
}

fn deps_cell(deps: &[String]) -> String {
    if deps.is_empty() {
        "—".to_string()
    } else {
        deps.join(", ")
    }
}

/// Column-aligned terminal table — the default view.
///
/// Columns: `namespace`, `id`, `status`, `title`, `depends_on`. Flat
/// (not grouped) so it greps cleanly, mirroring the `ctxgrd rules`
/// list view.
pub fn render_table(entries: &[DocEntry]) -> String {
    let headers = ("namespace", "id", "status", "title", "depends_on");
    let ns_w = col_width(entries, headers.0, |e| e.namespace.clone());
    let id_w = col_width(entries, headers.1, |e| e.id.clone());
    let status_w = col_width(entries, headers.2, |e| cell(&e.status));
    let title_w = col_width(entries, headers.3, |e| cell(&e.title));

    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "{:ns_w$}  {:id_w$}  {:status_w$}  {:title_w$}  {}",
        headers.0, headers.1, headers.2, headers.3, headers.4,
    );
    for e in entries {
        let _ = writeln!(
            out,
            "{:ns_w$}  {:id_w$}  {:status_w$}  {:title_w$}  {}",
            e.namespace,
            e.id,
            cell(&e.status),
            cell(&e.title),
            deps_cell(&e.depends_on),
        );
    }
    out
}

fn col_width<F>(entries: &[DocEntry], header: &str, f: F) -> usize
where
    F: Fn(&DocEntry) -> String,
{
    entries
        .iter()
        .map(|e| f(e).chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(0)
}

/// Markdown rendering: an H2 heading per namespace, each followed by a
/// GFM pipe table (`ID | Title | Status | Depends on`). Entries arrive
/// sorted, so a namespace's rows are already contiguous.
pub fn render_markdown(entries: &[DocEntry]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut current_ns: Option<&str> = None;
    for e in entries {
        if current_ns != Some(e.namespace.as_str()) {
            if current_ns.is_some() {
                out.push('\n');
            }
            let _ = writeln!(out, "## {}\n", e.namespace);
            out.push_str("| ID | Title | Status | Depends on |\n");
            out.push_str("| --- | --- | --- | --- |\n");
            current_ns = Some(e.namespace.as_str());
        }
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            md_escape(&e.id),
            md_escape(&cell(&e.title)),
            md_escape(&cell(&e.status)),
            md_escape(&deps_cell(&e.depends_on)),
        );
    }
    out
}

/// Escape the GFM cell separator so a `|` inside a title or dependency
/// list does not split the row into extra columns.
fn md_escape(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Serialize the inventory as a pretty-printed JSON array — the full
/// [`DocEntry`] shape, for dashboards and docs generators.
pub fn render_json(entries: &[DocEntry]) -> String {
    serde_json::to_string_pretty(entries).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::DocumentId;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn doc(raw_id: &str, meta: &[(&str, Value)], deps: &[&str]) -> Document {
        let id: DocumentId = raw_id.parse().expect("valid id in test fixture");
        let metadata: BTreeMap<String, Value> = meta
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        Document {
            id,
            raw_id: raw_id.to_owned(),
            location: format!("adrs/{raw_id}.md"),
            depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
            frontmatter_lines: BTreeMap::new(),
            metadata,
            ast: None,
            body: String::new(),
        }
    }

    #[test]
    fn entries_sort_by_namespace_then_numeric_id() {
        let docs = vec![
            doc("PRD-1", &[], &[]),
            doc("ADR-10", &[], &[]),
            doc("ADR-9", &[], &[]),
        ];
        let rows = entries(&docs, None);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["ADR-9", "ADR-10", "PRD-1"]);
    }

    #[test]
    fn entries_filter_restricts_to_one_namespace() {
        let docs = vec![doc("ADR-1", &[], &[]), doc("PRD-1", &[], &[])];
        let rows = entries(&docs, Some("PRD"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "PRD-1");
    }

    #[test]
    fn entries_extract_title_and_status_from_frontmatter() {
        let docs = vec![doc(
            "ADR-7",
            &[
                ("title", json!("Adoption-aware defaults")),
                ("status", json!("accepted")),
            ],
            &["ADR-1"],
        )];
        let rows = entries(&docs, None);
        assert_eq!(rows[0].title, "Adoption-aware defaults");
        assert_eq!(rows[0].status, "accepted");
        assert_eq!(rows[0].depends_on, vec!["ADR-1".to_string()]);
    }

    #[test]
    fn title_falls_back_to_name_when_title_key_absent() {
        let docs = vec![doc(
            "ADR-1",
            &[(
                "name",
                json!("T-Box modularization along the share/variant seam"),
            )],
            &[],
        )];
        let rows = entries(&docs, None);
        assert_eq!(
            rows[0].title,
            "T-Box modularization along the share/variant seam"
        );
    }

    #[test]
    fn title_key_wins_over_name_when_both_present() {
        let docs = vec![doc(
            "ADR-1",
            &[("title", json!("Preferred")), ("name", json!("Fallback"))],
            &[],
        )];
        let rows = entries(&docs, None);
        assert_eq!(rows[0].title, "Preferred");
    }

    #[test]
    fn metadata_str_renders_non_string_scalar_via_json() {
        let d = doc("ADR-1", &[("status", json!(3))], &[]);
        assert_eq!(metadata_str(&d, "status"), "3");
    }

    #[test]
    fn render_table_header_lists_all_five_columns() {
        let rows = entries(&[doc("ADR-1", &[], &[])], None);
        let text = render_table(&rows);
        let header = text.lines().next().expect("table has a header row");
        for col in ["namespace", "id", "status", "title", "depends_on"] {
            assert!(header.contains(col), "missing column {col} in: {header:?}");
        }
    }

    #[test]
    fn render_table_em_dashes_empty_title_and_deps() {
        let rows = entries(&[doc("ADR-1", &[], &[])], None);
        let text = render_table(&rows);
        let data = text.lines().nth(1).expect("table has a data row");
        // Title, status, and depends_on are all absent → three em-dashes.
        assert_eq!(data.matches('—').count(), 3, "row was: {data:?}");
    }

    #[test]
    fn render_markdown_emits_h2_and_pipe_table_per_namespace() {
        let docs = vec![
            doc("ADR-1", &[("title", json!("First"))], &[]),
            doc("PRD-1", &[("title", json!("Second"))], &[]),
        ];
        let md = render_markdown(&entries(&docs, None));
        assert!(md.contains("## ADR\n"));
        assert!(md.contains("## PRD\n"));
        assert!(md.contains("| ID | Title | Status | Depends on |"));
        assert!(md.contains("| ADR-1 | First | — | — |"));
    }

    #[test]
    fn render_markdown_escapes_pipe_in_title() {
        let docs = vec![doc("ADR-1", &[("title", json!("a | b"))], &[])];
        let md = render_markdown(&entries(&docs, None));
        assert!(md.contains(r"a \| b"), "pipe not escaped in: {md}");
    }

    #[test]
    fn render_json_serialises_full_entry_shape() {
        let docs = vec![doc("ADR-1", &[("title", json!("First"))], &["ADR-2"])];
        let json = render_json(&entries(&docs, None));
        let parsed: Vec<Value> = serde_json::from_str(&json).expect("render_json is valid JSON");
        assert_eq!(parsed.len(), 1);
        let first = &parsed[0];
        assert_eq!(first["namespace"], json!("ADR"));
        assert_eq!(first["id"], json!("ADR-1"));
        assert_eq!(first["title"], json!("First"));
        assert_eq!(first["depends_on"], json!(["ADR-2"]));
    }
}
