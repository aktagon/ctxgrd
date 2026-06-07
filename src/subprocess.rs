//! Bounded subprocess runner.
//!
//! Shared helper for external sources (SRC-002, 300 s timeout) and
//! external rules (EXT-002, 60 s timeout). The contract both obey:
//!
//! - env is cleared and repopulated with a platform baseline
//!   (`PATH`, `HOME`, `LANG`, `LC_*`) plus call-site variables;
//! - stdout and stderr are captured in full, concurrently, so a
//!   chatty child can't deadlock by filling a pipe while the parent
//!   waits on `wait_timeout`;
//! - a timeout kills the child (SIGKILL via `Child::kill`) and
//!   returns the partial output that was buffered so far.
//!
//! No tokio, no async. One thread per pipe, `wait_timeout` on the
//! child handle. Minimal and stdlib-ish.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Every environment variable the child will see.
///
/// Built via [`baseline_env`] + call-site insertions. Nothing from the
/// parent process leaks in unless it's listed in the baseline.
pub(crate) type Env = BTreeMap<OsString, OsString>;

/// How a subprocess exit terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExitKind {
    Success,
    /// Process exited non-zero with `code`. `Some(code)` when the OS
    /// reported a numeric status; `None` when the process was killed
    /// by a signal (Unix).
    Failure(Option<i32>),
    TimedOut,
}

#[derive(Debug, Clone)]
pub(crate) struct RunOutput {
    pub exit: ExitKind,
    pub stdout: Vec<u8>,
    /// Captured but not yet surfaced. Kept so a future `--debug` flag
    /// can render rule/source stderr without changing capture
    /// semantics; until then, drained-and-dropped is the documented
    /// behavior in `docs/rules.md` and `docs/sources.md`.
    #[allow(dead_code)]
    pub stderr: Vec<u8>,
}

impl RunOutput {
    /// Stdout decoded as UTF-8, lossy on invalid sequences. Both
    /// source envelope JSONL and rule diagnostic JSONL are UTF-8 by
    /// contract, but we don't want to panic on rogue bytes from a
    /// malformed script.
    pub(crate) fn stdout_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Symmetric to [`Self::stdout_utf8`]. See the field comment for
    /// why this is kept alongside an unread buffer.
    #[allow(dead_code)]
    pub(crate) fn stderr_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Platform baseline env — `PATH`, `HOME`, `LANG`, and every `LC_*`
/// variable the parent has. These are the only parent-process env
/// vars that propagate to sources and rules, per SRC-002 / EXT-002.
pub(crate) fn baseline_env() -> Env {
    let mut env = Env::new();
    for key in ["PATH", "HOME", "LANG"] {
        if let Ok(val) = std::env::var(key) {
            env.insert(OsString::from(key), OsString::from(val));
        }
    }
    for (k, v) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("LC_") {
            env.insert(k, v);
        }
    }
    env
}

/// Resolve `program` to an absolute path before spawning.
///
/// `Command::new` resolves relative paths against the parent process's
/// cwd, but we set `current_dir(cwd)` on the command — the child's cwd
/// then changes before the OS resolves the program path, and a
/// relative `program` like `examples/rules/.../run` becomes
/// `cwd/examples/rules/.../run` (ENOENT). Canonicalizing up front
/// keeps callers honest and the spawn deterministic regardless of cwd.
///
/// Errors propagate so callers see "no such file" with the original
/// path the caller passed in, not an opaque "ENOENT" from
/// `Command::spawn` after the cwd has shifted under the OS resolver.
fn absolutize(program: &Path) -> io::Result<PathBuf> {
    if program.is_absolute() {
        Ok(program.to_path_buf())
    } else {
        std::fs::canonicalize(program)
    }
}

