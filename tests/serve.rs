//! `ctxgrd serve` acceptance suite (ADR-097). Spawns the real binary
//! against the `examples/` fixture, reads the SRV-006 startup line off
//! stdout, and drives the hand-rolled HTTP server over a raw
//! `TcpStream` — the same transport a browser or agent would use.
//!
//! Verifies the contract the handoff fixes: an agent discovers the URL
//! without screen-scraping (SRV-006), a governed doc route renders with
//! `depends_on` as a link (SRV-002/003), and a path outside the governed
//! set is a 404 with no disk read (SRV-005).
//!
//! The child's stdout is captured to a per-test temp file rather than an
//! inherited pipe: the server writes one line then blocks forever in its
//! accept loop, so a file the parent polls is simpler and race-free
//! (and mirrors how an agent would capture `serve … > url.json`).

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// A spawned `ctxgrd serve` child that is killed (and its temp file
/// removed) on drop, so a panicking assertion never leaks a listening
/// process or a stray file.
struct Serve {
    child: Child,
    url: String,
    out_path: PathBuf,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.out_path);
    }
}

/// Monotonic suffix so parallel tests don't collide on the temp file.
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Start `ctxgrd serve --port 0 --root examples`, its stdout redirected
/// to a temp file, and block until the `{"url":…}` line lands (SRV-006).
/// Panics if it never does within the timeout.
fn start() -> Serve {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let out_path = std::env::temp_dir().join(format!("ctxgrd-serve-test-{}-{n}.out", std::process::id()));
    let file = fs::File::create(&out_path).expect("create temp stdout file");

    let child = Command::new(env!("CARGO_BIN_EXE_ctxgrd"))
        .args(["--root", "examples", "serve", "--port", "0"])
        .stdout(Stdio::from(file))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ctxgrd serve");

    let url = wait_for_url(&out_path, Duration::from_secs(30));
    Serve {
        child,
        url,
        out_path,
    }
}

/// Poll the stdout file until it holds the one-line startup contract.
fn wait_for_url(path: &PathBuf, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(path) {
            if let Some(line) = contents.lines().next() {
                if line.contains("\"url\"") {
                    return url_from_json(line);
                }
            }
        }
        sleep(Duration::from_millis(50));
    }
    panic!("serve did not print a startup url within {timeout:?}");
}

/// Pull the `url` field out of the `{"url":"http://127.0.0.1:<port>"}`
/// line without a JSON dependency — the shape is fixed by SRV-006.
fn url_from_json(line: &str) -> String {
    let key = "\"url\":\"";
    let start = line.find(key).expect("startup line has a url field") + key.len();
    let rest = &line[start..];
    let end = rest.find('"').expect("url value is quoted");
    rest[..end].to_string()
}

/// One GET over a fresh connection (the server sends `Connection: close`).
/// Returns `(status_code, body)`.
fn get(url: &str, path: &str) -> (u16, String) {
    let addr = url.strip_prefix("http://").expect("http url");
    let mut stream = TcpStream::connect(addr).expect("connect to serve");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code in response line");
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn startup_line_is_a_usable_url_on_stdout() {
    let serve = start();
    assert!(
        serve.url.starts_with("http://127.0.0.1:"),
        "startup url was {:?}",
        serve.url
    );
}

#[test]
fn index_renders_and_groups_by_namespace() {
    let serve = start();
    let (status, body) = get(&serve.url, "/");
    assert_eq!(status, 200);
    // The examples fixture defines an ADR namespace.
    assert!(body.contains("<h2>ADR</h2>"), "index missing ADR group: {body}");
    assert!(
        body.contains(r#"href="/doc/ADR-001""#),
        "index missing ADR-001 link"
    );
}

#[test]
fn governed_doc_renders_dependency_as_a_link() {
    let serve = start();
    let (status, body) = get(&serve.url, "/doc/ADR-001");
    assert_eq!(status, 200);
    // ADR-001 in the fixture depends on PRD-001 — rendered as a link.
    assert!(
        body.contains(r#"class="dep" href="/doc/PRD-001""#),
        "depends_on not rendered as a link: {body}"
    );
}

#[test]
fn index_lists_file_level_namespace_with_location_link() {
    // ADR-103 § SRF-003: the fixture's `[AGENTS]` namespace path-claims
    // CLAUDE.md — an id-less file-level doc. It gets its own index
    // section, keyed by location instead of id, with its frontmatter
    // title.
    let serve = start();
    let (status, body) = get(&serve.url, "/");
    assert_eq!(status, 200);
    assert!(body.contains("<h2>AGENTS</h2>"), "index missing AGENTS group: {body}");
    assert!(
        body.contains(r#"href="/file/CLAUDE.md""#),
        "index missing file-level link: {body}"
    );
    assert!(
        body.contains("Example agent guide"),
        "file-level entry missing its frontmatter title: {body}"
    );
}

#[test]
fn file_level_doc_renders_with_cross_ref_link_and_location_chrome() {
    // ADR-103 § SRF-002/004: the file page renders through the same
    // pipeline as id-keyed docs — the ADR-001 mention in the fixture
    // CLAUDE.md links into the id-keyed set — and the chrome shows the
    // location in place of an id.
    let serve = start();
    let (status, body) = get(&serve.url, "/file/CLAUDE.md");
    assert_eq!(status, 200);
    assert!(
        body.contains(r#"class="xref" href="/doc/ADR-001""#),
        "cross-ref not linkified in file-level body: {body}"
    );
    assert!(
        body.contains(r#"<span class="doc-id">CLAUDE.md</span>"#),
        "chrome missing location label: {body}"
    );
}

#[test]
fn file_route_outside_scanned_set_is_404() {
    // ADR-103 § SRF-002: `/file/` resolves only within the scanned set —
    // a real file on disk that no namespace claims is still a 404.
    let serve = start();
    let (status, _) = get(&serve.url, "/file/../Cargo.toml");
    assert_eq!(status, 404, "traversal path was not refused");
    let (status, _) = get(&serve.url, "/file/README.md");
    assert_eq!(status, 404, "unclaimed file must not resolve");
}

#[test]
fn path_outside_governed_set_is_404() {
    let serve = start();
    let (status, _) = get(&serve.url, "/doc/../../etc/passwd");
    assert_eq!(status, 404, "traversal path was not refused");
    let (status, _) = get(&serve.url, "/no-such-route");
    assert_eq!(status, 404);
}

#[test]
fn static_assets_and_reload_endpoint_are_served() {
    let serve = start();
    let (css, _) = get(&serve.url, "/static/app.css");
    assert_eq!(css, 200);
    let (reload, body) = get(&serve.url, "/reload");
    assert_eq!(reload, 200);
    assert!(
        body.contains("\"digest\""),
        "reload endpoint missing digest: {body}"
    );
}
