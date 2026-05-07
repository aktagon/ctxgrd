//! `ctxgrd rules` introspection. CLI-002 in the brief.
//!
//! Two views, each optimised for its audience (mirrors ruff's
//! `ruff rule` list / `ruff rule <code>` split):
//!
//! - [`render_table`] is the dense scanning view. Columns are
//!   `namespace`, `rule`, `source`, `summary`. Params and the long-form
//!   description live in the detail view, not here — rule discovery
//!   benefits from grep-friendliness more than from completeness.
//! - [`render_detail`] is the learning view. One ASCII-drawn box per
//!   `(namespace, rule)` entry matching a code, with a two-column grid
//!   (Source / Params / Summary) plus a wrapped description paragraph.
//!
//! [`render_json`] serialises the full [`RuleEntry`] shape, including
//! both `summary` and `description`, for LSP shims, dashboards,
//! policy audits, and docs generators.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::config::{Config, DiscoveredRule, Origin};

/// One row of `ctxgrd rules` output.
///
/// `code` serialises as `rule` in JSON so column names line up with
/// the text table's header. `summary` is the one-line phrase shown in
/// the table; `description` is the multi-line paragraph that appears
/// in the detail box.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuleEntry {
    pub namespace: String,
    #[serde(rename = "rule")]
    pub code: String,
    pub source: String,
    pub params: Value,
    pub summary: String,
    pub description: String,
}

/// Built-in `(code, summary, description)` triples for the nine
/// `core.*` rules. `summary` is the table cell; `description` is the
/// wrapped paragraph in the detail box. Source of truth: the brief's
/// "Built-in rules" table.
pub const CORE_DESCRIPTIONS: &[(&str, &str, &str)] = &[
    (
        "core.frontmatter",
        "Documents must contain parseable YAML frontmatter.",
        "Body has a `---`-fenced YAML frontmatter block that parses.",
    ),
    (
        "core.id",
        "Documents must declare a valid ID.",
        "`id` is present and matches the ID regex.",
    ),
    (
        "core.id-unique",
        "Document numbers must be unique per namespace.",
        "No two documents share `(namespace, number)`.",
    ),
    (
        "core.dep-resolved",
        "Dependency references must point to existing documents.",
        "Every `depends_on` entry refers to a present document.",
    ),
    (
        "core.dep-cycle",
        "Dependency graphs must be acyclic.",
        "`depends_on` graph is acyclic, including self-edges.",
    ),
    (
        "core.cross-ref",
        "Cross-reference tokens must resolve.",
        "Reads `ast.cross_ref_tokens`; emits a diagnostic for each unresolved token that is not in code / strikethrough.",
    ),
    (
        "core.required-headings",
        "Required H2 headings must be present.",
        "Reads `ast.headings`; emits a diagnostic per missing H2 heading, case-sensitive.",
    ),
    (
        "core.required-metadata",
        "Required metadata keys must be present and non-empty.",
        "Every listed key is present and non-empty in the unified metadata map.",
    ),
    (
        "core.allowed-values",
        "Metadata values must match configured allow-lists.",
        "For each listed key, if present in metadata, value is in the allow-list. Missing keys are skipped.",
    ),
];

/// Produce every `(namespace, rule-code)` entry the resolved config
/// activates, sorted by `(namespace, code)`.
///
/// `filter` restricts output to one namespace when `Some`.
pub fn list_rules(
    config: &Config,
    discovered: &BTreeMap<String, DiscoveredRule>,
    filter: Option<&str>,
) -> Vec<RuleEntry> {
    let core_map: BTreeMap<&str, (&str, &str)> = CORE_DESCRIPTIONS
        .iter()
        .map(|(c, s, d)| (*c, (*s, *d)))
        .collect();

    let mut entries = Vec::new();
    for (namespace, ns_cfg) in &config.namespaces {
        if let Some(ns) = filter {
            if ns != namespace {
                continue;
            }
        }
        for code in &ns_cfg.rules {
            let params = ns_cfg
                .params
                .get(code)
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            let (source, summary, description) = resolve(code, discovered, &core_map);
            entries.push(RuleEntry {
                namespace: namespace.clone(),
                code: code.clone(),
                source,
                params,
                summary,
                description,
            });
        }
    }
    entries.sort_by(|a, b| {
        (a.namespace.as_str(), a.code.as_str()).cmp(&(b.namespace.as_str(), b.code.as_str()))
    });
    entries
}

