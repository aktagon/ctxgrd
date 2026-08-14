//! `ctxgrd.toml` loader + validator. CFG-001 in the brief.
//!
//! The config shape is documented in the brief's "Contracts" section.
//! Each top-level table is either a namespace (arbitrary capitalised
//! key like `[ADR]`), or a reserved `[sources.<name>]`. Unknown
//! top-level keys are silently ignored — future-proofing against
//! additive kernel features.
//!
//! At CP3a we only validate `core.*` rule codes. Non-core codes are
//! assumed to be external rules that CP3b will discover on disk; if
//! they turn out to be missing then, the `cfg.rule-unknown` check
//! tightens at that layer.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use thiserror::Error;
use toml::Value as TomlValue;

use crate::diagnostic::KernelMessage;

pub(crate) const CORE_RULES: [&str; 13] = [
    "core.frontmatter",
    "core.id",
    "core.id-unique",
    "core.min-docs",
    "core.dep-resolved",
    "core.dep-cycle",
    "core.dep-status",
    "core.cross-ref",
    "core.required-headings",
    "core.required-metadata",
    "core.allowed-values",
    "core.requirement-ref",
    "core.successor-link",
];

/// Core rules whose zero-config default includes them — every
/// namespace without explicit config gets exactly this set. The kernel
/// brief calls these "the six non-parameterized core rules"; ADR-125
/// § LNK-008 added a seventh, so the count is no longer written down
/// anywhere but here. Derive it from `.len()`, never restate it.
pub const ZERO_CONFIG_RULES: [&str; 7] = [
    "core.frontmatter",
    "core.id",
    "core.id-unique",
    "core.dep-resolved",
    "core.dep-cycle",
    "core.cross-ref",
    // Warn-only by default (ADR-125 § LNK-008): it reports on a repo
    // that never opted in without changing that repo's exit code, which
    // is what makes a zero-config addition safe rather than a tightening.
    "core.link-resolved",
];

/// The envelope floor an activated source is held to when it does not
/// declare `expect_min` (ADR-119 § CLM-002).
///
/// One, not zero. A filesystem namespace with zero documents is often
/// legitimate — the directory is simply not populated yet — but a
/// `[sources.<name>]` table is an explicit statement that this source is
/// expected to produce something, and empty output there is nearly always
/// a bug. An opt-in floor would not reach the user whose source script
/// broke silently, which is the case this exists for.
pub(crate) const DEFAULT_SOURCE_EXPECT_MIN: u32 = 1;

const PARAMETERIZED_CORE_RULES: [&str; 3] = [
    "core.required-headings",
    "core.required-metadata",
    "core.allowed-values",
];

/// Whether `code` names a builtin-compiled rule (dispatched in-process,
/// not an external subprocess script). Derived from `BUILTIN_RULES`
/// so the resolver and the registry cannot drift (ADR-024 § REG-002).
pub(crate) fn is_builtin_compiled(code: &str) -> bool {
    crate::builtin_rules::BUILTIN_RULES
        .iter()
        .any(|r| r.code == code)
}

/// Whether `code` is a *file-level* builtin-compiled rule — one that lints
/// id-less path-claimed singletons rather than id-keyed documents.
/// Derived from `BUILTIN_RULES` (ADR-024 § REG-002).
pub(crate) fn is_file_level_compiled(code: &str) -> bool {
    crate::builtin_rules::BUILTIN_RULES
        .iter()
        .any(|r| r.code == code && r.level == crate::builtin_rules::Level::File)
}

/// The namespace part of a rule code (`agents` in `agents.context-budget`).
fn rule_prefix(code: &str) -> &str {
    code.split_once('.').map(|(ns, _)| ns).unwrap_or(code)
}

/// Whether `code` falls under a reserved built-in rule namespace — the
/// `<ns>` prefix of any `BUILTIN_RULES` entry (e.g. `agents`, `todo`).
/// Derived from the registry (ADR-024 § REG-002), so there is no second
/// list to keep in sync. External rules may not use a reserved prefix,
/// and an unknown rule under one is a missing built-in (a ctxgrd version
/// mismatch), not a missing script.
pub(crate) fn is_reserved_builtin_prefix(code: &str) -> bool {
    let ns = rule_prefix(code);
    crate::builtin_rules::BUILTIN_RULES
        .iter()
        .any(|r| rule_prefix(r.code) == ns)
}

impl Config {
    /// Namespaces that are *file-level*: their documents (CLAUDE.md /
    /// AGENTS.md / TODO.md) are id-less singletons that never become
    /// id-keyed [`Document`]s, so they are linted by walking path-claimed
    /// files directly (see [`crate::agent_guide::scan_file_level`])
    /// rather than through the per-document rule loop.
    ///
    /// A namespace qualifies only when it carries a file-level
    /// builtin-compiled rule AND does not run the id pipeline — `core.id`
    /// is the id-claim marker. The second clause matters since ADR-078
    /// made `core.required-headings` / `core.required-anchors` dual-use
    /// `Level::File` codes: an id-keyed namespace listing one of them
    /// must stay on the id pipeline, or its parse diagnostics are
    /// suppressed and its documents are linted twice (BUG-021).
    /// `core.frontmatter` deliberately does NOT disqualify: the persona
    /// pack lists it on genuinely file-level namespaces (DESIGN / STYLE /
    /// SOUL), whose singletons never carry ids.
    pub fn file_level_namespaces(&self) -> Vec<&str> {
        self.namespaces
            .iter()
            .filter(|(_, cfg)| {
                cfg.rules.iter().any(|c| is_file_level_compiled(c))
                    && !cfg.rules.iter().any(|c| c == "core.id")
            })
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// Fully-resolved configuration for one `ctxgrd` invocation.
///
/// The map is keyed by namespace. Every namespace that appears in
/// discovered documents OR is explicitly listed in TOML gets an
/// entry; the rule engine looks up the per-namespace [`NamespaceConfig`]
/// when deciding whether to emit a given diagnostic.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub namespaces: BTreeMap<String, NamespaceConfig>,
    /// Per-source activation table — the `[sources.<name>]` entries.
    /// A name in this map means the source is activated; the value is
    /// the params object passed via `$CTXGRD_SOURCE_PARAMS`.
    pub sources: BTreeMap<String, Value>,
    /// The `expect_min` declared by each activated source (ADR-119 §
    /// CLM-002), stripped out of the params object above. Absent means
    /// [`DEFAULT_SOURCE_EXPECT_MIN`]: writing `[sources.<name>]` at all
    /// is a statement that the source is expected to emit something, so
    /// the floor applies whether or not the key was written. `sources`
    /// stays the authority on which sources are activated — this map is
    /// consulted only for names already in it.
    pub source_expect_min: BTreeMap<String, u32>,
    /// Compiled path-match for the top-level `[ignore].patterns`
    /// list (globset syntax — root-anchored, no `!` negation, a
    /// slash-free pattern matches only at the root; NOT gitignore,
    /// despite the resemblance). Paths the markdown walker
    /// sees are checked against this, relative to the lint root;
    /// matches are skipped. `None` means "no ignore block configured".
    pub ignore: Option<globset::GlobSet>,
    /// Raw pattern strings round-tripped from the config file so
    /// downstream introspection can surface them in `ctxgrd rules`
    /// later if wanted. Keep in sync with `ignore`.
    pub ignore_patterns: Vec<String>,
    /// Namespace names exempted from `cfg.namespace-undeclared`
    /// (ADR-076 § OWN-004) — the `[ignore].namespaces` list. Deliberate
    /// staging ("we'll declare REPORT next sprint") and cross-repo id
    /// references opt out here. Suppressing the warning does NOT zero the
    /// coverage count: an ignored namespace still shows in
    /// `namespaces_undeclared`, because it really is linting under the
    /// zero-config rules.
    pub ignore_namespaces: Vec<String>,
    /// The `[roles].allowed` vocabulary (ADR-076 § OWN-003). `None` means
    /// the project declared no vocabulary, and `[<NS>].owner` is then
    /// declare-only — any string is accepted. Deliberately NOT compiled
    /// in: a built-in role list goes stale and over-fires, the failure
    /// that made the `model:` allowlist a config param.
    pub roles_allowed: Option<Vec<String>>,
    /// Globs from the top-level `[references].scan` array (ADR-001
    /// § REF-002, REF-003). The reference scanner walks files
    /// matching any of these globs and emits one [`Reference`] per
    /// `<NAMESPACE>-<number>` token found. Empty (the default) means
    /// the scanner is disabled — current behaviour.
    pub reference_scan_globs: Vec<String>,
    /// When true, `todo.listed` runs on every id-keyed document regardless
    /// of whether the namespace lists the rule explicitly. Set via
    /// `[todo.listed]\nenabled = true` in `ctxgrd.toml`.
    pub todo_listed_global: bool,
    /// Declared `[changelog]` table (ADR-084 § CHG-002). `None` means
    /// `ctxgrd changelog` has nothing to generate — the whitelist of
    /// contributing namespaces and their status→section mapping is
    /// entirely config-driven, never hardcoded. Local-only: a
    /// machine-wide changelog would claim documents from every project.
    pub changelog: Option<ChangelogConfig>,
    /// Kernel-level advisories raised during config loading — e.g.,
    /// `[sources.markdown-file]` was in the file but ignored
    /// (CORE-001). Surfaced through the same channel as runtime
    /// kernel messages so the renderer handles both uniformly.
    pub kernel_messages: Vec<KernelMessage>,
}

/// Resolved rules + params for a single namespace.
#[derive(Debug, Clone, Default)]
pub struct NamespaceConfig {
    /// Ordered list of rule codes active for this namespace.
    pub rules: Vec<String>,
    /// Keyed by rule code. Non-parameterized core rules have entry
    /// `Value::Null`; parameterized rules have a JSON object; external
    /// rules have whatever TOML shape the author provided.
    pub params: BTreeMap<String, Value>,
    /// Compiled glob matcher for `[<NS>].paths` (ADR 007 § DOC-002 /
    /// DOC-004). `None` means the user did not configure a `paths`
    /// list for this namespace; under DOC-001 such namespaces accept
    /// only id-claimed documents. Same globset grammar as
    /// `[ignore].patterns` (root-anchored, no `!` negation).
    pub paths: Option<globset::GlobSet>,
    /// Raw glob strings round-tripped from the config file. Kept in
    /// sync with `paths` so downstream introspection (e.g. a future
    /// `ctxgrd rules` column) can surface the configured locations.
    pub path_patterns: Vec<String>,
    /// The accountable *role* for this document type — `[<NS>].owner`
    /// (ADR-076 § OWN-003). A role (`developer`, `writer`), never a leaf
    /// skill: leaf skills are renamed, split and absorbed, which would
    /// turn every such change into a migration across every deployed
    /// config. `None` is what `cfg.namespace-unowned` reports.
    pub owner: Option<String>,
}

/// Declared `[changelog]` table (ADR-084 § CHG-002).
///
/// `namespaces` is the whitelist of namespaces that contribute changelog
/// entries; a namespace absent from it never appears (CHG-002). Each
/// whitelisted namespace has a [`ChangelogNamespace`] mapping its terminal
/// status to a Keep-a-Changelog section. `since` is the optional cutover
/// tag (CHG-006): released sections are rendered only for tags strictly
/// after it, and the `since` tag's tree seeds the already-shipped set that
/// `## [Unreleased]` subtracts. `None` derives the full tag history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangelogConfig {
    /// Ordered whitelist of contributing namespace names.
    pub namespaces: Vec<String>,
    /// Cutover tag (CHG-006). `None` = derive the full tag history.
    pub since: Option<String>,
    /// Per-namespace terminal-status → section mapping, keyed by
    /// namespace name. A namespace listed in `namespaces` but missing an
    /// entry here is a config error (rejected at parse time).
    pub entries: BTreeMap<String, ChangelogNamespace>,
}

