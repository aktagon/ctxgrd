//! `ctxgrd new` scaffolder. CLI-001 in the brief.
//!
//! Produces a new document stub in memory + resolves its target
//! filesystem path, leaving the I/O decisions (write vs stdout) to
//! the binary caller.
//!
//! The brief's numbered contract:
//! 1. Next ID = `max(existing ids in <NS>) + 1`, or `--id` override.
//! 2. Slug = lowercase, non-alnum → `-`, trim, truncate to 60,
//!    fallback `untitled`.
//! 3. Target dir = `--out` override, else parent of existing docs in
//!    `<NS>` (NEW-001), else literal prefix of the first `[<NS>].paths`
//!    glob (NEW-004), else `<root>/<lowercase-ns>s/` (NEW-003).
//! 4. Frontmatter keys: `id`, `title`, every
//!    `core.required-metadata.keys` (stubbed empty), `depends_on: []`.
//! 5. Body: one empty H2 per `core.required-headings.headings`.

use std::io;
use std::path::{Path, PathBuf};

use crate::config::NamespaceConfig;
use crate::document::Document;
use crate::id::DocumentId;

/// A scaffolded document, in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scaffold {
    pub id: DocumentId,
    pub title: String,
    pub slug: String,
    pub contents: String,
    /// Full absolute-or-relative path where the new file should land.
    pub target_path: PathBuf,
}

/// Compute the scaffold for a new document.
pub fn scaffold(
    namespace: &str,
    title: &str,
    id_override: Option<u32>,
    ns_cfg: &NamespaceConfig,
    existing: &[Document],
    root: &Path,
    out_override: Option<&Path>,
) -> Scaffold {
    let number = id_override.unwrap_or_else(|| next_id(namespace, existing));
    let slug = slugify(title);
    let contents = render_contents(namespace, number, title, ns_cfg);
    let dir = target_dir(namespace, ns_cfg, existing, root, out_override);
    let filename = format!("{number:03}-{slug}.md");
    let target_path = dir.join(filename);
    Scaffold {
        id: DocumentId::new(namespace, number),
        title: title.to_owned(),
        slug,
        contents,
        target_path,
    }
}

/// Next unused number: `max(existing.number) + 1`, or 1 when the
/// namespace has no docs yet.
///
/// Follows the brief's CLI-001 clause #1 literally (max + 1); holes
/// in the sequence are preserved. If the fixture has ADR-001 and
/// ADR-099, the next scaffold is ADR-100, not ADR-002. Users who
/// want gap-filling can always pass `--id <n>`.
pub fn next_id(namespace: &str, existing: &[Document]) -> u32 {
    existing
        .iter()
        .filter(|d| d.id.namespace == namespace)
        .map(|d| d.id.number)
        .max()
        .map(|m| m + 1)
        .unwrap_or(1)
}

/// Normalise a title into a kebab-case slug.
///
/// Lowercase, non-ASCII-alnum → `-`, collapse consecutive dashes,
/// trim edge dashes, truncate to 60 chars (then re-trim). Empty
/// → "untitled".
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

fn target_dir(
    namespace: &str,
    ns_cfg: &NamespaceConfig,
    existing: &[Document],
    root: &Path,
    out_override: Option<&Path>,
) -> PathBuf {
    // 1. `--out` override always wins (ADR-010 NEW-001 precondition).
    if let Some(o) = out_override {
        return o.to_path_buf();
    }
    // 2. NEW-001: co-locate with an existing doc of this namespace, so
    //    a filesystem convention beats config once one exists.
    if let Some(doc) = existing.iter().find(|d| d.id.namespace == namespace) {
        if let Some(parent) = Path::new(&doc.location).parent() {
            let p = parent.as_os_str();
            if !p.is_empty() {
                return root.join(parent);
            }
        }
    }
    // 3. NEW-004 (ADR-010 amendment, BUG-002): a greenfield path-claimed
    //    namespace lands in its declared `[<NS>].paths` home, derived
    //    from the first glob's literal prefix.
    if let Some(prefix) = ns_cfg
        .path_patterns
        .first()
        .and_then(|g| glob_literal_prefix(g))
    {
        return root.join(prefix);
    }
    // 4. NEW-003: lowercase-plural fallback — reached only when the
    //    namespace is neither populated nor path-claimed.
    root.join(format!("{}s", namespace.to_lowercase()))
}

/// Literal directory prefix of a glob: the longest leading run of path
/// segments containing no glob metacharacters. `docs/specs/**` →
/// `docs/specs`; `**/specs/**` → `None` (empty prefix). Drives ADR-010
/// NEW-004 / BUG-002 — landing a first scaffold in the namespace's
/// declared `paths` home instead of the hardcoded `<ns>s/` fallback.
fn glob_literal_prefix(glob: &str) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    let mut any = false;
    for segment in glob.split('/') {
        if segment.contains(['*', '?', '[', ']', '{', '}']) {
            break;
        }
        if segment.is_empty() {
            continue;
        }
        prefix.push(segment);
        any = true;
    }
    any.then_some(prefix)
}

fn render_contents(namespace: &str, number: u32, title: &str, ns_cfg: &NamespaceConfig) -> String {
    let mut out = String::new();
    let id = format!("{namespace}-{number:03}");

    // --- Frontmatter ---
    out.push_str("---\n");
    out.push_str(&format!("id: {id}\n"));
    out.push_str(&format!("title: {}\n", yaml_scalar(title)));
    // Every required-metadata key that isn't `id` or `title` gets
    // a stubbed-empty entry. Order preserved from config.
    if let Some(keys) = ns_cfg
        .params
        .get("core.required-metadata")
        .and_then(|p| p.get("keys"))
        .and_then(|v| v.as_array())
    {
        for k in keys {
            if let Some(key) = k.as_str() {
                if key != "id" && key != "title" {
                    out.push_str(&format!("{key}:\n"));
                }
            }
        }
    }
    out.push_str("depends_on: []\n");
    out.push_str("---\n");

    // --- Body ---
    out.push('\n');
    out.push_str(&format!("# {id}: {title}\n"));
    if let Some(headings) = ns_cfg
        .params
        .get("core.required-headings")
        .and_then(|p| p.get("headings"))
        .and_then(|v| v.as_array())
    {
        for h in headings {
            if let Some(heading) = h.as_str() {
                out.push('\n');
                out.push_str(&format!("## {heading}\n"));
            }
        }
    }
    out
}

/// Render a string as a YAML scalar.
///
/// Unquoted when safe (no special chars and doesn't start with a
/// YAML indicator char); double-quoted with `\` / `"` escaping
/// otherwise. Stays close to YAML 1.2 plain-scalar rules without
/// reaching for a full YAML emitter.
fn yaml_scalar(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.starts_with([
            '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%',
            '@', '`', ' ',
        ])
        || s.ends_with(' ')
        || s.contains([':', '#', '\n', '\t', '"', '\\'])
        || s.contains(" #");
    if !needs_quoting {
        return s.to_owned();
    }
    let mut escaped = String::with_capacity(s.len() + 2);
    escaped.push('"');
    for c in s.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

// -- new-rule: external rule scaffolder --------------------------------

/// A scaffolded external rule, in memory.
///
/// The struct carries the computed contents and target paths;
/// [`Self::write_run_script`] and [`Self::write_readme`] materialise
/// them on disk and own the ADR-002 § RUL-006 executable-bit
/// invariant. Callers (the CLI, a future LSP code-action) should not
/// chmod the run script themselves — they would re-implement the
/// policy and risk drifting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleScaffold {
    /// Full rule code, e.g. `design.open-questions-non-empty`.
    pub code: String,
    /// Lowercase namespace component (`design`).
    pub namespace: String,
    /// Lowercase kebab-case rule name (`open-questions-non-empty`).
    pub name: String,
    /// Where the executable `run` script should land.
    pub run_path: PathBuf,
    /// Bash template encoding the EXT-002 contract.
    pub run_contents: String,
    /// Where the rule's README should land.
    pub readme_path: PathBuf,
    pub readme_contents: String,
}

