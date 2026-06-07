//! External subprocess sources. SRC-001/002 in the brief.
//!
//! Discovery: walk `<root>/sources/*` for subdirectories whose name
//! matches `^[a-z][a-z0-9-]*$` and which contain an executable `run`
//! file. The name `markdown-file` is reserved. Dot-prefixed children
//! are skipped silently.
//!
//! Invocation: for each source the user has activated via
//! `[sources.<name>]` in `ctxgrd.toml`, spawn `run` with a scrubbed
//! environment + `CTXGRD_SOURCE_PARAMS` + `CTXGRD_SOURCE_NAME`, a 300-
//! second ceiling, and `<root>` as cwd. Parse stdout as JSONL
//! envelopes (one document per line). Non-zero exit, spawn failures,
//! and timeouts emit `src.runtime-error`; malformed lines emit
//! `src.doc-malformed` (warning, doesn't escalate exit).
//!
//! Global (`~/.ctxgrd/sources/<name>/`) is deferred to a later phase;
//! CP3b discovers local sources only.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::diagnostic::KernelMessage;
use crate::envelope::Envelope;
use crate::subprocess::{self, Env, ExitKind};

const SOURCE_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const RESERVED_SOURCE_NAME: &str = "markdown-file";

/// A source directory found on disk.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredSource {
    pub name: String,
    pub run_path: PathBuf,
}

/// Scan `<root>/sources/*` and `~/.ctxgrd/sources/*` and collect every
/// directory containing a `run` file. Local shadows global on name
/// collision. The executable bit is NOT checked here — spawn will
/// surface EACCES as a runtime error at invocation time.
///
/// Directories whose names fail the name regex or use the reserved
/// `markdown-file` name are skipped silently: they might be legit
/// utilities the user is storing next to real sources (e.g.
/// `sources/_shared/lib.sh`). A dot-prefix is also a silent skip.
pub(crate) fn discover_sources(root: &Path) -> BTreeMap<String, DiscoveredSource> {
    discover_sources_with_global(root, crate::config::global_ctxgrd_dir().as_deref())
}

/// Testable variant — pass `None` for "no global dir" or `Some(path)`
/// to point at a specific one. Used by the `run::lint` test harness
/// so parallel test workers don't trip over the real `$HOME`.
pub(crate) fn discover_sources_with_global(
    root: &Path,
    global_dir: Option<&Path>,
) -> BTreeMap<String, DiscoveredSource> {
    let mut out = BTreeMap::new();
    if let Some(g) = global_dir {
        collect_sources_in(&g.join("sources"), &mut out);
    }
    collect_sources_in(&root.join("sources"), &mut out);
    out
}

fn collect_sources_in(dir: &Path, out: &mut BTreeMap<String, DiscoveredSource>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if name == RESERVED_SOURCE_NAME {
            continue;
        }
        if !source_name_regex().is_match(name) {
            continue;
        }
        let run_path = path.join("run");
        if run_path.is_file() {
            out.insert(
                name.to_string(),
                DiscoveredSource {
                    name: name.to_string(),
                    run_path,
                },
            );
        }
    }
}

fn source_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]*$").expect("static regex compiles"))
}

/// Outcome of running every activated source.
#[derive(Debug, Clone, Default)]
pub(crate) struct SourceRunResult {
    /// Envelopes gathered from every source that exited cleanly.
    pub envelopes: Vec<(String, Envelope)>,
    /// Source-level runtime messages — `src.runtime-error`,
    /// `src.doc-malformed`, etc. Rendered by the binary ahead of the
    /// REP-001 report.
    pub messages: Vec<KernelMessage>,
}

/// Invoke every activated source in `activations`, serially.
///
/// Activation set = discovered sources whose name appears as a key in
/// the `activations` map (typically from `[sources.<name>]` in TOML).
/// Undiscovered activations (config references a source directory
/// that doesn't exist) are NOT an error at CP3b — they're silently
/// ignored; CP3c can tighten this into a `src.unknown` diagnostic if
/// we decide to.
pub(crate) fn run_activated_sources(
    root: &Path,
    discovered: &BTreeMap<String, DiscoveredSource>,
    activations: &BTreeMap<String, Value>,
) -> SourceRunResult {
    let mut result = SourceRunResult::default();
    for (name, params) in activations {
        let Some(source) = discovered.get(name) else {
            continue;
        };
        run_one(source, params, root, &mut result);
    }
    result
}