/// One whitelisted namespace's changelog mapping (ADR-084 § CHG-002):
/// the terminal `status` value that counts as a shippable change, and
/// the Keep-a-Changelog `section` its entries render under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogNamespace {
    /// Terminal status value (e.g. `fixed` for `BUG`).
    pub when: String,
    /// Keep-a-Changelog category (`Added`/`Changed`/`Fixed`/…).
    pub section: String,
}

impl NamespaceConfig {
    /// Zero-config default — the non-parameterized core rules,
    /// no params, no path declarations.
    pub fn zero_config() -> Self {
        Self {
            rules: ZERO_CONFIG_RULES.iter().map(|s| s.to_string()).collect(),
            ..Self::default()
        }
    }

    /// True if `code` is listed in this namespace's active rule set.
    pub fn enables(&self, code: &str) -> bool {
        self.rules.iter().any(|r| r == code)
    }

    /// The `status` allow-list this namespace declares via
    /// `core.allowed-values`, if any. `None` means the namespace does
    /// not constrain `status` — callers treat that as "no list to
    /// validate against" rather than an empty list. Used by the
    /// pipeline gate validator (T2.1) and gate evaluation (T2.2).
    pub fn allowed_status_values(&self) -> Option<Vec<String>> {
        let values = self.params.get("core.allowed-values")?;
        let statuses = values.get("status")?.as_array()?;
        Some(
            statuses
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    }
}

impl Config {
    /// Look up a namespace's config, falling back to the zero-config
    /// default when the namespace wasn't mentioned in TOML.
    pub fn namespace_config(&self, namespace: &str) -> NamespaceConfig {
        self.namespaces
            .get(namespace)
            .cloned()
            .unwrap_or_else(NamespaceConfig::zero_config)
    }

    /// The set of namespaces a given namespace's `core.dep-shape` admits
    /// as `depends_on` targets — the union of its `requires` and `allows`
    /// params (ADR-039 § DAG-002/DAG-003). Empty when the namespace does
    /// not declare `core.dep-shape` (or declares it with no targets).
    pub fn dep_shape_targets(&self, namespace: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(ns_cfg) = self.namespaces.get(namespace) else {
            return out;
        };
        let Some(params) = ns_cfg.params.get("core.dep-shape") else {
            return out;
        };
        for key in ["requires", "allows"] {
            if let Some(arr) = params.get(key).and_then(Value::as_array) {
                out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
        }
        out
    }

    /// The "managed" namespace set (ADR-039 § DAG-003): every namespace
    /// that appears in SOME namespace's `core.dep-shape` `requires`/`allows`
    /// anywhere in the config. Used by the admissibility check to exempt
    /// edges that point at an entirely unmanaged endpoint (EARS-06.3's
    /// successor).
    pub fn dep_shape_managed(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for name in self.namespaces.keys() {
            out.extend(self.dep_shape_targets(name));
        }
        out
    }

    /// Assemble the declared type-DAG ordering edges (ADR-039 § DAG-002):
    /// every namespace's `core.dep-shape` `requires`+`allows` lift.
    ///
    /// Edge direction follows the document lift convention: a
    /// `[NS."core.dep-shape"] requires = ["T"]` declares a doc-edge
    /// `NS → T` (`depends_on`), which lifts to the ordering edge
    /// `T → NS` (T first). A self-edge (a namespace listing itself) is
    /// dropped — it carries no ordering and would falsely trip the cycle
    /// check.
    ///
    /// ADR-118 § STG-002 removed the `[pipeline].stages` adjacency that
    /// also contributed here (DAG-005). `core.dep-shape` is now the only
    /// declaration surface for namespace-level edges, which is the whole
    /// point of the deletion — three overlapping surfaces became one.
    pub fn dep_shape_edges(&self) -> BTreeSet<(String, String)> {
        let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
        for name in self.namespaces.keys() {
            for target in self.dep_shape_targets(name) {
                if &target != name {
                    edges.insert((target, name.clone()));
                }
            }
        }
        edges
    }
}

/// Anything that can go wrong loading or validating the config.
///
/// The `Display` impls match the kernel-error transcript in the
/// brief's acceptance (e.g., `[cfg.rule-params-invalid]` prefix with
/// the TOML path in brackets).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("[cfg.rule-unknown] [{namespace}] rule '{code}' not found — expected core rule or directory at '{expected_path}'")]
    RuleUnknown {
        namespace: String,
        code: String,
        expected_path: String,
    },
    #[error(
        "[cfg.rule-params-missing] [{namespace}.\"{code}\"] params sub-table is required for this parameterized rule"
    )]
    RuleParamsMissing { namespace: String, code: String },
    #[error("[cfg.rule-params-invalid] [{namespace}.\"{code}\"] params invalid: {detail}")]
    RuleParamsInvalid {
        namespace: String,
        code: String,
        detail: String,
    },
    #[error("[{namespace}] `rules` must be an array of strings")]
    RulesListInvalid { namespace: String },
    #[error(
        "[cfg.namespace-name-invalid] namespace '{namespace}' lists `core.id` but its name is \
         not a legal id prefix — an id `{namespace}-<number>` cannot be parsed (the name must be \
         uppercase ASCII starting with a letter, with no hyphen: regex `^[A-Z][A-Z0-9]*$`)"
    )]
    NamespaceNameNotIdLegal { namespace: String },
    #[error("[ext.namespace-reserved] external rule directory '{path}' uses the reserved 'core' namespace")]
    NamespaceReserved { path: PathBuf },
    #[error(
        "[cfg.ignore-invalid] `[ignore].patterns` entry {pattern:?} is not a valid glob: {detail}"
    )]
    IgnorePatternInvalid { pattern: String, detail: String },
    #[error("[cfg.ignore-invalid] `[ignore]` block invalid: {detail}")]
    IgnoreInvalid { detail: String },
    #[error("[cfg.roles-invalid] `[roles]` block invalid: {detail}")]
    RolesInvalid { detail: String },
    #[error("[cfg.references-invalid] `[references]` block invalid: {detail}")]
    ReferencesInvalid { detail: String },
    #[error("[cfg.paths-invalid] `[{namespace}].paths` invalid: {detail}")]
    PathsInvalid {
        namespace: String,
        pattern: String,
        detail: String,
    },
    /// ADR-118 § STG-002: the `[pipeline]` block was removed with the
    /// namespace stage layer. This MUST be an error rather than an ignored
    /// key — silently dropping it would leave an author believing an
    /// ordering is still enforced when nothing evaluates it.
    #[error(
        "[cfg.pipeline-removed] `[pipeline]` was removed in ADR-118 — namespace stages and gates no longer exist"
    )]
    PipelineRemoved,
    #[error("[cfg.changelog-invalid] `[changelog]` invalid: {detail}")]
    ChangelogInvalid { detail: String },
}

impl ConfigError {
    /// Rule code attached to the single-line `error: [<code>] ...`
    /// output the CLI prints. `None` when the error is plain I/O /
    /// parse (no `cfg.*` code).
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::RuleUnknown { .. } => Some("cfg.rule-unknown"),
            Self::RuleParamsMissing { .. } => Some("cfg.rule-params-missing"),
            Self::RuleParamsInvalid { .. } => Some("cfg.rule-params-invalid"),
            Self::NamespaceReserved { .. } => Some("ext.namespace-reserved"),
            Self::IgnorePatternInvalid { .. } | Self::IgnoreInvalid { .. } => {
                Some("cfg.ignore-invalid")
            }
            Self::RolesInvalid { .. } => Some("cfg.roles-invalid"),
            Self::ReferencesInvalid { .. } => Some("cfg.references-invalid"),
            Self::NamespaceNameNotIdLegal { .. } => Some("cfg.namespace-name-invalid"),
            Self::PathsInvalid { .. } => Some("cfg.paths-invalid"),
            Self::PipelineRemoved => Some("cfg.pipeline-removed"),
            Self::ChangelogInvalid { .. } => Some("cfg.changelog-invalid"),
            _ => None,
        }
    }
}

// -- public loading surface --------------------------------------------

/// Resolve the effective config for `root`.
///
/// Contract:
/// - If `<root>/ctxgrd.toml` is missing, returns the empty config
///   (every namespace falls through to zero-config defaults).
/// - If it's present, it MUST parse as TOML and the validator MUST
///   accept every core-rule param shape. Any failure short-circuits
///   with a single [`ConfigError`].
///
/// Resolution order is `local > global > zero-config default`, with
/// whole-layer replacement per namespace (no per-rule merging, per
/// CFG-001).
pub fn load(root: &Path) -> Result<Config, ConfigError> {
    load_with_global(root, global_ctxgrd_dir().as_deref())
}

/// The nearest ancestor of `start` (inclusive) carrying a `ctxgrd.toml`,
/// or `None` when no ancestor has one (BUG-048).
///
/// Matches `git`, `cargo`, `npm` and `go`, and matches the sibling
/// linters: both `wrkgrd` and `trtlgrd` already resolve their project
/// root upward, leaving ctxgrd the only member of the family that
/// degraded to zero-config the moment you `cd` into a subdirectory —
/// lint 258 documents under 120 rules at the root, 120 documents under 6
/// from `docs/adrs/`, and print `ok` both times.
///
/// Nearest wins, which is what a subtree carrying its own `ctxgrd.toml`
/// means: a separate lint root with its own namespace DAG, not the
/// parent's.
///
/// Deliberately **not** applied when the user passes `--root` — an
/// explicit path means exactly that directory. `ctxgrd init --force`
/// resolves its target the same way every other command does, so an
/// upward search there would let `ctxgrd init --force` run from
/// `docs/adrs/` overwrite the repository's real config.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("ctxgrd.toml").is_file())
        .map(Path::to_path_buf)
}