impl RuleScaffold {
    /// Materialise the run script: create parent directories,
    /// write `run_contents`, and on Unix set mode `0o755` so the
    /// kernel's external-rule loader will accept it.
    ///
    /// ADR-002 § RUL-006 makes the executable bit load-bearing —
    /// non-executable scripts are refused at lint time. Callers MUST
    /// use this method instead of writing the script directly.
    ///
    /// Refuses to overwrite an existing run script: returns
    /// `io::ErrorKind::AlreadyExists` so the caller can surface a
    /// "use --out or remove the directory" hint without losing the
    /// user's edits.
    pub fn write_run_script(&self) -> io::Result<()> {
        if self.run_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", self.run_path.display()),
            ));
        }
        if let Some(parent) = self.run_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.run_path, self.run_contents.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.run_path, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    /// Materialise the README — but only if it doesn't already exist.
    /// The user's documentation edits live there; we never overwrite.
    /// `Ok(false)` indicates the file was left untouched, `Ok(true)`
    /// indicates a fresh write.
    ///
    /// README write failures are non-fatal in current callers (the
    /// run script is what makes the rule loadable); returning
    /// `io::Result` lets each caller decide whether to surface as a
    /// warning or a hard error.
    pub fn write_readme(&self) -> io::Result<bool> {
        if self.readme_path.exists() {
            return Ok(false);
        }
        std::fs::write(&self.readme_path, self.readme_contents.as_bytes())?;
        Ok(true)
    }
}

/// Compute the scaffold for a new external rule.
///
/// `code` must be `<lowercase-namespace>.<kebab-name>`, mirroring the
/// `rules/<namespace>/<name>/run` directory bijection. The `core`
/// namespace is reserved for built-in rules.
pub fn scaffold_rule(
    code: &str,
    description: Option<&str>,
    root: &Path,
    out_override: Option<&Path>,
) -> Result<RuleScaffold, String> {
    let (namespace, name) = parse_rule_code(code)?;
    if namespace == "core" {
        return Err(format!(
            "namespace '{namespace}' is reserved for built-in rules"
        ));
    }
    let dir = out_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| root.join("rules").join(&namespace).join(&name));
    let desc = description.unwrap_or("").trim();
    Ok(RuleScaffold {
        code: code.to_string(),
        namespace: namespace.clone(),
        name: name.clone(),
        run_path: dir.join("run"),
        run_contents: render_rule_run(code, desc),
        readme_path: dir.join("README.md"),
        readme_contents: render_rule_readme(code, &namespace, desc),
    })
}

/// Validate the rule-code shape and split into namespace + name.
///
/// Accepted: lowercase ASCII letters / digits / dashes in each side,
/// exactly one `.` separator, both sides non-empty. Mirrors the
/// directory layout the kernel's external-rule loader expects.
fn parse_rule_code(code: &str) -> Result<(String, String), String> {
    let (ns, name) = code.split_once('.').ok_or_else(|| {
        format!(
            "rule code '{code}' must be `<namespace>.<name>` \
             (e.g. `design.open-questions-non-empty`)"
        )
    })?;
    if ns.is_empty() || name.is_empty() {
        return Err(format!(
            "rule code '{code}' has an empty namespace or name component"
        ));
    }
    let ns_ok = ns
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let name_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ns_ok || !name_ok {
        return Err(format!(
            "rule code '{code}' must use lowercase ASCII; \
             namespace = letters/digits, name = letters/digits/dashes"
        ));
    }
    Ok((ns.to_string(), name.to_string()))
}

fn render_rule_run(code: &str, description: &str) -> String {
    let desc_line = if description.is_empty() {
        String::new()
    } else {
        format!("# {description}\n#\n")
    };
    format!(
        "#!/usr/bin/env bash
# {code}
{desc_line}# Contract (ADR-002 batch mode):
#   - stdin:  JSONL records, one per document. Each line is
#             {{\"path\": \"<abs path to body>\", \"context\": {{...}}}}.
#             `context` contains id, namespace, location, depends_on,
#             metadata, ast — the merged view the kernel has built.
#   - argv:   none.
#   - env:    CTXGRD_RULE_PARAMS = JSON of [<NS>.\"<rule.code>\"] table.
#   - stdout: zero or more JSONL diagnostics. Each diagnostic MUST
#             include a \"path\" field matching one of the input paths.
#             DO NOT emit a `code` field — the host attaches it from
#             the rule directory layout.
#   - exit 0: ran cleanly (with or without diagnostics).
#   - exit non-zero: runtime error (host emits ext.runtime-error).

set -euo pipefail

while IFS= read -r line; do
  path=$(printf '%s' \"$line\" | jq -r '.path')

  # Read merged metadata from the inline context (uncomment if needed):
  # status=$(printf '%s' \"$line\" | jq -r '.context.metadata.status // \"\"')

  # Read rule-specific params from [<NS>.\"<rule.code>\"] (uncomment if needed):
  # threshold=$(jq -r '.threshold // 1' <<< \"${{CTXGRD_RULE_PARAMS:-{{}}}}\")

  # TODO: implement your check. Example shape:
  #   printf '{{\"path\":%s,\"severity\":\"error\",\"message\":\"...\",\"line\":%d,\"col\":0}}\\n' \\
  #     \"$(printf '%s' \"$path\" | jq -Rs .)\" \"$line_no\"
  :
done
"
    )
}

fn render_rule_readme(code: &str, namespace: &str, description: &str) -> String {
    let ns_upper = namespace.to_uppercase();
    let desc_block = if description.is_empty() {
        String::new()
    } else {
        format!("\n{description}\n")
    };
    format!(
        "# {code}
{desc_block}
## What it checks

(describe the structural invariant this rule enforces)

## Activation

```toml
[{ns_upper}]
rules = [..., \"{code}\"]
```

## Example failure

(paste an example of a doc that fails this rule + the diagnostic it produces)
"
    )
}

// -- init: ctxgrd.toml templates ---------------------------------------

/// Namespaces shown (active) in the default `ctxgrd init` output.
pub const DEFAULT_ACTIVE_NAMESPACES: &[&str] = &["ADR", "PRD"];

/// Namespaces shown commented-out in the default `ctxgrd init` output —
/// a discoverable catalogue of the common record types the user can
/// un-comment as they adopt them.
pub const DEFAULT_COMMENTED_NAMESPACES: &[&str] = &["DDR", "RFC", "RUN", "PMR"];

/// Default glob patterns for the generated `[ignore].patterns` list.
///
/// Covers hidden files/directories (dot-prefix) and the most common
/// tool-maintained output directories. These remain in the defaults
/// because they are walker-cost optimizations — skipping large
/// irrelevant trees, not silencing false positives.
///
/// Per ADR-007 § DOC-006, `**/CHANGELOG.md` and `**/README.md` are
/// intentionally absent: under DOC-001 those files are skipped by
/// classification (they don't claim intent), so listing them as
/// `[ignore]` defaults would be a vestigial workaround for the
/// pre-DOC-001 over-firing problem.
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "**/.*",
    "target/**",
    "**/node_modules/**",
    "**/__pycache__/**",
    "**/dist/**",
    "**/build/**",
    "**/log/**",
    "**/logs/**",
    "**/tmp/**",
];