fn run_one(source: &DiscoveredSource, params: &Value, root: &Path, result: &mut SourceRunResult) {
    let env = build_env(params, &source.name);
    let spawn_result = subprocess::run(&source.run_path, &[], &env, root, SOURCE_TIMEOUT);

    let output = match spawn_result {
        Ok(o) => o,
        Err(e) => {
            result.messages.push(KernelMessage::error(
                "src.runtime-error",
                format!("source '{}' could not be invoked: {}", source.name, e),
            ));
            return;
        }
    };

    match output.exit {
        ExitKind::Success => {}
        ExitKind::Failure(Some(code)) => {
            result.messages.push(KernelMessage::error(
                "src.runtime-error",
                format!("source '{}' exited with code {}", source.name, code),
            ));
            return;
        }
        ExitKind::Failure(None) => {
            result.messages.push(KernelMessage::error(
                "src.runtime-error",
                format!("source '{}' was terminated by a signal", source.name),
            ));
            return;
        }
        ExitKind::TimedOut => {
            result.messages.push(KernelMessage::error(
                "src.runtime-error",
                format!(
                    "source '{}' timed out after {}s",
                    source.name,
                    SOURCE_TIMEOUT.as_secs()
                ),
            ));
            return;
        }
    }

    parse_envelopes(&source.name, &output.stdout_utf8(), result);
}

fn build_env(params: &Value, name: &str) -> Env {
    let mut env = subprocess::baseline_env();
    let params_json = serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string());
    subprocess::set_env(&mut env, "CTXGRD_SOURCE_PARAMS", params_json);
    subprocess::set_env(&mut env, "CTXGRD_SOURCE_NAME", name);
    env
}

fn parse_envelopes(source_name: &str, stdout: &str, result: &mut SourceRunResult) {
    for (idx, line) in stdout.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Envelope>(trimmed) {
            Ok(env) => result.envelopes.push((source_name.to_string(), env)),
            Err(e) => {
                result.messages.push(KernelMessage::warning(
                    "src.doc-malformed",
                    format!(
                        "source '{}' emitted malformed envelope on line {}: {}",
                        source_name,
                        idx + 1,
                        e
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn write_source(dir: &Path, name: &str, script: &str) -> PathBuf {
        let src_dir = dir.join("sources").join(name);
        fs::create_dir_all(&src_dir).unwrap();
        let run = src_dir.join("run");
        fs::write(&run, script).unwrap();
        let mut perms = fs::metadata(&run).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&run, perms).unwrap();
        run
    }

    #[test]
    fn discovers_directories_with_run_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "my-source", "#!/bin/sh\nexit 0\n");
        // A dir without a run file — should be skipped.
        fs::create_dir_all(tmp.path().join("sources").join("no-run")).unwrap();
        // Reserved name — should be skipped.
        write_source(tmp.path(), RESERVED_SOURCE_NAME, "#!/bin/sh\nexit 0\n");
        // Dot-prefixed — should be skipped.
        write_source(tmp.path(), ".hidden", "#!/bin/sh\nexit 0\n");
        // Bad name regex — should be skipped.
        write_source(tmp.path(), "Bad_Name", "#!/bin/sh\nexit 0\n");

        let discovered = discover_sources(tmp.path());
        assert!(discovered.contains_key("my-source"));
        assert!(!discovered.contains_key("no-run"));
        assert!(!discovered.contains_key(RESERVED_SOURCE_NAME));
        assert!(!discovered.contains_key(".hidden"));
        assert!(!discovered.contains_key("Bad_Name"));
    }

    #[test]
    fn run_collects_envelopes_from_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
printf '%s\n' '{"id":"JIRA-1","body":"","location":"x"}'
printf '%s\n' '{"id":"JIRA-2","body":"","location":"y"}'
"#;
        write_source(tmp.path(), "stub", script);

        let discovered = discover_sources(tmp.path());
        let mut activations: BTreeMap<String, Value> = BTreeMap::new();
        activations.insert("stub".to_string(), Value::Object(Default::default()));
        let result = run_activated_sources(tmp.path(), &discovered, &activations);

        assert!(result.messages.is_empty());
        assert_eq!(result.envelopes.len(), 2);
        assert_eq!(result.envelopes[0].1.id, "JIRA-1");
        assert_eq!(result.envelopes[1].1.id, "JIRA-2");
    }

    #[test]
    fn run_emits_src_runtime_error_on_non_zero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let script = "#!/bin/sh\nexit 5\n";
        write_source(tmp.path(), "broken", script);
        let discovered = discover_sources(tmp.path());
        let mut activations = BTreeMap::new();
        activations.insert("broken".to_string(), Value::Object(Default::default()));

        let result = run_activated_sources(tmp.path(), &discovered, &activations);
        assert!(result.envelopes.is_empty());
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].code, "src.runtime-error");
        assert!(result.messages[0].message.contains("exited with code 5"));
    }

    #[test]
    fn run_emits_src_doc_malformed_on_bad_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