/// Testable entry point: pass `None` for "no global config" or
/// `Some(path)` pointing at a specific `.ctxgrd` directory.
pub(crate) fn load_with_global(root: &Path, global_dir: Option<&Path>) -> Result<Config, ConfigError> {
    let external = discover_external_rules_with_global(root, global_dir)?;

    // Start with global namespace configs, if any.
    let mut config = load_global_namespaces(&external, root, global_dir)?;

    // Local ctxgrd.toml overrides per namespace.
    let path = root.join("ctxgrd.toml");
    if path.exists() {
        let text = fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        let value: TomlValue = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        let local = parse_and_validate(value, &external, root)?;
        merge_local_over_global(&mut config, local);
    }
    // After the merge, never before: a global `~/.ctxgrd/namespaces/<NS>.toml`
    // and a local block each contribute rules, and only the merged list says
    // what the namespace actually binds.
    let inert = inert_rule_messages(&config);
    config.kernel_messages.extend(inert);
    Ok(config)
}

/// `cfg.rule-inert` (`BUG-040`): one message per rule a namespace binds
/// that its own classification prevents from ever running.
///
/// The inertness needs **two** conditions, and testing either alone is
/// wrong. `core.id` puts the namespace on the id pipeline, which
/// [`NamespaceConfig::file_level_namespaces`] excludes from the file-level
/// scan — but a handful of `Level::File` codes carry an explicit id-keyed
/// arm in [`crate::run`]'s step 6 and still fire
/// ([`crate::builtin_rules::ID_KEYED_FILE_LEVEL_RULES`]). Flagging on
/// `core.id` + `Level::File` alone would false-positive on this repo's own
/// `[ADR]` block; flagging on the missing arm alone would fire on every
/// path-claimed namespace, where the file-level scan is the intended path.
///
/// Reported as a `KernelMessage` rather than a [`ConfigError`] because the
/// run is not prevented — everything else in the config resolves and lints
/// normally. It is an error rather than a warning because the config makes
/// a claim that is false: `ctxgrd rules` lists the binding as active and
/// the summary counts it in `N rules`, so a user verifying their gate
/// before trusting it is told the gate exists.
fn inert_rule_messages(config: &Config) -> Vec<KernelMessage> {
    let mut out = Vec::new();
    for (namespace, ns_cfg) in &config.namespaces {
        if !ns_cfg.rules.iter().any(|c| c == "core.id") {
            continue;
        }
        for code in &ns_cfg.rules {
            if !is_file_level_compiled(code)
                || crate::builtin_rules::ID_KEYED_FILE_LEVEL_RULES.contains(&code.as_str())
            {
                continue;
            }
            out.push(
                KernelMessage::error(
                    "cfg.rule-inert",
                    format!(
                        "[{namespace}] binds {code}, but core.id puts this namespace on the \
                         id pipeline where that rule cannot run"
                    ),
                )
                .with_help(format!(
                    "drop `core.id` from [{namespace}].rules to make the namespace \
                     path-claimed, or drop {code} — as written the rule is listed as \
                     active and never runs"
                ))
                .with_note(
                    "file-level rules lint id-less path-claimed files (CLAUDE.md, TODO.md); \
                     a namespace claiming ids is linted per document instead",
                ),
            );
        }
    }
    out
}

/// Load every `~/.ctxgrd/namespaces/<NS>.toml` as a per-namespace
/// global default. Each file's content is the inner body of what
/// would be `[<NS>]` in a local ctxgrd.toml — just `rules` +
/// per-rule params sub-tables, no outer `[NS]` heading.
///
/// Returns an empty `Config` when no global dir exists. A broken file
/// surfaces as the same `ConfigError` variants a local file would.
fn load_global_namespaces(
    external: &BTreeMap<String, DiscoveredRule>,
    root: &Path,
    global_dir: Option<&Path>,
) -> Result<Config, ConfigError> {
    let mut config = Config::default();
    let Some(global_dir) = global_dir else {
        return Ok(config);
    };
    let namespaces_dir = global_dir.join("namespaces");
    let Ok(entries) = fs::read_dir(&namespaces_dir) else {
        return Ok(config);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        if !is_namespace_key(stem) {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        let value: TomlValue = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        let (ns_cfg, warnings) = parse_namespace(stem, value, external, root)?;
        config.kernel_messages.extend(warnings);
        config.namespaces.insert(stem.to_string(), ns_cfg);
    }
    Ok(config)
}

fn merge_local_over_global(global: &mut Config, local: Config) {
    for (ns, cfg) in local.namespaces {
        global.namespaces.insert(ns, cfg);
    }
    for (name, params) in local.sources {
        global.sources.insert(name, params);
    }
    // Must travel with `sources` above, or a local `[sources.<name>]`
    // silently reverts to DEFAULT_SOURCE_EXPECT_MIN and the `expect_min = 0`
    // escape hatch stops working (ADR-119 § CLM-002).
    for (name, floor) in local.source_expect_min {
        global.source_expect_min.insert(name, floor);
    }
    // [ignore] is local-only for v1 — per-user global file patterns
    // would require careful semantics (who overrides whom?); deferred.
    if local.ignore.is_some() {
        global.ignore = local.ignore;
        global.ignore_patterns = local.ignore_patterns;
    }
    if !local.ignore_namespaces.is_empty() {
        global.ignore_namespaces = local.ignore_namespaces;
    }
    // [roles] is local-only for the same reason as [ignore]: a role
    // vocabulary is a property of one project's org, not of the machine.
    // (There is no global top-level toml to carry it anyway — the global
    // layer is per-namespace files, which may still set `owner`.)
    if local.roles_allowed.is_some() {
        global.roles_allowed = local.roles_allowed;
    }
    // [references] is also local-only for v1: global scan globs make
    // even less sense (paths only resolve against the lint root).
    if !local.reference_scan_globs.is_empty() {
        global.reference_scan_globs = local.reference_scan_globs;
    }
    // [changelog] is local-only: a machine-wide changelog whitelist would
    // claim documents from every repo on the machine.
    if local.changelog.is_some() {
        global.changelog = local.changelog;
    }
    global.kernel_messages.extend(local.kernel_messages);
}

/// Where a discovered external rule or source lives.
///
/// Drives the `source` column in `ctxgrd rules` and informs
/// precedence resolution (local wins on name collision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Repo,
    Global,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "ext:repo",
            Self::Global => "ext:global",
        }
    }
}

/// A rule directory found on disk, local or global.
#[derive(Debug, Clone)]
pub struct DiscoveredRule {
    pub code: String,
    pub run_path: PathBuf,
    pub origin: Origin,
}

/// Canonical rule-code discovery. EXT-001 in the brief.
///
/// Walks both `<root>/rules/<ns>/<name>/` and
/// `~/.ctxgrd/rules/<ns>/<name>/`, collecting directories that
/// contain a `run` file. Local shadows global on name collision.
///
/// `rules/core/` at either scope fires `ext.namespace-reserved`
/// (CORE-005). Dot-prefixed directories are skipped silently. Name
/// regex validation (for the rule name) is lenient here — anything
/// without dots is accepted and turned into a code; invocation will
/// surface a permission error later if the `run` file isn't usable.
pub fn discover_external_rules(
    root: &Path,
) -> Result<BTreeMap<String, DiscoveredRule>, ConfigError> {
    discover_external_rules_with_global(root, global_ctxgrd_dir().as_deref())
}

/// Testable variant of [`discover_external_rules`] — `None` for
/// "no global dir", `Some(path)` to point at a specific one.
pub(crate) fn discover_external_rules_with_global(
    root: &Path,
    global_dir: Option<&Path>,
) -> Result<BTreeMap<String, DiscoveredRule>, ConfigError> {
    let mut out: BTreeMap<String, DiscoveredRule> = BTreeMap::new();
    // Global first — local entries overwrite on collision.
    if let Some(global) = global_dir {
        collect_rules_in(&global.join("rules"), Origin::Global, &mut out)?;
    }
    collect_rules_in(&root.join("rules"), Origin::Repo, &mut out)?;
    Ok(out)
}

fn collect_rules_in(
    rules_dir: &Path,
    origin: Origin,
    out: &mut BTreeMap<String, DiscoveredRule>,
) -> Result<(), ConfigError> {
    let Ok(entries) = fs::read_dir(rules_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let ns_path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(ns_name) = ns_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if ns_name.starts_with('.') {
            continue;
        }
        if ns_name == "core" {
            return Err(ConfigError::NamespaceReserved { path: ns_path });
        }
        let Ok(rule_entries) = fs::read_dir(&ns_path) else {
            continue;
        };
        for rule_entry in rule_entries.flatten() {
            if !rule_entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let rule_path = rule_entry.path();
            let Some(rule_name) = rule_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if rule_name.starts_with('.') {
                continue;
            }
            let run_path = rule_path.join("run");
            if run_path.is_file() {
                let code = format!("{ns_name}.{rule_name}");
                out.insert(
                    code.clone(),
                    DiscoveredRule {
                        code,
                        run_path,
                        origin,
                    },
                );
            }
        }
    }
    Ok(())
}

/// `~/.ctxgrd/` — returned as an absolute path when `$HOME` is set
/// AND the directory exists, else `None`. Callers treat `None` as
/// "no global config available".
pub(crate) fn global_ctxgrd_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".ctxgrd");
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

fn parse_and_validate(
    value: TomlValue,
    external_rules: &BTreeMap<String, DiscoveredRule>,
    root: &Path,
) -> Result<Config, ConfigError> {
    let TomlValue::Table(table) = value else {
        // Top-level TOML is always a table by definition, but defend
        // against the impossible anyway.
        return Ok(Config::default());
    };

    let mut config = Config::default();
    for (key, val) in table {
        if key == "sources" {
            parse_sources(val, &mut config)?;
            continue;
        }
        if key == "ignore" {
            parse_ignore(val, &mut config)?;
            continue;
        }
        if key == "references" {
            parse_references(val, &mut config)?;
            continue;
        }
        if key == "roles" {
            config.roles_allowed = Some(parse_roles(val)?);
            continue;
        }
        // ADR-118 § STG-002: refuse rather than ignore. The key is still
        // matched here — not left to fall through to the unknown-key path —
        // so the error can name the ADR and point at the replacement.
        if key == "pipeline" {
            return Err(ConfigError::PipelineRemoved);
        }
        if key == "changelog" {
            config.changelog = Some(parse_changelog(val)?);
            continue;
        }
        if key == "todo" {
            if let TomlValue::Table(mut t) = val {
                if let Some(TomlValue::Table(listed)) = t.remove("listed") {
                    if matches!(listed.get("enabled"), Some(TomlValue::Boolean(true))) {
                        config.todo_listed_global = true;
                    }
                }
            }
            continue;
        }
        if !is_namespace_key(&key) {
            // Unknown top-level table — silently ignore.
            continue;
        }
        let (ns_cfg, warnings) = parse_namespace(&key, val, external_rules, root)?;
        config.kernel_messages.extend(warnings);
        config.namespaces.insert(key, ns_cfg);
    }
    Ok(config)
}

