//! End-to-end lint orchestration.
//!
//! [`lint`] ties everything together in the order the brief prescribes:
//! 1. Load `<root>/ctxgrd.toml` (or zero-config defaults).
//! 2. Run external sources (SRC-002 — "all sources MUST complete
//!    before any rule runs") and harvest envelopes.
//! 3. Scan markdown files into [`Document`]s (built-in source).
//! 4. Convert external envelopes → [`Document`]s and append.
//! 5. Run aggregate graph-floor rules, filtering by per-namespace
//!    rule enablement.
//! 6. Run per-document parameterised core rules (required-headings,
//!    required-metadata, allowed-values).
//! 7. Run external rules (EXT-002) per document through the
//!    subprocess pipeline with a per-run `TempDir`.
//! 8. Sort and return.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use thiserror::Error;

use crate::agent_guide;
use crate::config::{self, Config, ConfigError};
use crate::diagnostic::{Diagnostic, KernelMessage, Severity};
use crate::document::Document;
use crate::envelope::EnvelopeError;
use crate::ext;
use crate::reporter;
use crate::rules;
use crate::source::markdown::{ParseDiagnostic, ParseDiagnosticKind};
use crate::source::{external as source_ext, markdown};

/// Exit code bucket. RUN-001 in the brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Ok,
    LintFailure,
    KernelError,
}

impl ExitStatus {
    pub fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::LintFailure => 1,
            Self::KernelError => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LintOutcome {
    /// Per-document diagnostics, sorted per REP-001.
    pub diagnostics: Vec<Diagnostic>,
    /// Floating, non-anchored messages — runtime errors from sources
    /// (`src.runtime-error`, `src.doc-malformed`), reference-scanner
    /// warnings (`ref.scan-error`), and config-load advisories
    /// (`cfg.reserved-source`). Rendered by the binary ahead of the
    /// REP-001 block.
    pub kernel_messages: Vec<KernelMessage>,
    pub exit: ExitStatus,
    /// Count of documents scanned (built-in markdown source + external
    /// source envelopes). Drives the `ok:` summary line.
    pub documents_linted: usize,
    /// Sum of `(namespace, rule)` pairs that were in scope for at
    /// least one scanned document. Drives the `ok:` summary line.
    pub rules_active: usize,
}

/// What can go wrong at kernel-orchestration level.
///
/// IO errors carry the *operation* they failed during, not just the
/// underlying `io::Error`. A user seeing `error[ext.tempdir]:
/// Permission denied` knows where to look; `error[io.error]:
/// Permission denied` would not.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LintError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Walking the markdown tree failed (permission denied on a
    /// directory, mid-stream IO failure, etc.). The walker doesn't
    /// surface a specific failing path — see [`source::markdown::scan`].
    #[error("markdown source IO failed: {0}")]
    MarkdownScan(std::io::Error),
    /// Creating the per-run tempdir for external rules failed.
    /// External rules need a writable scratch dir to materialise
    /// document bodies into; without one, the rule pipeline cannot
    /// proceed.
    #[error("could not create external-rule tempdir: {0}")]
    TempDir(std::io::Error),
    /// An external rule's batch invocation failed at the IO layer
    /// (subprocess spawn, stdin write, body-path materialisation).
    /// Distinct from `ext.runtime-error`, which the rule itself
    /// reports through the diagnostic channel.
    #[error("external rule {code} IO failed: {source}")]
    ExternalRule {
        code: String,
        #[source]
        source: std::io::Error,
    },
    /// The run checked nothing at all: no `ctxgrd.toml` at the root,
    /// no namespaces configured anywhere (local or global), and no
    /// file claimed intent. Distinguished from a legitimate
    /// zero-config run (id-claimed docs, no toml — still exits 0) so
    /// an unconfigured root fails loudly instead of reporting a
    /// false-confidence `ok: 0 documents`.
    #[error("no ctxgrd.toml found and no documents claim intent — nothing was linted")]
    NothingToLint,
}

impl LintError {
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Config(c) => c.code(),
            Self::MarkdownScan(_) => Some("src.markdown-io"),
            Self::NothingToLint => Some("cfg.missing"),
            Self::TempDir(_) => Some("ext.tempdir"),
            Self::ExternalRule { .. } => Some("ext.io"),
        }
    }

    /// Convert a kernel-orchestration failure into a synthetic
    /// [`Diagnostic`] for the rich renderer. Gives config / IO errors
    /// the same `error[code]: msg` / anchor / help / note shape as
    /// rule violations, so the tool speaks one language everywhere.
    ///
    /// `root` is used to render relative paths in the location anchor.
    pub fn to_diagnostic(&self, root: &Path) -> Diagnostic {
        match self {
            Self::Config(c) => config_error_to_diagnostic(c, root),
            Self::MarkdownScan(e) => Diagnostic::error(
                "src.markdown-io",
                root.display().to_string(),
                0,
                0,
                format!("could not walk markdown tree: {e}"),
            )
            .with_note(format!("cause: {}", e.kind())),
            Self::TempDir(e) => Diagnostic::error(
                "ext.tempdir",
                "",
                0,
                0,
                format!("could not create tempdir for external rules: {e}"),
            )
            .with_help("check $TMPDIR is writable; external rules need a scratch directory")
            .with_note(format!("cause: {}", e.kind())),
            Self::ExternalRule { code, source } => Diagnostic::error(
                "ext.io",
                "",
                0,
                0,
                format!("external rule {code} could not be invoked: {source}"),
            )
            .with_help(format!(
                "check that rules/{}/run is executable and the body-path tempdir is intact",
                code.replace('.', "/")
            ))
            .with_note(format!("cause: {}", source.kind())),
            Self::NothingToLint => Diagnostic::error(
                "cfg.missing",
                "ctxgrd.toml",
                0,
                0,
                "no ctxgrd.toml found and no documents claim intent — nothing was linted",
            )
            .with_help(
                "run `ctxgrd init` to create a starter config, or pass `--root <dir>` \
                 if you meant a different directory",
            ),
        }
    }
}

