//! `ctxgrd pin --bless <ID>` — re-pin a document to the current HEAD
//! (ADR-040 § PIN-005).
//!
//! The snapshot-test `--update` pattern: `core.commit-freshness` detects
//! that a pinned document's scoped code has drifted; a human re-validates
//! it; this command records the new green commit by rewriting only the
//! `pin.commit` line in place, leaving the rest of the frontmatter
//! untouched.
//!
//! Refuses (unless `--force`) when any scoped path has uncommitted
//! changes: blessing `HEAD` while the review's scoped edits are
//! uncommitted would record a state that excludes them (PIN-005).

use std::path::Path;
use std::process::Command;

use thiserror::Error;

use crate::frontmatter::Pin;

/// What can go wrong blessing a document's pin.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BlessError {
    /// The ingest pipeline failed (config error, IO).
    #[error(transparent)]
    Lint(#[from] crate::run::LintError),
    /// No document with the requested id was found in the tree.
    #[error("no document with id `{0}` was found under the lint root")]
    NotFound(String),
    /// The named document carries no `pin` block to bless.
    #[error("document `{0}` has no `pin` block to bless — add a `pin:` block first")]
    NoPin(String),
    /// Git is unavailable or `git rev-parse HEAD` failed — there is no
    /// HEAD to bless to.
    #[error("could not read HEAD via `git rev-parse HEAD` (not a git repository, or no commits yet)")]
    NoHead,
    /// A scoped path has uncommitted changes and `--force` was not set.
    #[error(
        "scoped paths have uncommitted changes — blessing HEAD would exclude them; \
         commit them first, or pass --force to bless anyway"
    )]
    DirtyScope,
    /// Reading or writing the document file failed.
    #[error("failed to update {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The `pin.commit` line could not be located in the file to rewrite.
    #[error("could not locate the `pin.commit` line in {0} to rewrite")]
    CommitLineNotFound(String),
}

/// Re-pin the document with id `target_id` to the current HEAD.
///
/// Returns the new commit SHA on success. `force` skips the
/// dirty-scoped-tree refusal (PIN-005).
pub fn bless(root: &Path, target_id: &str, force: bool) -> Result<String, BlessError> {
    let ingest = crate::run::ingest(root)?;
    let doc = ingest
        .documents
        .iter()
        .find(|d| d.raw_id == target_id || d.id.to_string() == target_id)
        .ok_or_else(|| BlessError::NotFound(target_id.to_owned()))?;

    let pin = doc
        .pin
        .as_ref()
        .ok_or_else(|| BlessError::NoPin(target_id.to_owned()))?;

    let head = git_head(root).ok_or(BlessError::NoHead)?;

    if !force && scope_is_dirty(root, pin) {
        return Err(BlessError::DirtyScope);
    }

    let doc_path = root.join(&doc.location);
    let original = std::fs::read_to_string(&doc_path).map_err(|e| BlessError::Io {
        path: doc.location.clone(),
        source: e,
    })?;
    let rewritten = rewrite_pin_commit(&original, &pin.commit, &head)
        .ok_or_else(|| BlessError::CommitLineNotFound(doc.location.clone()))?;
    std::fs::write(&doc_path, rewritten).map_err(|e| BlessError::Io {
        path: doc.location.clone(),
        source: e,
    })?;

    Ok(head)
}

/// `git rev-parse HEAD`, trimmed. `None` when git is unavailable or there
/// is no HEAD (empty repo).
fn git_head(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!sha.is_empty()).then_some(sha)
}

/// True when any scoped path has uncommitted (staged or unstaged) changes
/// in the working tree. The scope globs are passed to `git status` as
/// pathspecs; a non-empty porcelain result means dirty.
fn scope_is_dirty(root: &Path, pin: &Pin) -> bool {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--"]);
    for glob in &pin.scope {
        cmd.arg(glob);
    }
    let Ok(output) = cmd.output() else {
        // Cannot tell — be conservative and treat as clean so a transient
        // git hiccup does not block a bless; the working-tree drift would
        // still surface on the next lint (PIN-003).
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|l| !l.trim().is_empty())
}

/// Rewrite the single `commit:` line inside the `pin:` mapping, replacing
/// `old_commit` with `new_commit` and leaving everything else byte-for-byte.
///
/// Scans for the `pin:` key at the top level, then the first indented
/// `commit:` line under it whose value is `old_commit`. Returns `None` if
/// no such line is found.
fn rewrite_pin_commit(content: &str, old_commit: &str, new_commit: &str) -> Option<String> {
    let mut in_pin = false;
    let mut out = String::with_capacity(content.len());
    let mut rewritten = false;
    // Preserve the original line endings by splitting on '\n' and
    // re-joining, keeping a trailing newline if present.
    let had_trailing_newline = content.ends_with('\n');
    let lines: Vec<&str> = content.split('\n').collect();
    let last = lines.len().saturating_sub(1);

    for (i, line) in lines.iter().enumerate() {
        let mut emitted = line.to_string();
        if !rewritten {
            let trimmed_start = line.trim_start();
            let indent = line.len() - trimmed_start.len();
            if indent == 0 && trimmed_start.starts_with("pin:") {
                in_pin = true;
            } else if indent == 0 && !trimmed_start.is_empty() {
                // A new top-level key ends the pin mapping.
                in_pin = false;
            } else if in_pin {
                let body = trimmed_start;
                if let Some(rest) = body.strip_prefix("commit:") {
                    if rest.trim() == old_commit {
                        let indent_str = &line[..indent];
                        emitted = format!("{indent_str}commit: {new_commit}");
                        rewritten = true;
                    }
                }
            }
        }
        out.push_str(&emitted);
        if i != last {
            out.push('\n');
        }
    }
    if had_trailing_newline && !out.ends_with('\n') {
        out.push('\n');
    }
    rewritten.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_replaces_only_the_commit_line() {
        let doc = "---\nid: SECREV-001\ntitle: Auth review\npin:\n  commit: aaa111\n  scope:\n    - src/auth/**\n---\n\n# Body\n";
        let out = rewrite_pin_commit(doc, "aaa111", "bbb222").expect("rewrites");
        assert!(out.contains("commit: bbb222"));
        assert!(!out.contains("aaa111"));
        // Everything else is untouched.
        assert!(out.contains("title: Auth review"));
        assert!(out.contains("- src/auth/**"));
        assert!(out.contains("id: SECREV-001"));
    }

    #[test]
    fn rewrite_preserves_two_space_indent() {
        let doc = "---\npin:\n  commit: old\n  scope:\n    - a/**\n---\n";
        let out = rewrite_pin_commit(doc, "old", "new").unwrap();
        assert!(
            out.contains("\n  commit: new\n"),
            "indent preserved: {out:?}"
        );
    }

    #[test]
    fn rewrite_ignores_commit_outside_pin() {
        // A `commit:` key in body prose or another mapping must not match.
        let doc = "---\nid: ADR-1\nmeta:\n  commit: old\n---\n";
        assert_eq!(rewrite_pin_commit(doc, "old", "new"), None);
    }

    #[test]
    fn rewrite_returns_none_when_commit_mismatch() {
        let doc = "---\npin:\n  commit: aaa\n  scope:\n    - a/**\n---\n";
        assert_eq!(rewrite_pin_commit(doc, "zzz", "new"), None);
    }
}
