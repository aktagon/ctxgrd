//! Small one-off commands: `pin --bless` and the `lsp` server entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;

use ctxgrd::diagnostic::Diagnostic;
use ctxgrd::run;

use super::emit_error;

pub(super) fn pin_bless_cmd(root: &PathBuf, target_id: &str, force: bool) -> Result<ExitCode> {
    match ctxgrd::pin::bless(root, target_id, force) {
        Ok(new_commit) => {
            println!("blessed {target_id}: pin.commit -> {new_commit}");
            Ok(ExitCode::from(run::ExitStatus::Ok.code()))
        }
        Err(e) => {
            let d = Diagnostic::error("pin.bless", "", 0, 0, format!("{e}"));
            emit_error(&d, root);
            Ok(ExitCode::from(run::ExitStatus::KernelError.code()))
        }
    }
}

#[tokio::main]
pub(super) async fn lsp_cmd() -> Result<ExitCode> {
    ctxgrd::lsp::run_server().await;
    Ok(ExitCode::from(run::ExitStatus::Ok.code()))
}