pub fn config_error_to_diagnostic(err: &ConfigError, root: &Path) -> Diagnostic {
    use crate::config::ConfigError as C;
    let relative = |p: &Path| -> String { p.strip_prefix(root).unwrap_or(p).display().to_string() };
    match err {
        C::Io { path, source } => Diagnostic::error(
            "cfg.io",
            relative(path),
            0,
            0,
            format!("could not read {}", path.display()),
        )
        .with_help("check the file exists and is readable")
        .with_note(format!("cause: {source}")),
        C::Parse { path, source } => Diagnostic::error(
            "cfg.invalid",
            relative(path),
            0,
            0,
            format!("failed to parse {}", relative(path)),
        )
        .with_help("run `ctxgrd init --stdout` to see a valid starter config")
        .with_note(source.to_string()),
        C::RuleUnknown {
            namespace,
            code,
            expected_path,
        } => {
            // `expected_path` for non-core codes is a filesystem path
            // where the rule's `run` file was expected; for `core.*`
            // codes it's a descriptive parenthetical. Only the former
            // makes sense as a "create this path" hint.
            // ADR-025 § PKD-002: if a discoverable pack provides this code,
            // the most actionable advice is the `pack add` that installs it —
            // name the pack ahead of the generic forks below. A mistyped
            // `core.*` or a genuinely-unknown code matches no pack and falls
            // through to the legacy help.
            let providers = crate::pack::providers_of(root, code);
            // Three shapes of unknown rule: a mistyped `core.*`, a
            // reserved built-in (e.g. `agents.*`/`todo.*`) the binary
            // doesn't ship, and an external rule whose `run` script is
            // missing. Each needs different advice — sending the built-in
            // case down the "write a script" path is actively wrong
            // (ADR-020 § ACX-010).
            let (message, help) = if !providers.is_empty() {
                let names = providers.join("`, `");
                let cmd = providers
                    .iter()
                    .map(|n| format!("ctxgrd pack add {n}"))
                    .collect::<Vec<_>>()
                    .join("` or `");
                (
                    format!("[{namespace}] rule '{code}' is not known"),
                    format!(
                        "rule '{code}' is provided by pack `{names}` — run `{cmd}` to install it"
                    ),
                )
            } else if code.starts_with("core.") {
                (
                    format!("[{namespace}] rule '{code}' is not known"),
                    format!("remove '{code}' from [{namespace}].rules, or pick a real core rule (see `ctxgrd rules`)"),
                )
            } else if config::is_reserved_builtin_prefix(code) {
                (
                    format!("[{namespace}] rule '{code}' is a built-in rule not provided by this ctxgrd build"),
                    format!("upgrade ctxgrd (built-in rules ship in the binary, not as scripts), or check the name with `ctxgrd rules`; if intentional, remove '{code}' from [{namespace}].rules"),
                )
            } else {
                (
                    format!("[{namespace}] rule '{code}' is not known"),
                    format!("add a rule directory at {expected_path}, or remove '{code}' from [{namespace}].rules"),
                )
            };
            Diagnostic::error("cfg.rule-unknown", "ctxgrd.toml", 0, 0, message)
                .with_help(help)
                .with_note("run `ctxgrd rules` to see all available rules")
        }
        C::RuleParamsMissing { namespace, code } => Diagnostic::error(
            "cfg.rule-params-missing",
            "ctxgrd.toml",
            0,
            0,
            format!(
                "[{namespace}.\"{code}\"] is a parameterised rule and requires a params sub-table"
            ),
        )
        .with_help(format!(
            "add `[{namespace}.\"{code}\"]` with the rule's parameters"
        )),
        C::RuleParamsInvalid {
            namespace,
            code,
            detail,
        } => Diagnostic::error(
            "cfg.rule-params-invalid",
            "ctxgrd.toml",
            0,
            0,
            format!("[{namespace}.\"{code}\"] {detail}"),
        )
        .with_help(format!(
            "run `ctxgrd rules {code}` to see the expected params shape"
        )),
        C::RulesListInvalid { namespace } => Diagnostic::error(
            "cfg.rules-list-invalid",
            "ctxgrd.toml",
            0,
            0,
            format!("[{namespace}] `rules` must be an array of strings"),
        )
        .with_help("rules = [\"core.frontmatter\", \"core.id\", …]".to_string()),
        C::NamespaceNameNotIdLegal { namespace } => Diagnostic::error(
            "cfg.namespace-name-invalid",
            "ctxgrd.toml",
            0,
            0,
            format!(
                "[{namespace}] lists `core.id` but '{namespace}' is not a legal id prefix — \
                 an id `{namespace}-<number>` cannot be parsed"
            ),
        )
        .with_help(
            "rename the namespace to uppercase ASCII with no hyphen (regex `^[A-Z][A-Z0-9]*$`), \
             e.g. `SAFEGUARD` not `SR-MAP`",
        )
        .with_note("the hyphen separates the namespace from the number in an id, so it cannot appear inside the namespace"),
        C::NamespaceReserved { path } => Diagnostic::error(
            "ext.namespace-reserved",
            relative(path),
            0,
            0,
            "external rule directory uses the reserved `core` namespace".to_string(),
        )
        .with_help("rename the directory to use a non-reserved namespace")
        .with_note("the `core` namespace is reserved for built-in rules"),
        C::IgnorePatternInvalid { pattern, detail } => Diagnostic::error(
            "cfg.ignore-invalid",
            "ctxgrd.toml",
            0,
            0,
            if pattern.is_empty() {
                format!("[ignore].patterns: {detail}")
            } else {
                format!("[ignore].patterns entry {pattern:?} is not a valid glob: {detail}")
            },
        )
        .with_help(
            "globs follow globset syntax, anchored at the lint root — prefix `**/` to match \
             at any depth (e.g. `docs/drafts/**`, `**/node_modules/**`); `!` negation is not \
             supported",
        ),
        C::ReferencesInvalid { detail } => Diagnostic::error(
            "cfg.references-invalid",
            "ctxgrd.toml",
            0,
            0,
            format!("[references] {detail}"),
        )
        .with_help("expected `[references]\\nscan = [<glob>, ...]`"),
        C::PathsInvalid {
            namespace,
            pattern,
            detail,
        } => Diagnostic::error(
            "cfg.paths-invalid",
            "ctxgrd.toml",
            0,
            0,
            if pattern.is_empty() {
                format!("[{namespace}].paths: {detail}")
            } else {
                format!("[{namespace}].paths entry {pattern:?} is not a valid glob: {detail}")
            },
        )
        .with_help(
            "globs follow globset syntax, anchored at the lint root — prefix `**/` to match \
             at any depth (e.g. `docs/adrs/**`)",
        ),
        C::PipelineInvalid { detail } => Diagnostic::error(
            "cfg.pipeline-invalid",
            "ctxgrd.toml",
            0,
            0,
            format!("[pipeline] {detail}"),
        )
        .with_help(
            "expected `[pipeline]\\nstages = [\"PRD\", \"ADR\", …]` with an optional \
             `[pipeline.gate]` table",
        ),
        C::PipelineStageUnknown { stage } => Diagnostic::error(
            "cfg.pipeline-stage-unknown",
            "ctxgrd.toml",
            0,
            0,
            format!("[pipeline].stages entry '{stage}' is not an active namespace"),
        )
        .with_help(format!(
            "declare [{stage}] in ctxgrd.toml, or remove '{stage}' from `stages`"
        )),
        C::PipelineGateInvalid { namespace, detail } => Diagnostic::error(
            "cfg.pipeline-gate-invalid",
            "ctxgrd.toml",
            0,
            0,
            format!("[pipeline.gate].{namespace}: {detail}"),
        )
        .with_help(
            "gate predicates are `any:<status>` or `all:<status>` for a namespace \
             listed in `stages`",
        ),
        C::PipelineGateStatusUnknown { namespace, status } => Diagnostic::error(
            "cfg.pipeline-gate-status",
            "ctxgrd.toml",
            0,
            0,
            format!(
                "[pipeline.gate].{namespace} status '{status}' is not in {namespace}'s \
                 core.allowed-values"
            ),
        )
        .with_help(format!(
            "use a status from [{namespace}.\"core.allowed-values\"].status, or add '{status}' to it"
        )),
    }
}

