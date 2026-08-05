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

/// The raw `lint --pack <name> --namespace <NS>` selectors, straight from
/// argv (ADR-080 § AVS-001). Both are repeatable and comma-separable;
/// values within one flag union, the two flags intersect. Empty on both
/// sides means "the whole resolved config" — lint's behaviour to date.
#[derive(Debug, Clone, Default)]
pub struct ScopeSelector {
    pub packs: Vec<String>,
    pub namespaces: Vec<String>,
}

impl ScopeSelector {
    pub fn is_empty(&self) -> bool {
        self.packs.is_empty() && self.namespaces.is_empty()
    }
}

/// A [`ScopeSelector`] resolved against one project's config: the set of
/// namespace names a run is restricted to, or `None` for an unscoped run.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    names: Option<BTreeSet<String>>,
}

impl Scope {
    /// The unscoped run — every namespace the config resolves.
    pub fn all() -> Self {
        Self { names: None }
    }

    /// Whether a scope filter is in force at all. Kept explicit so the
    /// unscoped path stays byte-identical to pre-ADR-080 behaviour.
    pub fn is_scoped(&self) -> bool {
        self.names.is_some()
    }

    /// Whether `namespace` is in scope. Always true when unscoped.
    pub fn allows(&self, namespace: &str) -> bool {
        match &self.names {
            Some(names) => names.contains(namespace),
            None => true,
        }
    }
}

/// Resolve `selector` against a loaded config plus the project's own
/// `ctxgrd.toml` text (ADR-080 § AVS-001/AVS-004).
///
/// `--namespace` resolves directly against the resolved namespace names;
/// `--pack` resolves through the ADR-053 provenance stamps in the
/// project's config, never through the built-in pack definition. A value
/// matching nothing, or an empty intersection, is a config-class error
/// (exit 2) rather than a run that quietly lints nothing.
fn resolve_scope(root: &Path, config: &Config, selector: &ScopeSelector) -> Result<Scope, LintError> {
    if selector.is_empty() {
        return Ok(Scope::all());
    }
    let declared: BTreeSet<&str> = config.namespaces.keys().map(String::as_str).collect();

    // A namespace the config never declares can still be governed — an
    // id-claimed document pulls it in and it lints under the zero-config
    // six. `ctxgrd rules` reports exactly those (BUG-049), so rejecting
    // them here made the obvious follow-up fail: enumerate namespaces from
    // `rules --format json`, then `lint --namespace <each>`, and every
    // zero-config row exits 2.
    //
    // Resolved lazily. The scan costs a full markdown walk, so it runs only
    // on the branch that would otherwise have errored — a scope naming
    // declared namespaces (the normal case) never pays for it.
    let mut governed: Option<BTreeSet<String>> = None;
    let mut from_namespaces: BTreeSet<String> = BTreeSet::new();
    for name in &selector.namespaces {
        if !declared.contains(name.as_str()) {
            let claimed = match &governed {
                Some(g) => g,
                None => governed.insert(governed_namespaces(root, config)?),
            };
            if !claimed.contains(name) {
                return Err(LintError::ScopeUnknown {
                    flag: "namespace",
                    value: name.clone(),
                });
            }
        }
        from_namespaces.insert(name.clone());
    }

    let mut from_packs: BTreeSet<String> = BTreeSet::new();
    if !selector.packs.is_empty() {
        let toml = std::fs::read_to_string(root.join("ctxgrd.toml")).unwrap_or_default();
        for name in &selector.packs {
            // Only namespaces the resolved config still declares count —
            // a stamped block that was later deleted names nothing.
            let matched: BTreeSet<String> = crate::pack::stamped_namespaces(root, name, &toml)
                .into_iter()
                .filter(|ns| declared.contains(ns.as_str()))
                .collect();
            if matched.is_empty() {
                return Err(LintError::ScopeUnknown {
                    flag: "pack",
                    value: name.clone(),
                });
            }
            from_packs.extend(matched);
        }
    }

    let names = match (selector.namespaces.is_empty(), selector.packs.is_empty()) {
        (false, true) => from_namespaces,
        (true, false) => from_packs,
        // Both given: intersect (AVS-001).
        _ => from_namespaces
            .intersection(&from_packs)
            .cloned()
            .collect::<BTreeSet<String>>(),
    };
    if names.is_empty() {
        return Err(LintError::ScopeEmpty);
    }
    Ok(Scope { names: Some(names) })
}

