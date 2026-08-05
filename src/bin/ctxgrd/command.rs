//! The `Command` trait + typed `Outcome` (ADR-101 § ENF-002/003/004).
//!
//! Every user-facing subcommand is a type implementing [`Command`]. The
//! associated `Json` result type is the compile-time proof that the command
//! has a `--format json` path (ENF-002): a command with no structured payload
//! declares the explicit exemption `type Json = Prose` / `type Json = Server`,
//! and omitting the associated type is a compile error — so "a command
//! without `--format json`" cannot ship past `cargo check`.
//!
//! Exit codes derive from **one** mapping over the typed [`Outcome`] in
//! [`emit`] (ENF-003): `Did`/`Noop` → `0`, `Findings` → `1`, and every
//! [`KernelError`] → `2`. No handler returns a bare code or calls
//! `process::exit`; the `new`-vs-`init` "exists" divergence (both exit 0 by
//! construction, as `Noop`) is now unrepresentable.
//!
//! The dispatcher — not the handlers — writes the machine `Json` payload to
//! stdout in JSON mode (ENF-004), so `… --format json | jq` can never be
//! broken by a stray handler `println!` on the machine stream. Handlers still
//! own their *human* (text-mode) rendering: it is per-command (tables,
//! receipts, next-steps) and legitimately lands on stdout for read commands,
//! so a single `human() -> String` to stderr could not reproduce it
//! byte-for-byte. The split this module enforces is therefore "the dispatcher
//! owns the JSON stream; handlers own the human stream," which is the
//! behaviour-preserving realisation of ENF-004.

use std::path::PathBuf;
use std::process::ExitCode;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::run::ExitStatus;

use super::emit_error;

/// Shared context every command's [`Command::run`] receives. Only the project
/// root today; a seam for future ambient state (verbosity, clock) without
/// re-threading every handler signature.
pub(crate) struct Ctx {
    pub(crate) root: PathBuf,
}

/// A kernel / config / IO error: exit `2` via the one map in [`emit`].
///
/// Either it carries the diagnostic for the dispatcher to render, or it
/// signals the handler already wrote its own error output — needed where a
/// handler interleaves the diagnostic with other stderr it must still flush
/// (e.g. `init`'s body-header advisory).
pub(crate) enum KernelError {
    /// The dispatcher renders this diagnostic to stderr, then exits `2`.
    Report(Box<Diagnostic>),
    /// The handler already emitted its own error output; the dispatcher only
    /// maps it to exit `2`.
    Reported,
}

impl KernelError {
    /// Wrap a diagnostic the dispatcher will render.
    pub(crate) fn report(d: Diagnostic) -> Self {
        KernelError::Report(Box::new(d))
    }
}

/// A command's typed result, carrying its machine payload and — via the one
/// map in [`emit`] — its exit code (ENF-003).
pub(crate) enum Outcome<T> {
    /// Did the thing → exit `0`.
    Did(T),
    /// Benign refuse-to-act ("already exists") → exit `0`. A named variant so
    /// every "already there" is one place that maps to `0` (ADR-086 § WIRE-007).
    Noop(T),
    /// Diagnostics / drift / not-done present → exit `1`.
    Findings(T),
}

impl<T> Outcome<T> {
    /// The machine payload, borrowed for the dispatcher's JSON render.
    pub(crate) fn payload(&self) -> &T {
        match self {
            Outcome::Did(t) | Outcome::Noop(t) | Outcome::Findings(t) => t,
        }
    }

    /// The one `Outcome → ExitStatus` map (ENF-003).
    pub(crate) fn status(&self) -> ExitStatus {
        match self {
            Outcome::Did(_) | Outcome::Noop(_) => ExitStatus::Ok,
            Outcome::Findings(_) => ExitStatus::LintFailure,
        }
    }
}

/// The command contract. Implementors are the ~16 subcommand types.
pub(crate) trait Command {
    /// The machine-readable result. Its presence is the ENF-002 guarantee.
    /// Exempt commands declare [`Prose`] or [`Server`].
    type Json: serde::Serialize;

    /// `true` when the command has already written its machine payload to
    /// stdout itself. The category-3 escape for `lint`/`status`, whose
    /// bespoke, multi-shape wire rendering + verdict-driven exit predate this
    /// trait: the dispatcher then only maps the returned [`Outcome`] to an
    /// exit code and does NOT re-serialize `Json`. Defaults `false` — the
    /// dispatcher owns the stdout JSON write (ENF-004).
    const SELF_RENDERS_JSON: bool = false;