/// Result of the source-aggregation phase shared by [`lint`] and
/// [`find_references`]. Captures every output of "load config and
/// produce the canonical `Vec<Document>`" so downstream code can
/// run rules, look up references, or render a summary without
/// re-implementing the pipeline.
///
/// The five steps it absorbs:
/// 1. Load `ctxgrd.toml` (per-root + global merge).
/// 2. Discover external sources on disk.
/// 3. Run the activated subset (ADR-005 § SRC-002 — "all sources
///    MUST complete before any rule runs").
/// 4. Walk the markdown tree.
/// 5. Translate source envelopes into documents, capturing failures
///    on the same `ParseDiagnostic` channel as markdown failures.
///
/// `parse_diagnostics` are NOT yet rendered as user-facing
/// `Diagnostic`s — that is the caller's choice. `lint` always
/// renders them; `find_references` currently discards them.
///
/// `path_claims` is built once here and threaded to callers so
/// `lint` and `scan_file_level` share the same instance rather than
/// each rebuilding from config (review finding #7).
pub(crate) struct IngestResult {
    pub(crate) config: Config,
    pub(crate) documents: Vec<Document>,
    pub(crate) parse_diagnostics: Vec<ParseDiagnostic>,
    pub(crate) kernel_messages: Vec<KernelMessage>,
    pub(crate) path_claims: crate::path_claims::PathClaims,
}

/// Source-aggregation pipeline shared by [`lint`] and
/// [`find_references`]. See [`IngestResult`] for what each field
/// carries. This function is the single home of the ADR-005 §
/// SRC-002 invariant — "all sources MUST complete before any rule
/// runs" — so any future change to source ordering or envelope
/// translation lives here, not in two places.
pub(crate) fn ingest(root: &Path) -> Result<IngestResult, LintError> {
    let mut config = config::load(root)?;
    // Drain config-load advisories so they ride the same channel as
    // every other kernel-level message. `config` is local; the take()
    // leaves it in a clean state for downstream rule scheduling.
    let mut kernel_messages = std::mem::take(&mut config.kernel_messages);

    let discovered_sources = source_ext::discover_sources(root);
    let source_run = source_ext::run_activated_sources(root, &discovered_sources, &config.sources);
    kernel_messages.extend(source_run.messages);

    let path_claims = crate::path_claims::PathClaims::from_config(&config);
    let scan = markdown::scan(root, config.ignore.as_ref(), Some(&path_claims))
        .map_err(LintError::MarkdownScan)?;
    let mut documents = scan.documents;
    let mut parse_diagnostics = scan.parse_diagnostics;

    // DOC-007: cross-namespace path-conflicts surface at ingest time
    // as `cfg.path-conflict` `KernelMessage`s, before any rule runs.
    // Conflicting files are excluded from `documents` by parse_one
    // (the ParseOutcome::Conflict variant), so per-document
    // diagnostics never fire against them.
    for conflict in &scan.path_conflicts {
        kernel_messages.push(conflict.to_kernel_message());
    }

    // Translate source envelopes into documents. Malformed envelopes
    // become `ParseDiagnostic`s on the same channel as markdown parse
    // failures so downstream renderers handle both uniformly.
    for (_source_name, envelope) in source_run.envelopes {
        let loc = envelope.location.clone();
        match envelope.into_document() {
            Ok(doc) => documents.push(doc),
            Err(EnvelopeError::IdMalformed { raw_id }) => {
                parse_diagnostics.push(ParseDiagnostic {
                    location: loc,
                    kind: ParseDiagnosticKind::IdMalformed { raw_id },
                });
            }
            Err(EnvelopeError::Frontmatter(e)) => {
                parse_diagnostics.push(ParseDiagnostic {
                    location: loc,
                    kind: ParseDiagnosticKind::Frontmatter(e.to_string()),
                });
            }
        }
    }

    Ok(IngestResult {
        config,
        documents,
        parse_diagnostics,
        kernel_messages,
        path_claims,
    })
}

/// The full result of a lint run: the user-facing [`LintOutcome`] plus
/// the resolved [`Config`] and [`Document`] set it was computed from.
///
/// `ctxgrd status` (SPEC-002) needs the same document set and rule
/// diagnostics `lint` produces — statuses come from the documents,
/// per-document lint-cleanliness from `outcome.diagnostics` joined on
/// `location`. Exposing them here keeps status on the *exact* same
/// pipeline rather than re-implementing any check (SPEC § Workflows
/// step 2).
pub(crate) struct LintRun {
    pub(crate) outcome: LintOutcome,
    pub(crate) config: Config,
    pub(crate) documents: Vec<Document>,
}

/// Discover every directory under `root` that holds a `ctxgrd.toml`,
/// for `--recursive` multi-project linting. Each such directory is an
/// independent project root that [`lint`] can be called on directly.
///
/// Walks with the `ignore` crate — the ripgrep stack the reference
/// scanner already uses — so configs under `.gitignore`'d or hidden
/// directories (vendored deps, build output, `.git`) are skipped. The
/// markdown walker's `walkdir`/`globset` stack is deliberately *not*
/// reused here: its ignore set is keyed to one `ctxgrd.toml`, but
/// discovery runs before any single config is chosen.
///
/// Roots are returned sorted and de-duplicated for deterministic
/// output, and include `root` itself when it carries a config.
pub fn discover_config_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .standard_filters(true) // honour .gitignore / .ignore / hidden (matches REF-010)
        .build()
        .flatten()
        .filter(|entry| entry.file_name() == "ctxgrd.toml")
        .filter_map(|entry| entry.path().parent().map(Path::to_path_buf))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

pub fn lint(root: &Path) -> Result<LintOutcome, LintError> {
    lint_run(root).map(|r| r.outcome)
}