/// Exit code bucket. RUN-001 in the brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitStatus {
    #[default]
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
#[derive(Default)]
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
    /// How many namespaces documents claim by id that the config never
    /// declares (ADR-076 § OWN-005). Always `0` in zero-config mode and
    /// under a `--pack`/`--namespace` scope. Reported on the human `ok:`
    /// line only when nonzero, and always in `--format json`, so an agent
    /// can branch on it without having to distinguish "none" from "this
    /// ctxgrd is too old to know".
    pub namespaces_undeclared: usize,
    /// The root this run actually resolved (BUG-048 follow-up).
    ///
    /// Every diagnostic `file` is relative to this, and since the upward
    /// search made the root an *ancestor* of the working directory, a
    /// reader standing in a subdirectory can no longer assume the two are
    /// the same — `docs/adrs/001.md` printed from inside `docs/adrs/` does
    /// not resolve from there. Carrying the root is what makes those paths
    /// resolvable again, for an agent reading JSON as much as for a human.
    pub root: PathBuf,
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
    /// A `--pack` / `--namespace` scope value matched nothing in the
    /// resolved config (ADR-080 § AVS-004). A config-class error, not an
    /// empty run: scoping to nothing would report a false clean.
    #[error("--{flag} {value} matches nothing in the resolved config")]
    ScopeUnknown { flag: &'static str, value: String },
    /// `--pack` and `--namespace` were both given and their intersection
    /// is empty — the run would lint nothing (ADR-080 § AVS-001).
    #[error("--pack and --namespace select no namespace in common")]
    ScopeEmpty,
}