/// Run `program args...` with a fully scrubbed environment and a hard
/// timeout. On timeout the child is killed and whatever stdout/stderr
/// was buffered up to that point is returned.
pub(crate) fn run(
    program: &Path,
    args: &[&Path],
    env: &Env,
    cwd: &Path,
    timeout: Duration,
) -> io::Result<RunOutput> {
    let program = absolutize(program)?;
    let mut cmd = Command::new(&program);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    for a in args {
        cmd.arg(a);
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    // Drain stdout and stderr concurrently so a chatty child can't
    // fill its pipe buffer and block.
    let stdout = child
        .stdout
        .take()
        .expect("stdout is piped above — Child::stdout must be Some");
    let stderr = child
        .stderr
        .take()
        .expect("stderr is piped above — Child::stderr must be Some");
    let stdout_thread = drain(stdout);
    let stderr_thread = drain(stderr);

    let (exit, killed) = match child.wait_timeout(timeout)? {
        Some(status) => {
            let kind = if status.success() {
                ExitKind::Success
            } else {
                ExitKind::Failure(status.code())
            };
            (kind, false)
        }
        None => {
            // Timed out. Kill, reap, return the partial output we
            // collected so far.
            let _ = child.kill();
            let _ = child.wait();
            (ExitKind::TimedOut, true)
        }
    };

    let _ = killed; // sink for potential future use
    let stdout_buf = stdout_thread.join().unwrap_or_default();
    let stderr_buf = stderr_thread.join().unwrap_or_default();

    Ok(RunOutput {
        exit,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    })
}

/// Run `program args...` like [`run`], but pipe `stdin_bytes` to the
/// child's stdin and close it to signal end-of-stream.
///
/// Used by the external-rule batch contract (ADR-002 § RUL-001/002):
/// the kernel writes one JSONL document envelope per line on stdin,
/// then closes stdin so the rule's `read` loop terminates.
///
/// The stdin write happens on a dedicated thread so it cannot deadlock
/// against the stdout/stderr drains. If the child closes stdin early
/// (broken pipe), the writer simply errors out and exits.
pub(crate) fn run_with_stdin(
    program: &Path,
    args: &[&Path],
    env: &Env,
    cwd: &Path,
    timeout: Duration,
    stdin_bytes: Vec<u8>,
) -> io::Result<RunOutput> {
    let program = absolutize(program)?;
    let mut cmd = Command::new(&program);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    for a in args {
        cmd.arg(a);
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    let mut child_stdin = child
        .stdin
        .take()
        .expect("stdin is piped above — Child::stdin must be Some");
    let stdout = child
        .stdout
        .take()
        .expect("stdout is piped above — Child::stdout must be Some");
    let stderr = child
        .stderr
        .take()
        .expect("stderr is piped above — Child::stderr must be Some");

    // Writer thread. Closes stdin via `drop` after the buffer is
    // flushed, signalling EOF to the child's `read` loop.
    let stdin_thread = thread::spawn(move || {
        let _ = child_stdin.write_all(&stdin_bytes);
    });
    let stdout_thread = drain(stdout);
    let stderr_thread = drain(stderr);

    let exit = match child.wait_timeout(timeout)? {
        Some(status) => {
            if status.success() {
                ExitKind::Success
            } else {
                ExitKind::Failure(status.code())
            }
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            ExitKind::TimedOut
        }
    };

    let _ = stdin_thread.join();
    let stdout_buf = stdout_thread.join().unwrap_or_default();
    let stderr_buf = stderr_thread.join().unwrap_or_default();

    Ok(RunOutput {
        exit,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Insert a `CTXGRD_*` variable into an env map. Convenience.
pub(crate) fn set_env(env: &mut Env, key: &str, value: impl Into<OsString>) {
    env.insert(OsString::from(key), value.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_program() -> &'static Path {
        Path::new("/bin/echo")
    }

    #[test]
    fn baseline_env_contains_path() {
        let env = baseline_env();
        assert!(env.contains_key(std::ffi::OsStr::new("PATH")));
    }

    #[test]
    fn run_captures_stdout() {
        let env = baseline_env();
        let args_path = Path::new("hello from ctxgrd");
        let out = run(
            echo_program(),
            &[args_path],
            &env,
            Path::new("."),
            Duration::from_secs(5),
        )
        .expect("echo runs");
        assert_eq!(out.exit, ExitKind::Success);
        assert!(out.stdout_utf8().contains("hello from ctxgrd"));
    }

    #[test]
    fn run_times_out_on_slow_child() {
        // sleep 30, timeout 200ms → child killed, exit=TimedOut.
        let env = baseline_env();
        let arg = Path::new("30");
        let out = run(
            Path::new("/bin/sleep"),
            &[arg],
            &env,
            Path::new("."),
            Duration::from_millis(200),
        )
        .expect("sleep runs");
        assert_eq!(out.exit, ExitKind::TimedOut);
    }

    #[test]
    fn run_reports_non_zero_exit() {
        // `false` exits 1.
        let env = baseline_env();
        let out = run(
            Path::new("/usr/bin/false"),
            &[],
            &env,
            Path::new("."),
            Duration::from_secs(5),
        )
        .expect("false runs");
        match out.exit {
            ExitKind::Failure(Some(1)) => {}
            other => panic!("expected Failure(Some(1)), got {other:?}"),
        }
    }
}