fn resolve(
    code: &str,
    discovered: &BTreeMap<String, DiscoveredRule>,
    core_map: &BTreeMap<&str, (&str, &str)>,
) -> (String, String, String) {
    if code.starts_with("core.") {
        let (summary, description) = core_map.get(code).copied().unwrap_or(("", ""));
        return (
            "core".to_string(),
            summary.to_string(),
            description.to_string(),
        );
    }
    let Some(rule) = discovered.get(code) else {
        // Shouldn't happen in a validated config, but a defensive
        // fallthrough is cheaper than panicking in introspection.
        return ("ext:missing".to_string(), String::new(), String::new());
    };
    let (summary, description) = read_readme_summary_and_body(&rule.run_path);
    let source = match rule.origin {
        Origin::Repo => "ext:repo",
        Origin::Global => "ext:global",
    };
    (source.to_string(), summary, description)
}

/// External-rule README parser.
///
/// Splits the README into a one-line summary (the first non-empty
/// trimmed line) and a body (subsequent lines up to the first blank,
/// joined with spaces — one paragraph). Avoids consuming the whole
/// file so the detail box stays compact.
fn read_readme_summary_and_body(run_path: &Path) -> (String, String) {
    let Some(dir) = run_path.parent() else {
        return (String::new(), String::new());
    };
    let readme = dir.join("README.md");
    let Ok(contents) = fs::read_to_string(&readme) else {
        return (String::new(), String::new());
    };
    let lines: Vec<&str> = contents.lines().map(str::trim).collect();
    let Some(start) = lines.iter().position(|l| !l.is_empty()) else {
        return (String::new(), String::new());
    };
    let summary = lines[start].to_string();
    let body: Vec<&str> = lines[start + 1..]
        .iter()
        .take_while(|l| !l.is_empty())
        .copied()
        .collect();
    (summary, body.join(" "))
}

/// Dense scanning table — the list view.
///
/// Columns: `namespace`, `rule`, `source`, `summary`. Params and the
/// long-form description live in [`render_detail`]; the list view
/// optimises for grep-friendliness and vertical scanning.
pub fn render_table(entries: &[RuleEntry]) -> String {
    let headers = ("namespace", "rule", "source", "summary");
    let ns_w = col_width(entries, |e| &e.namespace, headers.0);
    let code_w = col_width(entries, |e| &e.code, headers.1);
    let src_w = col_width(entries, |e| &e.source, headers.2);

    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "{:ns_w$}  {:code_w$}  {:src_w$}  {}",
        headers.0,
        headers.1,
        headers.2,
        headers.3,
        ns_w = ns_w,
        code_w = code_w,
        src_w = src_w,
    );
    for e in entries {
        let _ = writeln!(
            out,
            "{:ns_w$}  {:code_w$}  {:src_w$}  {}",
            e.namespace,
            e.code,
            e.source,
            e.summary,
            ns_w = ns_w,
            code_w = code_w,
            src_w = src_w,
        );
    }
    out
}

fn col_width<F>(entries: &[RuleEntry], f: F, header: &str) -> usize
where
    F: Fn(&RuleEntry) -> &String,
{
    entries
        .iter()
        .map(|e| f(e).len())
        .chain(std::iter::once(header.len()))
        .max()
        .unwrap_or(0)
}

/// Serialize entries as a pretty-printed JSON array.
///
/// Per CLI-002 "MUST emit the same data as a JSON array" — this is
/// the full record shape including both `summary` and `description`,
/// so consumers (LSP shims, dashboards, policy audits, docs gens)
/// get the identical text the human-facing views render.
pub fn render_json(entries: &[RuleEntry]) -> String {
    serde_json::to_string_pretty(entries).unwrap_or_else(|_| "[]".to_string())
}

