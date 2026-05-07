//! Diagnostic records. REP-001 + RUN-001 in the brief.
//!
//! One record per problem found. Kernel collects them from rule
//! evaluation and from the sources' parse stages, then sorts and
//! prints via [`crate::reporter`]. Severity governs exit code:
//! `Error` contributes to exit 1, `Warning` never escalates past 0.

/// Severity of a single diagnostic.
///
/// Two levels is intentional — the brief (RUN-001) only distinguishes
/// "an error happened" from "everything else". Adding `info` or
/// `hint` would force the reporter and exit-code rules to care about
/// more variants without a corresponding user-visible need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// A single reporter line's worth of data.
///
/// `line` / `col` are 1-indexed when known; `0` is the sentinel for
/// "don't know" (matches the reporter's expected rendering in the
/// acceptance transcript, e.g. `:0:0:` for a file-level diagnostic).
/// `location` is a display string — typically a path rendered
/// relative to the lint root.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub location: String,
    pub line: u32,
    pub col: u32,
    /// Actionable fix suggestion, rendered as `help:` in the rich
    /// reporter. `None` when the diagnostic doesn't have a
    /// deterministic fix hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Supplementary context the consumer would need to verify the
    /// fix — the full required-headings list, the ID regex, etc.
    /// Rendered as `note:`. Omitted when the content would be
    /// redundant with `message:` or `help:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Byte length of the offending span starting at `col`. Powers
    /// the caret width in the rich reporter; `None` falls back to a
    /// single-character caret at `col`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_len: Option<u32>,
}

impl Diagnostic {
    /// Convenience constructor for the error case.
    pub fn error(
        code: impl Into<String>,
        location: impl Into<String>,
        line: u32,
        col: u32,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            severity: Severity::Error,
            message: message.into(),
            location: location.into(),
            line,
            col,
            help: None,
            note: None,
            span_len: None,
        }
    }

    /// Convenience constructor for a warning-severity diagnostic.
    pub fn warning(
        code: impl Into<String>,
        location: impl Into<String>,
        line: u32,
        col: u32,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            severity: Severity::Warning,
            message: message.into(),
            location: location.into(),
            line,
            col,
            help: None,
            note: None,
            span_len: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn with_span_len(mut self, span_len: u32) -> Self {
        self.span_len = Some(span_len);
        self
    }
}

/// Runtime-level kernel message that has no document anchor.
///
/// Used for `src.runtime-error` (the whole source failed, no doc to
/// blame), `src.doc-malformed` (a single JSONL line from a source
/// wouldn't parse; we know the source but not which document was
/// intended), `ref.scan-error` (reference-scanner walker/searcher
/// errors), and `cfg.reserved-source` (config-load advisories).
/// Per-document variants (`ext.runtime-error`, `ext.malformed-output`)
/// use [`Diagnostic`] instead because they DO have a document to
/// anchor against.
///
/// `help` and `note` are populated when there's actionable advice or
/// supplementary context — same fields, same purpose, same renderer
/// path as [`Diagnostic`]. The two types differ only in whether a
/// `(location, line, col)` anchor is meaningful.
///
/// Rendered by the binary with the format
/// `<severity>[<code>]: <message>` ahead of the REP-001 block, with
/// `help:` / `note:` lines indented underneath when present.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct KernelMessage {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    /// Actionable fix suggestion. Same semantics as [`Diagnostic::help`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Supplementary context. Same semantics as [`Diagnostic::note`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl KernelMessage {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            help: None,
            note: None,
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code: code.into(),
            message: message.into(),
            help: None,
            note: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}
