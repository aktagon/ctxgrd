//! Generate a regulation pack's `pack.toml` from its canonical extract
//! (ADR-066 § CMP-005). The regulation text is the single source of
//! truth: `packs/<regulation>/regulation.json` carries the normative
//! skeleton (summary, closed vocabularies, namespace set with paths,
//! required metadata, freshness), and this generator projects it to the
//! `[<NS>]` blocks the binary embeds via `include_str!`.
//!
//! Shipped as a cargo *example*, not a `[[bin]]`, so it never becomes an
//! installed binary. Run it on a regulation revision and review the diff:
//!
//!   cargo run --example gen_compliance_pack -- gdpr
//!
//! An optional second argument overrides the repo root the generator reads
//! and writes under (default: `CARGO_MANIFEST_DIR`); the round-trip test
//! uses it to regenerate into an isolated copy without touching the
//! committed `pack.toml`.
//!
//! Output is deterministic — namespace, rule, key, and value order all
//! follow the order they appear in the JSON (concrete `Vec` fields, no
//! map reordering), so an unchanged extract reproduces the committed
//! `pack.toml` byte-for-byte.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process;

use serde::Deserialize;

/// The canonical extract schema. Concrete structs (no `serde_json::Value`
/// soup) so the projection is total and order-preserving.
#[derive(Deserialize)]
struct Regulation {
    /// Free-text provenance note (e.g. the DRAFT-catalog caveat). Carried in
    /// the extract for the human reviewing the law; not emitted.
    #[serde(default, rename = "_note")]
    #[allow(dead_code)]
    note: Option<String>,
    /// The rule edition the catalog encodes (HIPAA-001 "edition recorded").
    /// gdpr's extract omits it; hipaa records it. Surfaced to the human in
    /// the extract and woven into `summary`, not emitted as a separate line.
    #[serde(default)]
    #[allow(dead_code)]
    edition: Option<String>,
    summary: String,
    /// Base packs this pack depends on (ADR-068 § PKD-004). Emitted as the
    /// `# depends:` comment; `pack add` pulls them transitively. gdpr/hipaa
    /// declare `["security"]`.
    #[serde(default)]
    depends: Vec<String>,
    vocabularies: BTreeMap<String, Vocabulary>,
    namespaces: Vec<Namespace>,
}

#[derive(Deserialize)]
struct Vocabulary {
    #[allow(dead_code)]
    cite: String,
    values: Vec<VocabTerm>,
}

/// A single vocabulary term. Either a plain string (gdpr's lawful bases)
/// or a tagged object carrying an `id` plus arbitrary extra fields the
/// emitter ignores — the `requirement`/`citation` flags hipaa's safeguard
/// catalog records for a future conditional rule, kept out of the emitted
/// `core.allowed-values` (which is the id list only). Untagged so the
/// plain-string form deserializes exactly as before: gdpr's extract is
/// unchanged and regenerates byte-for-byte.
#[derive(Deserialize)]
#[serde(untagged)]
enum VocabTerm {
    Plain(String),
    Tagged {
        id: String,
        /// The Required/Addressable flag (HIPAA safeguards). Projected by
        /// the generator into the `hipaa.safeguard-evidence` rule's
        /// `addressable` param; ignored for vocabularies that omit it.
        #[serde(default)]
        requirement: Option<String>,
        #[serde(flatten)]
        _rest: serde_json::Map<String, serde_json::Value>,
    },
}

impl VocabTerm {
    /// The value emitted into `core.allowed-values`: the string itself, or
    /// the tagged term's `id`.
    fn emitted(&self) -> &str {
        match self {
            VocabTerm::Plain(s) => s,
            VocabTerm::Tagged { id, .. } => id,
        }
    }

    /// The Required/Addressable flag, when the term carries one.
    fn requirement(&self) -> Option<&str> {
        match self {
            VocabTerm::Plain(_) => None,
            VocabTerm::Tagged { requirement, .. } => requirement.as_deref(),
        }
    }
}

#[derive(Deserialize)]
struct Namespace {
    name: String,
    comment: String,
    paths: Vec<String>,
    rules: Vec<String>,
    required_metadata: Vec<String>,
    #[serde(default)]
    allowed_values: Vec<String>,
    #[serde(default)]
    calendar_freshness: Option<CalendarFreshness>,
    /// When present, emit the `hipaa.safeguard-evidence` param block: the
    /// Addressable id subset of the named vocabulary. Keeps the
    /// Required/Addressable distinction sourced from the canonical extract
    /// rather than hand-listed in the rule (ADR-066 § CMP-005, HIPAA-002).
    #[serde(default)]
    safeguard_evidence: Option<SafeguardEvidence>,
    /// When present, emit a generic conditional-evidence param block (e.g.
    /// `soc2.control-evidence`, ADR-069 § SOC-002). Unlike `safeguard_evidence`
    /// it carries no derived Addressable subset — SOC 2 has no
    /// addressable/required split — so the params are taken literally from the
    /// extract: the evidence fields whose non-empty value satisfies the rule
    /// and the statuses that mark a control out of scope.
    #[serde(default)]
    control_evidence: Option<ControlEvidence>,
}

#[derive(Deserialize)]
struct CalendarFreshness {
    field: String,
    stale_days: u32,
}

#[derive(Deserialize)]
struct SafeguardEvidence {
    /// The vocabulary whose tagged terms carry the Required/Addressable
    /// `requirement` flag (e.g. `safeguard`).
    vocabulary: String,
}