pub(crate) fn lint_run(root: &Path) -> Result<LintRun, LintError> {
    // Steps 1–4: load config, run sources, walk markdown, translate
    // envelopes. See `ingest()` for the SRC-002 invariant.
    let IngestResult {
        config,
        documents,
        parse_diagnostics,
        mut kernel_messages,
        path_claims,
    } = ingest(root)?;

    // An unconfigured root that produced no work at all fails loudly
    // (exit 2) instead of a false-confidence `ok: 0 documents`. The
    // conjunction is deliberate: a missing toml alone is zero-config
    // mode (config::load contract — id-claimed docs still lint), and
    // a non-empty `parse_diagnostics` means a file *tried* to claim
    // intent — that finding must surface, not be masked by this error.
    if config.namespaces.is_empty()
        && documents.is_empty()
        && parse_diagnostics.is_empty()
        && !root.join("ctxgrd.toml").exists()
    {
        return Err(LintError::NothingToLint);
    }

    // Files claimed by a file-level namespace (AGENTS, TODO) are linted
    // by the builtin-compiled file-level pass below, not the id pipeline —
    // suppress the `core.id` / `core.frontmatter` parse diagnostics that
    // their missing frontmatter would otherwise produce (ADR-020 § ACX-003).
    // `path_claims` was built once in ingest() — reused here (finding #7).
    let file_level_ns = config.file_level_namespaces();
    let mut diagnostics: Vec<Diagnostic> = parse_diagnostics
        .iter()
        .filter(|pd| {
            file_level_ns.is_empty()
                || !path_claims
                    .matching_namespaces(&pd.location)
                    .any(|ns| file_level_ns.contains(&ns))
        })
        .map(rules::parse_diagnostic_to_diagnostic)
        .collect();

    // --- Step 5: aggregate core rules, filtered by per-namespace config. ---
    let mut aggregate: Vec<Diagnostic> = Vec::new();
    aggregate.extend(rules::id_unique(&documents));
    // ADR-064 § DAG-001: build the document dependency graph once and
    // share it between the two dep rules, instead of each rebuilding the
    // node index and re-parsing `depends_on`.
    let dep_graph = crate::dag::DepGraph::new(&documents);
    aggregate.extend(rules::dep_resolved(&dep_graph));
    aggregate.extend(rules::dep_cycle(&dep_graph));
    let declared_namespaces: BTreeSet<&str> =
        config.namespaces.keys().map(String::as_str).collect();
    // ADR-001 § REF-002: scan non-markdown files for pointer mentions
    // when `[references].scan` is configured. Empty globs short-circuit
    // (current behaviour).
    let references = if config.reference_scan_globs.is_empty() {
        Vec::new()
    } else {
        match crate::reference::scan(root, &config.reference_scan_globs) {
            Ok(report) => {
                if report.walker_errors > 0 || report.searcher_errors > 0 {
                    kernel_messages.push(
                        KernelMessage::warning(
                            "ref.scan-error",
                            format!(
                                "[references] scan absorbed {} walker error(s) and {} searcher error(s)",
                                report.walker_errors, report.searcher_errors,
                            ),
                        )
                        .with_help(
                            "some files may be missing from cross-reference checks; \
                             check directory permissions on paths matching `[references].scan` globs",
                        ),
                    );
                }
                report.references
            }
            Err(e) => {
                kernel_messages.push(
                    KernelMessage::warning(
                        "ref.scan-error",
                        format!("[references] scan failed: {e}"),
                    )
                    .with_help(
                        "check the `[references].scan` globs in ctxgrd.toml — \
                         malformed patterns abort the whole walk",
                    ),
                );
                Vec::new()
            }
        }
    };
    aggregate.extend(rules::requirement_ref(&documents));
    aggregate.extend(rules::cross_ref(
        &documents,
        &declared_namespaces,
        &references,
    ));

    let loc_to_ns: BTreeMap<&str, &str> = documents
        .iter()
        .map(|d| (d.location.as_str(), d.id.namespace.as_str()))
        .collect();
    aggregate.retain(|d| {
        let Some(ns) = loc_to_ns.get(d.location.as_str()).copied() else {
            return true;
        };
        config.namespace_config(ns).enables(&d.code)
    });
    diagnostics.extend(aggregate);

    // --- Step 6 + 7: per-document rules (core parameterised + external). ---
    let ext_tmp = any_doc_has_external_rule(&documents, &config).then(ext::RunTempDir::new);
    let ext_tmp = match ext_tmp {
        Some(Ok(tmp)) => Some(tmp),
        Some(Err(e)) => return Err(LintError::TempDir(e)),
        None => None,
    };

    // ADR-073 § SUCC-001: an empty params table so `core.successor-link`
    // runs on its documented defaults when a namespace lists it without a
    // `[NS."core.successor-link"]` block.
    let empty_params = Value::Object(Default::default());

    for doc in &documents {
        let ns_cfg = config.namespace_config(&doc.id.namespace);

        // 6: core parameterised rules
        for code in &ns_cfg.rules {
            // `core.successor-link` resolves its successor against the shared
            // document graph (the same index `core.dep-resolved` uses,
            // ADR-029), and its params are optional — so it runs even when
            // `ns_cfg.params.get(code)` is `None`.
            if code == "core.successor-link" {
                let params = ns_cfg.params.get(code).unwrap_or(&empty_params);
                diagnostics.extend(rules::successor_link(doc, params, &dep_graph));
                continue;
            }
            let Some(params) = ns_cfg.params.get(code) else {
                continue;
            };
            match code.as_str() {
                "core.required-headings" => {
                    diagnostics.extend(rules::required_headings(doc, params));
                }
                "core.required-metadata" => {
                    diagnostics.extend(rules::required_metadata(doc, params));
                }
                "core.allowed-values" => {
                    diagnostics.extend(rules::allowed_values(doc, params));
                }
                _ => {}
            }
        }
    }

    // 6b: file-level builtin-compiled rules (the `agents.*` / `todo.*`
    // rules) lint id-less path-claimed singletons (CLAUDE.md/AGENTS.md/
    // TODO.md) that never become id-keyed documents, so they run outside
    // the per-document loop above (ADR-020 § ACX-003/ACX-004).
    let file_level_scan = agent_guide::scan_file_level(root, &config, &path_claims)
        .map_err(LintError::MarkdownScan)?;
    diagnostics.extend(file_level_scan.diagnostics);

    // 6b': core.min-docs (ADR-048 § SEED-001) — the node-existence seed. It
    // fires on a declared namespace that opted in but holds zero documents,
    // so it runs after both presence corpora are known: id-keyed `documents`
    // and the file-level singletons (CLAUDE.md / TODO.md) just scanned, which
    // never become id-keyed documents. (The aggregate phase above runs before
    // the file-level scan, so a file-level charter's presence is not yet
    // established there.)
    diagnostics.extend(rules::min_docs(
        &config.namespaces,
        &documents,
        &file_level_scan.namespaces,
    ));

    // 6c: document-level builtin-compiled rules (id-claim namespaces, e.g.
    // `tasks.*`). Unlike 6b these lint real id-keyed documents in the
    // per-document loop — `tasks.files-allowed` is the first (ADR-022 §
    // ABP-004), dispatched by code via `agent_guide::document_check`.
    // The "managed" namespace set (ADR-039 § DAG-003): every namespace
    // that appears in some `core.dep-shape` `requires`/`allows` anywhere
    // in the config. `core.dep-shape`'s admissibility half needs this
    // cross-config view, so we thread it through a synthesized `managed`
    // param — the same edge-level channel `pipeline.conformance` used for
    // `stages` (which it replaces). Computed once here.
    let managed: Vec<String> = config.dep_shape_managed().into_iter().collect();
    let managed_json = Value::Array(managed.iter().map(|s| Value::String(s.clone())).collect());

    for doc in &documents {
        let ns_cfg = config.namespace_config(&doc.id.namespace);
        for code in &ns_cfg.rules {
            if let Some(check_fn) = agent_guide::document_check(code) {
                // core.dep-shape's admissibility half reads the managed set
                // (ADR-039 § DAG-003); merge it into the namespace's own
                // dep-shape params before dispatch.
                let params = if code == "core.dep-shape" {
                    let mut merged = ns_cfg
                        .params
                        .get(code)
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Default::default()));
                    if let Value::Object(map) = &mut merged {
                        map.insert("managed".to_string(), managed_json.clone());
                    }
                    Some(merged)
                } else {
                    ns_cfg.params.get(code).cloned()
                };
                diagnostics.extend(check_fn(doc, params.as_ref(), root));
            }
        }
        if config.todo_listed_global && !ns_cfg.rules.iter().any(|c| c == "todo.listed") {
            diagnostics.extend(agent_guide::check_todo_listed(doc, None, root));
        }
    }

    // 7: external rules — one subprocess per (namespace, rule) batch
    // (ADR-002 § RUL-001). Group docs by namespace, then run each
    // configured external rule once over all docs in that namespace.
    if let Some(tmp) = ext_tmp.as_ref() {
        let mut docs_by_ns: BTreeMap<&str, Vec<&Document>> = BTreeMap::new();
        for doc in &documents {
            docs_by_ns
                .entry(doc.id.namespace.as_str())
                .or_default()
                .push(doc);
        }
        for (ns_name, ns_docs) in &docs_by_ns {
            let ns_cfg = config.namespace_config(ns_name);
            let ext_rules: Vec<(String, PathBuf)> = ns_cfg
                .rules
                .iter()
                .filter(|c| !c.starts_with("core.") && !config::is_builtin_compiled(c))
                .filter_map(|c| {
                    let (ns, name) = c.split_once('.')?;
                    let path = root.join("rules").join(ns).join(name).join("run");
                    path.is_file().then_some((c.clone(), path))
                })
                .collect();
            for (code, run_path) in &ext_rules {
                let params = ns_cfg
                    .params
                    .get(code)
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default()));
                let timeout = params
                    .get("timeout_sec")
                    .and_then(|v| v.as_u64())
                    .map(Duration::from_secs)
                    .unwrap_or(ext::DEFAULT_RULE_TIMEOUT);
                let ext_diags =
                    ext::run_rule_batch(code, run_path, ns_docs, &params, timeout, root, tmp)
                        .map_err(|source| LintError::ExternalRule {
                            code: code.clone(),
                            source,
                        })?;
                diagnostics.extend(ext_diags);
            }
        }
    }

    reporter::sort(&mut diagnostics);

    let any_error = diagnostics.iter().any(|d| d.severity == Severity::Error)
        || kernel_messages
            .iter()
            .any(|m| m.severity == Severity::Error);
    let exit = if any_error {
        ExitStatus::LintFailure
    } else {
        ExitStatus::Ok
    };

    // Coverage spans both document models: id-keyed documents *and* the
    // path-claimed file-level singletons (AGENTS/TODO) that never become
    // id-keyed documents but are still linted (finding #3). Excluding the
    // latter understated coverage — a user could conclude their
    // instruction files were not being linted.
    let documents_linted = documents.len() + file_level_scan.files_linted;
    let mut namespaces: BTreeSet<&str> =
        documents.iter().map(|d| d.id.namespace.as_str()).collect();
    namespaces.extend(file_level_scan.namespaces.iter().map(String::as_str));
    let rules_active: usize = namespaces
        .iter()
        .map(|ns| config.namespace_config(ns).rules.len())
        .sum();

    Ok(LintRun {
        outcome: LintOutcome {
            diagnostics,
            kernel_messages,
            exit,
            documents_linted,
            rules_active,
        },
        config,
        documents,
    })
}

