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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use thiserror::Error;
use toml::Value as TomlValue;

use crate::diagnostic::KernelMessage;

pub const CORE_RULES: [&str; 10] = [
    "core.frontmatter",
    "core.id",
    "core.id-unique",
    "core.dep-resolved",
    "core.dep-cycle",
    "core.cross-ref",
    "core.required-headings",
    "core.required-metadata",
    "core.allowed-values",
    "core.requirement-ref",
];

/// Core rules whose zero-config default includes them — every
/// namespace without explicit config gets exactly this set. The brief
/// calls these "the six non-parameterized core rules."
pub const ZERO_CONFIG_RULES: [&str; 6] = [
    "core.frontmatter",
    "core.id",
    "core.id-unique",
    "core.dep-resolved",
    "core.dep-cycle",
    "core.cross-ref",
];

const PARAMETERIZED_CORE_RULES: [&str; 3] = [
    "core.required-headings",
    "core.required-metadata",
    "core.allowed-values",
];

/// Whether `code` names a builtin-compiled rule (dispatched in-process,
/// not an external subprocess script). Derived from `BUILTIN_RULES`
/// so the resolver and the registry cannot drift (ADR-024 § REG-002).
pub fn is_builtin_compiled(code: &str) -> bool {
    crate::builtin_rules::BUILTIN_RULES
        .iter()
        .any(|r| r.code == code)
}

/// Whether `code` is a *file-level* builtin-compiled rule — one that lints
/// id-less path-claimed singletons rather than id-keyed documents.
/// Derived from `BUILTIN_RULES` (ADR-024 § REG-002).
pub fn is_file_level_compiled(code: &str) -> bool {
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
pub fn is_reserved_builtin_prefix(code: &str) -> bool {
    let ns = rule_prefix(code);
    crate::builtin_rules::BUILTIN_RULES
        .iter()
        .any(|r| rule_prefix(r.code) == ns)
}

impl Config {
    /// Namespaces that carry a builtin-compiled rule. These are
    /// *file-level*: their documents (CLAUDE.md / AGENTS.md / TODO.md)
    /// are id-less singletons that never become id-keyed [`Document`]s,
    /// so they are linted by walking path-claimed files directly (see
    /// [`crate::agent_guide::scan_file_level`]) rather than through the
    /// per-document rule loop.
    pub fn file_level_namespaces(&self) -> Vec<&str> {
        self.namespaces
            .iter()
            .filter(|(_, cfg)| cfg.rules.iter().any(|c| is_file_level_compiled(c)))
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
}

impl NamespaceConfig {
    /// Zero-config default — the six non-parameterized core rules,
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
    #[error("[ext.namespace-reserved] external rule directory '{path}' uses the reserved 'core' namespace")]
    NamespaceReserved { path: PathBuf },
    #[error(
        "[cfg.ignore-invalid] `[ignore].patterns` entry {pattern:?} is not a valid glob: {detail}"
    )]
    IgnorePatternInvalid { pattern: String, detail: String },
    #[error("[cfg.references-invalid] `[references]` block invalid: {detail}")]
    ReferencesInvalid { detail: String },
    #[error("[cfg.paths-invalid] `[{namespace}].paths` invalid: {detail}")]
    PathsInvalid {
        namespace: String,
        pattern: String,
        detail: String,
    },
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
            Self::IgnorePatternInvalid { .. } => Some("cfg.ignore-invalid"),
            Self::ReferencesInvalid { .. } => Some("cfg.references-invalid"),
            Self::PathsInvalid { .. } => Some("cfg.paths-invalid"),
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

/// Testable entry point: pass `None` for "no global config" or
/// `Some(path)` pointing at a specific `.ctxgrd` directory.
pub fn load_with_global(root: &Path, global_dir: Option<&Path>) -> Result<Config, ConfigError> {
    let external = discover_external_rules_with_global(root, global_dir)?;

    // Start with global namespace configs, if any.
    let mut config = load_global_namespaces(&external, root, global_dir)?;

    // Local ctxgrd.toml overrides per namespace.
    let path = root.join("ctxgrd.toml");
    if !path.exists() {
        return Ok(config);
    }
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
    Ok(config)
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
    // [ignore] is local-only for v1 — per-user global file patterns
    // would require careful semantics (who overrides whom?); deferred.
    if local.ignore.is_some() {
        global.ignore = local.ignore;
        global.ignore_patterns = local.ignore_patterns;
    }
    // [references] is also local-only for v1: global scan globs make
    // even less sense (paths only resolve against the lint root).
    if !local.reference_scan_globs.is_empty() {
        global.reference_scan_globs = local.reference_scan_globs;
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
pub fn discover_external_rules_with_global(
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
pub fn global_ctxgrd_dir() -> Option<PathBuf> {
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

fn parse_ignore(val: TomlValue, config: &mut Config) -> Result<(), ConfigError> {
    let TomlValue::Table(mut table) = val else {
        return Ok(());
    };
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

fn parse_sources(val: TomlValue, config: &mut Config) -> Result<(), ConfigError> {
    let TomlValue::Table(sources) = val else {
        return Ok(());
    };
    for (name, source_val) in sources {
        if name == "markdown-file" {
            config.kernel_messages.push(KernelMessage::warning(
                "cfg.reserved-source",
                "[sources.markdown-file] is reserved and was ignored",
            ));
            continue;
        }
        let params = toml_to_json(&source_val);
        config.sources.insert(name, params);
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

    let (paths, path_patterns) = match table.remove("paths") {
        Some(v) => parse_namespace_paths(namespace, v)?,
        None => (None, Vec::new()),
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

    Ok((
        NamespaceConfig {
            rules: valid_rules,
            params,
            paths,
            path_patterns,
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
        if !CORE_RULES.contains(&code) {
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
}
