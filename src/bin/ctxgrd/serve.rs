//! `ctxgrd serve` CLI wiring (ADR-097 § SRV-006).
//!
//! Binds the read-only viewer and enforces the agent-drivable startup
//! contract: the single machine-readable `{"url":…}` line goes to a clean
//! stdout, every human log to stderr, and a bind failure exits `2` so an agent
//! can branch on outcome without screen-scraping.

use std::io::Write as _;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::serve::Server as WebServer;

use super::command::{Command, Ctx, KernelError, Outcome, Server};

/// `ctxgrd serve` — start the localhost doc viewer and serve until killed.
///
/// ENF-002 exemption: the server streams HTML and prints a one-line `{"url":…}`
/// startup banner, not a one-shot `--format json` payload — so it declares
/// [`Server`]. ADR-101 named `docs`+`lsp`; `serve` (ADR-097) is the third
/// server exemption.
pub(super) struct ServeCmd {
    pub(super) port: u16,
}

impl Command for ServeCmd {
    type Json = Server;

    fn run(self, ctx: &Ctx) -> Result<Outcome<Self::Json>, KernelError> {
        let root = &ctx.root;
        let server = match WebServer::bind(root, self.port) {
            Ok(server) => server,
            Err(e) => {
                let d = Diagnostic::error(
                    "serve.bind",
                    "",
                    0,
                    0,
                    format!("could not bind 127.0.0.1:{}: {e}", self.port),
                )
                .with_help("pass a free --port, or --port 0 to let the OS assign one");
                return Err(KernelError::report(d));
            }
        };

        let addr = server.local_addr().map_err(|e| {
            KernelError::report(Diagnostic::error(
                "serve.addr",
                "",
                0,
                0,
                format!("could not read the bound address: {e}"),
            ))
        })?;
        let url = format!("http://127.0.0.1:{}", addr.port());

        // SRV-006: exactly one machine-readable line on stdout, then keep
        // stdout clean. An agent discovers the URL with `… | jq -r .url`.
        println!("{{\"url\":\"{url}\"}}");
        std::io::stdout().flush().ok();

        // Everything a human wants goes to stderr, off the machine stream.
        eprintln!("ctxgrd serve: {url}  (read-only; Ctrl-C to stop)");

        server.serve_forever();
        Ok(Outcome::Did(Server))
    }
}