/// One pointer to the document with id `<target>` (ADR-001 § REF-008).
///
/// `kind` distinguishes (a) the document itself when it's file-backed,
/// (b) other documents whose `depends_on:` lists the target, (c) other
/// documents whose body has a non-suppressed cross-ref token to the
/// target, and (d) reference-scanner hits in non-markdown files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceHit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub kind: ReferenceHitKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReferenceHitKind {
    /// The target document itself, when file-backed. `line`/`col` are 0.
    SelfDoc,
    /// `depends_on:` entry in another document. `from` is the depending
    /// document's id; `line` is the YAML key line, `col` is 0.
    DependsOn { from: String },
    /// Body cross-ref token in another document. `from` is the
    /// containing document's id.
    BodyCrossRef { from: String },
    /// Reference-scanner hit (non-markdown file).
    ScannerHit,
}

/// Find every location pointing at `target_id` (ADR-001 § REF-008).
///
/// Output is the deterministic union of the four hit kinds:
///
/// 1. The document itself if it's file-backed.
/// 2. Documents whose `depends_on:` contains the target id.
/// 3. Documents whose body has a non-suppressed cross-ref token to
///    the target id.
/// 4. Reference-scanner records (`<NS>-<number>` tokens in non-markdown
///    files) pointing at the target id, when `[references].scan` is
///    configured.
///
/// Sorted by (file, line, col, kind-discriminant). Stable across runs
/// so tooling can diff outputs reliably.
pub fn find_references(root: &Path, target_id: &str) -> Result<Vec<ReferenceHit>, LintError> {
    let target: crate::id::DocumentId = match target_id.parse() {
        Ok(id) => id,
        Err(_) => return Ok(Vec::new()),
    };

    // Reuse the shared ingestion pipeline so this subcommand sees
    // the exact same document set as `lint`. We discard
    // `parse_diagnostics` and `kernel_messages` here — the refs
    // subcommand is a reverse-lookup, not a linter, and surfacing
    // ingestion warnings here would be noise. If this proves
    // problematic the channel is intact and a future revision can
    // surface them on stderr or in `--format json`.
    let IngestResult {
        config, documents, ..
    } = ingest(root)?;

    let mut hits: Vec<ReferenceHit> = Vec::new();

    // Hit kind 1: the target itself if it's file-backed.
    if let Some(doc) = documents
        .iter()
        .find(|d| d.id.namespace == target.namespace && d.id.number == target.number)
    {
        // File-backed when location resolves under root.
        if root.join(&doc.location).is_file() {
            hits.push(ReferenceHit {
                file: doc.location.clone(),
                line: 0,
                col: 0,
                kind: ReferenceHitKind::SelfDoc,
            });
        }
    }

    // Hit kind 2: depends_on edges.
    for doc in &documents {
        if !doc.depends_on.iter().any(|d| d == target_id) {
            continue;
        }
        let line = doc
            .frontmatter_lines
            .get("depends_on")
            .copied()
            .unwrap_or(0);
        hits.push(ReferenceHit {
            file: doc.location.clone(),
            line,
            col: 0,
            kind: ReferenceHitKind::DependsOn {
                from: doc.raw_id.clone(),
            },
        });
    }

    // Hit kind 3: body cross-ref tokens.
    for doc in &documents {
        let Some(ast) = doc.ast.as_ref() else {
            continue;
        };
        for tok in &ast.cross_ref_tokens {
            if tok.in_code || tok.in_strikethrough {
                continue;
            }
            if tok.namespace != target.namespace || tok.number != target.number {
                continue;
            }
            hits.push(ReferenceHit {
                file: doc.location.clone(),
                line: tok.line,
                col: tok.col,
                kind: ReferenceHitKind::BodyCrossRef {
                    from: doc.raw_id.clone(),
                },
            });
        }
    }

    // Hit kind 4: reference-scanner records.
    if !config.reference_scan_globs.is_empty() {
        if let Ok(report) = crate::reference::scan(root, &config.reference_scan_globs) {
            for r in report.references {
                // Equality on the full id is sufficient — any token
                // matching `target_id` necessarily has the right
                // namespace prefix, so the prefix check earlier
                // versions did was redundant (and allocated a
                // throwaway `String` per record).
                if r.token != target_id {
                    continue;
                }
                hits.push(ReferenceHit {
                    file: r.file_path.to_string_lossy().into_owned(),
                    line: r.line,
                    col: r.col,
                    kind: ReferenceHitKind::ScannerHit,
                });
            }
        }
    }

    hits.sort_by(|a, b| {
        (a.file.as_str(), a.line, a.col, kind_ordinal(&a.kind)).cmp(&(
            b.file.as_str(),
            b.line,
            b.col,
            kind_ordinal(&b.kind),
        ))
    });
    hits.dedup();
    Ok(hits)
}

