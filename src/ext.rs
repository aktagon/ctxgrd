//! External rule invocation — batch mode (ADR-002 § RUL-001..RUL-006).
//!
//! For each external rule the orchestrator wants to run, this module:
//!
//! 1. resolves an absolute body path per document — the real markdown
//!    file for `markdown-file` docs, or a materialised temp file at
//!    `<tmp>/<ns>-<n>.md` for source-derived docs (cached across rules);
//! 2. spawns the rule's `run` script ONCE, passing every namespace
//!    document on stdin as JSONL (`{"path": ..., "context": {...}}`),
//!    then closing stdin to signal EOF;
//! 3. enforces a per-batch wall-clock timeout (default 60 s,
//!    overridable per rule via `[NS."<rule.code>".timeout_sec]`);
//! 4. parses stdout as JSONL diagnostics with a `path` field, looks up
//!    the matching `Document`, attaches the host-supplied `code`, and
//!    returns them in input order;
//! 5. emits `ext.runtime-error` when the rule exits non-zero / times
//!    out / can't be spawned, and `ext.malformed-output` (warning) for
//!    JSONL lines that don't parse or reference unknown paths.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::diagnostic::{Diagnostic, Severity};
use crate::document::Document;
use crate::subprocess::{self, Env, ExitKind};

/// Default per-batch timeout when `[NS."<rule.code>".timeout_sec]` is
/// not set. Applies to the entire batch (all docs through one
/// invocation), not per-doc.
pub(crate) const DEFAULT_RULE_TIMEOUT: Duration = Duration::from_secs(60);

/// Scratch directory used for the lifetime of one lint run.
///
/// Created on construction as `$TMPDIR/ctxgrd.<pid>/`. Removed via
/// [`Drop`] — best-effort; a SIGKILL or panic-with-abort will leave it,
/// but normal exit paths (including panics with unwind) clean up.
///
/// Used to materialise body files for source-derived documents so
/// rules can refer to them by path on disk. The body cache is reused
/// across rules within one lint run.
pub(crate) struct RunTempDir {
    path: PathBuf,
    body_cache: std::cell::RefCell<BTreeMap<String, PathBuf>>,
}