/// Render a starter `ctxgrd.toml`.
///
/// `active` namespaces render as regular TOML blocks — fully enabled.
/// `commented` namespaces render with every line prefixed by `# `, as
/// an in-file example the user can enable later by un-commenting.
///
/// Every block gets the full nine-rule set and populated
/// `required-headings` / `required-metadata` / `allowed-values`
/// sub-tables. Heading defaults are namespace-specific: the built-in
/// doc pack shape (`project-docs` / `ops`) where a pack defines the
/// namespace (with the conventional minimal shape as a commented
/// alternative),
/// else the conventional shape (ADR: Status/Context/Decision/
/// Consequences; etc.) — so the generated file is immediately
/// lintable without editing.
///
/// Intended entry point for `ctxgrd init` and for an LSP-driven
/// "Initialize workspace" quick-fix down the road.
pub fn render_init_toml(active: &[&str], commented: &[&str], paths: &DetectedPaths) -> String {
    let mut out = String::new();
    out.push_str("# ctxgrd.toml — generated by `ctxgrd init`.\n");
    out.push_str("# Each [<NAMESPACE>] section maps a rule list + params to\n");
    out.push_str("# documents with ids starting with that namespace.\n");
    out.push_str("# Uncomment any of the example blocks below to enable that\n");
    out.push_str("# namespace. Customize the headings / allowed values to match\n");
    out.push_str("# your team's conventions.\n\n");

    // [ignore] block at the top — it scopes which files ctxgrd looks
    // at in the first place. Ship with walker-cost optimizations only
    // (hidden dirs / build output); intent-based classification
    // (ADR-007 § DOC-001) handles non-document markdown like READMEs
    // without needing an [ignore] entry.
    out.push_str("# Files matching any of these globs are skipped by the\n");
    out.push_str("# markdown walker — no diagnostics, not listed anywhere.\n");
    out.push_str("# Patterns are relative to the lint root; gitignore-style.\n");
    out.push_str("[ignore]\n");
    out.push_str("patterns = [\n");
    for p in DEFAULT_IGNORE_PATTERNS {
        out.push_str(&format!("  \"{p}\",\n"));
    }
    out.push_str("]\n\n");

    let mut first = true;
    for ns in active {
        if !first {
            out.push('\n');
        }
        first = false;
        render_namespace_block(ns, &mut out, false, paths.get(*ns).map(|v| v.as_slice()));
    }
    for ns in commented {
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(&format!(
            "# --- Example: {ns}. Uncomment the block below to enable. ---\n"
        ));
        render_namespace_block(ns, &mut out, true, paths.get(*ns).map(|v| v.as_slice()));
    }
    out
}

/// Append a one-line TOML string array (`["a", "b"]`) plus newline.
fn push_toml_list<'a>(buf: &mut String, items: impl Iterator<Item = &'a str>) {
    buf.push('[');
    for (i, item) in items.enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        buf.push_str(&format!("\"{item}\""));
    }
    buf.push_str("]\n");
}

fn render_namespace_block(
    namespace: &str,
    out: &mut String,
    comment: bool,
    paths: Option<&[String]>,
) {
    let mut buf = String::new();
    buf.push_str(&format!("[{namespace}]\n"));
    buf.push_str("rules = [\n");
    for rule in [
        "core.frontmatter",
        "core.id",
        "core.id-unique",
        "core.dep-resolved",
        "core.dep-cycle",
        "core.cross-ref",
        "core.required-headings",
        "core.required-metadata",
        "core.allowed-values",
    ] {
        buf.push_str(&format!("  \"{rule}\",\n"));
    }
    buf.push_str("]\n");

    // ADR 007 § DOC-005: when init detected a conventional ADR/PRD
    // directory for this namespace, pre-fill `paths` so the user
    // can run `ctxgrd` immediately and see their docs lint.
    // Empty `paths` slice would still write `paths = []` — DOC-002
    // treats that as "match nothing", so callers should pass `None`
    // instead when they have nothing to inject.
    if let Some(globs) = paths {
        if !globs.is_empty() {
            buf.push_str("paths = [");
            for (i, g) in globs.iter().enumerate() {
                if i > 0 {
                    buf.push_str(", ");
                }
                buf.push_str(&format!("\"{g}\""));
            }
            buf.push_str("]\n");
        }
    }
    buf.push('\n');

    // Active headings come from a built-in doc pack (`project-docs` or
    // `ops`) when one defines this namespace (the full shape); the
    // conventional minimal shape is kept below as a commented alternative.
    // Namespaces no pack covers fall back to the conventional shape with
    // no alternative line.
    let conventional = default_headings(namespace);
    let pack_shape = crate::pack::builtin_pack_headings(namespace).filter(|p| {
        !p.iter()
            .map(String::as_str)
            .eq(conventional.iter().copied())
    });
    buf.push_str(&format!("[{namespace}.\"core.required-headings\"]\n"));
    let active: Vec<&str> = match &pack_shape {
        Some(p) => p.iter().map(String::as_str).collect(),
        None => conventional.clone(),
    };
    buf.push_str("headings = ");
    push_toml_list(&mut buf, active.iter().copied());
    if pack_shape.is_some() {
        buf.push_str("# Conventional minimal shape, if you prefer it:\n");
        buf.push_str("# headings = ");
        push_toml_list(&mut buf, conventional.iter().copied());
    }
    buf.push('\n');

    // Metadata keys and the status vocabulary follow the pack outright
    // (no commented alternative — vocabularies are pack-specific, e.g.
    // PMR's incident_date key or RUN's active/deprecated lifecycle).
    let metadata_keys = crate::pack::builtin_pack_metadata_keys(namespace)
        .unwrap_or_else(|| vec!["id".into(), "title".into(), "status".into()]);
    buf.push_str(&format!("[{namespace}.\"core.required-metadata\"]\n"));
    buf.push_str("keys = ");
    push_toml_list(&mut buf, metadata_keys.iter().map(String::as_str));
    buf.push('\n');

    let status_values = crate::pack::builtin_pack_status_values(namespace).unwrap_or_else(|| {
        vec![
            "draft".into(),
            "accepted".into(),
            "rejected".into(),
            "superseded".into(),
        ]
    });
    buf.push_str(&format!("[{namespace}.\"core.allowed-values\"]\n"));
    buf.push_str("status = ");
    push_toml_list(&mut buf, status_values.iter().map(String::as_str));

    if comment {
        for line in buf.lines() {
            if line.is_empty() {
                out.push_str("#\n");
            } else {
                out.push_str(&format!("# {line}\n"));
            }
        }
    } else {
        out.push_str(&buf);
    }
}

/// Conventional minimal H2 heading sets, per namespace.
///
/// These reflect the industry-standard structures for each record
/// type: ADR (Nygard), PRD (feature / goals / NFRs), DDR (design
/// decision + state matrix), RFC (IETF-shape), RUN (runbook).
/// Unknown namespaces get a minimal two-section template the
/// author can grow into.
///
/// For namespaces a built-in doc pack (`project-docs`, `ops`) defines,
/// the pack's fuller shape is rendered active and this set appears as
/// the commented alternative (see `render_namespace_block`).
fn default_headings(namespace: &str) -> Vec<&'static str> {
    match namespace {
        "ADR" => vec!["Status", "Context", "Decision", "Consequences"],
        "PRD" => vec!["Overview", "Goals", "Requirements", "Success metrics"],
        "DDR" => vec!["Status", "Context", "Decision", "State Matrix"],
        "RFC" => vec!["Abstract", "Motivation", "Proposal", "Alternatives"],
        "RUN" => vec![
            "Trigger",
            "Prerequisites",
            "Steps",
            "Rollback",
            "Verification",
        ],
        "PMR" => vec![
            "Summary",
            "Impact",
            "Timeline",
            "Root Cause",
            "Action Items",
        ],
        _ => vec!["Overview", "Details"],
    }
}

// -- init: body-header advisory (ADR 006 § EXT-003) ---------------------

/// Hardcoded list of conventionally-named ADR/PRD/RFC directories
/// that `ctxgrd init` sniffs for body-header `.md` files.
///
/// Each `(path, namespace)` entry maps a relative directory to the
/// namespace whose convention it implements. The list is intentionally
/// short and biased toward common shapes; ADR 006 § EXT-003 picks
/// "prefer misses to nags" — false positives in init output are
/// corrosive to user trust, so niche layouts go unflagged on purpose.
pub const BODY_HEADER_SCAN_DIRS: &[(&str, &str)] = &[
    // ADR
    ("docs/adr", "ADR"),
    ("docs/adrs", "ADR"),
    ("doc/adr", "ADR"),
    ("doc/adrs", "ADR"),
    ("docs/decisions", "ADR"),
    ("decisions", "ADR"),
    ("docs/architecture/decisions", "ADR"),
    ("docs/architecture", "ADR"),
    ("adr", "ADR"),
    ("adrs", "ADR"),
    // PRD
    ("docs/prd", "PRD"),
    ("docs/prds", "PRD"),
    // RFC
    ("docs/rfc", "RFC"),
    ("docs/rfcs", "RFC"),
    ("docs/proposals", "RFC"),
];

/// Result of scanning the EXT-003 directory list under a lint root.
///
/// Carries every directory from `BODY_HEADER_SCAN_DIRS` that exists
/// and contains at least one `.md` file. The advisory iterates over
/// directories whose `body_header_files` list is non-empty; future
/// callers (ADR 007 § DOC-005's `[<NS>].paths` pre-fill) iterate
/// over `detected_dirs` directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyHeaderSniff {
    pub detected_dirs: Vec<DetectedDir>,
}