printf '%s\n' '{"id":"JIRA-1","body":"","location":"x"}'
printf '%s\n' 'this is not JSON'
printf '%s\n' '{"id":"JIRA-2","body":"","location":"y"}'
"#;
        write_source(tmp.path(), "chatty", script);
        let discovered = discover_sources(tmp.path());
        let mut activations = BTreeMap::new();
        activations.insert("chatty".to_string(), Value::Object(Default::default()));

        let result = run_activated_sources(tmp.path(), &discovered, &activations);
        assert_eq!(result.envelopes.len(), 2);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].code, "src.doc-malformed");
        assert!(result.messages[0].message.contains("line 2"));
    }

    #[test]
    fn run_passes_params_and_name_via_env() {
        let tmp = tempfile::tempdir().unwrap();
        // Script dumps the two env vars into a file we can read back —
        // simpler than trying to JSON-escape them into an envelope.
        let marker = tmp.path().join("env.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$CTXGRD_SOURCE_NAME\" \"$CTXGRD_SOURCE_PARAMS\" > {:?}\n",
            marker
        );
        write_source(tmp.path(), "echo-env", &script);
        let discovered = discover_sources(tmp.path());
        let mut activations = BTreeMap::new();
        let params: Value = serde_json::json!({"key": "val"});
        activations.insert("echo-env".to_string(), params);

        let _ = run_activated_sources(tmp.path(), &discovered, &activations);

        let written = fs::read_to_string(&marker).expect("script wrote env file");
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines[0], "echo-env");
        assert_eq!(lines[1], r#"{"key":"val"}"#);
    }

    #[test]
    fn activation_without_discovery_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let discovered = discover_sources(tmp.path());
        let mut activations = BTreeMap::new();
        activations.insert("phantom".to_string(), Value::Object(Default::default()));
        let result = run_activated_sources(tmp.path(), &discovered, &activations);
        assert!(result.envelopes.is_empty());
        assert!(result.messages.is_empty());
    }

    #[test]
    fn spawn_failure_produces_src_runtime_error() {
        let tmp = tempfile::tempdir().unwrap();
        // Create run but make it non-executable → spawn will fail.
        let src_dir = tmp.path().join("sources").join("noexec");
        fs::create_dir_all(&src_dir).unwrap();
        let run = src_dir.join("run");
        fs::write(&run, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&run).unwrap().permissions();
        perms.set_mode(0o644); // readable but not executable
        fs::set_permissions(&run, perms).unwrap();

        let discovered = discover_sources(tmp.path());
        let mut activations = BTreeMap::new();
        activations.insert("noexec".to_string(), Value::Object(Default::default()));

        let result = run_activated_sources(tmp.path(), &discovered, &activations);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].code, "src.runtime-error");
        // Message mentions the source name.
        assert!(result.messages[0].message.contains("noexec"));
    }
}