#[derive(Deserialize)]
struct ControlEvidence {
    /// The rule code this block configures (e.g. `soc2.control-evidence`).
    rule: String,
    /// Metadata fields whose non-empty value counts as evidence
    /// (SOC 2's `evidence_link`). Emitted as the `evidence-fields` param.
    #[serde(default)]
    evidence_fields: Vec<String>,
    /// `status` values that mark a control out of scope and exempt it
    /// (SOC 2's `not-applicable`). Emitted as the `out-of-scope-status` param.
    #[serde(default)]
    out_of_scope_status: Vec<String>,
}

fn main() {
    let regulation = match std::env::args().nth(1) {
        Some(name) => name,
        None => {
            eprintln!(
                "usage: cargo run --example gen_compliance_pack -- <regulation> [root]"
            );
            process::exit(2);
        }
    };

    // Repo root the generator reads and writes under. Defaults to the
    // compile-time manifest dir; an explicit second argument overrides it so
    // the round-trip test can regenerate into an isolated copy instead of
    // overwriting the committed pack.toml.
    let manifest_dir = match std::env::args().nth(2) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")),
    };
    let extract_path = manifest_dir
        .join("packs")
        .join(&regulation)
        .join("regulation.json");
    let pack_path = manifest_dir
        .join("packs")
        .join(&regulation)
        .join("pack.toml");

    let raw = std::fs::read_to_string(&extract_path).unwrap_or_else(|e| {
        eprintln!("cannot read {}: {e}", extract_path.display());
        process::exit(2);
    });
    let reg: Regulation = serde_json::from_str(&raw).unwrap_or_else(|e| {
        eprintln!("invalid extract {}: {e}", extract_path.display());
        process::exit(2);
    });

    let toml = render(&reg);
    std::fs::write(&pack_path, &toml).unwrap_or_else(|e| {
        eprintln!("cannot write {}: {e}", pack_path.display());
        process::exit(2);
    });
    eprintln!("wrote {}", pack_path.display());
}

/// Format a string list as a single-line TOML array: `["a", "b"]`.
fn inline_list(values: &[String]) -> String {
    let quoted: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

fn render(reg: &Regulation) -> String {
    let mut out = String::new();
    writeln!(out, "# summary: {}", reg.summary).unwrap();
    if !reg.depends.is_empty() {
        writeln!(out, "# depends: {}", reg.depends.join(", ")).unwrap();
    }

    for ns in &reg.namespaces {
        out.push('\n');
        // Comment block above the [NS] header.
        writeln!(out, "# {}", ns.comment).unwrap();
        writeln!(out, "[{}]", ns.name).unwrap();
        writeln!(out, "paths = {}", inline_list(&ns.paths)).unwrap();
        writeln!(out, "rules = [").unwrap();
        for rule in &ns.rules {
            writeln!(out, "  \"{rule}\",").unwrap();
        }
        writeln!(out, "]").unwrap();

        if !ns.required_metadata.is_empty() {
            out.push('\n');
            writeln!(out, "[{}.\"core.required-metadata\"]", ns.name).unwrap();
            writeln!(out, "keys = {}", inline_list(&ns.required_metadata)).unwrap();
        }

        if !ns.allowed_values.is_empty() {
            out.push('\n');
            writeln!(out, "[{}.\"core.allowed-values\"]", ns.name).unwrap();
            for field in &ns.allowed_values {
                let vocab = reg.vocabularies.get(field).unwrap_or_else(|| {
                    eprintln!("namespace {} cites unknown vocabulary `{field}`", ns.name);
                    process::exit(2);
                });
                let values: Vec<String> =
                    vocab.values.iter().map(|t| t.emitted().to_string()).collect();
                writeln!(out, "{field} = {}", inline_list(&values)).unwrap();
            }
        }

        if let Some(fresh) = &ns.calendar_freshness {
            out.push('\n');
            writeln!(out, "[{}.\"core.calendar-freshness\"]", ns.name).unwrap();
            writeln!(out, "field = \"{}\"", fresh.field).unwrap();
            writeln!(out, "stale_days = {}", fresh.stale_days).unwrap();
        }

        if let Some(evidence) = &ns.safeguard_evidence {
            let vocab = reg.vocabularies.get(&evidence.vocabulary).unwrap_or_else(|| {
                eprintln!(
                    "namespace {} cites unknown safeguard_evidence vocabulary `{}`",
                    ns.name, evidence.vocabulary
                );
                process::exit(2);
            });
            let addressable: Vec<String> = vocab
                .values
                .iter()
                .filter(|t| t.requirement() == Some("addressable"))
                .map(|t| t.emitted().to_string())
                .collect();
            out.push('\n');
            writeln!(out, "[{}.\"hipaa.safeguard-evidence\"]", ns.name).unwrap();
            writeln!(out, "addressable = {}", inline_list(&addressable)).unwrap();
        }

        if let Some(evidence) = &ns.control_evidence {
            out.push('\n');
            writeln!(out, "[{}.\"{}\"]", ns.name, evidence.rule).unwrap();
            if !evidence.evidence_fields.is_empty() {
                writeln!(out, "evidence-fields = {}", inline_list(&evidence.evidence_fields))
                    .unwrap();
            }
            if !evidence.out_of_scope_status.is_empty() {
                writeln!(
                    out,
                    "out-of-scope-status = {}",
                    inline_list(&evidence.out_of_scope_status)
                )
                .unwrap();
            }
        }
    }

    out
}