/// Compact human-facing rendering of a rule's params.
///
/// Objects with string/array leaves become `key=value, key=[v1, v2]`
/// sequences. Empty objects render as `—` — the common case for
/// non-parameterised core rules, where a blank cell would be visually
/// ambiguous ("missing?" vs "no params").
fn compact_params(v: &Value) -> String {
    match v {
        Value::Object(map) if map.is_empty() => "—".to_string(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", compact_value(v)))
            .collect::<Vec<_>>()
            .join(", "),
        _ => compact_value(v),
    }
}

fn compact_value(v: &Value) -> String {
    match v {
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(compact_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

/// ASCII-drawn detail box per `(namespace, rule)` entry matching
/// `code`. When the code is active in multiple namespaces (e.g. both
/// ADR and PRD list `core.required-headings` with different params),
/// each namespace gets its own box — users see the per-namespace
/// differences side by side.
pub fn render_detail(
    entries: &[RuleEntry],
    code: &str,
    _discovered: &BTreeMap<String, DiscoveredRule>,
) -> String {
    let relevant: Vec<&RuleEntry> = entries.iter().filter(|e| e.code == code).collect();
    if relevant.is_empty() {
        return format!("(rule '{code}' is not active in any namespace)\n");
    }
    let mut out = String::new();
    for (i, e) in relevant.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&render_box(e));
    }
    out
}

// --- box geometry ---------------------------------------------------

/// Interior width of a box — the character count between the left and
/// right `│` borders. 90 chars keeps the outer width at 92, which
/// fits a standard 100-col terminal comfortably.
const BOX_INTERIOR: usize = 90;
/// Width (chars) of the left label column including the `│` on its
/// right side. The label text budget is this minus 2 padding spaces.
const LABEL_COL: usize = 12;
/// Width (chars) of the right value column. Value text budget is
/// this minus 2 padding spaces.
const VALUE_COL: usize = BOX_INTERIOR - LABEL_COL - 1;
/// Content budget inside a full-width row (header / body paragraph).
const FULL_CONTENT: usize = BOX_INTERIOR - 2;
/// Content budget inside a value cell in a label row.
const VALUE_CONTENT: usize = VALUE_COL - 2;
/// Content budget inside a label cell.
const LABEL_CONTENT: usize = LABEL_COL - 2;

fn render_box(e: &RuleEntry) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let dash_full = "─".repeat(BOX_INTERIOR);
    let dash_label = "─".repeat(LABEL_COL);
    let dash_value = "─".repeat(VALUE_COL);

    // Top border
    let _ = writeln!(out, "┌{dash_full}┐");

    // Header: "NS :: code"
    let header = format!("{} :: {}", e.namespace, e.code);
    let _ = writeln!(
        out,
        "│ {} │",
        pad_right(&truncate(&header, FULL_CONTENT), FULL_CONTENT)
    );

    // Grid divider (┬)
    let _ = writeln!(out, "├{dash_label}┬{dash_value}┤");

    write_row(&mut out, "Source", &e.source);
    write_row(&mut out, "Params", &compact_params(&e.params));
    write_row(&mut out, "Summary", &e.summary);

    // Body divider (┴)
    let _ = writeln!(out, "├{dash_label}┴{dash_value}┤");

    // Description body — word-wrapped paragraph
    if e.description.is_empty() {
        let _ = writeln!(out, "│ {} │", pad_right("", FULL_CONTENT));
    } else {
        for line in wrap(&e.description, FULL_CONTENT) {
            let _ = writeln!(out, "│ {} │", pad_right(&line, FULL_CONTENT));
        }
    }

    // Bottom border
    let _ = writeln!(out, "└{dash_full}┘");
    out
}

fn write_row(out: &mut String, label: &str, value: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "│ {} │ {} │",
        pad_right(&truncate(label, LABEL_CONTENT), LABEL_CONTENT),
        pad_right(&truncate(value, VALUE_CONTENT), VALUE_CONTENT),
    );
}

/// Pad `s` to exactly `width` display columns with trailing spaces.
/// Uses `chars().count()` as the display-width proxy — accurate for
/// the ASCII-plus-em-dash content we actually render, and cheaper
/// than pulling in a `unicode-width` dependency.
fn pad_right(s: &str, width: usize) -> String {
    let current = s.chars().count();
    if current >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + (width - current));
        out.push_str(s);
        for _ in current..width {
            out.push(' ');
        }
        out
    }
}

/// Truncate to at most `max` display columns, inserting a trailing
/// `…` when truncation occurs. Used for header and cell contents that
/// might exceed their budget (long external-rule names, very long
/// summaries, etc.).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        let head: String = s.chars().take(max - 1).collect();
        format!("{head}…")
    }
}