impl LintError {
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Config(c) => c.code(),
            Self::MarkdownScan(_) => Some("src.markdown-io"),
            Self::NothingToLint => Some("cfg.missing"),
            Self::TempDir(_) => Some("ext.tempdir"),
            Self::ExternalRule { .. } => Some("ext.io"),
            Self::ScopeUnknown { flag: "pack", .. } => Some("cli.unknown-pack"),
            Self::ScopeUnknown { .. } => Some("cli.unknown-namespace"),
            Self::ScopeEmpty => Some("cli.empty-scope"),
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
            Self::ScopeUnknown { flag, value } => Diagnostic::error(
                if *flag == "pack" {
                    "cli.unknown-pack"
                } else {
                    "cli.unknown-namespace"
                },
                "ctxgrd.toml",
                0,
                0,
                format!("--{flag} '{value}' matches nothing in the resolved config"),
            )
            .with_help(if *flag == "pack" {
                "`--pack` selects the namespaces stamped `# pack: <name>` in this \
                 project's ctxgrd.toml — run `ctxgrd pack add <name>` to adopt it, \
                 or select a hand-written block with `--namespace <NS>`"
            } else {
                "run `ctxgrd list` to see the namespaces this config declares"
            }),
            Self::ScopeEmpty => Diagnostic::error(
                "cli.empty-scope",
                "ctxgrd.toml",
                0,
                0,
                "--pack and --namespace select no namespace in common",
            )
            .with_help(
                "the two scope flags intersect — drop one, or name a namespace the \
                 pack actually stamped",
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
        C::IgnoreInvalid { detail } => {
            Diagnostic::error("cfg.ignore-invalid", "ctxgrd.toml", 0, 0, detail.clone()).with_help(
                "expected `[ignore]\\nnamespaces = [\"REPORT\"]` — namespace names, \
                 not globs (globs go in `patterns`)",
            )
        }
        C::RolesInvalid { detail } => {
            Diagnostic::error("cfg.roles-invalid", "ctxgrd.toml", 0, 0, detail.clone()).with_help(
                "expected `[roles]\\nallowed = [\"developer\", \"writer\"]`, and \
                 `owner = \"<role>\"` inside each namespace block",
            )
        }
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
        // ADR-118 § STG-002. The help text carries the migration because
        // this is the only place a project on an older config meets the
        // change — an author who deletes the block without being told where
        // ordering went will assume it stopped being enforceable.
        C::PipelineRemoved => Diagnostic::error(
            "cfg.pipeline-removed",
            "ctxgrd.toml",
            0,
            0,
            "[pipeline] was removed in ADR-118 — namespace stages and gates no longer exist"
                .to_string(),
        )
        .with_help(
            "delete the [pipeline] block. Readiness now comes from each document's \
             depends_on; for namespace-level edge constraints use \
             [<NS>.\"core.dep-shape\"] requires/allows, which lint enforces per document",
        ),
        C::ChangelogInvalid { detail } => Diagnostic::error(
            "cfg.changelog-invalid",
            "ctxgrd.toml",
            0,
            0,
            format!("[changelog] {detail}"),
        )
        .with_help(
            "expected `[changelog]\\nnamespaces = [\"BUG\"]` with a `[changelog.BUG]` \
             table declaring `when` (terminal status) and `section` (Keep-a-Changelog category)",
        ),
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
    /// The resolved ADR-080 scope. `config.namespaces` has already been
    /// narrowed to it; the rule loops consult it for id-claimed documents,
    /// which reach the corpus without a path claim.
    pub(crate) scope: Scope,
}

/// Source-aggregation pipeline shared by [`lint`] and
/// [`find_references`]. See [`IngestResult`] for what each field
/// carries. This function is the single home of the ADR-005 §
/// SRC-002 invariant — "all sources MUST complete before any rule
/// runs" — so any future change to source ordering or envelope
/// translation lives here, not in two places.
pub(crate) fn ingest(root: &Path) -> Result<IngestResult, LintError> {
    ingest_scoped(root, &ScopeSelector::default())
}

/// [`ingest`], restricted to an ADR-080 scope. Narrowing `config.namespaces`
/// here — before [`crate::path_claims::PathClaims`] is built — is what makes
/// out-of-scope path-claimed files *unclaimed* rather than errored, preserving
/// DOC-001 first-touch silence (ADR-007).
///
/// The document corpus itself is deliberately NOT narrowed: id-claimed
/// documents stay in `documents` so the shared dependency graph is still
/// whole. Scoping the graph would make every cross-namespace `depends_on`
/// edge report a phantom `core.dep-resolved` failure. The scope is applied
/// to *rule execution and diagnostics* in [`lint_run`] instead.
pub(crate) fn ingest_scoped(
    root: &Path,
    selector: &ScopeSelector,
) -> Result<IngestResult, LintError> {
    let mut config = config::load(root)?;
    let scope = resolve_scope(root, &config, selector)?;
    if scope.is_scoped() {
        config.namespaces.retain(|name, _| scope.allows(name));
    }
    // Drain config-load advisories so they ride the same channel as
    // every other kernel-level message. `config` is local; the take()
    // leaves it in a clean state for downstream rule scheduling.
    let mut kernel_messages = std::mem::take(&mut config.kernel_messages);

    let discovered_sources = source_ext::discover_sources(root);
    let source_run = source_ext::run_activated_sources(
        root,
        &discovered_sources,
        &config.sources,
        &config.source_expect_min,
    );
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

    // ADR-119 § CLM-003: a namespace whose `paths` matched files on disk
    // that the walker skipped, and which ingested no markdown at all.
    // The `help:` line is most of the value — this is the one point in the
    // product where a user meets external sources at the moment they need
    // them. A glob matching *nothing* is deliberately absent from the
    // tally: that is indistinguishable from an unpopulated directory, and
    // warning on it would erode ADR-007 § DOC-001 (CLM-001).
    for (namespace, by_ext) in &scan.skipped_by_extension {
        let total: usize = by_ext.values().sum();
        let extensions = by_ext
            .keys()
            .map(|e| format!(".{e}"))
            .collect::<Vec<_>>()
            .join(", ");
        kernel_messages.push(
            KernelMessage::warning(
                "cfg.paths-skipped",
                format!(
                    "[{namespace}] paths match {} the walker skipped, and the \
                     namespace ingested no documents",
                    crate::reporter::plural(total, "file")
                ),
            )
            .with_note(format!(
                "only .md files are ingested; the skipped files had: {extensions}"
            ))
            .with_help(
                "to lint non-markdown facts, emit them from a source \
                 (`ctxgrd docs sources`) — a namespace cannot claim them directly",
            ),
        );
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
        scope,
    })
}