fn kind_ordinal(kind: &ReferenceHitKind) -> u8 {
    match kind {
        ReferenceHitKind::SelfDoc => 0,
        ReferenceHitKind::DependsOn { .. } => 1,
        ReferenceHitKind::BodyCrossRef { .. } => 2,
        ReferenceHitKind::ScannerHit => 3,
    }
}

/// Serialize a [`LintOutcome`] as pretty-printed JSON.
///
/// Consumers: LSP shims, CI pipelines, dashboards. The wire shape
/// is intentionally decoupled from `LintOutcome`'s in-memory fields
/// via a dedicated struct — so we can rename / reshape the internal
/// type without breaking downstream tooling.
pub fn render_json_outcome(outcome: &LintOutcome) -> String {
    #[derive(serde::Serialize)]
    struct Wire<'a> {
        exit_code: u8,
        diagnostics: &'a [Diagnostic],
        kernel_messages: &'a [KernelMessage],
        // ADR-038 § HINT-002/003: the "fix the documents, not the
        // config" advisory, present only when rule diagnostics were
        // emitted. Additive optional field — absent on a clean run.
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<&'static str>,
    }
    let wire = Wire {
        exit_code: outcome.exit.code(),
        diagnostics: &outcome.diagnostics,
        kernel_messages: &outcome.kernel_messages,
        hint: (!outcome.diagnostics.is_empty()).then_some(crate::reporter::LINT_HINT),
    };
    serde_json::to_string_pretty(&wire).unwrap_or_else(|_| "{}".to_string())
}

/// Render the Claude Code `Stop`-hook decision for a completed lint
/// (ADR-062 § STOP-001).
///
/// Returns `Some(json)` — the `{"decision":"block","reason":…}` object —
/// when the run failed (`exit == LintFailure`, i.e. at least one
/// error-severity diagnostic *or* kernel message), and `None` when it is
/// clean or carries warnings only. Driving the choice off `exit` rather
/// than re-counting keeps the block decision identical to the `0/1/2`
/// exit-code contract: warnings never block, exactly as they never
/// escalate past exit 0.
///
/// The config/kernel-error path (where [`lint`] returns `Err`, with no
/// `LintOutcome`) blocks too — the binary builds that body and calls
/// [`claude_stop_block`] directly, so a broken setup fails closed rather
/// than letting the turn pass silently.
pub fn render_claude_stop(outcome: &LintOutcome) -> Option<String> {
    if !matches!(outcome.exit, ExitStatus::LintFailure) {
        return None;
    }
    let mut body = String::new();
    for m in outcome
        .kernel_messages
        .iter()
        .filter(|m| m.severity == Severity::Error)
    {
        body.push_str(&reporter::render_kernel_message_simple(m));
    }
    let errors: Vec<Diagnostic> = outcome
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .cloned()
        .collect();
    body.push_str(&reporter::render(&errors));
    Some(claude_stop_block(&body))
}

/// Wrap a compact, colour-free failure body in the Stop decision JSON.
/// Shared by [`render_claude_stop`] and the binary's kernel-error path so
/// the `{"decision":"block",…}` shape lives in exactly one place. The
/// `reason` mirrors the bash-era hook: a `Verification failed:` header,
/// the failing diagnostics, and a `Fix before completing.` trailer, so the
/// agent receives the same actionable detail the commit hook would.
pub fn claude_stop_block(reason_body: &str) -> String {
    let reason = format!("Verification failed:\n{reason_body}Fix before completing.\n");
    serde_json::json!({ "decision": "block", "reason": reason }).to_string()
}