impl RunTempDir {
    /// Production constructor — creates `$TMPDIR/ctxgrd.<pid>/`.
    pub(crate) fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("ctxgrd.{}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self {
            path,
            body_cache: Default::default(),
        })
    }

    /// Test-only constructor: create a `ctxgrd-run/` subdirectory
    /// inside `parent` (typically a `tempfile::TempDir`'s path) so
    /// parallel `cargo test` workers don't collide on the shared
    /// process-wide tempdir path that `new()` uses.
    #[cfg(test)]
    pub(crate) fn in_parent(parent: &Path) -> io::Result<Self> {
        let path = parent.join("ctxgrd-run");
        fs::create_dir_all(&path)?;
        Ok(Self {
            path,
            body_cache: Default::default(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunTempDir {
    fn drop(&mut self) {
        // Best-effort cleanup; we'd rather leak tmpfiles than panic
        // from a destructor. The OS's tmp-cleaner catches stragglers.
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Run one external rule against every document in its namespace.
///
/// One subprocess invocation total (RUL-001). All docs are streamed to
/// the rule's stdin as JSONL (RUL-002); diagnostics arrive on stdout
/// with a `path` field that the kernel uses to attribute each one back
/// to its source `Document` (RUL-003).
pub(crate) fn run_rule_batch(
    code: &str,
    run_path: &Path,
    docs: &[&Document],
    params: &Value,
    timeout: Duration,
    root: &Path,
    tmp: &RunTempDir,
) -> io::Result<Vec<Diagnostic>> {
    if docs.is_empty() {
        return Ok(Vec::new());
    }

    // Resolve body paths and build a path → doc lookup. We canonicalise
    // both sides so symlink / `/private/tmp` divergence on macOS doesn't
    // sabotage attribution.
    let mut path_to_doc: BTreeMap<PathBuf, &Document> = BTreeMap::new();
    let mut stdin_buf: Vec<u8> = Vec::new();
    for doc in docs {
        let body_path = ensure_body_path(doc, root, tmp)?;
        let record = serde_json::json!({
            "path": body_path,
            "context": {
                "id": doc.raw_id,
                "namespace": doc.id.namespace,
                "number": doc.id.number,
                "location": doc.location,
                "depends_on": doc.depends_on,
                "metadata": doc.metadata,
                "ast": doc.ast,
            }
        });
        let line = serde_json::to_string(&record).expect("record always serialises");
        stdin_buf.extend_from_slice(line.as_bytes());
        stdin_buf.push(b'\n');
        path_to_doc.insert(body_path, doc);
    }

    let env = build_env(params);
    let spawn_result = subprocess::run_with_stdin(run_path, &[], &env, root, timeout, stdin_buf);

    let mut out = Vec::new();
    // Batch-level failures (spawn error, non-zero exit, signal,
    // timeout) anchor at docs[0]'s location only because there is
    // nowhere better to point — the rule never finished emitting
    // per-doc diagnostics. The help line tells the user the failure
    // is rule-wide so they don't blame docs[0] specifically.
    let fallback_loc = || docs[0].location.clone();
    let batch_help = || format!("affects all {} documents in the batch", docs.len());

    let output = match spawn_result {
        Ok(o) => o,
        Err(e) => {
            out.push(
                Diagnostic::error(
                    "ext.runtime-error",
                    fallback_loc(),
                    0,
                    0,
                    format!("rule '{code}' could not be invoked: {e}"),
                )
                .with_help(batch_help()),
            );
            return Ok(out);
        }
    };

    match output.exit {
        ExitKind::Success => {}
        ExitKind::Failure(Some(code_num)) => {
            out.push(
                Diagnostic::error(
                    "ext.runtime-error",
                    fallback_loc(),
                    0,
                    0,
                    format!("rule '{code}' exited with code {code_num}"),
                )
                .with_help(batch_help()),
            );
            return Ok(out);
        }
        ExitKind::Failure(None) => {
            out.push(
                Diagnostic::error(
                    "ext.runtime-error",
                    fallback_loc(),
                    0,
                    0,
                    format!("rule '{code}' was terminated by a signal"),
                )
                .with_help(batch_help()),
            );
            return Ok(out);
        }
        ExitKind::TimedOut => {
            out.push(
                Diagnostic::error(
                    "ext.runtime-error",
                    fallback_loc(),
                    0,
                    0,
                    format!("rule '{code}' exceeded timeout of {}s", timeout.as_secs()),
                )
                .with_help(batch_help()),
            );
            return Ok(out);
        }
    }

    parse_diagnostics_batch(code, &path_to_doc, &output.stdout_utf8(), &mut out);
    Ok(out)
}

fn ensure_body_path(doc: &Document, root: &Path, tmp: &RunTempDir) -> io::Result<PathBuf> {
    // If `location` resolves to a real file under root, that's a
    // `markdown-file` doc — pass the real path through.
    let on_disk = root.join(&doc.location);
    if on_disk.is_file() {
        return Ok(canonicalize_or_leave(&on_disk));
    }
    // Otherwise materialise into the temp dir and cache for re-use.
    let key = format!("{}-{}", doc.id.namespace, doc.id.number);
    {
        let cache = tmp.body_cache.borrow();
        if let Some(path) = cache.get(&key) {
            return Ok(path.clone());
        }
    }
    let path = tmp.path().join(format!("{key}.md"));
    fs::write(&path, doc.body.as_bytes())?;
    // Canonicalise so the on-disk and materialised branches return
    // paths under the same surface form. macOS resolves $TMPDIR-derived
    // paths through `/private/var/folders/...`; if a rule script calls
    // `realpath` on the path it received, it would round-trip to the
    // canonical form and fail attribution against the kernel's
    // path_to_doc lookup.
    let path = canonicalize_or_leave(&path);
    tmp.body_cache.borrow_mut().insert(key, path.clone());
    Ok(path)
}

fn canonicalize_or_leave(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn build_env(params: &Value) -> Env {
    let mut env = subprocess::baseline_env();
    let params_json = serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string());
    subprocess::set_env(&mut env, "CTXGRD_RULE_PARAMS", params_json);
    env
}

#[derive(Debug, Deserialize)]
struct RawDiagnostic {
    path: String,
    severity: String,
    message: String,
    #[serde(default)]
    line: u32,
    #[serde(default)]
    col: u32,
}

fn parse_diagnostics_batch(
    code: &str,
    path_to_doc: &BTreeMap<PathBuf, &Document>,
    stdout: &str,
    out: &mut Vec<Diagnostic>,
) {
    // Three orthogonal failure modes; each gets its own one-shot dedup
    // so a flood of one kind cannot mask another. (Earlier versions
    // collapsed all three into a single `saw_malformed` flag, which
    // dropped the second-and-third signal classes entirely.)
    let mut saw_unknown_severity = false;
    let mut saw_unknown_path = false;
    let mut saw_parse_error = false;
    let fallback_loc = || {
        path_to_doc
            .values()
            .next()
            .map(|d| d.location.clone())
            .unwrap_or_default()
    };
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<RawDiagnostic>(trimmed) {
            Ok(raw) => {
                let sev = match raw.severity.as_str() {
                    "error" => Severity::Error,
                    "warning" => Severity::Warning,
                    other => {
                        if !saw_unknown_severity {
                            saw_unknown_severity = true;
                            out.push(Diagnostic::warning(
                                "ext.malformed-output",
                                fallback_loc(),
                                0,
                                0,
                                format!(
                                    "rule '{code}' emitted diagnostic with unknown severity {other:?}"
                                ),
                            ));
                        }
                        continue;
                    }
                };
                let path_buf = PathBuf::from(&raw.path);
                let Some(doc) = path_to_doc.get(&path_buf) else {
                    if !saw_unknown_path {
                        saw_unknown_path = true;
                        out.push(Diagnostic::warning(
                            "ext.malformed-output",
                            fallback_loc(),
                            0,
                            0,
                            format!(
                                "rule '{code}' emitted diagnostic for unknown path {:?}",
                                raw.path
                            ),
                        ));
                    }
                    continue;
                };
                out.push(Diagnostic {
                    code: code.to_string(),
                    severity: sev,
                    message: raw.message,
                    location: doc.location.clone(),
                    line: raw.line,
                    col: raw.col,
                    help: None,
                    note: None,
                    span_len: None,
                });
            }
            Err(e) => {
                if !saw_parse_error {
                    saw_parse_error = true;
                    out.push(Diagnostic::warning(
                        "ext.malformed-output",
                        fallback_loc(),
                        0,
                        0,
                        format!("rule '{code}' emitted malformed JSONL: {e}"),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use crate::ast::Ast;
    use crate::id::DocumentId;

    fn write_rule(root: &Path, ns: &str, name: &str, script: &str) -> PathBuf {
        let dir = root.join("rules").join(ns).join(name);
        fs::create_dir_all(&dir).unwrap();
        let run = dir.join("run");
        fs::write(&run, script).unwrap();
        let mut perms = fs::metadata(&run).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&run, perms).unwrap();
        run
    }

    fn file_doc(root: &Path, n: u32) -> Document {
        let md_dir = root.join("adrs");
        fs::create_dir_all(&md_dir).unwrap();
        let md = md_dir.join(format!("ADR-{n:03}.md"));
        let body = format!(
            "---\nid: ADR-{n:03}\n---\n\n# ADR-{n:03}\n\n## Consequences\n\n- One bullet\n"
        );
        fs::write(&md, &body).unwrap();
        Document {
            id: DocumentId::new("ADR", n),
            raw_id: format!("ADR-{n:03}"),
            location: format!("adrs/ADR-{n:03}.md"),
            depends_on: vec![],
            frontmatter_lines: Default::default(),
            metadata: Default::default(),
            ast: Some(Ast::default()),
            body,
        }
    }

    #[test]
    fn body_path_uses_real_file_when_present() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = file_doc(root.path(), 1);
        let path = ensure_body_path(&doc, root.path(), &tmp).unwrap();
        assert!(path.starts_with(
            fs::canonicalize(root.path()).unwrap_or_else(|_| root.path().to_path_buf())
        ));
        assert!(path.ends_with("ADR-001.md"));
    }

    #[test]
    fn body_path_materializes_for_source_derived_docs() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = Document {
            id: DocumentId::new("JIRA", 100),
            raw_id: "JIRA-100".to_string(),
            location: "https://jira.example/100".to_string(),
            depends_on: vec![],
            frontmatter_lines: Default::default(),
            metadata: Default::default(),
            ast: None,
            body: "synthetic body".to_string(),
        };
        let path = ensure_body_path(&doc, root.path(), &tmp).unwrap();
        // After RUL-006 fix the materialised branch canonicalises too,
        // so on macOS `path` resolves through /private/var/folders/...
        // while tmp.path() is /var/folders/... — compare canonical forms.
        let canonical_tmp =
            fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
        assert!(
            path.starts_with(&canonical_tmp),
            "expected {path:?} to start with {canonical_tmp:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "synthetic body");
    }

    /// RUL-001 verification: one rule, two docs, one spawn.
    ///
    /// The script appends a literal "tick" line to a counter file each
    /// time it's invoked. After the batch the counter must equal 1, not 2.
    #[test]
    fn rule_invoked_once_per_batch() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let counter = root.path().join("invocations.log");
        let doc1 = file_doc(root.path(), 1);
        let doc2 = file_doc(root.path(), 2);

        let script = format!(
            r#"#!/bin/sh
echo tick >> '{}'
# Drain stdin so the parent's writer doesn't block on broken pipe.
while IFS= read -r _line; do :; done
"#,
            counter.display()
        );
        let run = write_rule(root.path(), "adr", "counted", &script);
        let docs = [&doc1, &doc2];
        let diags = run_rule_batch(
            "adr.counted",
            &run,
            &docs,
            &Value::Object(Default::default()),
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        assert!(diags.is_empty(), "no rule output → no diagnostics");
        let invocations = fs::read_to_string(&counter).unwrap();
        assert_eq!(
            invocations.lines().count(),
            1,
            "rule must be spawned exactly once per batch (RUL-001)"
        );
    }

    /// RUL-002 full payload verification: every context sub-field
    /// named by the ADR (id, namespace, number, location, depends_on,
    /// metadata, ast) actually arrives in the stdin record.
    ///
    /// The script captures the first stdin line verbatim to a sidecar
    /// file; the test reads it back and asserts on the parsed JSON
    /// shape — so we don't depend on the rule itself being able to
    /// extract every field with sed/jq.
    #[test]
    fn rule_receives_full_context_on_stdin() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = {
            let mut d = file_doc(root.path(), 1);
            d.depends_on = vec!["PRD-007".to_string()];
            d.metadata
                .insert("id".into(), Value::String("ADR-001".into()));
            d.metadata
                .insert("title".into(), Value::String("Test ADR".into()));
            d.metadata
                .insert("status".into(), Value::String("accepted".into()));
            d
        };
        let captured = root.path().join("captured-stdin.jsonl");
        let script = format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" > '{}'
done
"#,
            captured.display()
        );
        let run = write_rule(root.path(), "adr", "capture", &script);
        let docs = [&doc];
        let _ = run_rule_batch(
            "adr.capture",
            &run,
            &docs,
            &Value::Object(Default::default()),
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        let raw = fs::read_to_string(&captured).unwrap();
        let record: Value = serde_json::from_str(raw.trim()).unwrap_or_else(|e| {
            panic!("stdin line not valid JSON: {e} ({raw})");
        });
        assert!(record.get("path").and_then(|v| v.as_str()).is_some());
        let ctx = record.get("context").expect("context present");
        assert_eq!(ctx["id"], "ADR-001");
        assert_eq!(ctx["namespace"], "ADR");
        assert_eq!(ctx["number"], 1);
        assert_eq!(ctx["location"], "adrs/ADR-001.md");
        assert_eq!(ctx["depends_on"], serde_json::json!(["PRD-007"]));
        assert_eq!(ctx["metadata"]["status"], "accepted");
        assert!(ctx.get("ast").is_some(), "ast field must be present");
    }

    /// RUL-002 verification: rule receives JSONL on stdin with a `path`
    /// field and a structured `context`.
    ///
    /// Script reads stdin, asserts each record parses as JSON with the
    /// expected keys, and emits one diagnostic per input doc echoing
    /// the context's `id`.
    #[test]
    fn rule_receives_jsonl_on_stdin() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc1 = file_doc(root.path(), 1);
        let doc2 = file_doc(root.path(), 2);

        let script = r#"#!/bin/sh
while IFS= read -r line; do
  path=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  printf '{"path":"%s","severity":"error","message":"saw %s","line":1,"col":0}\n' "$path" "$id"
done
"#;
        let run = write_rule(root.path(), "adr", "echo", script);
        let docs = [&doc1, &doc2];
        let diags = run_rule_batch(
            "adr.echo",
            &run,
            &docs,
            &Value::Object(Default::default()),
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        assert_eq!(diags.len(), 2, "one diagnostic per input doc");
        let messages: Vec<_> = diags.iter().map(|d| d.message.as_str()).collect();
        assert!(messages.iter().any(|m| *m == "saw ADR-001"));
        assert!(messages.iter().any(|m| *m == "saw ADR-002"));
    }

    /// RUL-003 verification: diagnostics are attributed to the right
    /// document via the `path` field, regardless of emission order, and
    /// multiple diagnostics for one document are supported.
    #[test]
    fn diagnostics_attributed_by_path() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc1 = file_doc(root.path(), 1);
        let doc2 = file_doc(root.path(), 2);

        // Script emits: diag for doc2, diag for doc1, second diag for doc2.
        let script = r#"#!/bin/sh
paths=""
while IFS= read -r line; do
  path=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
  paths="$paths $path"
done
set -- $paths
p1="$1"; p2="$2"
printf '{"path":"%s","severity":"error","message":"second-doc-first","line":1,"col":0}\n' "$p2"
printf '{"path":"%s","severity":"warning","message":"first-doc","line":2,"col":0}\n' "$p1"
printf '{"path":"%s","severity":"error","message":"second-doc-again","line":3,"col":0}\n' "$p2"
"#;
        let run = write_rule(root.path(), "adr", "shuffled", script);
        let docs = [&doc1, &doc2];
        let diags = run_rule_batch(
            "adr.shuffled",
            &run,
            &docs,
            &Value::Object(Default::default()),
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        assert_eq!(diags.len(), 3);
        let by_msg: BTreeMap<&str, &Diagnostic> =
            diags.iter().map(|d| (d.message.as_str(), d)).collect();
        assert_eq!(by_msg["first-doc"].location, "adrs/ADR-001.md");
        assert_eq!(by_msg["second-doc-first"].location, "adrs/ADR-002.md");
        assert_eq!(by_msg["second-doc-again"].location, "adrs/ADR-002.md");
    }

    /// RUL-004 verification: per-batch timeout fires when the rule
    /// exceeds it; other rules are unaffected (the kernel keeps going).
    #[test]
    fn per_batch_timeout_fires_runtime_error() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = file_doc(root.path(), 1);
        let script = r#"#!/bin/sh
while IFS= read -r _line; do :; done
sleep 30
"#;
        let run = write_rule(root.path(), "adr", "slow", script);
        let docs = [&doc];
        let diags = run_rule_batch(
            "adr.slow",
            &run,
            &docs,
            &Value::Object(Default::default()),
            Duration::from_millis(200),
            root.path(),
            &tmp,
        )
        .unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "ext.runtime-error");
        assert!(
            diags[0].message.contains("exceeded timeout"),
            "got: {}",
            diags[0].message
        );
    }

    #[test]
    fn rule_non_zero_emits_ext_runtime_error() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = file_doc(root.path(), 1);
        let run = write_rule(
            root.path(),
            "adr",
            "broken",
            "#!/bin/sh\nwhile IFS= read -r _l; do :; done\nexit 7\n",
        );
        let docs = [&doc];
        let diags = run_rule_batch(
            "adr.broken",
            &run,
            &docs,
            &Value::Object(Default::default()),
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "ext.runtime-error");
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.contains("exited with code 7"));
    }

    #[test]
    fn malformed_jsonl_emits_one_warning() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = file_doc(root.path(), 1);
        // Script ignores stdin, emits one valid + two malformed lines.
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  path=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
  printf '%s\n' 'not json'
  printf '{"path":"%s","severity":"error","message":"ok","line":1,"col":0}\n' "$path"
  printf '%s\n' 'also not json'
done
"#;
        let run = write_rule(root.path(), "adr", "chatty", script);
        let docs = [&doc];
        let diags = run_rule_batch(
            "adr.chatty",
            &run,
            &docs,
            &Value::Object(Default::default()),
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        let malformed_count = diags
            .iter()
            .filter(|d| d.code == "ext.malformed-output")
            .count();
        assert_eq!(malformed_count, 1);
        let rule_diag_count = diags.iter().filter(|d| d.code == "adr.chatty").count();
        assert_eq!(rule_diag_count, 1);
    }

    /// RUL-005 verification: `CTXGRD_RULE_PARAMS` carries the
    /// JSON-serialised rule sub-table to the rule script, unchanged
    /// from today.
    ///
    /// The rule writes the env var to a sidecar file so we can
    /// inspect it without escaping JSON-in-JSON.
    #[test]
    fn rule_can_read_rule_params_env() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = file_doc(root.path(), 1);
        let captured = root.path().join("captured-params.json");
        let script = format!(
            r#"#!/bin/sh
printf '%s' "$CTXGRD_RULE_PARAMS" > '{}'
while IFS= read -r _line; do :; done
"#,
            captured.display()
        );
        let run = write_rule(root.path(), "adr", "echo-params", &script);
        let docs = [&doc];
        let params = serde_json::json!({"min_items": 3});
        let _diags = run_rule_batch(
            "adr.echo-params",
            &run,
            &docs,
            &params,
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        let captured_text = fs::read_to_string(&captured).unwrap();
        let parsed: Value = serde_json::from_str(&captured_text)
            .unwrap_or_else(|e| panic!("CTXGRD_RULE_PARAMS not valid JSON: {e} ({captured_text})"));
        assert_eq!(parsed, params);
    }

    /// RUL-006 verification: `CTXGRD_DOC_CONTEXT` is gone — context
    /// flows on stdin instead. The rule MUST NOT see this env var.
    #[test]
    fn doc_context_env_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let tmp = RunTempDir::in_parent(root.path()).unwrap();
        let doc = file_doc(root.path(), 1);
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  path=$(printf '%s' "$line" | sed 's/.*"path":"\([^"]*\)".*/\1/')
  if [ -n "$CTXGRD_DOC_CONTEXT" ]; then
    printf '{"path":"%s","severity":"error","message":"sidecar leaked: %s","line":0,"col":0}\n' "$path" "$CTXGRD_DOC_CONTEXT"
  else
    printf '{"path":"%s","severity":"warning","message":"no sidecar","line":0,"col":0}\n' "$path"
  fi
done
"#;
        let run = write_rule(root.path(), "adr", "no-sidecar", script);
        let docs = [&doc];
        let diags = run_rule_batch(
            "adr.no-sidecar",
            &run,
            &docs,
            &Value::Object(Default::default()),
            DEFAULT_RULE_TIMEOUT,
            root.path(),
            &tmp,
        )
        .unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[0].message, "no sidecar");
    }
}