/// One directory from the EXT-003 sniff that exists and contains
/// at least one `.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedDir {
    /// Path relative to the lint root, e.g. `"docs/adr"`. Forward
    /// slashes — matches the entries in `BODY_HEADER_SCAN_DIRS`.
    pub directory: String,
    /// Conventional namespace owning this directory.
    pub namespace: &'static str,
    /// Basenames of `.md` files in this directory whose first 16
    /// bytes do not begin with the YAML frontmatter delimiter.
    /// Empty when every `.md` file in the directory has frontmatter.
    pub body_header_files: Vec<String>,
}

impl BodyHeaderSniff {
    /// Subset of detected directories that contain at least one
    /// body-header `.md` file. EXT-003's advisory iterates here.
    pub fn directories_with_body_headers(&self) -> impl Iterator<Item = &DetectedDir> {
        self.detected_dirs
            .iter()
            .filter(|d| !d.body_header_files.is_empty())
    }
}

/// Walk the EXT-003 directory list under `root` and return a sniff
/// result. Missing or unreadable directories are skipped silently —
/// a fresh `ctxgrd init` against a tree without any of these
/// directories returns an empty `BodyHeaderSniff` and no advisory.
///
/// The scan is non-recursive on purpose: ADR-shaped directories
/// conventionally hold ADRs as flat files, and recursing risks
/// flagging unrelated nested content.
pub fn scan_body_headers(root: &Path) -> BodyHeaderSniff {
    let mut detected_dirs: Vec<DetectedDir> = Vec::new();
    for &(rel, namespace) in BODY_HEADER_SCAN_DIRS {
        let abs = root.join(rel);
        let read = match std::fs::read_dir(&abs) {
            Ok(it) => it,
            Err(_) => continue,
        };

        let mut basenames: Vec<String> = Vec::new();
        let mut body_header_files: Vec<String> = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let basename = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_owned(),
                None => continue,
            };
            let has_fm = file_starts_with_frontmatter(&path);
            basenames.push(basename.clone());
            if !has_fm {
                body_header_files.push(basename);
            }
        }

        if basenames.is_empty() {
            continue;
        }
        body_header_files.sort();
        detected_dirs.push(DetectedDir {
            directory: rel.to_string(),
            namespace,
            body_header_files,
        });
    }
    BodyHeaderSniff { detected_dirs }
}

/// Returns true iff `path`'s first 16 bytes begin with a `---` line —
/// the YAML frontmatter delimiter ctxgrd expects (ADR 003 § MD-002).
///
/// Accepts `---\n`, `---\r\n`, and a bare `---` at EOF. A `---` not
/// followed by a line terminator (e.g. `---foo`) is rejected: it's
/// not a frontmatter delimiter.
fn file_starts_with_frontmatter(path: &Path) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 16];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let head = &buf[..n];
    if !head.starts_with(b"---") {
        return false;
    }
    matches!(head.get(3), None | Some(b'\n' | b'\r'))
}

/// Render the EXT-003 advisory text for printing on stderr after
/// init's "Next steps" block. Returns `None` when no body-header
/// files were found — the advisory must stay silent in that case
/// so `ctxgrd init` against a clean tree produces no noise.
///
/// The leading newline is intentional: callers print this directly
/// after the "Next steps" block on stdout, and the blank line
/// separates the two visually even when stderr and stdout are
/// merged into one terminal stream.
pub fn render_body_header_advisory(sniff: &BodyHeaderSniff) -> Option<String> {
    let hits: Vec<&DetectedDir> = sniff.directories_with_body_headers().collect();
    if hits.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("\nDetected ADR-shaped directories without YAML frontmatter:\n");
    for hit in &hits {
        out.push_str(&format!("  • {}/\n", hit.directory));
        for name in &hit.body_header_files {
            out.push_str(&format!("      - {name}\n"));
        }
    }
    out.push_str("\nctxgrd lints documents with YAML frontmatter only (ADR 003 § MD-002).\n");
    out.push_str(
        "  • To migrate, move id/title/status/date from the body header into a\n    `---`-fenced YAML block at the top of each file.\n",
    );
    out.push_str("  • To silence, add the directory to [ignore].patterns in ctxgrd.toml.\n");
    Some(out)
}

// -- init: paths pre-fill (ADR 007 § DOC-005) ----------------------------

/// `namespace -> sorted, deduplicated list of `**`-suffixed globs`.
///
/// Result of grouping a `BodyHeaderSniff`'s detected directories by
/// the namespace they belong to. Consumed by `render_init_toml` (to
/// inject `paths = [...]` into each block) and by
/// `render_paths_announcement` (to print the receipt on stderr).
///
/// `BTreeMap` for deterministic iteration order — keeps test output
/// stable across runs without callers having to sort.
pub type DetectedPaths = std::collections::BTreeMap<String, Vec<String>>;

/// Group `sniff.detected_dirs` by namespace, mapping each detected
/// directory to a `**`-suffixed glob (e.g. `docs/adrs` → `docs/adrs/**`).
/// Globs within each namespace are sorted and deduplicated.
pub fn detected_paths_for_namespaces(sniff: &BodyHeaderSniff) -> DetectedPaths {
    let mut out: DetectedPaths = DetectedPaths::new();
    for d in &sniff.detected_dirs {
        out.entry(d.namespace.to_string())
            .or_default()
            .push(format!("{}/**", d.directory));
    }
    for v in out.values_mut() {
        v.sort();
        v.dedup();
    }
    out
}