/// Word-wrap `text` at `width` columns. Whitespace-only input yields
/// a single empty line so the box always has at least one body row.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamespaceConfig;
    use serde_json::json;

    fn sample_config() -> Config {
        let mut config = Config::default();
        config.namespaces.insert(
            "ADR".to_string(),
            NamespaceConfig {
                rules: vec![
                    "core.frontmatter".to_string(),
                    "core.allowed-values".to_string(),
                    "adr.consequences-non-empty".to_string(),
                ],
                params: [(
                    "core.allowed-values".to_string(),
                    json!({ "status": ["draft", "accepted"] }),
                )]
                .into(),
                paths: None,
                path_patterns: Vec::new(),
            },
        );
        config
    }

    fn discovered_with(codes: &[(&str, Origin)]) -> BTreeMap<String, DiscoveredRule> {
        codes
            .iter()
            .map(|(code, origin)| {
                let code = (*code).to_string();
                let rule = DiscoveredRule {
                    code: code.clone(),
                    run_path: std::path::PathBuf::from(format!(
                        "./rules/{}/run",
                        code.replace('.', "/")
                    )),
                    origin: *origin,
                };
                (code, rule)
            })
            .collect()
    }

    #[test]
    fn list_rules_sorts_and_annotates_source_column() {
        let config = sample_config();
        let discovered = discovered_with(&[("adr.consequences-non-empty", Origin::Repo)]);
        let entries = list_rules(&config, &discovered, None);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].code, "adr.consequences-non-empty");
        assert_eq!(entries[0].source, "ext:repo");
        assert_eq!(entries[1].code, "core.allowed-values");
        assert_eq!(entries[1].source, "core");
        assert_eq!(entries[2].code, "core.frontmatter");
    }

    #[test]
    fn list_rules_filters_by_namespace() {
        let config = sample_config();
        let discovered = BTreeMap::new();
        assert!(list_rules(&config, &discovered, Some("PRD")).is_empty());
        assert_eq!(list_rules(&config, &discovered, Some("ADR")).len(), 3);
    }

    #[test]
    fn core_rules_pick_up_summary_and_description() {
        let config = sample_config();
        let discovered = BTreeMap::new();
        let entries = list_rules(&config, &discovered, Some("ADR"));
        let fm = entries
            .iter()
            .find(|e| e.code == "core.frontmatter")
            .unwrap();
        // Summary is the compact one-liner for the table cell.
        assert_eq!(
            fm.summary,
            "Documents must contain parseable YAML frontmatter."
        );
        // Description is the longer explanatory body.
        assert!(fm
            .description
            .contains("YAML frontmatter block that parses"));
    }

    #[test]
    fn external_source_marked_global_when_origin_is_global() {
        let mut config = Config::default();
        config.namespaces.insert(
            "ADR".to_string(),
            NamespaceConfig {
                rules: vec!["adr.shared-rule".to_string()],
                params: Default::default(),
                paths: None,
                path_patterns: Vec::new(),
            },
        );
        let discovered = discovered_with(&[("adr.shared-rule", Origin::Global)]);
        let entries = list_rules(&config, &discovered, None);
        assert_eq!(entries[0].source, "ext:global");
    }

    #[test]
    fn render_table_columns_are_namespace_rule_source_summary() {
        let config = sample_config();
        let discovered = discovered_with(&[("adr.consequences-non-empty", Origin::Repo)]);
        let entries = list_rules(&config, &discovered, Some("ADR"));
        let text = render_table(&entries);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("namespace"));
        assert!(lines[0].contains("rule"));
        assert!(lines[0].contains("source"));
        assert!(lines[0].contains("summary"));
        // `params` must NOT be a column header anymore — it lives in
        // the detail view.
        assert!(
            !lines[0].contains("params"),
            "params column must not appear in table: {:?}",
            lines[0]
        );
        for data in &lines[1..] {
            assert!(data.starts_with("ADR"));
        }
    }

    #[test]
    fn render_table_includes_summary_text_per_row() {
        let config = sample_config();
        let discovered = BTreeMap::new();
        let entries = list_rules(&config, &discovered, Some("ADR"));
        let text = render_table(&entries);
        // Summary strings for the two core rules in the sample.
        assert!(text.contains("Documents must contain parseable YAML frontmatter."));
        assert!(text.contains("Metadata values must match configured allow-lists."));
        // Raw JSON params must NOT appear in the table.
        assert!(!text.contains(r#"{"status""#));
    }

    #[test]
    fn compact_params_em_dashes_empty_object() {
        assert_eq!(compact_params(&json!({})), "—");
    }

    #[test]
    fn compact_params_renders_key_equals_array() {
        let v = json!({ "headings": ["Status", "Context", "Decision"] });
        assert_eq!(compact_params(&v), "headings=[Status, Context, Decision]");
    }

    #[test]
    fn compact_params_handles_multiple_keys() {
        let v = json!({
            "keys": ["id", "title"],
            "status": ["draft", "accepted"]
        });
        // serde_json preserves insertion / sorted order; assert both
        // substrings are present rather than pinning the exact join.
        let out = compact_params(&v);
        assert!(out.contains("keys=[id, title]"));
        assert!(out.contains("status=[draft, accepted]"));
    }

    #[test]
    fn render_detail_handles_absent_code() {
        let entries: Vec<RuleEntry> = Vec::new();
        let discovered = BTreeMap::new();
        let text = render_detail(&entries, "core.ghost", &discovered);
        assert!(text.contains("not active in any namespace"));
    }

    #[test]
    fn render_detail_renders_box_with_all_four_sections() {
        let config = sample_config();
        let discovered = BTreeMap::new();
        let entries = list_rules(&config, &discovered, None);
        let text = render_detail(&entries, "core.allowed-values", &discovered);

        // Box-drawing borders present.
        assert!(text.contains("┌"), "missing top-left corner:\n{text}");
        assert!(text.contains("┐"), "missing top-right corner:\n{text}");
        assert!(text.contains("└"), "missing bottom-left corner:\n{text}");
        assert!(text.contains("┘"), "missing bottom-right corner:\n{text}");
        assert!(text.contains("┬"), "missing grid divider:\n{text}");
        assert!(text.contains("┴"), "missing body divider:\n{text}");

        // Header line: "ADR :: core.allowed-values"
        assert!(text.contains("ADR :: core.allowed-values"));

        // Grid cells for the three labels.
        assert!(text.contains("Source"));
        assert!(text.contains("Params"));
        assert!(text.contains("Summary"));

        // Compact params format (not raw JSON).
        assert!(
            text.contains("status=[draft, accepted]"),
            "expected compact params, got:\n{text}"
        );
        assert!(!text.contains(r#"{"status""#));

        // Summary text in the grid.
        assert!(text.contains("Metadata values must match configured allow-lists."));
    }

    #[test]
    fn render_detail_draws_one_box_per_matching_namespace() {
        let mut config = Config::default();
        for ns in ["ADR", "PRD"] {
            config.namespaces.insert(
                ns.to_string(),
                NamespaceConfig {
                    rules: vec!["core.cross-ref".to_string()],
                    params: Default::default(),
                    paths: None,
                    path_patterns: Vec::new(),
                },
            );
        }
        let discovered = BTreeMap::new();
        let entries = list_rules(&config, &discovered, None);
        let text = render_detail(&entries, "core.cross-ref", &discovered);
        // Two distinct headers — one per namespace.
        assert!(text.contains("ADR :: core.cross-ref"));
        assert!(text.contains("PRD :: core.cross-ref"));
        // Two top borders.
        assert_eq!(
            text.matches('┌').count(),
            2,
            "expected two boxes, got:\n{text}"
        );
    }

    #[test]
    fn render_box_rows_are_all_the_same_width() {
        let config = sample_config();
        let discovered = BTreeMap::new();
        let entries = list_rules(&config, &discovered, Some("ADR"));
        let text = render_detail(&entries, "core.allowed-values", &discovered);
        // Collect the character widths of every non-empty line. They
        // must all be equal — otherwise the box borders misalign in
        // narrow terminals. (Trailing spaces intentional for flush
        // right borders.)
        let widths: Vec<usize> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.chars().count())
            .collect();
        assert!(!widths.is_empty());
        let first = widths[0];
        for w in &widths {
            assert_eq!(
                *w, first,
                "rows must share a single width; got: {widths:?}\n{text}"
            );
        }
    }

    #[test]
    fn render_json_serialises_full_entry_shape_including_summary() {
        let config = sample_config();
        let discovered = discovered_with(&[("adr.consequences-non-empty", Origin::Repo)]);
        let entries = list_rules(&config, &discovered, Some("ADR"));
        let json = render_json(&entries);
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("render_json is valid JSON");
        assert_eq!(parsed.len(), 3);
        let first = &parsed[0];
        // `code` must serialise as `rule`, not `code`.
        assert!(first.get("rule").is_some());
        assert!(first.get("code").is_none());
        assert!(first.get("namespace").is_some());
        assert!(first.get("source").is_some());
        assert!(first.get("params").is_some());
        assert!(first.get("summary").is_some());
        assert!(first.get("description").is_some());
    }

    #[test]
    fn render_json_empty_returns_empty_array() {
        assert_eq!(render_json(&[]), "[]");
    }

    #[test]
    fn wrap_breaks_long_text_at_word_boundaries() {
        let lines = wrap("one two three four five six seven", 10);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
        // Every word must appear exactly once across the wrapped lines.
        let joined = lines.join(" ");
        for w in ["one", "two", "three", "four", "five", "six", "seven"] {
            assert!(joined.contains(w));
        }
    }

    #[test]
    fn truncate_inserts_ellipsis_when_over_budget() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abcdef", 1), "…");
    }
}
