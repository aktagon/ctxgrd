//! Small one-off commands: `pin --bless` and the `lsp` server entry point.

use ctxgrd::diagnostic::Diagnostic;

use super::command::{Command, Ctx, KernelError, Outcome, Server};
use super::Format;

/// `ctxgrd pin --bless <ID>` — re-pin a document's `pin.commit` to HEAD.
pub(super) struct PinCmd {
    pub(super) target_id: String,
    pub(super) force: bool,
    pub(super) format: Format,
}

/// ADR-096 § CMD-001 wire shape for `ctxgrd pin --bless <ID> --format json`.
#[derive(serde::Serialize)]
pub(super) struct PinJson {
    status: &'static str,
    id: String,
    pin: String,
}

impl Command for PinCmd {
    type Json = PinJson;

    fn emits_json(&self) -> bool {
        matches!(self.format, Format::Json)
    }

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        match ctxgrd::pin::bless(root, &self.target_id, self.force) {
            Ok(new_commit) => {
                // ADR-096 § CMD-001: `--format json` emits `{status, id, pin}`
                // on a clean stdout (the dispatcher writes it); the human
                // summary stays on stderr. In text mode it is the stdout line.
                if matches!(self.format, Format::Json) {
                    eprintln!("blessed {}: pin.commit -> {new_commit}", self.target_id);
                } else {
                    println!("blessed {}: pin.commit -> {new_commit}", self.target_id);
                }
                Ok(Outcome::Did(PinJson {
                    status: "blessed",
                    id: self.target_id,
                    pin: new_commit,
                }))
            }
            Err(e) => Err(KernelError::report(Diagnostic::error(
                "pin.bless",
                "",
                0,
                0,
                format!("{e}"),
            ))),
        }
    }
}

/// `ctxgrd lsp` — the stdio Language Server.
///
/// ENF-002 exemption: an LSP protocol server speaks JSON-RPC over stdio, not a
/// one-shot `--format json` payload — so it declares [`Server`].
pub(super) struct LspCmd;

impl Command for LspCmd {
    type Json = Server;

    fn run(self, _ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        run_lsp();
        Ok(Outcome::Did(Server))
    }
}

#[tokio::main]
async fn run_lsp() {
    ctxgrd::lsp::run_server().await;
}