/// Parse the top-level `[roles]` block (ADR-076 § OWN-003).
///
/// Shape is `allowed = ["developer", "writer", …]` — the vocabulary
/// `[<NS>].owner` values are validated against. A `[roles]` table with no
/// `allowed` key declares an *empty* vocabulary, which rejects every owner;
/// that is almost certainly a typo, so it is an error rather than a silently
/// inert table. To opt out of value-checking, omit `[roles]` entirely.
fn parse_roles(val: TomlValue) -> Result<Vec<String>, ConfigError> {
    let TomlValue::Table(mut table) = val else {
        return Err(ConfigError::RolesInvalid {
            detail: "`[roles]` must be a table".into(),
        });
    };
    let Some(allowed) = table.remove("allowed") else {
        return Err(ConfigError::RolesInvalid {
            detail: "`allowed` is required — a `[roles]` table without it would reject \
                     every owner; omit the table to leave `owner` declare-only"
                .into(),
        });
    };
    let TomlValue::Array(items) = allowed else {
        return Err(ConfigError::RolesInvalid {
            detail: "`allowed` must be an array of role-name strings".into(),
        });
    };
    items
        .into_iter()
        .map(|v| match v {
            TomlValue::String(s) => Ok(s),
            _ => Err(ConfigError::RolesInvalid {
                detail: "`allowed` entries must be strings".into(),
            }),
        })
        .collect()
}

/// Parse the top-level `[references]` block (ADR-001 § REF-003).
/// Currently supports only `scan = [<glob>, ...]`. Per-namespace
/// `[<NS>.references]` blocks are explicitly rejected — the scanner
/// is global, not per-namespace, because a single scanned file may
/// reference any namespace.
fn parse_references(val: TomlValue, config: &mut Config) -> Result<(), ConfigError> {
    let TomlValue::Table(mut table) = val else {
        return Ok(());
    };
    let Some(scan_val) = table.remove("scan") else {
        return Ok(());
    };
    let TomlValue::Array(items) = scan_val else {
        return Err(ConfigError::ReferencesInvalid {
            detail: "`scan` must be an array of glob strings".into(),
        });
    };
    let mut globs = Vec::with_capacity(items.len());
    for item in items {
        let TomlValue::String(s) = item else {
            return Err(ConfigError::ReferencesInvalid {
                detail: "`scan` entries must be strings".into(),
            });
        };
        globs.push(s);
    }
    config.reference_scan_globs = globs;
    Ok(())
}