/// Render the DOC-005 stderr announcement for `paths` filtered to
/// `active` namespaces. Returns `None` when no active namespace has
/// any pre-filled paths — silence is correct for greenfield trees.
///
/// Format: one line per active namespace, naming each detected
/// directory (with trailing slash, comma-separated for multi-dir
/// matches). Sample lines:
///
/// ```text
/// Pre-filled [ADR].paths from detected docs/adrs/.
/// Pre-filled [ADR].paths from detected docs/adr/, docs/adrs/.
/// ```
///
/// Commented namespaces are deliberately not announced — their
/// `paths` are correct-on-uncomment but the user has not opted in
/// yet, and `init` should not report behaviour the user did not ask
/// for.
pub fn render_paths_announcement(paths: &DetectedPaths, active: &[&str]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for ns in active {
        let Some(globs) = paths.get(*ns) else {
            continue;
        };
        if globs.is_empty() {
            continue;
        }
        let dirs: Vec<String> = globs
            .iter()
            .map(|g| {
                let stem = g.strip_suffix("/**").unwrap_or(g);
                format!("{stem}/")
            })
            .collect();
        lines.push(format!(
            "Pre-filled [{ns}].paths from detected {}.",
            dirs.join(", ")
        ));
    }
    if lines.is_empty() {
        None
    } else {
        let mut out = String::from("\n");
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamespaceConfig;
    use crate::id::DocumentId;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn make_doc(raw_id: &str, location: &str) -> Document {
        Document {
            id: raw_id.parse().unwrap(),
            raw_id: raw_id.to_owned(),
            location: location.to_owned(),
            depends_on: vec![],
            frontmatter_lines: Default::default(),
            metadata: Default::default(),
            ast: None,
            body: String::new(),
        }
    }

    fn ns_cfg(keys: Vec<&str>, headings: Vec<&str>) -> NamespaceConfig {
        let mut params: BTreeMap<String, serde_json::Value> = Default::default();
        params.insert(
            "core.required-metadata".to_string(),
            json!({ "keys": keys }),
        );
        params.insert(
            "core.required-headings".to_string(),
            json!({ "headings": headings }),
        );
        NamespaceConfig {
            rules: vec![],
            params,
            paths: None,
            path_patterns: Vec::new(),
        }
    }

    #[test]
    fn slug_basic() {
        assert_eq!(
            slugify("Switch to append-only object storage"),
            "switch-to-append-only-object-storage"
        );
    }

    #[test]
    fn slug_uppercase_and_punctuation() {
        assert_eq!(slugify("Hello, World!!!"), "hello-world");
    }

    #[test]
    fn slug_empty_and_non_alnum_only() {
        assert_eq!(slugify(""), "untitled");
        assert_eq!(slugify("!!!"), "untitled");
        assert_eq!(slugify("   "), "untitled");
    }

    #[test]
    fn slug_truncates_to_60() {
        // 80-char string → 60 chars max, with no trailing dash.
        let long = "a".repeat(80);
        let s = slugify(&long);
        assert!(s.len() <= 60);
        assert!(!s.ends_with('-'));
    }

    #[test]
    fn slug_truncate_trims_trailing_dash() {
        // 60-char boundary lands in the middle of a dash.
        let title = format!("{}-suffix", "a".repeat(58));
        let s = slugify(&title);
        assert!(!s.ends_with('-'));
    }

    #[test]
    fn next_id_empty_namespace_starts_at_1() {
        let docs = vec![make_doc("PRD-001", "prds/PRD-001-a.md")];
        assert_eq!(next_id("ADR", &docs), 1);
    }

    #[test]
    fn next_id_follows_max_plus_one() {
        let docs = vec![
            make_doc("ADR-001", "adrs/ADR-001-a.md"),
            make_doc("ADR-099", "adrs/ADR-099-b.md"),
        ];
        // Brief's CLI-001 clause 1 is literal `max + 1`.
        assert_eq!(next_id("ADR", &docs), 100);
    }

    /// `NamespaceConfig` carrying only `path_patterns` — the field
    /// `target_dir`'s NEW-004 step reads.
    fn ns_cfg_with_paths(globs: &[&str]) -> NamespaceConfig {
        NamespaceConfig {
            path_patterns: globs.iter().map(|g| g.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn target_dir_follows_existing_docs() {
        let docs = vec![make_doc("ADR-001", "adrs/ADR-001-foo.md")];
        let dir = target_dir("ADR", &NamespaceConfig::default(), &docs, Path::new("/root"), None);
        assert_eq!(dir, Path::new("/root/adrs"));
    }

    #[test]
    fn target_dir_falls_back_to_lowercase_plural() {
        let docs = vec![];
        let dir = target_dir("ADR", &NamespaceConfig::default(), &docs, Path::new("/root"), None);
        assert_eq!(dir, Path::new("/root/adrs"));
    }

    #[test]
    fn target_dir_honours_out_override() {
        let docs = vec![make_doc("ADR-001", "adrs/ADR-001-foo.md")];
        let dir = target_dir(
            "ADR",
            &NamespaceConfig::default(),
            &docs,
            Path::new("/root"),
            Some(Path::new("/custom")),
        );
        assert_eq!(dir, Path::new("/custom"));
    }

    // -- ADR-010 NEW-004 / BUG-002: greenfield path-claimed namespace --

    #[test]
    fn target_dir_uses_paths_prefix_when_namespace_empty() {
        // Empty namespace + declared paths: land in the declared home,
        // not the hardcoded `runs/` fallback (BUG-002 acceptance row 1).
        let docs = vec![];
        let cfg = ns_cfg_with_paths(&["docs/runbooks/**"]);
        let dir = target_dir("RUN", &cfg, &docs, Path::new("/root"), None);
        assert_eq!(dir, Path::new("/root/docs/runbooks"));
    }

    #[test]
    fn target_dir_existing_doc_beats_paths() {
        // Populated namespace: NEW-001 still wins over config, so
        // mid-migration co-location is unchanged (acceptance row 3).
        let docs = vec![make_doc("RUN-001", "ops/RUN-001-deploy.md")];
        let cfg = ns_cfg_with_paths(&["docs/runbooks/**"]);
        let dir = target_dir("RUN", &cfg, &docs, Path::new("/root"), None);
        assert_eq!(dir, Path::new("/root/ops"));
    }

    #[test]
    fn target_dir_out_override_beats_paths() {
        // `--out` still wins over a declared paths home (acceptance row 4).
        let docs = vec![];
        let cfg = ns_cfg_with_paths(&["docs/runbooks/**"]);
        let dir = target_dir("RUN", &cfg, &docs, Path::new("/root"), Some(Path::new("/custom")));
        assert_eq!(dir, Path::new("/custom"));
    }

    #[test]
    fn target_dir_empty_prefix_glob_falls_through() {
        // A wildcard in the first segment has no literal prefix, so the
        // ladder falls through to NEW-003 (acceptance row 5).
        let docs = vec![];
        let cfg = ns_cfg_with_paths(&["**/runbooks/**"]);
        let dir = target_dir("RUN", &cfg, &docs, Path::new("/root"), None);
        assert_eq!(dir, Path::new("/root/runs"));
    }

    #[test]
    fn target_dir_uses_first_glob_when_multiple() {
        // First-entry prefix is deterministic when globs disagree.
        let docs = vec![];
        let cfg = ns_cfg_with_paths(&["docs/runbooks/**", "ops/runbooks/**"]);
        let dir = target_dir("RUN", &cfg, &docs, Path::new("/root"), None);
        assert_eq!(dir, Path::new("/root/docs/runbooks"));
    }

    #[test]
    fn glob_literal_prefix_cases() {
        assert_eq!(
            glob_literal_prefix("docs/runbooks/**"),
            Some(PathBuf::from("docs/runbooks"))
        );
        assert_eq!(
            glob_literal_prefix("docs/specs/*.md"),
            Some(PathBuf::from("docs/specs"))
        );
        assert_eq!(
            glob_literal_prefix("docs/runbooks"),
            Some(PathBuf::from("docs/runbooks"))
        );
        assert_eq!(glob_literal_prefix("**/runbooks/**"), None);
        assert_eq!(glob_literal_prefix("**"), None);
    }

    #[test]
    fn scaffold_matches_golden_shape() {
        let cfg = ns_cfg(
            vec!["id", "title", "status"],
            vec!["Status", "Context", "Decision", "Consequences"],
        );
        let existing = vec![make_doc("ADR-001", "adrs/ADR-001-foo.md")];
        let s = scaffold(
            "ADR",
            "Switch to append-only object storage",
            Some(2),
            &cfg,
            &existing,
            Path::new("."),
            None,
        );
        let expected = "\
---
id: ADR-002
title: Switch to append-only object storage
status:
depends_on: []
---

# ADR-002: Switch to append-only object storage

## Status

## Context

## Decision

## Consequences
";
        assert_eq!(s.contents, expected);
        assert_eq!(
            s.target_path,
            Path::new("./adrs/002-switch-to-append-only-object-storage.md")
        );
    }

    #[test]
    fn scaffold_honours_id_override() {
        let cfg = ns_cfg(vec!["id", "title"], vec!["Status"]);
        let s = scaffold("ADR", "Doc", Some(7), &cfg, &[], Path::new("."), None);
        assert_eq!(s.id, DocumentId::new("ADR", 7));
        assert!(s.contents.contains("id: ADR-007"));
    }

    #[test]
    fn yaml_scalar_quotes_when_needed() {
        assert_eq!(yaml_scalar("plain text"), "plain text");
        assert_eq!(yaml_scalar("has: colon"), "\"has: colon\"");
        assert_eq!(yaml_scalar("starts with -"), "starts with -");
        assert_eq!(yaml_scalar("-starts with dash"), "\"-starts with dash\"");
        assert_eq!(yaml_scalar(""), "\"\"");
        assert_eq!(yaml_scalar("has \"quotes\""), "\"has \\\"quotes\\\"\"");
    }

    #[test]
    fn scaffold_omits_required_metadata_keys_that_would_duplicate_id_or_title() {
        // Config lists [id, title, status, owner]. id and title live
        // in their own slots; status and owner come through as stubs.
        let cfg = ns_cfg(vec!["id", "title", "status", "owner"], vec!["Summary"]);
        let s = scaffold("ADR", "Some doc", Some(1), &cfg, &[], Path::new("."), None);
        let lines: Vec<&str> = s.contents.lines().collect();
        assert!(lines.contains(&"id: ADR-001"));
        assert!(lines.contains(&"title: Some doc"));
        assert!(lines.contains(&"status:"));
        assert!(lines.contains(&"owner:"));
        // id/title must only appear once (once in their own slot, not
        // again as a stubbed required-metadata entry).
        assert_eq!(lines.iter().filter(|l| l.starts_with("id:")).count(), 1);
        assert_eq!(lines.iter().filter(|l| l.starts_with("title:")).count(), 1);
    }

    // -- ctxgrd init tests -------------------------------------------

    #[test]
    fn init_toml_default_is_valid_and_lintable() {
        let toml_text = render_init_toml(&["ADR"], &[], &DetectedPaths::new());
        // Must be valid TOML.
        let value: toml::Value = toml::from_str(&toml_text).expect("valid TOML");
        let adr = value.get("ADR").expect("ADR section present");
        let rules = adr.get("rules").and_then(|v| v.as_array()).unwrap();
        // All nine core rules listed.
        assert_eq!(rules.len(), 9);
        // ADR gets the project-docs pack shape as the active headings.
        let headings = adr
            .get("core.required-headings")
            .and_then(|v| v.get("headings"))
            .and_then(|v| v.as_array())
            .unwrap();
        let names: Vec<&str> = headings.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Status",
                "Context",
                "Requirements",
                "Consequences",
                "Open Questions",
                "References",
                "Change log"
            ]
        );
        // The conventional Nygard shape rides along as a commented
        // alternative the user can swap in.
        assert!(
            toml_text.contains("# Conventional minimal shape, if you prefer it:"),
            "expected commented conventional alternative"
        );
        assert!(
            toml_text
                .contains("# headings = [\"Status\", \"Context\", \"Decision\", \"Consequences\"]"),
            "expected Nygard headings as comment"
        );
        // Metadata keys and status vocabulary follow the pack too.
        let keys: Vec<&str> = adr
            .get("core.required-metadata")
            .and_then(|v| v.get("keys"))
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(keys, vec!["id", "title", "status", "date"]);
    }

    #[test]
    fn init_toml_multiple_namespaces_each_get_a_block() {
        let toml_text = render_init_toml(&["ADR", "PRD"], &[], &DetectedPaths::new());
        let value: toml::Value = toml::from_str(&toml_text).expect("valid TOML");
        assert!(value.get("ADR").is_some());
        assert!(value.get("PRD").is_some());
        // PRD gets its own heading list — NOT the ADR ones.
        let prd_headings = value
            .get("PRD")
            .and_then(|v| v.get("core.required-headings"))
            .and_then(|v| v.get("headings"))
            .and_then(|v| v.as_array())
            .unwrap();
        let names: Vec<&str> = prd_headings.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"User stories"));
        assert!(names.contains(&"Definition of Done"));
        assert!(names.contains(&"Goals"));
        // The conventional minimal PRD shape is the commented alternative.
        assert!(toml_text.contains(
            "# headings = [\"Overview\", \"Goals\", \"Requirements\", \"Success metrics\"]"
        ));
        // PRD status vocabulary follows the pack (no "rejected").
        let status: Vec<&str> = value
            .get("PRD")
            .and_then(|v| v.get("core.allowed-values"))
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(status, vec!["draft", "accepted", "superseded"]);
    }

    #[test]
    fn init_toml_pack_uncovered_namespace_has_no_alternative_comment() {
        // DDR is not defined by the project-docs pack — its conventional
        // shape stays active with no commented alternative.
        let toml_text = render_init_toml(&["DDR"], &[], &DetectedPaths::new());
        let value: toml::Value = toml::from_str(&toml_text).expect("valid TOML");
        let headings = value
            .get("DDR")
            .and_then(|v| v.get("core.required-headings"))
            .and_then(|v| v.get("headings"))
            .and_then(|v| v.as_array())
            .unwrap();
        let names: Vec<&str> = headings.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(names, vec!["Status", "Context", "Decision", "State Matrix"]);
        assert!(!toml_text.contains("# Conventional minimal shape"));
        // Pack-uncovered namespaces keep the generic metadata and status.
        assert!(toml_text.contains("keys = [\"id\", \"title\", \"status\"]"));
        assert!(
            toml_text.contains("status = [\"draft\", \"accepted\", \"rejected\", \"superseded\"]")
        );
    }

    #[test]
    fn init_toml_unknown_namespace_gets_generic_template() {
        let toml_text = render_init_toml(&["XYZ"], &[], &DetectedPaths::new());
        let value: toml::Value = toml::from_str(&toml_text).expect("valid TOML");
        let headings = value
            .get("XYZ")
            .and_then(|v| v.get("core.required-headings"))
            .and_then(|v| v.get("headings"))
            .and_then(|v| v.as_array())
            .unwrap();
        let names: Vec<&str> = headings.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(names, vec!["Overview", "Details"]);
    }

    #[test]
    fn init_toml_passes_the_kernel_config_validator() {
        // The generated config must pass ctxgrd's own validator —
        // otherwise users get a kernel error the first time they
        // run `ctxgrd` after `ctxgrd init`.
        let toml_text = render_init_toml(&["ADR", "PRD", "DDR"], &[], &DetectedPaths::new());
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ctxgrd.toml"), &toml_text).unwrap();
        let config = crate::config::load_with_global(tmp.path(), None)
            .expect("generated config must validate");
        assert_eq!(config.namespaces.len(), 3);
        assert!(config.namespaces["ADR"].enables("core.cross-ref"));
    }

    #[test]
    fn init_toml_default_starter_has_active_adr_prd_and_commented_rest() {
        let toml_text = render_init_toml(
            DEFAULT_ACTIVE_NAMESPACES,
            DEFAULT_COMMENTED_NAMESPACES,
            &DetectedPaths::new(),
        );
        // ADR + PRD must appear as active TOML (parseable).
        let value: toml::Value = toml::from_str(&toml_text).expect("valid TOML");
        assert!(value.get("ADR").is_some(), "ADR should be active");
        assert!(value.get("PRD").is_some(), "PRD should be active");
        // DDR/RFC/RUN/PMR must NOT be active top-level tables —
        // they're commented out.
        for ns in DEFAULT_COMMENTED_NAMESPACES {
            assert!(
                value.get(*ns).is_none(),
                "{ns} should NOT be an active TOML section"
            );
        }
        // But their blocks should appear as `# [<NS>]` comments.
        for ns in DEFAULT_COMMENTED_NAMESPACES {
            assert!(
                toml_text.contains(&format!("# [{ns}]")),
                "expected commented-out [{ns}] block in output"
            );
        }
    }

    // -- new-rule scaffolder tests -----------------------------------

    #[test]
    fn rule_scaffold_basic_paths_and_code_split() {
        let s = scaffold_rule(
            "design.open-questions-non-empty",
            Some("Open Questions section must be non-empty"),
            Path::new("/root"),
            None,
        )
        .unwrap();
        assert_eq!(s.code, "design.open-questions-non-empty");
        assert_eq!(s.namespace, "design");
        assert_eq!(s.name, "open-questions-non-empty");
        assert_eq!(
            s.run_path,
            Path::new("/root/rules/design/open-questions-non-empty/run")
        );
        assert_eq!(
            s.readme_path,
            Path::new("/root/rules/design/open-questions-non-empty/README.md")
        );
    }

    #[test]
    fn rule_scaffold_run_template_encodes_contract() {
        let s =
            scaffold_rule("design.foo", Some("Description here"), Path::new("."), None).unwrap();
        // ADR-002 batch contract pitfalls the template must encode.
        assert!(s.run_contents.starts_with("#!/usr/bin/env bash\n"));
        assert!(s.run_contents.contains("set -euo pipefail"));
        // RUL-002: stdin loop reading JSONL records.
        assert!(
            s.run_contents.contains("while IFS= read -r line"),
            "template must use stdin loop for batch mode (RUL-002)"
        );
        assert!(
            s.run_contents.contains("jq -r '.path'"),
            "template must extract `.path` from each record"
        );
        // RUL-005: rule params still flow via env.
        assert!(s.run_contents.contains("CTXGRD_RULE_PARAMS"));
        // RUL-006: sidecar gone, context flows inline on stdin.
        assert!(
            !s.run_contents.contains("CTXGRD_DOC_CONTEXT"),
            "template must NOT reference CTXGRD_DOC_CONTEXT (RUL-006: removed)"
        );
        assert!(
            s.run_contents.contains(".context.metadata"),
            "template must show how to extract metadata from inline context"
        );
        assert!(
            s.run_contents.contains("DO NOT emit a `code` field"),
            "template must warn against emitting a `code` field"
        );
        // The rule code should appear as a header comment.
        assert!(s.run_contents.contains("# design.foo"));
        // The description should appear as a comment.
        assert!(s.run_contents.contains("# Description here"));
    }

    #[test]
    fn rule_scaffold_run_template_omits_description_block_when_empty() {
        let s = scaffold_rule("design.foo", None, Path::new("."), None).unwrap();
        // No empty description comment line.
        let header_block: Vec<&str> = s.run_contents.lines().take(4).collect();
        // After "#!/usr/bin/env bash" and "# design.foo", the next
        // non-blank comment should be the Contract block, NOT an
        // empty "# " line from a stripped description.
        assert_eq!(header_block[0], "#!/usr/bin/env bash");
        assert_eq!(header_block[1], "# design.foo");
        assert!(
            header_block[2].starts_with("# Contract"),
            "expected contract block on line 3, got: {:?}",
            header_block[2]
        );
    }

    #[test]
    fn rule_scaffold_readme_uppercases_namespace_for_toml_block() {
        let s = scaffold_rule("design.foo", Some("desc"), Path::new("."), None).unwrap();
        // README's TOML example must reference [DESIGN] (uppercase),
        // not [design] — namespaces in ctxgrd.toml are uppercase.
        assert!(s.readme_contents.contains("[DESIGN]"));
        assert!(s.readme_contents.contains("\"design.foo\""));
    }

    #[test]
    fn rule_scaffold_honours_out_override() {
        let s = scaffold_rule(
            "design.foo",
            None,
            Path::new("/root"),
            Some(Path::new("/custom/place")),
        )
        .unwrap();
        assert_eq!(s.run_path, Path::new("/custom/place/run"));
        assert_eq!(s.readme_path, Path::new("/custom/place/README.md"));
    }

    #[test]
    fn rule_scaffold_rejects_invalid_codes() {
        // Missing dot.
        assert!(scaffold_rule("designfoo", None, Path::new("."), None).is_err());
        // Empty namespace.
        assert!(scaffold_rule(".foo", None, Path::new("."), None).is_err());
        // Empty name.
        assert!(scaffold_rule("design.", None, Path::new("."), None).is_err());
        // Uppercase letters (rule codes are always lowercase by convention).
        assert!(scaffold_rule("Design.foo", None, Path::new("."), None).is_err());
        // Underscore in name (kebab-only).
        assert!(scaffold_rule("design.foo_bar", None, Path::new("."), None).is_err());
        // Multiple dots.
        assert!(scaffold_rule("design.foo.bar", None, Path::new("."), None).is_err());
    }

    #[test]
    fn rule_scaffold_rejects_reserved_core_namespace() {
        let err = scaffold_rule("core.frontmatter", None, Path::new("."), None).unwrap_err();
        assert!(err.contains("reserved"));
    }

    #[test]
    fn init_toml_commented_block_preserves_rule_list() {
        // The commented-out block must contain every rule so a user
        // who uncomments it gets the same full config as an active
        // block.
        let text = render_init_toml(&[], &["DDR"], &DetectedPaths::new());
        for rule in ["core.frontmatter", "core.cross-ref", "core.allowed-values"] {
            // `  "<rule>"` indented → commented as `#   "<rule>"`.
            assert!(
                text.contains(&format!("#   \"{rule}\"")),
                "commented block should contain rule {rule}"
            );
        }
        assert!(text.contains("# [DDR.\"core.required-headings\"]"));
    }

    #[test]
    fn write_run_script_creates_parents_and_sets_executable_bit() {
        let tmp = tempfile::tempdir().unwrap();
        let scaffold = scaffold_rule(
            "design.has-decision",
            Some("Every DESIGN must end with a Decision section."),
            tmp.path(),
            None,
        )
        .expect("valid code scaffolds");

        scaffold.write_run_script().unwrap();

        assert!(scaffold.run_path.is_file());
        // Parent directory was created on demand.
        assert!(scaffold.run_path.parent().unwrap().is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&scaffold.run_path)
                .unwrap()
                .permissions()
                .mode();
            // ADR-002 § RUL-006: 0o755 is the load-bearing bit.
            // Mask off the file-type bits (0o170000) before comparing.
            assert_eq!(mode & 0o777, 0o755, "run script must be 0o755");
        }
    }

    #[test]
    fn write_run_script_refuses_to_overwrite_existing_user_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let scaffold = scaffold_rule("design.has-decision", None, tmp.path(), None)
            .expect("valid code scaffolds");
        std::fs::create_dir_all(scaffold.run_path.parent().unwrap()).unwrap();
        std::fs::write(&scaffold.run_path, b"# user-edited content").unwrap();

        let err = scaffold.write_run_script().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // The user's edits survived — load-bearing for ADR-002 § RUL-006.
        let after = std::fs::read_to_string(&scaffold.run_path).unwrap();
        assert_eq!(after, "# user-edited content");
    }

    #[test]
    fn write_readme_skips_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let scaffold = scaffold_rule("design.has-decision", None, tmp.path(), None)
            .expect("valid code scaffolds");
        std::fs::create_dir_all(scaffold.readme_path.parent().unwrap()).unwrap();
        std::fs::write(&scaffold.readme_path, b"# user docs").unwrap();

        // First call: file already there → Ok(false), no overwrite.
        let written = scaffold.write_readme().unwrap();
        assert!(!written, "must not overwrite existing README");
        let preserved = std::fs::read_to_string(&scaffold.readme_path).unwrap();
        assert_eq!(preserved, "# user docs");
    }

    #[test]
    fn write_readme_writes_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let scaffold = scaffold_rule("design.has-decision", None, tmp.path(), None)
            .expect("valid code scaffolds");
        std::fs::create_dir_all(scaffold.readme_path.parent().unwrap()).unwrap();

        let written = scaffold.write_readme().unwrap();
        assert!(written, "fresh write returns Ok(true)");
        assert!(scaffold.readme_path.is_file());
    }

    // -- ADR 006 § EXT-003: body-header sniff -------------------------

    fn write_md(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn sniff_empty_root_returns_no_detected_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let sniff = scan_body_headers(tmp.path());
        assert!(sniff.detected_dirs.is_empty());
        assert!(render_body_header_advisory(&sniff).is_none());
    }

    #[test]
    fn sniff_frontmatter_only_dir_does_not_advise() {
        let tmp = tempfile::tempdir().unwrap();
        write_md(
            tmp.path(),
            "docs/adr/0001-clean.md",
            "---\nid: ADR-001\n---\n\n# x\n",
        );
        let sniff = scan_body_headers(tmp.path());

        // Directory IS detected (it exists and contains an .md), but
        // its body-header list is empty so the advisory stays silent.
        let detected = sniff
            .detected_dirs
            .iter()
            .find(|d| d.directory == "docs/adr")
            .expect("docs/adr is detected");
        assert_eq!(detected.namespace, "ADR");
        assert!(detected.body_header_files.is_empty());
        assert!(render_body_header_advisory(&sniff).is_none());
    }

    #[test]
    fn sniff_body_header_file_triggers_advisory() {
        let tmp = tempfile::tempdir().unwrap();
        write_md(
            tmp.path(),
            "docs/adr/0001-old-shape.md",
            "# 1. Use Postgres\n\nDate: 2024-01-01\n\nStatus: Accepted\n",
        );
        write_md(
            tmp.path(),
            "docs/adr/0002-clean.md",
            "---\nid: ADR-002\n---\n\n# x\n",
        );
        let sniff = scan_body_headers(tmp.path());

        let detected = sniff
            .detected_dirs
            .iter()
            .find(|d| d.directory == "docs/adr")
            .expect("docs/adr is detected");
        assert_eq!(detected.body_header_files, vec!["0001-old-shape.md"]);

        let advisory = render_body_header_advisory(&sniff).expect("advisory rendered");
        assert!(
            advisory.contains("docs/adr/"),
            "names directory: {advisory}"
        );
        assert!(
            advisory.contains("0001-old-shape.md"),
            "names file: {advisory}"
        );
        assert!(
            advisory.contains("YAML")
                && advisory.contains("frontmatter")
                && advisory.contains("To migrate"),
            "explains the migration without depending on an internal-doc link: {advisory}"
        );
        assert!(
            advisory.contains("[ignore].patterns"),
            "names the escape hatch: {advisory}"
        );
    }

    #[test]
    fn sniff_treats_crlf_frontmatter_as_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        // Windows line endings — `---\r\n` must still count as frontmatter.
        write_md(
            tmp.path(),
            "docs/adr/0001-crlf.md",
            "---\r\nid: ADR-001\r\n---\r\n\r\n# x\r\n",
        );
        let sniff = scan_body_headers(tmp.path());
        let detected = sniff
            .detected_dirs
            .iter()
            .find(|d| d.directory == "docs/adr")
            .unwrap();
        assert!(
            detected.body_header_files.is_empty(),
            "CRLF frontmatter must not be flagged"
        );
    }

    #[test]
    fn sniff_rejects_three_dashes_with_no_terminator() {
        // `---foo` is not a frontmatter delimiter (no newline / EOF
        // after the dashes). MUST count as body-header.
        let tmp = tempfile::tempdir().unwrap();
        write_md(tmp.path(), "docs/adr/0001-weird.md", "---foo\n# title\n");
        let sniff = scan_body_headers(tmp.path());
        let detected = sniff
            .detected_dirs
            .iter()
            .find(|d| d.directory == "docs/adr")
            .unwrap();
        assert_eq!(detected.body_header_files, vec!["0001-weird.md"]);
    }

    #[test]
    fn sniff_skips_non_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("docs/adr")).unwrap();
        std::fs::write(
            tmp.path().join("docs/adr/README.txt"),
            "# README, not flagged",
        )
        .unwrap();
        let sniff = scan_body_headers(tmp.path());
        // No .md files at all → directory is not even reported.
        assert!(
            !sniff
                .detected_dirs
                .iter()
                .any(|d| d.directory == "docs/adr"),
            "empty-of-md directories are skipped"
        );
    }

    #[test]
    fn sniff_namespace_mapping_covers_adr_prd_rfc() {
        // Smoke: every directory in the hardcoded list maps to one of
        // the namespace prefixes we currently emit defaults for.
        for &(_, ns) in BODY_HEADER_SCAN_DIRS {
            assert!(
                matches!(ns, "ADR" | "PRD" | "RFC"),
                "unexpected namespace mapping: {ns}"
            );
        }
    }

    #[test]
    fn sniff_directory_list_matches_adr_006_ext_003() {
        // ADR 006 § EXT-003 specifies a minimum directory list. Lock
        // it in so future edits to BODY_HEADER_SCAN_DIRS that drop
        // any of the spec'd entries fail loudly here.
        let required = [
            "docs/adr",
            "docs/adrs",
            "doc/adr",
            "doc/adrs",
            "docs/decisions",
            "decisions",
            "docs/architecture",
            "docs/architecture/decisions",
            "adr",
            "adrs",
            "docs/prd",
            "docs/prds",
            "docs/rfc",
            "docs/rfcs",
            "docs/proposals",
        ];
        let actual: Vec<&str> = BODY_HEADER_SCAN_DIRS.iter().map(|(d, _)| *d).collect();
        for dir in required {
            assert!(actual.contains(&dir), "EXT-003 minimum list missing: {dir}");
        }
    }

    // -- ADR 007 § DOC-005: paths pre-fill -----------------------------

    fn fake_sniff(entries: &[(&str, &'static str)]) -> BodyHeaderSniff {
        BodyHeaderSniff {
            detected_dirs: entries
                .iter()
                .map(|(dir, ns)| DetectedDir {
                    directory: dir.to_string(),
                    namespace: ns,
                    body_header_files: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn detected_paths_groups_by_namespace_and_appends_glob() {
        let sniff = fake_sniff(&[("docs/adrs", "ADR"), ("docs/prd", "PRD")]);
        let paths = detected_paths_for_namespaces(&sniff);
        assert_eq!(paths.get("ADR"), Some(&vec!["docs/adrs/**".to_string()]));
        assert_eq!(paths.get("PRD"), Some(&vec!["docs/prd/**".to_string()]));
    }

    #[test]
    fn detected_paths_sorts_and_dedups_within_namespace() {
        let sniff = fake_sniff(&[
            ("docs/adrs", "ADR"),
            ("docs/adr", "ADR"),
            ("docs/adrs", "ADR"), // duplicate
        ]);
        let paths = detected_paths_for_namespaces(&sniff);
        assert_eq!(
            paths.get("ADR"),
            Some(&vec!["docs/adr/**".to_string(), "docs/adrs/**".to_string()])
        );
    }

    #[test]
    fn render_init_toml_injects_paths_into_active_block() {
        let mut paths = DetectedPaths::new();
        paths.insert("ADR".to_string(), vec!["docs/adrs/**".to_string()]);
        let text = render_init_toml(&["ADR"], &[], &paths);
        // Single-line TOML array, sits right after `]` of `rules = [...]`.
        assert!(
            text.contains("paths = [\"docs/adrs/**\"]"),
            "expected paths line; rendered:\n{text}"
        );
        // Still parses as valid TOML.
        let value: toml::Value = toml::from_str(&text).expect("valid TOML");
        let adr = value.get("ADR").expect("ADR section");
        let arr = adr
            .get("paths")
            .and_then(|v| v.as_array())
            .expect("ADR.paths array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].as_str(), Some("docs/adrs/**"));
    }

    #[test]
    fn render_init_toml_emits_paths_in_commented_block() {
        // Commented blocks must also carry pre-fill so users who
        // un-comment a namespace later get the right paths without
        // a second round of `init`. Every line of the block is
        // prefixed with `# `, including the new paths line.
        let mut paths = DetectedPaths::new();
        paths.insert("RFC".to_string(), vec!["docs/rfc/**".to_string()]);
        let text = render_init_toml(&[], &["RFC"], &paths);
        assert!(
            text.contains("# paths = [\"docs/rfc/**\"]"),
            "expected commented paths line; rendered:\n{text}"
        );
    }

    #[test]
    fn render_init_toml_omits_paths_when_namespace_has_none() {
        let text = render_init_toml(&["ADR"], &[], &DetectedPaths::new());
        assert!(
            !text.contains("paths ="),
            "no paths key when nothing detected; rendered:\n{text}"
        );
    }

    #[test]
    fn render_init_toml_emits_multiple_globs_in_one_array() {
        let mut paths = DetectedPaths::new();
        paths.insert(
            "ADR".to_string(),
            vec!["docs/adr/**".to_string(), "docs/adrs/**".to_string()],
        );
        let text = render_init_toml(&["ADR"], &[], &paths);
        assert!(
            text.contains(r#"paths = ["docs/adr/**", "docs/adrs/**"]"#),
            "expected multi-glob line; rendered:\n{text}"
        );
    }

    #[test]
    fn paths_announcement_filters_to_active_namespaces() {
        let mut paths = DetectedPaths::new();
        paths.insert("ADR".to_string(), vec!["docs/adrs/**".to_string()]);
        paths.insert("RFC".to_string(), vec!["docs/rfc/**".to_string()]);
        let out = render_paths_announcement(&paths, &["ADR"])
            .expect("announcement when active match present");
        assert!(out.contains("Pre-filled [ADR].paths from detected docs/adrs/."));
        // RFC is detected but not active; announcement must stay
        // silent about it (DOC-005: no reporting un-opted-in behaviour).
        assert!(
            !out.contains("[RFC]"),
            "commented namespaces are not announced; got:\n{out}"
        );
    }

    #[test]
    fn paths_announcement_silent_when_no_active_match() {
        let mut paths = DetectedPaths::new();
        paths.insert("RFC".to_string(), vec!["docs/rfc/**".to_string()]);
        // RFC is detected but the user opted only into ADR/PRD.
        // No announcement at all — silence is correct.
        assert!(render_paths_announcement(&paths, &["ADR", "PRD"]).is_none());
    }

    #[test]
    fn paths_announcement_lists_multiple_dirs_with_trailing_slash() {
        let mut paths = DetectedPaths::new();
        paths.insert(
            "ADR".to_string(),
            vec!["docs/adr/**".to_string(), "docs/adrs/**".to_string()],
        );
        let out = render_paths_announcement(&paths, &["ADR"]).unwrap();
        assert!(out.contains("Pre-filled [ADR].paths from detected docs/adr/, docs/adrs/."));
    }

    #[test]
    fn paths_announcement_silent_on_empty_input() {
        assert!(render_paths_announcement(&DetectedPaths::new(), &["ADR"]).is_none());
    }
}
