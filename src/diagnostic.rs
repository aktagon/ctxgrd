//! Diagnostic records. REP-001 + RUN-001 in the brief.
//!
//! One record per problem found. Kernel collects them from rule
//! evaluation and from the sources' parse stages, then sorts and
//! prints via [`crate::reporter`]. Severity governs exit code:
//! `Error` contributes to exit 1, `Warning` and `Info` never escalate
//! past 0.

/// Severity of a single diagnostic.
///
/// The uniform three-level set the *grd family shares (ADR-086 §
/// WIRE-003): `error` escalates the exit code, `warning` and `info`
/// never do — `info` behaves exactly like `warning` for the exit-code
/// verdict and differs only in how the reporter labels it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// A single reporter line's worth of data.
///
/// `line` / `col` are 1-indexed when known and `None` when unknown
/// (ADR-086 § WIRE-004 — no `0` sentinel on the wire). The
/// convenience constructors accept a plain `u32` for ergonomics and
/// normalise a `0` argument to `None`, so an internal caller with no
/// position can still pass `0`. `location` is a display string —
/// typically a path rendered relative to the lint root.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub location: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
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
        line: impl Into<Option<u32>>,
        col: impl Into<Option<u32>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            severity: Severity::Error,
            message: message.into(),
            location: location.into(),
            line: line.into().filter(|&n| n > 0),
            col: col.into().filter(|&n| n > 0),
            help: None,
            note: None,
            span_len: None,
        }
    }

    /// Convenience constructor for a warning-severity diagnostic.
    pub fn warning(
        code: impl Into<String>,
        location: impl Into<String>,
        line: impl Into<Option<u32>>,
        col: impl Into<Option<u32>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            severity: Severity::Warning,
            message: message.into(),
            location: location.into(),
            line: line.into().filter(|&n| n > 0),
            col: col.into().filter(|&n| n > 0),
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