/// Parse the top-level `[changelog]` block (ADR-084 § CHG-002).
///
/// Shape:
/// ```toml
/// [changelog]
/// namespaces = ["BUG"]
/// since = "v0.48.0"      # optional cutover tag (CHG-006)
///
/// [changelog.BUG]
/// when = "fixed"          # terminal status
/// section = "Fixed"       # Keep-a-Changelog category
/// ```
///
/// Every namespace listed in `namespaces` MUST have a matching
/// `[changelog.<NS>]` sub-table with both `when` and `section`; a missing
/// mapping is a hard error, not a silent skip (the whitelist and the
/// status→section map must stay in lockstep). The binary hardcodes no
/// namespace set and no default mapping.
fn parse_changelog(val: TomlValue) -> Result<ChangelogConfig, ConfigError> {
    let TomlValue::Table(mut table) = val else {
        return Err(ConfigError::ChangelogInvalid {
            detail: "`[changelog]` must be a table".into(),
        });
    };

    let namespaces = match table.remove("namespaces") {
        Some(TomlValue::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                TomlValue::String(s) => Ok(s),
                _ => Err(ConfigError::ChangelogInvalid {
                    detail: "`namespaces` must be an array of namespace-name strings".into(),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(ConfigError::ChangelogInvalid {
                detail: "`namespaces` must be an array of namespace-name strings".into(),
            });
        }
        None => {
            return Err(ConfigError::ChangelogInvalid {
                detail: "`namespaces` is required — a `[changelog]` with no whitelist \
                         contributes nothing"
                    .into(),
            });
        }
    };
    if namespaces.is_empty() {
        return Err(ConfigError::ChangelogInvalid {
            detail: "`namespaces` must list at least one namespace".into(),
        });
    }

    let since = match table.remove("since") {
        Some(TomlValue::String(s)) => Some(s),
        Some(_) => {
            return Err(ConfigError::ChangelogInvalid {
                detail: "`since` must be a tag-name string (e.g. \"v0.48.0\")".into(),
            });
        }
        None => None,
    };

    // Remaining table-valued keys are per-namespace `[changelog.<NS>]`
    // mappings. Collect them, then verify every whitelisted namespace has
    // one (and no orphan mapping names an un-whitelisted namespace).
    let mut entries: BTreeMap<String, ChangelogNamespace> = BTreeMap::new();
    for (key, value) in table {
        let TomlValue::Table(sub) = value else {
            return Err(ConfigError::ChangelogInvalid {
                detail: format!("unexpected key `{key}` — expected `[changelog.{key}]` sub-table"),
            });
        };
        let when = match sub.get("when") {
            Some(TomlValue::String(s)) if !s.is_empty() => s.clone(),
            _ => {
                return Err(ConfigError::ChangelogInvalid {
                    detail: format!(
                        "`[changelog.{key}]` requires a non-empty `when` (terminal status) string"
                    ),
                });
            }
        };
        let section = match sub.get("section") {
            Some(TomlValue::String(s)) if !s.is_empty() => s.clone(),
            _ => {
                return Err(ConfigError::ChangelogInvalid {
                    detail: format!(
                        "`[changelog.{key}]` requires a non-empty `section` \
                         (Keep-a-Changelog category) string"
                    ),
                });
            }
        };
        entries.insert(key, ChangelogNamespace { when, section });
    }

    for ns in &namespaces {
        if !entries.contains_key(ns) {
            return Err(ConfigError::ChangelogInvalid {
                detail: format!(
                    "namespace `{ns}` is whitelisted but has no `[changelog.{ns}]` \
                     mapping (needs `when` and `section`)"
                ),
            });
        }
    }
    for ns in entries.keys() {
        if !namespaces.contains(ns) {
            return Err(ConfigError::ChangelogInvalid {
                detail: format!(
                    "`[changelog.{ns}]` maps a namespace not listed in `namespaces`"
                ),
            });
        }
    }

    Ok(ChangelogConfig {
        namespaces,
        since,
        entries,
    })
}

fn parse_ignore(val: TomlValue, config: &mut Config) -> Result<(), ConfigError> {
    let TomlValue::Table(mut table) = val else {
        return Ok(());
    };
    // ADR-076 § OWN-004: `namespaces` exempts a namespace from the
    // undeclared-coverage warning. Independent of `patterns` — a config may
    // carry either, both, or neither.
    if let Some(ns_val) = table.remove("namespaces") {
        let TomlValue::Array(items) = ns_val else {
            return Err(ConfigError::IgnoreInvalid {
                detail: "`[ignore].namespaces` must be an array of namespace-name strings".into(),
            });
        };
        config.ignore_namespaces = items
            .into_iter()
            .map(|v| match v {
                TomlValue::String(s) => Ok(s),
                _ => Err(ConfigError::IgnoreInvalid {
                    detail: "`[ignore].namespaces` entries must be strings".into(),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?;
    }
    let Some(patterns_val) = table.remove("patterns") else {
        return Ok(());
    };
    let TomlValue::Array(items) = patterns_val else {
        return Err(ConfigError::IgnorePatternInvalid {
            pattern: String::new(),
            detail: "`[ignore].patterns` must be an array of strings".to_string(),
        });
    };

    let mut patterns = Vec::with_capacity(items.len());
    let mut builder = globset::GlobSetBuilder::new();
    for item in items {
        let TomlValue::String(s) = item else {
            return Err(ConfigError::IgnorePatternInvalid {
                pattern: String::new(),
                detail: "`[ignore].patterns` must be an array of strings".to_string(),
            });
        };
        let glob = globset::Glob::new(&s).map_err(|e| ConfigError::IgnorePatternInvalid {
            pattern: s.clone(),
            detail: e.to_string(),
        })?;
        builder.add(glob);
        patterns.push(s);
    }
    let set = builder
        .build()
        .map_err(|e| ConfigError::IgnorePatternInvalid {
            pattern: String::new(),
            detail: e.to_string(),
        })?;
    config.ignore = Some(set);
    config.ignore_patterns = patterns;
    Ok(())
}

/// Namespaces start with an uppercase ASCII letter — same prefix
/// grammar as document IDs. Lowercase top-level keys that aren't
/// `sources` are treated as "future-reserved, ignore for now".
fn is_namespace_key(key: &str) -> bool {
    key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Whether `name` is a legal id-prefix — i.e. an id `<name>-1` parses back
/// to a [`DocumentId`] whose namespace is exactly `name`. Reuses the id
/// grammar itself (no second regex to drift) so the guard tracks
/// [`crate::id`] precisely. The round-trip equality rejects a hyphenated
/// name like `SR-MAP`, which would otherwise parse as namespace `SR`.
fn is_id_legal_namespace(name: &str) -> bool {
    format!("{name}-1")
        .parse::<crate::id::DocumentId>()
        .is_ok_and(|id| id.namespace == name)
}

fn parse_sources(val: TomlValue, config: &mut Config) -> Result<(), ConfigError> {
    let TomlValue::Table(sources) = val else {
        return Ok(());
    };
    for (name, mut source_val) in sources {
        if name == "markdown-file" {
            config.kernel_messages.push(KernelMessage::warning(
                "cfg.reserved-source",
                "[sources.markdown-file] is reserved and was ignored",
            ));
            continue;
        }
        // ADR-119 § CLM-002. `expect_min` is ctxgrd's own knob, so it is
        // removed from the table before it becomes `$CTXGRD_SOURCE_PARAMS`
        // — a source script must not have to know about a reserved key it
        // never set.
        let mut expect_min = DEFAULT_SOURCE_EXPECT_MIN;
        if let TomlValue::Table(table) = &mut source_val {
            if let Some(raw) = table.remove("expect_min") {
                match raw.as_integer() {
                    Some(n) if n >= 0 => expect_min = u32::try_from(n).unwrap_or(u32::MAX),
                    _ => config.kernel_messages.push(
                        KernelMessage::warning(
                            "cfg.expect-min-invalid",
                            format!(
                                "[sources.{name}].expect_min must be a non-negative integer; \
                                 using the default of {DEFAULT_SOURCE_EXPECT_MIN}"
                            ),
                        )
                        .with_help(
                            "set `expect_min = 0` to accept a source that legitimately \
                             emits nothing",
                        ),
                    ),
                }
            }
        }
        config.source_expect_min.insert(name.clone(), expect_min);
        config.sources.insert(name, toml_to_json(&source_val));
    }
    Ok(())
}

fn parse_namespace(
    namespace: &str,
    val: TomlValue,
    external_rules: &BTreeMap<String, DiscoveredRule>,
    root: &Path,
) -> Result<(NamespaceConfig, Vec<KernelMessage>), ConfigError> {
    let TomlValue::Table(mut table) = val else {
        return Ok((NamespaceConfig::default(), Vec::new()));
    };

    let rules = match table.remove("rules") {
        Some(TomlValue::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                TomlValue::String(s) => Ok(s),
                _ => Err(ConfigError::RulesListInvalid {
                    namespace: namespace.to_string(),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
        _ => {
            return Err(ConfigError::RulesListInvalid {
                namespace: namespace.to_string(),
            });
        }
    };

    // A namespace that lists `core.id` expects its documents to carry an
    // `<NS>-<number>` id, so its name must itself be a legal id prefix. A
    // hyphenated name (e.g. `SR-MAP`) makes every such id unparseable —
    // documents are rejected as malformed with no hint that the *namespace
    // name* is the cause (BUG-013). Catch it once, at config load, instead.
    if rules.iter().any(|c| c == "core.id") && !is_id_legal_namespace(namespace) {
        return Err(ConfigError::NamespaceNameNotIdLegal {
            namespace: namespace.to_string(),
        });
    }

    let (paths, path_patterns) = match table.remove("paths") {
        Some(v) => parse_namespace_paths(namespace, v)?,
        None => (None, Vec::new()),
    };

    // ADR-076 § OWN-003. Removed from the table before the params sweep
    // below, or it would land in `params` as a phantom rule named `owner`.
    let owner = match table.remove("owner") {
        Some(TomlValue::String(s)) => Some(s),
        Some(_) => {
            return Err(ConfigError::RolesInvalid {
                detail: format!("[{namespace}].owner must be a role-name string"),
            });
        }
        None => None,
    };

    let mut params: BTreeMap<String, Value> = BTreeMap::new();
    for (k, v) in table {
        params.insert(k, toml_to_json(&v));
    }

    // Validate each rule: unknown rules produce a warning and are skipped;
    // invalid params (wrong type etc.) are still hard errors.
    let mut valid_rules = Vec::with_capacity(rules.len());
    let mut warnings = Vec::new();
    for code in rules {
        match validate_rule(namespace, &code, &params, external_rules, root)? {
            None => valid_rules.push(code),
            Some(km) => warnings.push(km),
        }
    }

    // ADR-048 § SEED-002: `core.min-docs`'s `count` is reserved for a future
    // cardinality floor but pinned to 1 in this release. A non-1 value must not
    // be silently inert — warn — while leaving the seed rule active. Emitted
    // here, not in `validate_rule`, so the seed stays in `valid_rules` (a `Some`
    // return there means "warn and skip the rule").
    if valid_rules.iter().any(|c| c == "core.min-docs") {
        if let Some(count) = params
            .get("core.min-docs")
            .and_then(|p| p.get("count"))
            .and_then(Value::as_i64)
        {
            if count != 1 {
                warnings.push(KernelMessage::warning(
                    "cfg.reserved-param",
                    format!(
                        "[{namespace}.\"core.min-docs\"] `count` = {count} is reserved \
                         (pinned to 1 in this release) and was ignored; the seed stays active"
                    ),
                ));
            }
        }
    }

    Ok((
        NamespaceConfig {
            rules: valid_rules,
            params,
            paths,
            path_patterns,
            owner,
        },
        warnings,
    ))
}

/// Parse `[<NS>].paths` per ADR 007 § DOC-002 + DOC-004. Always a
/// list of glob strings — no string-shorthand to keep the grammar
/// in lockstep with `[ignore].patterns`. An empty list compiles to
/// `None` so downstream "any path configured?" checks stay simple.
fn parse_namespace_paths(
    namespace: &str,
    val: TomlValue,
) -> Result<(Option<globset::GlobSet>, Vec<String>), ConfigError> {
    let TomlValue::Array(items) = val else {
        return Err(ConfigError::PathsInvalid {
            namespace: namespace.to_string(),
            pattern: String::new(),
            detail: "must be an array of glob strings".into(),
        });
    };
    let mut patterns = Vec::with_capacity(items.len());
    let mut builder = globset::GlobSetBuilder::new();
    for item in items {
        let TomlValue::String(s) = item else {
            return Err(ConfigError::PathsInvalid {
                namespace: namespace.to_string(),
                pattern: String::new(),
                detail: "entries must be strings".into(),
            });
        };
        let glob = globset::Glob::new(&s).map_err(|e| ConfigError::PathsInvalid {
            namespace: namespace.to_string(),
            pattern: s.clone(),
            detail: e.to_string(),
        })?;
        builder.add(glob);
        patterns.push(s);
    }
    if patterns.is_empty() {
        return Ok((None, patterns));
    }
    let set = builder.build().map_err(|e| ConfigError::PathsInvalid {
        namespace: namespace.to_string(),
        pattern: String::new(),
        detail: e.to_string(),
    })?;
    Ok((Some(set), patterns))
}

/// Returns `Ok(None)` when the rule is valid, `Ok(Some(warning))` when the
/// rule is unrecognised but tolerated (the rule will be skipped), or
/// `Err` for hard configuration mistakes (invalid params, etc.).
fn validate_rule(
    namespace: &str,
    code: &str,
    params: &BTreeMap<String, Value>,
    external_rules: &BTreeMap<String, DiscoveredRule>,
    root: &Path,
) -> Result<Option<KernelMessage>, ConfigError> {
    if code.starts_with("core.") {
        // Some `core.*` rules ship compiled in BUILTIN_RULES rather than
        // in the pure `rules.rs` set (ADR-040: `core.commit-freshness`,
        // `core.calendar-freshness`). They are real core rules, just
        // dispatched via `document_check`; accept them here and let their
        // params validate as builtin params (free-form, validated by the
        // rule itself).
        if !CORE_RULES.contains(&code) {
            if is_builtin_compiled(code) {
                return Ok(None);
            }
            return Ok(Some(unknown_rule_warning(
                namespace,
                code,
                &format!("remove '{code}' from [{namespace}].rules, or pick a real core rule (see `ctxgrd rules`)"),
                external_rules,
            )));
        }
        if PARAMETERIZED_CORE_RULES.contains(&code) {
            let p = params
                .get(code)
                .ok_or_else(|| ConfigError::RuleParamsMissing {
                    namespace: namespace.to_string(),
                    code: code.to_string(),
                })?;
            validate_core_rule_params(namespace, code, p)?;
        } else if code == "core.successor-link" || code == "core.dep-status" {
            // These rules' params are optional (SUCC-003, DPS-003): absent
            // values fall back to documented defaults, so they must stay
            // out of PARAMETERIZED_CORE_RULES — membership there makes a
            // missing block a `RuleParamsMissing` error. Validate the shape
            // only when a table is present.
            if let Some(p) = params.get(code) {
                validate_core_rule_params(namespace, code, p)?;
            }
        }
        return Ok(None);
    }

    if is_builtin_compiled(code) {
        return Ok(None);
    }

    if is_reserved_builtin_prefix(code) {
        return Ok(Some(unknown_rule_warning(
            namespace,
            code,
            &format!("upgrade ctxgrd (built-in rules ship in the binary, not as scripts), or check the name with `ctxgrd rules`; if intentional, remove '{code}' from [{namespace}].rules"),
            external_rules,
        )));
    }

    if external_rules.contains_key(code) {
        return Ok(None);
    }
    let (ns, name) = code.split_once('.').unwrap_or((code, ""));
    let expected = root.join("rules").join(ns).join(name).join("run");
    Ok(Some(unknown_rule_warning(
        namespace,
        code,
        &format!(
            "add a rule directory at {}, or remove '{code}' from [{namespace}].rules",
            expected.display()
        ),
        external_rules,
    )))
}

/// Build a `cfg.rule-unknown` warning kernel message, appending a
/// "did you mean …?" hint when other rules share the same namespace prefix.
fn unknown_rule_warning(
    namespace: &str,
    code: &str,
    help: &str,
    external_rules: &BTreeMap<String, DiscoveredRule>,
) -> KernelMessage {
    let prefix = code.split_once('.').map(|(p, _)| p).unwrap_or("");
    let suggestions: Vec<&str> = CORE_RULES
        .iter()
        .copied()
        .chain(crate::builtin_rules::BUILTIN_RULES.iter().map(|r| r.code))
        .chain(external_rules.keys().map(String::as_str))
        .filter(|k| *k != code && k.split_once('.').map(|(p, _)| p).unwrap_or("") == prefix)
        .collect();

    let full_help = if suggestions.is_empty() {
        help.to_string()
    } else {
        let list = suggestions
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{help} — did you mean {list}?")
    };

    KernelMessage::warning(
        "cfg.rule-unknown",
        format!("[{namespace}] rule '{code}' is not known — skipping"),
    )
    .with_help(full_help)
    .with_note("run `ctxgrd rules` to see all available rules")
}

fn validate_core_rule_params(
    namespace: &str,
    code: &str,
    params: &Value,
) -> Result<(), ConfigError> {
    let Value::Object(map) = params else {
        return Err(ConfigError::RuleParamsInvalid {
            namespace: namespace.to_string(),
            code: code.to_string(),
            detail: format!("expected table, got {}", value_kind(params)),
        });
    };

    match code {
        "core.required-headings" => require_array_of_strings(namespace, code, map, "headings"),
        "core.required-metadata" => require_array_of_strings(namespace, code, map, "keys"),
        "core.allowed-values" => {
            // Every entry must be an array of strings. Key names are
            // arbitrary (they're the metadata keys the rule checks).
            for (key, value) in map {
                let Value::Array(items) = value else {
                    return Err(ConfigError::RuleParamsInvalid {
                        namespace: namespace.to_string(),
                        code: code.to_string(),
                        detail: format!(
                            "`{key}` expected array of strings, got {}",
                            value_kind(value)
                        ),
                    });
                };
                for item in items {
                    if !item.is_string() {
                        return Err(ConfigError::RuleParamsInvalid {
                            namespace: namespace.to_string(),
                            code: code.to_string(),
                            detail: format!(
                                "`{key}` expected array of strings, got array of mixed types"
                            ),
                        });
                    }
                }
            }
            Ok(())
        }
        "core.successor-link" => {
            // Three optional string params: `trigger`, `field`, `target`
            // (SUCC-003). Reject any other key shape; a non-string value is
            // a configuration mistake, not a silent fallback.
            for key in ["trigger", "field", "target"] {
                if let Some(value) = map.get(key) {
                    if !value.is_string() {
                        return Err(ConfigError::RuleParamsInvalid {
                            namespace: namespace.to_string(),
                            code: code.to_string(),
                            detail: format!(
                                "`{key}` expected string, got {}",
                                value_kind(value)
                            ),
                        });
                    }
                }
            }
            Ok(())
        }
        "core.dep-status" => {
            // Two optional params: `terminal` (array of strings) and
            // `severity` (`error` | `warning`), both defaulted (DPS-003).
            // Absent is fine; present-but-wrong-shape is a configuration
            // mistake, not a silent fallback.
            if let Some(value) = map.get("terminal") {
                let Value::Array(items) = value else {
                    return Err(ConfigError::RuleParamsInvalid {
                        namespace: namespace.to_string(),
                        code: code.to_string(),
                        detail: format!(
                            "`terminal` expected array of strings, got {}",
                            value_kind(value)
                        ),
                    });
                };
                if items.iter().any(|i| !i.is_string()) {
                    return Err(ConfigError::RuleParamsInvalid {
                        namespace: namespace.to_string(),
                        code: code.to_string(),
                        detail: "`terminal` expected array of strings, got array of mixed types"
                            .to_string(),
                    });
                }
            }
            if let Some(value) = map.get("severity") {
                let is_valid = value
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("error") || s.eq_ignore_ascii_case("warning"));
                if !is_valid {
                    return Err(ConfigError::RuleParamsInvalid {
                        namespace: namespace.to_string(),
                        code: code.to_string(),
                        detail: format!(
                            "`severity` expected \"error\" or \"warning\", got {}",
                            value_kind(value)
                        ),
                    });
                }
            }
            Ok(())
        }
        _ => Ok(()), // non-parameterized core rules don't reach here
    }
}

fn require_array_of_strings(
    namespace: &str,
    code: &str,
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(), ConfigError> {
    let Some(value) = map.get(field) else {
        return Err(ConfigError::RuleParamsInvalid {
            namespace: namespace.to_string(),
            code: code.to_string(),
            detail: format!("missing required field `{field}`"),
        });
    };
    let Value::Array(items) = value else {
        return Err(ConfigError::RuleParamsInvalid {
            namespace: namespace.to_string(),
            code: code.to_string(),
            detail: format!(
                "`{field}` expected array of strings, got {}",
                value_kind(value)
            ),
        });
    };
    for item in items {
        if !item.is_string() {
            return Err(ConfigError::RuleParamsInvalid {
                namespace: namespace.to_string(),
                code: code.to_string(),
                detail: format!("`{field}` expected array of strings, got array of mixed types"),
            });
        }
    }
    Ok(())
}

/// Cheap TOML→JSON value coercion for pass-through storage.
///
/// We route everything through `serde_json::Value` internally so the
/// rule-stdin writer and rule consumers share a single representation.
/// TOML datetime / local-date values collapse to their string form —
/// good enough for the kernel's needs since core rules don't read
/// dates.
fn toml_to_json(v: &TomlValue) -> Value {
    match v {
        TomlValue::String(s) => Value::String(s.clone()),
        TomlValue::Integer(i) => json!(i),
        TomlValue::Float(f) => json!(f),
        TomlValue::Boolean(b) => Value::Bool(*b),
        TomlValue::Datetime(dt) => Value::String(dt.to_string()),
        TomlValue::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        TomlValue::Table(t) => {
            let mut map = serde_json::Map::new();
            for (k, v) in t {
                map.insert(k.clone(), toml_to_json(v));
            }
            Value::Object(map)
        }
    }
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "float"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "table",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Regression test for the v0.3.0 fixture-wiring bug: when a
    /// non-empty local `[references].scan` overrode the (empty) global,
    /// `merge_local_over_global` previously dropped the local globs on
    /// the floor — a silently-broken kernel. Pin the contract here so
    /// the next refactor doesn't reintroduce the omission.
    #[test]
    fn merge_local_over_global_carries_reference_scan_globs() {
        let mut global = Config::default();
        let mut local = Config::default();
        local.reference_scan_globs = vec!["**/*.rs".to_string(), "Cargo.toml".to_string()];
        merge_local_over_global(&mut global, local);
        assert_eq!(
            global.reference_scan_globs,
            vec!["**/*.rs".to_string(), "Cargo.toml".to_string()]
        );
    }

    /// Empty local must NOT clobber a populated global. (We don't
    /// currently support global `[references].scan`, but the merge
    /// rule should be future-proof: empty local = leave global alone.)
    #[test]
    fn merge_local_over_global_empty_local_preserves_global() {
        let mut global = Config::default();
        global.reference_scan_globs = vec!["src/**/*.rs".to_string()];
        let local = Config::default();
        merge_local_over_global(&mut global, local);
        assert_eq!(global.reference_scan_globs, vec!["src/**/*.rs".to_string()]);
    }

    fn parse(toml_text: &str) -> Result<Config, ConfigError> {
        let value: TomlValue = toml::from_str(toml_text).expect("test TOML is valid");
        parse_and_validate(value, &BTreeMap::new(), Path::new("."))
    }

    /// Variant that seeds the external-rule discovery set so tests for
    /// external-code validation can run without touching the filesystem.
    fn parse_with_external(toml_text: &str, externals: &[&str]) -> Result<Config, ConfigError> {
        let value: TomlValue = toml::from_str(toml_text).expect("test TOML is valid");
        let set: BTreeMap<String, DiscoveredRule> = externals
            .iter()
            .map(|code| {
                let code = code.to_string();
                let rule = DiscoveredRule {
                    code: code.clone(),
                    run_path: PathBuf::from(format!("./rules/{}/run", code.replace('.', "/"))),
                    origin: Origin::Repo,
                };
                (code, rule)
            })
            .collect();
        parse_and_validate(value, &set, Path::new("."))
    }

    #[test]
    fn empty_file_yields_empty_config() {
        let config = parse("").unwrap();
        assert!(config.namespaces.is_empty());
        assert!(config.sources.is_empty());
    }

    // -- BUG-013 guard: an id-claiming namespace name must be id-legal -----

    #[test]
    fn hyphenated_namespace_with_core_id_is_rejected() {
        // The SR-MAP class: a hyphen in the name makes `SR-MAP-001`
        // unparseable, so every document would be flagged malformed.
        let text = "[SR-MAP]\npaths = [\"docs/x/**\"]\nrules = [\"core.id\"]\n";
        let err = parse(text).unwrap_err();
        assert!(
            matches!(&err, ConfigError::NamespaceNameNotIdLegal { namespace } if namespace == "SR-MAP"),
            "expected NamespaceNameNotIdLegal for SR-MAP, got {err:?}"
        );
    }

    #[test]
    fn id_legal_namespace_with_core_id_is_accepted() {
        let text = "[SAFEGUARD]\npaths = [\"docs/x/**\"]\nrules = [\"core.id\"]\n";
        assert!(parse(text).is_ok());
    }

    #[test]
    fn namespace_with_trailing_digit_and_core_id_is_accepted() {
        // A digit inside the namespace is id-legal (only an internal hyphen
        // is not — that is the SR-MAP class above). `SOC2-001` parses back to
        // namespace `SOC2`, so the soc2 compliance pack (ADR-069) may claim
        // ids under `[SOC2]`. The prose report name is "SOC 2"; the ctxgrd
        // namespace is the hyphen-free `SOC2`.
        let text = "[SOC2]\npaths = [\"docs/compliance/soc2/**\"]\nrules = [\"core.id\"]\n";
        assert!(
            parse(text).is_ok(),
            "[SOC2] with core.id must load: SOC2-001 parses to namespace SOC2"
        );
    }

    #[test]
    fn hyphenated_namespace_without_core_id_is_allowed() {
        // The guard is scoped to namespaces that claim ids: an id-less
        // path-claimed namespace is unaffected by the name grammar.
        let text = "[MY-DOCS]\npaths = [\"docs/x/**\"]\nrules = [\"core.frontmatter\"]\n";
        assert!(parse(text).is_ok());
    }

    #[test]
    fn namespace_with_six_defaults_parses_cleanly() {
        let text = r#"
[ADR]
rules = [
  "core.frontmatter",
  "core.id",
  "core.id-unique",
  "core.dep-resolved",
  "core.dep-cycle",
  "core.cross-ref",
]
"#;
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert_eq!(adr.rules.len(), 6);
        assert!(adr.enables("core.cross-ref"));
    }

    #[test]
    fn required_headings_accepts_array_of_strings() {
        let text = r#"
[ADR]
rules = ["core.required-headings"]
[ADR."core.required-headings"]
headings = ["Status", "Context", "Decision", "Consequences"]
"#;
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        let params = adr.params.get("core.required-headings").unwrap();
        assert_eq!(
            params.pointer("/headings/0").and_then(|v| v.as_str()),
            Some("Status")
        );
    }

    #[test]
    fn required_headings_rejects_non_array() {
        let text = r#"
[ADR]
rules = ["core.required-headings"]
[ADR."core.required-headings"]
headings = 42
"#;
        let err = parse(text).unwrap_err();
        match err {
            ConfigError::RuleParamsInvalid { code, detail, .. } => {
                assert_eq!(code, "core.required-headings");
                assert!(detail.contains("`headings`"));
                assert!(detail.contains("integer"), "got detail: {detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn required_headings_rejects_mixed_array() {
        let text = r#"
[ADR]
rules = ["core.required-headings"]
[ADR."core.required-headings"]
headings = ["Status", 42]
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::RuleParamsInvalid { .. }));
    }

    #[test]
    fn parameterized_rule_without_params_errors() {
        let text = r#"
[ADR]
rules = ["core.required-headings"]
"#;
        let err = parse(text).unwrap_err();
        match err {
            ConfigError::RuleParamsMissing { code, .. } => {
                assert_eq!(code, "core.required-headings");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn unknown_core_rule_warns_and_skips() {
        let text = "[ADR]\nrules = [\"core.nope\"]\n";
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert!(!adr.enables("core.nope"), "unknown rule must be skipped");
        let km = config
            .kernel_messages
            .iter()
            .find(|m| m.code == "cfg.rule-unknown")
            .expect("expected cfg.rule-unknown warning");
        assert_eq!(km.severity, crate::diagnostic::Severity::Warning);
        assert!(km.message.contains("core.nope"));
    }

    #[test]
    fn external_rule_accepted_when_discovered_on_disk() {
        let text = r#"
[ADR]
rules = ["core.frontmatter", "adr.consequences-non-empty"]
"#;
        let config = parse_with_external(text, &["adr.consequences-non-empty"]).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert!(adr.enables("adr.consequences-non-empty"));
    }

    #[test]
    fn external_rule_rejected_when_not_discovered_warns_and_skips() {
        let text = "[ADR]\nrules = [\"adr.typoed-name\"]\n";
        let config = parse_with_external(text, &[]).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert!(
            !adr.enables("adr.typoed-name"),
            "unknown rule must be skipped"
        );
        let km = config
            .kernel_messages
            .iter()
            .find(|m| m.code == "cfg.rule-unknown")
            .expect("expected cfg.rule-unknown warning");
        assert_eq!(km.severity, crate::diagnostic::Severity::Warning);
        assert!(km.message.contains("adr.typoed-name"));
        assert!(
            km.help
                .as_deref()
                .unwrap_or("")
                .contains("rules/adr/typoed-name/run"),
            "help should mention the expected path: {:?}",
            km.help
        );
    }

    #[test]
    fn reserved_builtin_prefix_is_derived_from_the_rule_list() {
        // Derived from BUILTIN_COMPILED_RULES — no separate list to sync.
        // Both `agents.` and `todo.` are reserved because rules under
        // each are registered (ADR-020 § ACX-010).
        assert!(is_reserved_builtin_prefix("agents.context-headings"));
        assert!(is_reserved_builtin_prefix("agents.some-future-rule"));
        assert!(is_reserved_builtin_prefix("todo.freshness"));
        assert!(is_reserved_builtin_prefix("todo.some-future-rule"));
        assert!(!is_reserved_builtin_prefix("ctx.stability"));
        assert!(!is_reserved_builtin_prefix("adr.foo"));
    }

    #[test]
    fn unknown_reserved_rule_warns_as_missing_builtin_not_missing_script() {
        let text = "[AGENTS]\nrules = [\"agents.bogus\"]\n";
        let config = parse_with_external(text, &[]).unwrap();
        let km = config
            .kernel_messages
            .iter()
            .find(|m| m.code == "cfg.rule-unknown")
            .expect("expected cfg.rule-unknown warning");
        assert!(km.message.contains("agents.bogus"));
        let help = km.help.as_deref().unwrap_or("");
        assert!(
            help.contains("upgrade ctxgrd"),
            "should read as a missing built-in: {help}"
        );
        assert!(
            !help.contains("rules/agents"),
            "must not point at an external script path: {help}"
        );
    }

    #[test]
    fn unknown_rule_did_you_mean_suggests_same_prefix_rules() {
        // "todo.frehsness" is a typo of "todo.freshness"; the warning
        // help text should suggest real todo.* rules.
        let text = "[TODO]\npaths = [\"TODO.md\"]\nrules = [\"todo.frehsness\"]\n";
        let config = parse(text).unwrap();
        let km = config
            .kernel_messages
            .iter()
            .find(|m| m.code == "cfg.rule-unknown")
            .expect("expected cfg.rule-unknown warning");
        let help = km.help.as_deref().unwrap_or("");
        assert!(
            help.contains("todo.freshness"),
            "did-you-mean should suggest 'todo.freshness': {help}"
        );
    }

    #[test]
    fn known_builtin_rule_validates_without_a_script() {
        let text = r#"
[AGENTS]
rules = ["agents.context-headings", "agents.context-budget", "agents.context-cache"]

[TODO]
rules = ["todo.freshness", "todo.structure"]
"#;
        assert!(parse_with_external(text, &[]).is_ok());
    }

    #[test]
    fn dep_shape_params_validate_and_round_trip() {
        // ADR-039 § DAG-002: `core.dep-shape` is builtin-compiled under the
        // `core.` prefix, so its params are stored loosely (no key schema)
        // and must not raise cfg.rule-unknown or a param error — even with
        // the reserved `allows` param present.
        let text = r#"
[SPEC]
rules = ["core.dep-shape"]
[SPEC."core.dep-shape"]
requires = ["PRD"]
allows = ["ADR", "PRD"]
"#;
        let config = parse(text).expect("core.dep-shape config must validate");
        assert!(
            !config
                .kernel_messages
                .iter()
                .any(|m| m.code == "cfg.rule-unknown"),
            "core.dep-shape must not warn as unknown: {:?}",
            config.kernel_messages
        );
        let spec = config.namespaces.get("SPEC").unwrap();
        let params = spec.params.get("core.dep-shape").unwrap();
        assert_eq!(params["requires"], json!(["PRD"]));
        assert_eq!(params["allows"], json!(["ADR", "PRD"]));
    }

    #[test]
    fn allowed_values_table_round_trips() {
        let text = r#"
[ADR]
rules = ["core.allowed-values"]
[ADR."core.allowed-values"]
status = ["draft", "accepted", "rejected"]
kind = ["architecture", "product"]
"#;
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        let params = adr.params.get("core.allowed-values").unwrap();
        assert_eq!(params["status"], json!(["draft", "accepted", "rejected"]));
        assert_eq!(params["kind"], json!(["architecture", "product"]));
    }

    #[test]
    fn allowed_values_rejects_non_array_member() {
        let text = r#"
[ADR]
rules = ["core.allowed-values"]
[ADR."core.allowed-values"]
status = "not-an-array"
"#;
        let err = parse(text).unwrap_err();
        match err {
            ConfigError::RuleParamsInvalid { detail, .. } => {
                assert!(detail.contains("`status`"));
                assert!(detail.contains("array of strings"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn sources_markdown_file_emits_warning_and_is_ignored() {
        let text = r#"
[sources.markdown-file]
some_param = "value"
"#;
        let config = parse(text).unwrap();
        assert!(!config.sources.contains_key("markdown-file"));
        assert_eq!(config.kernel_messages.len(), 1);
        assert_eq!(config.kernel_messages[0].code, "cfg.reserved-source");
        assert!(config.kernel_messages[0].message.contains("markdown-file"));
    }

    #[test]
    fn min_docs_reserved_count_warns_but_keeps_rule_active() {
        // ADR-048 § SEED-002: `count` is reserved for a future cardinality
        // floor but pinned to 1 in this release. A non-1 value must not be
        // silently inert (the whole point of the warning) and must not
        // disable the seed it was attached to.
        let text = r#"
[POLICY]
paths = ["docs/policies/**"]
rules = ["core.min-docs"]

[POLICY."core.min-docs"]
count = 2
"#;
        let config = parse(text).unwrap();
        assert_eq!(
            config.namespaces["POLICY"].rules,
            vec!["core.min-docs".to_string()],
            "a reserved param must not drop the rule from the namespace"
        );
        let warn = config
            .kernel_messages
            .iter()
            .find(|m| m.code == "cfg.reserved-param")
            .expect("count = 2 should emit a cfg.reserved-param warning");
        assert!(warn.message.contains("count"));
    }

    #[test]
    fn min_docs_count_one_is_silent() {
        let text = r#"
[POLICY]
paths = ["docs/policies/**"]
rules = ["core.min-docs"]

[POLICY."core.min-docs"]
count = 1
"#;
        let config = parse(text).unwrap();
        assert!(
            !config
                .kernel_messages
                .iter()
                .any(|m| m.code == "cfg.reserved-param"),
            "count = 1 matches the effective floor and must not warn"
        );
    }

    #[test]
    fn min_docs_without_count_is_silent() {
        let text = r#"
[POLICY]
paths = ["docs/policies/**"]
rules = ["core.min-docs"]
"#;
        let config = parse(text).unwrap();
        assert!(
            !config
                .kernel_messages
                .iter()
                .any(|m| m.code == "cfg.reserved-param"),
            "an absent count must not warn"
        );
    }

    #[test]
    fn other_sources_stored_with_params() {
        let text = r#"
[sources.jira-stub]
project = "AUDIT"
"#;
        let config = parse(text).unwrap();
        let params = config.sources.get("jira-stub").unwrap();
        assert_eq!(params["project"], json!("AUDIT"));
    }

    #[test]
    fn rules_list_must_be_array_of_strings() {
        let text = r#"
[ADR]
rules = "core.frontmatter"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::RulesListInvalid { .. }));
    }

    #[test]
    fn config_namespace_config_falls_back_to_zero_config() {
        let config = Config::default();
        let ns = config.namespace_config("ADR");
        assert_eq!(
            ns.rules,
            ZERO_CONFIG_RULES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn config_error_code_mapping() {
        let err = ConfigError::RuleParamsInvalid {
            namespace: "ADR".into(),
            code: "core.required-headings".into(),
            detail: "x".into(),
        };
        assert_eq!(err.code(), Some("cfg.rule-params-invalid"));
    }

    // -- global config tests ---------------------------------------------

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn global_namespace_loaded_when_no_local() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        write(
            &global.join("namespaces/ADR.toml"),
            r#"
rules = ["core.frontmatter", "core.id"]
"#,
        );
        let config = load_with_global(tmp.path(), Some(&global)).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert_eq!(adr.rules, vec!["core.frontmatter", "core.id"]);
    }

    #[test]
    fn local_namespace_overrides_global_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        // Global config lists three rules:
        write(
            &global.join("namespaces/ADR.toml"),
            r#"rules = ["core.frontmatter", "core.id", "core.dep-cycle"]"#,
        );
        // Local config lists only one, and that single rule wins —
        // there's NO merging.
        write(
            &tmp.path().join("ctxgrd.toml"),
            r#"
[ADR]
rules = ["core.id-unique"]
"#,
        );
        let config = load_with_global(tmp.path(), Some(&global)).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert_eq!(adr.rules, vec!["core.id-unique"]);
    }

    #[test]
    fn global_unaffected_namespaces_kept_when_local_overrides_a_different_one() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        write(
            &global.join("namespaces/ADR.toml"),
            r#"rules = ["core.frontmatter"]"#,
        );
        write(
            &global.join("namespaces/PRD.toml"),
            r#"rules = ["core.id"]"#,
        );
        write(
            &tmp.path().join("ctxgrd.toml"),
            r#"
[ADR]
rules = ["core.dep-cycle"]
"#,
        );
        let config = load_with_global(tmp.path(), Some(&global)).unwrap();
        assert_eq!(
            config.namespaces.get("ADR").unwrap().rules,
            vec!["core.dep-cycle"]
        );
        assert_eq!(config.namespaces.get("PRD").unwrap().rules, vec!["core.id"]);
    }

    // -- ADR 007 § DOC-002 / DOC-004: `[<NS>].paths` schema -------------

    #[test]
    fn namespace_paths_default_is_none() {
        let text = r#"
[ADR]
rules = ["core.frontmatter"]
"#;
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert!(adr.paths.is_none());
        assert!(adr.path_patterns.is_empty());
    }

    #[test]
    fn namespace_paths_single_glob_compiles_and_matches() {
        let text = r#"
[ADR]
rules = ["core.frontmatter"]
paths = ["docs/adrs/**"]
"#;
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert_eq!(adr.path_patterns, vec!["docs/adrs/**".to_string()]);
        let set = adr.paths.as_ref().expect("paths compiled");
        assert!(set.is_match("docs/adrs/001.md"));
        assert!(set.is_match("docs/adrs/sub/nested.md"));
        assert!(!set.is_match("notes/idea.md"));
    }

    #[test]
    fn namespace_paths_list_matches_any_entry() {
        let text = r#"
[ADR]
rules = ["core.frontmatter"]
paths = ["docs/adrs/**", "vendor/lib/docs/adrs/**"]
"#;
        let config = parse(text).unwrap();
        let set = config
            .namespaces
            .get("ADR")
            .unwrap()
            .paths
            .as_ref()
            .unwrap();
        assert!(set.is_match("docs/adrs/001.md"));
        assert!(set.is_match("vendor/lib/docs/adrs/007.md"));
        assert!(!set.is_match("docs/prds/001.md"));
    }

    #[test]
    fn namespace_paths_classification_is_order_independent() {
        // DOC-004: ordering of entries within `paths` MUST NOT affect
        // classification — the matcher is OR-of-matches.
        let a = r#"
[ADR]
rules = []
paths = ["docs/adrs/**", "vendor/lib/docs/adrs/**"]
"#;
        let b = r#"
[ADR]
rules = []
paths = ["vendor/lib/docs/adrs/**", "docs/adrs/**"]
"#;
        let set_a = parse(a)
            .unwrap()
            .namespaces
            .get("ADR")
            .unwrap()
            .paths
            .clone()
            .unwrap();
        let set_b = parse(b)
            .unwrap()
            .namespaces
            .get("ADR")
            .unwrap()
            .paths
            .clone()
            .unwrap();
        for sample in [
            "docs/adrs/001.md",
            "vendor/lib/docs/adrs/007.md",
            "elsewhere/x.md",
        ] {
            assert_eq!(set_a.is_match(sample), set_b.is_match(sample));
        }
    }

    #[test]
    fn namespace_paths_empty_list_is_none() {
        let text = r#"
[ADR]
rules = []
paths = []
"#;
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert!(adr.paths.is_none());
        assert!(adr.path_patterns.is_empty());
    }

    #[test]
    fn namespace_paths_rejects_non_array() {
        let text = r#"
[ADR]
rules = []
paths = "docs/adrs/**"
"#;
        let err = parse(text).unwrap_err();
        match err {
            ConfigError::PathsInvalid {
                namespace, detail, ..
            } => {
                assert_eq!(namespace, "ADR");
                assert!(detail.contains("array"), "got detail: {detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn namespace_paths_rejects_non_string_entries() {
        let text = r#"
[ADR]
rules = []
paths = ["docs/adrs/**", 42]
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::PathsInvalid { .. }));
    }

    #[test]
    fn namespace_paths_rejects_invalid_glob() {
        // globset rejects unmatched `[` brackets — use that for the
        // failure case rather than guessing at platform-specific
        // syntax errors.
        let text = r#"
[ADR]
rules = []
paths = ["docs/[adrs/**"]
"#;
        let err = parse(text).unwrap_err();
        match err {
            ConfigError::PathsInvalid { pattern, .. } => {
                assert_eq!(pattern, "docs/[adrs/**");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn namespace_paths_does_not_leak_into_params() {
        // Belt-and-braces: `paths` must not be slurped into the
        // per-rule params map, where it would collide with a
        // rule-code key shaped `paths`.
        let text = r#"
[ADR]
rules = ["core.frontmatter"]
paths = ["docs/adrs/**"]
"#;
        let config = parse(text).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert!(!adr.params.contains_key("paths"));
    }

    #[test]
    fn config_error_paths_invalid_code() {
        let err = ConfigError::PathsInvalid {
            namespace: "ADR".into(),
            pattern: "docs/[".into(),
            detail: "x".into(),
        };
        assert_eq!(err.code(), Some("cfg.paths-invalid"));
    }

    #[test]
    fn global_namespace_paths_round_trip() {
        // Global `~/.ctxgrd/namespaces/<NS>.toml` files share the
        // same parser — `paths` MUST work there too.
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        write(
            &global.join("namespaces/ADR.toml"),
            r#"
rules = ["core.frontmatter"]
paths = ["docs/adrs/**"]
"#,
        );
        let config = load_with_global(tmp.path(), Some(&global)).unwrap();
        let adr = config.namespaces.get("ADR").unwrap();
        assert_eq!(adr.path_patterns, vec!["docs/adrs/**".to_string()]);
        assert!(adr.paths.is_some());
    }

    #[test]
    fn missing_global_dir_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = load_with_global(tmp.path(), None).unwrap();
        assert!(config.namespaces.is_empty());
    }

    // -- ADR-118 § STG-002: `[pipeline]` is refused, not ignored --------

    #[test]
    fn stg002_a_declared_pipeline_is_refused() {
        let err = parse("[ADR]\nrules = []\n\n[pipeline]\nstages = [\"ADR\"]\n").unwrap_err();
        assert!(matches!(err, ConfigError::PipelineRemoved), "got {err:?}");
        assert_eq!(err.code(), Some("cfg.pipeline-removed"));
    }

    #[test]
    fn stg002_a_bare_gate_table_is_refused_too() {
        // `[pipeline.gate]` with no `[pipeline]` header still creates the
        // `pipeline` table in TOML, so the check must catch it. Missing this
        // would leave the gate half of the layer quietly accepted.
        let err = parse("[ADR]\nrules = []\n\n[pipeline.gate]\nADR = \"any:accepted\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::PipelineRemoved), "got {err:?}");
    }

    #[test]
    fn stg002_a_config_without_a_pipeline_still_loads() {
        // The paired half. Without it, "every `[pipeline]` config fails" is
        // indistinguishable from "every config fails".
        let config = parse("[ADR]\nrules = []\n").unwrap();
        assert!(config.namespaces.contains_key("ADR"));
    }

    #[test]
    fn stg002_the_error_names_the_adr_so_the_fix_is_findable() {
        let err = parse("[pipeline]\nstages = [\"ADR\"]\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ADR-118"), "message must name the decision: {msg}");
        assert!(msg.contains("[pipeline]"), "message must name the key: {msg}");
    }

    #[test]
    fn todo_listed_global_enabled_parses() {
        let config = parse("[todo.listed]\nenabled = true\n").unwrap();
        assert!(config.todo_listed_global);
    }

    #[test]
    fn todo_listed_global_absent_is_false() {
        let config = parse("[ADR]\nrules = []\n").unwrap();
        assert!(!config.todo_listed_global);
    }

    #[test]
    fn todo_listed_global_enabled_false_is_false() {
        let config = parse("[todo.listed]\nenabled = false\n").unwrap();
        assert!(!config.todo_listed_global);
    }

    // -- ADR-084 § CHG-002: the [changelog] whitelist + status→section map ---

    #[test]
    fn changelog_absent_is_none() {
        let config = parse("[ADR]\nrules = []\n").unwrap();
        assert!(config.changelog.is_none());
    }

    #[test]
    fn changelog_parses_whitelist_since_and_per_ns_mapping() {
        let text = r#"
[BUG]
paths = ["docs/bugs/**"]
rules = ["core.frontmatter"]

[changelog]
namespaces = ["BUG"]
since = "v0.48.0"

[changelog.BUG]
when = "fixed"
section = "Fixed"
"#;
        let cfg = parse(text).unwrap().changelog.expect("changelog present");
        assert_eq!(cfg.namespaces, vec!["BUG".to_string()]);
        assert_eq!(cfg.since.as_deref(), Some("v0.48.0"));
        let bug = &cfg.entries["BUG"];
        assert_eq!(bug.when, "fixed");
        assert_eq!(bug.section, "Fixed");
    }

    #[test]
    fn changelog_since_is_optional() {
        let text = "[changelog]\nnamespaces = [\"BUG\"]\n\n[changelog.BUG]\nwhen = \"fixed\"\nsection = \"Fixed\"\n";
        let cfg = parse(text).unwrap().changelog.unwrap();
        assert_eq!(cfg.since, None);
    }

    #[test]
    fn changelog_missing_namespaces_is_error() {
        let text = "[changelog]\n[changelog.BUG]\nwhen = \"fixed\"\nsection = \"Fixed\"\n";
        assert!(matches!(
            parse(text).unwrap_err(),
            ConfigError::ChangelogInvalid { .. }
        ));
    }

    #[test]
    fn changelog_whitelisted_ns_without_mapping_is_error() {
        // BUG is whitelisted but has no [changelog.BUG] table.
        let text = "[changelog]\nnamespaces = [\"BUG\"]\n";
        let err = parse(text).unwrap_err();
        assert!(
            matches!(&err, ConfigError::ChangelogInvalid { detail } if detail.contains("BUG")),
            "expected ChangelogInvalid naming BUG, got {err:?}"
        );
    }

    #[test]
    fn changelog_mapping_without_when_or_section_is_error() {
        let text = "[changelog]\nnamespaces = [\"BUG\"]\n\n[changelog.BUG]\nwhen = \"fixed\"\n";
        assert!(matches!(
            parse(text).unwrap_err(),
            ConfigError::ChangelogInvalid { .. }
        ));
    }

    #[test]
    fn changelog_orphan_mapping_is_error() {
        // A [changelog.CR] mapping for a namespace not in the whitelist.
        let text = r#"
[changelog]
namespaces = ["BUG"]

[changelog.BUG]
when = "fixed"
section = "Fixed"

[changelog.CR]
when = "implemented"
section = "Changed"
"#;
        let err = parse(text).unwrap_err();
        assert!(
            matches!(&err, ConfigError::ChangelogInvalid { detail } if detail.contains("CR")),
            "expected ChangelogInvalid naming CR, got {err:?}"
        );
    }
}