fn any_doc_has_external_rule(documents: &[Document], config: &config::Config) -> bool {
    documents.iter().any(|d| {
        config
            .namespace_config(&d.id.namespace)
            .rules
            .iter()
            .any(|c| !c.starts_with("core.") && !config::is_builtin_compiled(c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_code_mapping() {
        assert_eq!(ExitStatus::Ok.code(), 0);
        assert_eq!(ExitStatus::LintFailure.code(), 1);
        assert_eq!(ExitStatus::KernelError.code(), 2);
    }

    #[test]
    fn lint_error_has_cfg_code_for_config_variants() {
        let err = LintError::Config(ConfigError::RuleUnknown {
            namespace: "ADR".into(),
            code: "core.nope".into(),
            expected_path: "(not a core rule)".into(),
        });
        assert_eq!(err.code(), Some("cfg.rule-unknown"));
    }

    #[test]
    fn lint_error_io_variants_carry_specific_codes() {
        let m = LintError::MarkdownScan(std::io::Error::other("boom"));
        assert_eq!(m.code(), Some("src.markdown-io"));
        let t = LintError::TempDir(std::io::Error::other("boom"));
        assert_eq!(t.code(), Some("ext.tempdir"));
        let r = LintError::ExternalRule {
            code: "adr.consequences-non-empty".into(),
            source: std::io::Error::other("boom"),
        };
        assert_eq!(r.code(), Some("ext.io"));
    }

    #[test]
    fn config_error_to_diagnostic_uses_cfg_code_and_helpful_anchor() {
        let err = LintError::Config(ConfigError::RuleUnknown {
            namespace: "ADR".into(),
            code: "core.nope".into(),
            expected_path: "(not a core rule)".into(),
        });
        let d = err.to_diagnostic(Path::new("."));
        assert_eq!(d.code, "cfg.rule-unknown");
        assert_eq!(d.location, "ctxgrd.toml");
        assert!(d.message.contains("[ADR]"));
        assert!(d.message.contains("core.nope"));
        // Help for an unknown `core.*` code points users at the
        // built-in rule list, not at a filesystem path.
        let help = d.help.as_deref().unwrap();
        assert!(help.contains("core.nope"));
        assert!(help.contains("ctxgrd rules"));
    }

    #[test]
    fn rule_unknown_suggests_pack_add_when_a_pack_provides_the_code() {
        // ADR-025 § PKD-002: `agent.frontmatter` is bundled by the `claude`
        // pack (ADR-051), so the help points at `pack add claude` rather than
        // telling the user to author a `run` script.
        let tmp = tempfile::tempdir().unwrap();
        let err = LintError::Config(ConfigError::RuleUnknown {
            namespace: "CLAUDEAGENTS".into(),
            code: "agent.frontmatter".into(),
            expected_path: tmp
                .path()
                .join("rules/agent/frontmatter/run")
                .display()
                .to_string(),
        });
        let d = err.to_diagnostic(tmp.path());
        assert_eq!(d.code, "cfg.rule-unknown");
        let help = d.help.as_deref().unwrap();
        assert!(
            help.contains("ctxgrd pack add claude"),
            "expected pack-add suggestion, got: {help}"
        );
        assert!(
            help.contains("provided by pack `claude`"),
            "expected pack provenance, got: {help}"
        );
    }

    #[test]
    fn rule_unknown_falls_back_to_script_hint_when_no_pack_provides_it() {
        // A non-core code no pack provides keeps the legacy external-rule
        // advice (write a `run` script) — the pack lookup degrades cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let err = LintError::Config(ConfigError::RuleUnknown {
            namespace: "ADR".into(),
            code: "report.custom-check".into(),
            expected_path: "rules/report/custom-check/run".into(),
        });
        let d = err.to_diagnostic(tmp.path());
        let help = d.help.as_deref().unwrap();
        assert!(
            help.contains("add a rule directory at rules/report/custom-check/run"),
            "expected legacy script hint, got: {help}"
        );
        assert!(
            !help.contains("pack add"),
            "must not invent a pack suggestion, got: {help}"
        );
    }

    #[test]
    fn config_error_parse_routes_to_cfg_invalid() {
        let bad: Result<toml::Value, _> = toml::from_str("bad = 'unterminated\n");
        let parse_err = bad.unwrap_err();
        let err = LintError::Config(ConfigError::Parse {
            path: PathBuf::from("/tmp/whatever/ctxgrd.toml"),
            source: parse_err,
        });
        let d = err.to_diagnostic(Path::new("/tmp/whatever"));
        assert_eq!(d.code, "cfg.invalid");
        assert_eq!(d.location, "ctxgrd.toml");
        assert!(d.help.is_some());
    }

    #[test]
    fn lint_error_markdown_scan_renders_anchored_diagnostic() {
        let err = LintError::MarkdownScan(std::io::Error::other("boom"));
        let d = err.to_diagnostic(Path::new("/tmp/proj"));
        assert_eq!(d.code, "src.markdown-io");
        // The walker error is anchored on the lint root so users see
        // *where* to look, not just an opaque "io.error".
        assert!(d.location.contains("/tmp/proj"));
        assert!(d.message.contains("boom"));
        assert!(d.note.as_deref().unwrap().contains("cause"));
    }

    #[test]
    fn lint_error_external_rule_renders_with_code_in_help() {
        let err = LintError::ExternalRule {
            code: "adr.consequences-non-empty".into(),
            source: std::io::Error::other("boom"),
        };
        let d = err.to_diagnostic(Path::new("."));
        assert_eq!(d.code, "ext.io");
        assert!(d.message.contains("adr.consequences-non-empty"));
        // Help converts the rule code to the directory it lives in
        // so the user can `ls` directly.
        let help = d.help.as_deref().unwrap();
        assert!(help.contains("rules/adr/consequences-non-empty/run"));
    }

    #[test]
    fn lint_error_nothing_to_lint_carries_cfg_missing_code() {
        let err = LintError::NothingToLint;
        assert_eq!(err.code(), Some("cfg.missing"));
    }

    #[test]
    fn lint_error_nothing_to_lint_renders_init_hint() {
        let d = LintError::NothingToLint.to_diagnostic(Path::new("/tmp/billing-service"));
        assert_eq!(d.code, "cfg.missing");
        assert_eq!(d.location, "ctxgrd.toml");
        assert!(d.message.contains("nothing was linted"));
        let help = d.help.as_deref().unwrap();
        assert!(help.contains("ctxgrd init"));
        assert!(help.contains("--root"));
    }

    #[test]
    fn config_default_has_empty_sources() {
        let c = crate::config::Config::default();
        assert!(c.sources.is_empty());
    }

    #[test]
    fn json_outcome_has_all_four_top_level_fields() {
        let outcome = LintOutcome {
            diagnostics: vec![Diagnostic::error(
                "core.dep-resolved",
                "adrs/ADR-099.md",
                5,
                0,
                "PRD-999 does not resolve",
            )],
            kernel_messages: vec![
                KernelMessage::error("src.runtime-error", "source 'jira' timed out"),
                KernelMessage::warning("cfg.reserved-source", "noop"),
            ],
            exit: ExitStatus::LintFailure,
            documents_linted: 1,
            rules_active: 6,
        };
        let json = render_json_outcome(&outcome);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["exit_code"], 1);
        assert_eq!(parsed["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["kernel_messages"].as_array().unwrap().len(), 2);
        assert!(parsed.get("warnings").is_none(), "warnings channel removed");
        // Severity is lowercase.
        assert_eq!(parsed["diagnostics"][0]["severity"], "error");
        assert_eq!(parsed["diagnostics"][0]["code"], "core.dep-resolved");
        assert_eq!(parsed["kernel_messages"][0]["severity"], "error");
        assert_eq!(parsed["kernel_messages"][1]["severity"], "warning");
    }

    #[test]
    fn json_outcome_carries_hint_when_diagnostics_present() {
        // ADR-038 § HINT-002/003: a failing run carries the canonical
        // hint verbatim in the additive `hint` field.
        let outcome = LintOutcome {
            diagnostics: vec![Diagnostic::error(
                "core.dep-resolved",
                "adrs/ADR-099.md",
                5,
                0,
                "PRD-999 does not resolve",
            )],
            kernel_messages: vec![],
            exit: ExitStatus::LintFailure,
            documents_linted: 1,
            rules_active: 6,
        };
        let json = render_json_outcome(&outcome);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["hint"], crate::reporter::LINT_HINT);
    }

    #[test]
    fn claude_stop_blocks_on_error_diagnostic() {
        // ADR-062 § STOP-001: a failing run yields a block decision whose
        // reason carries the failing rule, location, and the fix trailer.
        let outcome = LintOutcome {
            diagnostics: vec![Diagnostic::error(
                "core.dep-resolved",
                "adrs/099-broken-demo.md",
                5,
                0,
                "PRD-999 does not resolve",
            )],
            kernel_messages: vec![],
            exit: ExitStatus::LintFailure,
            documents_linted: 1,
            rules_active: 6,
        };
        let json = render_claude_stop(&outcome).expect("error run blocks");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["decision"], "block");
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("Verification failed:"));
        assert!(reason.contains("core.dep-resolved"));
        assert!(reason.contains("099-broken-demo.md"));
        assert!(reason.contains("Fix before completing."));
    }

    #[test]
    fn claude_stop_blocks_on_error_kernel_message() {
        // A source runtime error is an error-severity kernel message, not
        // a per-document diagnostic — it must still block (exit == 1).
        let outcome = LintOutcome {
            diagnostics: vec![],
            kernel_messages: vec![KernelMessage::error(
                "src.runtime-error",
                "source 'jira' timed out",
            )],
            exit: ExitStatus::LintFailure,
            documents_linted: 0,
            rules_active: 0,
        };
        let json = render_claude_stop(&outcome).expect("kernel error blocks");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["decision"], "block");
        assert!(parsed["reason"].as_str().unwrap().contains("src.runtime-error"));
    }

    #[test]
    fn claude_stop_allows_clean_run() {
        // ADR-062 § STOP-001: a clean run emits nothing (None) — the agent
        // stops freely.
        let outcome = LintOutcome {
            diagnostics: vec![],
            kernel_messages: vec![],
            exit: ExitStatus::Ok,
            documents_linted: 7,
            rules_active: 6,
        };
        assert_eq!(render_claude_stop(&outcome), None);
    }

    #[test]
    fn claude_stop_allows_warnings_only_run() {
        // Warnings never block (they keep exit == Ok), so the gate stays
        // silent even though diagnostics are present — mirrors the
        // exit-code contract where a warning never escalates past 0.
        let outcome = LintOutcome {
            diagnostics: vec![Diagnostic::warning(
                "core.cross-ref",
                "adrs/058-scaffolding.md",
                12,
                0,
                "ADR-999 does not resolve",
            )],
            kernel_messages: vec![],
            exit: ExitStatus::Ok,
            documents_linted: 1,
            rules_active: 6,
        };
        assert_eq!(render_claude_stop(&outcome), None);
    }

    #[test]
    fn claude_stop_block_wraps_a_kernel_error_body() {
        // The binary's Err path (no LintOutcome) builds a body and calls
        // claude_stop_block directly; the shape must match the outcome path.
        let json = claude_stop_block("cfg.missing: no ctxgrd.toml found\n");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["decision"], "block");
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.starts_with("Verification failed:\n"));
        assert!(reason.contains("cfg.missing"));
        assert!(reason.ends_with("Fix before completing.\n"));
    }

    #[test]
    fn json_outcome_ok_run_has_empty_arrays_and_zero_exit() {
        let outcome = LintOutcome {
            diagnostics: vec![],
            kernel_messages: vec![],
            exit: ExitStatus::Ok,
            documents_linted: 0,
            rules_active: 0,
        };
        let json = render_json_outcome(&outcome);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["exit_code"], 0);
        assert!(parsed["diagnostics"].as_array().unwrap().is_empty());
        // ADR-038 § HINT-002: the field is absent on a clean run, not
        // null — skip_serializing_if keeps the v1 shape for OK runs.
        assert!(parsed.get("hint").is_none(), "no hint on a clean run");
    }

    #[test]
    fn discover_config_roots_finds_every_config_dir_sorted() {
        // `--recursive` discovery: each directory holding a ctxgrd.toml
        // is its own project root, returned sorted and de-duplicated.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for sub in ["", "services/billing", "services/auth"] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("ctxgrd.toml"), "[ADR]\npaths = [\"docs/**\"]\n").unwrap();
        }
        // A directory with no config must not appear.
        std::fs::create_dir_all(root.join("services/web")).unwrap();

        let found = discover_config_roots(root);
        assert_eq!(
            found,
            vec![
                root.to_path_buf(),
                root.join("services/auth"),
                root.join("services/billing"),
            ]
        );
    }

    #[test]
    fn discover_config_roots_skips_gitignored_dirs() {
        // Discovery uses the `ignore` crate, so a config under a
        // .gitignore'd build/vendor directory is not treated as a
        // project root — only real source projects are linted.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // The `ignore` crate only applies .gitignore inside a git repo
        // (require_git defaults true) — same as the reference scanner.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("ctxgrd.toml"), "[ADR]\npaths = [\"docs/**\"]\n").unwrap();
        std::fs::write(root.join(".gitignore"), "vendored/\n").unwrap();
        let vendored = root.join("vendored/dep");
        std::fs::create_dir_all(&vendored).unwrap();
        std::fs::write(vendored.join("ctxgrd.toml"), "[ADR]\npaths = [\"d/**\"]\n").unwrap();

        let found = discover_config_roots(root);
        assert_eq!(found, vec![root.to_path_buf()]);
    }
}