/// The full result of a lint run.
///
/// This carried the resolved [`Config`] and [`Document`] set alongside the
/// outcome so `ctxgrd status` could join per-document lint-cleanliness onto
/// its stage gates. ADR-118 removed the gates, and with them the only
/// caller — `status` now runs [`ingest`] and never executes a rule. The
/// extra fields went with that caller rather than being kept warm behind an
/// `allow(dead_code)`; [`ingest`] is the seam for anything that needs the
/// document set without the diagnostics.
pub(crate) struct LintRun {
    pub(crate) outcome: LintOutcome,
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

/// The namespaces a lint of `root` would dispatch rules for: every
/// namespace `config` declares, plus every namespace the markdown on
/// disk claims by id but the config never mentions (BUG-049).
///
/// The undeclared half is the part that matters. `lint` derives its
/// namespace set from the *documents* and resolves each through
/// [`Config::namespace_config`], which falls back to
/// [`crate::config::ZERO_CONFIG_RULES`] — so an id-claimed namespace no
/// config declares is really linted with six rules. Introspection that
/// reads `config.namespaces` alone cannot see those, and reports an
/// empty set for a tree `lint` is actively governing.
///
/// The two sets are deliberately not identical. `lint` counts only
/// namespaces that have documents; this also returns a namespace the
/// config declares but nothing populates yet, because "what am I held
/// to" must stay answerable in a project that has written its config
/// before its first document. The containment direction is the
/// invariant: everything `lint` dispatches appears here.
///
/// **External sources are not run.** [`ingest`] executes
/// `sources/<name>/run` as a subprocess; introspection must not, so
/// this walks markdown only. A namespace that exists solely in an
/// external source's envelopes is therefore absent — the one gap, and
/// the right trade against giving `ctxgrd rules` side effects.
/// A scan failure is propagated rather than swallowed: falling back to
/// the config's own namespaces on error would silently reproduce the
/// under-reporting this function exists to fix, and do it in exactly
/// the case — an unreadable tree — where the caller most needs to know.
pub fn governed_namespaces(root: &Path, config: &Config) -> Result<BTreeSet<String>, LintError> {
    let mut namespaces: BTreeSet<String> = config.namespaces.keys().cloned().collect();
    let path_claims = crate::path_claims::PathClaims::from_config(config);
    let scan = markdown::scan(root, config.ignore.as_ref(), Some(&path_claims))
        .map_err(LintError::MarkdownScan)?;
    namespaces.extend(scan.documents.into_iter().map(|d| d.id.namespace));
    Ok(namespaces)
}

pub fn lint(root: &Path) -> Result<LintOutcome, LintError> {
    lint_run(root).map(|r| r.outcome)
}

/// [`lint`], restricted to the ADR-080 `--pack` / `--namespace` scope.
/// An empty selector is exactly [`lint`].
pub fn lint_scoped(root: &Path, selector: &ScopeSelector) -> Result<LintOutcome, LintError> {
    lint_run_scoped(root, selector).map(|r| r.outcome)
}

pub(crate) fn lint_run(root: &Path) -> Result<LintRun, LintError> {
    lint_run_scoped(root, &ScopeSelector::default())
}

pub(crate) fn lint_run_scoped(
    root: &Path,
    selector: &ScopeSelector,
) -> Result<LintRun, LintError> {
    // Steps 1–4: load config, run sources, walk markdown, translate
    // envelopes. See `ingest()` for the SRC-002 invariant.
    let IngestResult {
        config,
        documents,
        parse_diagnostics,
        mut kernel_messages,
        path_claims,
        scope,
    } = ingest_scoped(root, selector)?;

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
        // ADR-080 § AVS-001: under a scope, a file only reaches the report
        // if an in-scope namespace claims it. `path_claims` was built from
        // the already-narrowed config, so this is the whole test — an
        // out-of-scope file is skipped, never errored.
        .filter(|pd| {
            !scope.is_scoped() || path_claims.matching_namespaces(&pd.location).next().is_some()
        })
        .filter(|pd| {
            // An unreadable file is never suppressed: the file-level pass
            // cannot lint what it cannot read, so this parse diagnostic
            // is the only signal the record exists at all (BUG-024).
            matches!(pd.kind, ParseDiagnosticKind::Unreadable { .. })
                || file_level_ns.is_empty()
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
    // ADR-106 § DPS-001: the graph's first state assertion. Takes `config`
    // because its `terminal` / `severity` params resolve from the source
    // document's namespace while the edge is in hand (DPS-003) — the
    // per-namespace `retain` below can only filter by location afterwards.
    aggregate.extend(rules::dep_status(&dep_graph, &config));
    let declared_namespaces: BTreeSet<&str> =
        config.namespaces.keys().map(String::as_str).collect();
    // ADR-001 § REF-002: scan non-markdown files for pointer mentions
    // when `[references].scan` is configured. Empty globs short-circuit
    // (current behaviour). The scanner side of `core.cross-ref` is
    // corpus-gated: it runs only when at least one namespace enables the
    // rule (zero-config namespaces enable it by default), so disabling
    // `core.cross-ref` everywhere silences code-file hits too — scanner
    // diagnostics anchor in non-document files, which the per-namespace
    // retain filter below cannot see (BUG-025).
    let scanner_cross_ref_enabled = config.namespaces.is_empty()
        || config
            .namespaces
            .values()
            .any(|ns| ns.enables("core.cross-ref"));
    let references = if config.reference_scan_globs.is_empty() || !scanner_cross_ref_enabled {
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
            // Diagnostics anchored outside the document corpus (reference-
            // scanner hits in source files) belong to no namespace, so a
            // scoped run — which reports only its own slice — drops them.
            return !scope.is_scoped();
        };
        scope.allows(ns) && config.namespace_config(ns).enables(&d.code)
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
        if !scope.allows(&doc.id.namespace) {
            continue;
        }
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
            // ADR-109 § BDG-003: the id-keyed half of `core.file-budget`'s
            // dual dispatch. Registered `Level::File` for path-claimed
            // singletons, it needs this arm to reach id-keyed documents —
            // and, like `core.successor-link`, must run on its documented
            // default when the namespace lists it without a params block.
            if code == "core.file-budget" {
                diagnostics.extend(agent_guide::check_file_budget(
                    doc,
                    ns_cfg.params.get(code),
                    root,
                ));
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

    // 6b'': the ADR-076 coverage gates (`cfg.namespace-undeclared`,
    // `cfg.namespace-unowned`). Extended straight onto `diagnostics`, never
    // through `aggregate`: that vector is retained against
    // `namespace_config(ns).enables(code)`, which would drop an always-on
    // `cfg.*` code — no namespace lists it, by design. Runs here for the
    // same reason `core.min-docs` does: it needs both presence corpora,
    // id-keyed documents and the file-level singletons.
    //
    // Skipped entirely under an ADR-080 scope (OWN-004): coverage is a
    // property of the whole config, but `config.namespaces` has been
    // narrowed to the slice — every namespace outside it would read as
    // undeclared, and a whole-config finding anchored at `ctxgrd.toml`
    // is not part of any one namespace's slice (AVS-001).
    let coverage = if scope.is_scoped() {
        crate::coverage::Coverage::none()
    } else {
        crate::coverage::check(&config, &documents, &file_level_scan.namespaces, root)
    };
    diagnostics.extend(coverage.diagnostics);

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

    // BUG-030/BUG-031: the conditional-link rules
    // (`builtin_rules::RESOLUTION_AWARE_RULES`) decide whether a document
    // cites evidence, and until now they tested only that a token of the
    // right namespace was *present* — never that it resolved, and never
    // excluding the document's own id. Resolution is a whole-corpus fact a
    // per-document rule cannot see, so it rides the same synthesized-param
    // channel `core.dep-shape` uses for `managed`.
    //
    // Each document is passed only its *own* resolving references, not the
    // corpus index, so the payload stays proportional to the document.
    // Keyed by `location`, which is unique per document even when two share
    // an id (that collision is `core.id-unique`'s to report).
    let known_ids: BTreeSet<crate::id::DocumentId> =
        documents.iter().map(|d| d.id.clone()).collect();
    let resolved_refs: BTreeMap<&str, Value> = documents
        .iter()
        .map(|d| {
            let refs = agent_guide::resolved_refs(d, &known_ids);
            (
                d.location.as_str(),
                Value::Array(refs.into_iter().map(Value::String).collect()),
            )
        })
        .collect();
    let no_refs = Value::Array(Vec::new());

    for doc in &documents {
        if !scope.allows(&doc.id.namespace) {
            continue;
        }
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
                } else if crate::builtin_rules::RESOLUTION_AWARE_RULES.contains(&code.as_str()) {
                    // Merged unconditionally — a namespace may list one of
                    // these rules with no `[NS."rule"]` block at all, and
                    // `ns_cfg.params.get(code)` would then be `None`,
                    // leaving the rule to fail closed on a tree that is
                    // actually fine.
                    let mut merged = ns_cfg
                        .params
                        .get(code)
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Default::default()));
                    if let Value::Object(map) = &mut merged {
                        map.insert(
                            agent_guide::RESOLVED_REFS_PARAM.to_string(),
                            resolved_refs
                                .get(doc.location.as_str())
                                .unwrap_or(&no_refs)
                                .clone(),
                        );
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
        for doc in documents.iter().filter(|d| scope.allows(&d.id.namespace)) {
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
    let scoped_documents = documents
        .iter()
        .filter(|d| scope.allows(&d.id.namespace))
        .count();
    let documents_linted = scoped_documents + file_level_scan.files_linted;
    let mut namespaces: BTreeSet<&str> = documents
        .iter()
        .filter(|d| scope.allows(&d.id.namespace))
        .map(|d| d.id.namespace.as_str())
        .collect();
    namespaces.extend(file_level_scan.namespaces.iter().map(String::as_str));
    let rules_active: usize = namespaces
        .iter()
        .map(|ns| config.namespace_config(ns).rules.len())
        .sum();

    // BUG-048: a run that found no `ctxgrd.toml` anywhere and linted
    // documents anyway must say so. The disclosure used to be exactly
    // inverted — ctxgrd spoke up in an *empty* directory ("run `ctxgrd
    // init`…") and went quiet in one containing markdown, because the
    // condition it fired on was "nothing to lint" rather than "no config
    // found". The warning appeared when it was least needed and vanished
    // when it was most.
    //
    // Warning, not error: it keeps `exit == Ok`, so a project that
    // deliberately runs zero-config is not broken by the upgrade. What it
    // does change is the summary — the reporter renders `found:` instead
    // of `ok:` once any message exists — so a reduced run no longer reads
    // identically to a fully configured clean one.
    //
    // Gated on a namespace actually falling back, not merely on the file
    // being absent: a global `~/.ctxgrd/namespaces/<NS>.toml` can supply
    // real config with no local file, and calling that run "zero-config"
    // would be its own false statement.
    let fell_back: Vec<&str> = namespaces
        .iter()
        .copied()
        .filter(|ns| !config.namespaces.contains_key(*ns))
        .collect();
    if !root.join("ctxgrd.toml").exists() && !fell_back.is_empty() {
        kernel_messages.push(
            KernelMessage::warning(
                "cfg.zero-config",
                // States only what is verifiable from here. An earlier
                // wording said "or any parent", which is a lie whenever
                // `--root` was passed explicitly — no parent is searched in
                // that case, and this warning exists precisely because
                // ctxgrd must not misreport what it inspected (BUG-048).
                // The lib cannot see how the root was chosen without
                // plumbing a flag through the public `lint` signature; the
                // note below states the rule generically instead.
                format!(
                    "no ctxgrd.toml at {} — linted {} across {} under the {} zero-config core rules",
                    root.display(),
                    crate::reporter::plural(documents_linted, "document"),
                    plural_namespaces(&fell_back),
                    crate::config::ZERO_CONFIG_RULES.len(),
                ),
            )
            .with_help(
                "run `ctxgrd init` to create a config here, or pass `--root <dir>` to point at an existing project",
            )
            .with_note(format!(
                "without `--root`, ctxgrd searches parent directories for ctxgrd.toml; with it, only that directory. \
                 the zero-config set is {} — no required-headings, required-metadata, allowed-values, or min-docs",
                crate::config::ZERO_CONFIG_RULES.join(", ")
            )),
        );
    }

    Ok(LintRun {
        outcome: LintOutcome {
            diagnostics,
            kernel_messages,
            exit,
            documents_linted,
            rules_active,
            namespaces_undeclared: coverage.namespaces_undeclared,
            root: root.to_path_buf(),
        },
    })
}

/// `2 namespaces (ADR, SPEC)` / `1 namespace (ADR)` — the namespace list
/// is named rather than counted so the message says which documents were
/// checked under the reduced set.
///
/// The document count rides this message rather than the summary line
/// because a warning flips the trailer from `ok: N documents · M rules`
/// to `found:`, which carries diagnostic counts only. Without it the
/// disclosure fix would cost the corpus size it exists to disclose —
/// BUG-039's complaint, which this must not deepen.
fn plural_namespaces(names: &[&str]) -> String {
    let noun = if names.len() == 1 {
        "namespace"
    } else {
        "namespaces"
    };
    format!("{} {noun} ({})", names.len(), names.join(", "))
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

    // Hit kind 2: depends_on edges. Parsed comparison, not raw string —
    // `depends_on: ["ADR-07"]` must be found by `refs ADR-7` and vice
    // versa, matching the (namespace, number) semantics of every other
    // consumer of these edges (BUG-022).
    for doc in &documents {
        if !doc.depends_on.iter().any(|d| {
            d.parse::<crate::id::DocumentId>()
                .is_ok_and(|id| id.namespace == target.namespace && id.number == target.number)
        }) {
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
                // Parsed comparison, not raw string — a `// see ADR-07`
                // mention must be found by `refs ADR-7` and vice versa
                // (BUG-022). Unparseable tokens cannot match any target.
                let matches_target = r
                    .token
                    .parse::<crate::id::DocumentId>()
                    .is_ok_and(|id| id.namespace == target.namespace && id.number == target.number);
                if !matches_target {
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
    // The ADR-086 canonical diagnostic shape (WIRE-003/004): the path is
    // carried under `file`. For one transition release `location` is also
    // emitted with the same value so downstream readers of the old key do
    // not break; a later phase drops the alias. `line`/`col` always
    // serialise (as `null` when unknown) — no `0` sentinel.
    #[derive(serde::Serialize)]
    struct DiagWire<'a> {
        code: &'a str,
        severity: crate::diagnostic::Severity,
        message: &'a str,
        file: &'a str,
        location: &'a str,
        line: Option<u32>,
        col: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        span_len: Option<u32>,
    }

    // ADR-086 § WIRE-005: fixed-key counts over the `diagnostics` array.
    // `infos` is always present (0 when no info-severity finding fired).
    // `namespaces_undeclared` is ctxgrd's ADR-076 § OWN-005 extension —
    // additive, and always present for the same reason `infos` is.
    #[derive(serde::Serialize)]
    struct Summary {
        errors: usize,
        warnings: usize,
        infos: usize,
        files: usize,
        namespaces_undeclared: usize,
    }

    #[derive(serde::Serialize)]
    struct Wire<'a> {
        exit_code: u8,
        // The root every `file` below is relative to (BUG-048 follow-up).
        // Since the upward search made the root an ancestor of the working
        // directory, a consumer cannot assume cwd == root and needs this to
        // resolve a diagnostic to a file on disk. Additive extension field;
        // grd-output.schema.json sets additionalProperties: true.
        root: String,
        summary: Summary,
        diagnostics: Vec<DiagWire<'a>>,
        kernel_messages: &'a [KernelMessage],
        // ADR-038 § HINT-002/003: the "fix the documents, not the
        // config" advisory, present only when rule diagnostics were
        // emitted. Additive optional field — absent on a clean run.
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<&'static str>,
    }

    let (mut errors, mut warnings, mut infos) = (0usize, 0usize, 0usize);
    for d in &outcome.diagnostics {
        match d.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => infos += 1,
        }
    }

    let diagnostics: Vec<DiagWire<'_>> = outcome
        .diagnostics
        .iter()
        .map(|d| DiagWire {
            code: &d.code,
            severity: d.severity,
            message: &d.message,
            file: &d.location,
            location: &d.location,
            line: d.line,
            col: d.col,
            help: d.help.as_deref(),
            note: d.note.as_deref(),
            span_len: d.span_len,
        })
        .collect();

    let wire = Wire {
        exit_code: outcome.exit.code(),
        root: outcome.root.display().to_string(),
        summary: Summary {
            errors,
            warnings,
            infos,
            files: outcome.documents_linted,
            namespaces_undeclared: outcome.namespaces_undeclared,
        },
        diagnostics,
        kernel_messages: &outcome.kernel_messages,
        // ADR-038 § HINT-004: the "fix the documents, not `ctxgrd.toml`" nudge
        // must not fire for a config error, where the fault *is* the
        // configuration and the advice is therefore backwards. The rich
        // renderer already honoured this; the JSON path did not, because it
        // gated on the array being non-empty rather than on the verdict. Only
        // reachable once the exit-2 path emits an object at all, so it was a
        // latent divergence until now, not a live one.
        hint: (!outcome.diagnostics.is_empty()
            && !matches!(outcome.exit, ExitStatus::KernelError))
        .then_some(crate::reporter::LINT_HINT),
    };
    serde_json::to_string_pretty(&wire).unwrap_or_else(|_| "{}".to_string())
}

/// The exit-2 wire object (ADR-086 § WIRE-005).
///
/// `exit_code` lives *inside* the object, so an error path that writes nothing
/// to stdout makes the documented value `2` unreachable by construction: a
/// `--format json` consumer gets an empty stream and cannot tell a broken
/// config from a binary that never ran. The human error block still goes to
/// stderr (WIRE-007); this is what stdout owes a machine caller.
///
/// Deliberately built as a [`LintOutcome`] and passed through
/// [`render_json_outcome`] rather than assembling a second struct: the error
/// object is then *the same wire shape by construction*, and a later change to
/// the envelope cannot reshape one path while leaving the other behind. The
/// counts are honest on their own — `files` is 0 because nothing was linted.
pub fn render_error_json(d: &Diagnostic, root: &Path) -> String {
    render_json_outcome(&LintOutcome {
        exit: ExitStatus::KernelError,
        diagnostics: vec![d.clone()],
        root: root.to_path_buf(),
        ..Default::default()
    })
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
            namespaces_undeclared: 0,
            root: PathBuf::new(),
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
            namespaces_undeclared: 0,
            root: PathBuf::new(),
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
            namespaces_undeclared: 0,
            root: PathBuf::new(),
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
            namespaces_undeclared: 0,
            root: PathBuf::new(),
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
            namespaces_undeclared: 0,
            root: PathBuf::new(),
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
            namespaces_undeclared: 0,
            root: PathBuf::new(),
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
            namespaces_undeclared: 0,
            root: PathBuf::new(),
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

    /// BUG-022 fixture: ADR-07 authored zero-padded; ADR-8 points at it
    /// three ways — `depends_on: ["ADR-07"]`, a body mention, and a
    /// scanner hit in a non-markdown file.
    fn refs_padding_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("docs/adrs")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("ctxgrd.toml"),
            "[ADR]\npaths = [\"docs/adrs/**\"]\nrules = [\"core.frontmatter\", \"core.id\", \"core.dep-resolved\"]\n\n[references]\nscan = [\"src/**/*.rs\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("docs/adrs/007-first.md"),
            "---\nid: ADR-07\ntitle: First\n---\n## Context\n",
        )
        .unwrap();
        std::fs::write(
            root.join("docs/adrs/008-second.md"),
            "---\nid: ADR-8\ntitle: Second\ndepends_on: [\"ADR-07\"]\n---\n## Context\nSee ADR-07 for background.\n",
        )
        .unwrap();
        std::fs::write(root.join("src/main.rs"), "// see ADR-07\n").unwrap();
        tmp
    }

    #[test]
    fn refs_finds_padded_depends_on_and_scanner_hits_via_collapsed_query() {
        // BUG-022: hit kinds 2 (depends_on) and 4 (scanner) compared raw
        // strings against the typed query, so `refs ADR-7` missed edges
        // authored `ADR-07`. All four kinds match on (namespace, number).
        let tmp = refs_padding_fixture();
        let hits = find_references(tmp.path(), "ADR-7").unwrap();
        let kinds: Vec<u8> = hits.iter().map(|h| kind_ordinal(&h.kind)).collect();
        assert!(
            hits.iter()
                .any(|h| matches!(h.kind, ReferenceHitKind::DependsOn { .. })),
            "depends_on [\"ADR-07\"] must be found via `refs ADR-7`; kinds: {kinds:?}"
        );
        assert!(
            hits.iter()
                .any(|h| matches!(h.kind, ReferenceHitKind::ScannerHit)),
            "scanner hit `ADR-07` must be found via `refs ADR-7`; kinds: {kinds:?}"
        );
        assert_eq!(hits.len(), 4, "self + depends_on + body + scanner: {hits:?}");
    }

    #[test]
    fn refs_finds_collapsed_spellings_via_padded_query() {
        // The reverse asymmetry: querying the zero-padded form must find
        // everything too, and both spellings return the same hit set.
        let tmp = refs_padding_fixture();
        let padded = find_references(tmp.path(), "ADR-07").unwrap();
        let collapsed = find_references(tmp.path(), "ADR-7").unwrap();
        assert_eq!(padded.len(), 4, "{padded:?}");
        assert_eq!(
            padded, collapsed,
            "the hit set must not depend on the query's zero-padding"
        );
    }
}