    /// Whether this invocation requested `--format json`. The dispatcher reads
    /// it (before [`Command::run`] consumes `self`) to decide whether to write
    /// the machine payload. Defaults `false` for exempt / no-format commands.
    fn emits_json(&self) -> bool {
        false
    }

    /// Do the work. In text mode, render the human output to its streams here
    /// (byte-exact, reusing the existing renderers). In JSON mode, render only
    /// any stderr hints — the dispatcher owns the stdout payload. Errors carry
    /// their own diagnostic via [`KernelError`].
    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError>;

    /// Serialize the machine payload for stdout. Defaults to compact; commands
    /// that historically pretty-printed override this so the refactor stays
    /// byte-preserving.
    fn render_json(out: &Self::Json) -> String {
        serde_json::to_string(out).expect("Command::Json is Serialize")
    }
}

/// ENF-002 exemption: a command that emits unstructured **prose** (`docs`, and
/// the `new rule` scaffold sub-mode) — no `--format json`. A named marker so
/// the exemption is a decision the compiler sees, not a silent gap.
#[derive(serde::Serialize)]
pub(crate) struct Prose;

/// ENF-002 exemption: a long-running **server** (`lsp` stdio protocol, `serve`
/// HTTP viewer) whose stdout is a protocol stream / URL banner, not a one-shot
/// JSON payload. ADR-101 named `docs`+`lsp`; `serve` (ADR-097) is the third
/// server exemption, recorded here explicitly.
#[derive(serde::Serialize)]
pub(crate) struct Server;

/// Category-3 marker (ADR-101 Open Questions): `lint`/`status`/`changelog` DO
/// emit `--format json`, but their multiple wire shapes / root-based library
/// renderers and verdict-driven exit are rendered inside [`Command::run`] (they
/// set `SELF_RENDERS_JSON = true`), so the dispatcher never serializes this
/// type. Distinct from [`Prose`]/[`Server`] (which mean "no JSON at all"): this
/// type is the ENF-002 presence proof for a command whose JSON is real but
/// self-rendered for byte-fidelity.
#[derive(serde::Serialize)]
pub(crate) struct SelfRendered;

/// The single dispatch: run the command, own the machine-stream render
/// (ENF-004), and map its [`Outcome`]/[`KernelError`] to the one exit code
/// (ENF-003). Every subcommand arm in `main` routes through here.
pub(crate) fn emit<C: Command>(cmd: C, ctx: &Ctx) -> ExitCode {
    let emits_json = cmd.emits_json();
    match cmd.run(ctx) {
        Ok(outcome) => {
            if emits_json && !C::SELF_RENDERS_JSON {
                println!("{}", C::render_json(outcome.payload()));
            }
            ExitCode::from(outcome.status().code())
        }
        Err(KernelError::Report(d)) => {
            emit_error(&d, &ctx.root);
            // ENF-004 applies to the failure path too. ADR-086 § WIRE-005 puts
            // `exit_code` *inside* the object, so a `--format json` run that
            // died here used to leave stdout empty — making the documented
            // value `2` unreachable by construction, and a broken config
            // indistinguishable from a binary that never ran.
            //
            // The object is the lint wire shape for every command, not just
            // `lint`. A caller that asked `pack list` for an array gets an
            // object instead, which is a clean type-level "this is not your
            // payload" — strictly more than the empty stream it got before, and
            // the alternative (a bespoke error shape per command) would be a
            // second contract to keep in step with the schema.
            //
            // `SELF_RENDERS_JSON` is not consulted: those commands self-render
            // on the *success* path only, and reaching here means they returned
            // before writing anything.
            if emits_json {
                println!("{}", ctxgrd::run::render_error_json(&d, &ctx.root));
            }
            ExitCode::from(ExitStatus::KernelError.code())
        }
        // The handler already wrote its own output — including, where it emits
        // JSON at all, its own machine payload (`lint --recursive` writes the
        // labelled per-root object before any root fails). Re-rendering here
        // would append a second object to a stream that must carry exactly one.
        Err(KernelError::Reported) => ExitCode::from(ExitStatus::KernelError.code()),
    }
}

// ENF-002 is structural, not a lint: a new command that forgets its
// `--format json` path cannot compile. The following impl omits `type Json`
// and, if uncommented, fails `cargo check` with
//
//   error[E0046]: not all trait items implemented, missing: `Json`
//
// (verified 2026-07-15). The fix is to declare a real result type or the
// explicit `type Json = Prose` / `Server` exemption — there is no way to have
// a `Command` with no machine-readable result declared.
//
//   struct HypotheticalNewCommand;
//   impl Command for HypotheticalNewCommand {
//       fn run(self, _ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
//           Ok(Outcome::Did(Prose))
//       }
//   }
