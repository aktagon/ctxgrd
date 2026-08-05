//! `ctxgrd serve` — read-only, localhost, graph-aware doc viewer (ADR-097).
//!
//! A hand-rolled HTTP/1.1 server over [`std::net`] (SRV-001): `GET`-only,
//! bound to loopback, one [`std::thread`] per connection, `Connection:
//! close` with an explicit `Content-Length`. No HTTP or async-runtime
//! dependency.
//!
//! Markdown renders **server-side** (SRV-002) through the same
//! `pulldown-cmark` the linter parses with — one engine, zero drift. The
//! v1 wedge is graph-awareness (SRV-003): a namespace-grouped index with
//! status badges, `depends_on`/cross-ref targets as clickable links to
//! the referenced docs, and the `status` pipeline as HTML/CSS
//! stage-columns. Drawn diagrams are v2 (SRV-004): in-doc ` ```mermaid `
//! blocks render as source here.
//!
//! Routes address the **closed governed-doc set** (SRV-005), never a
//! filesystem path: `/doc/<id>` resolves only within the in-process
//! enumeration ([`run::ingest`]), and `/file/<location>` only within the
//! file-level scan ([`crate::agent_guide::scan_file_level`] — the id-less
//! path-claimed singletons, ADR-103), so an unknown route is a `404` with
//! no disk read — traversal-safe by construction.
//!
//! The hand-built HTML chrome escapes every interpolated frontmatter
//! value; `pulldown-cmark` escapes the body, but `format!`-assembled
//! markup does not.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use regex::Regex;

use crate::document::Document;
use crate::id::DocumentId;
use crate::{agent_guide, frontmatter, list, run, status};

/// Stylesheet and live-reload poller, compiled in so `serve` is
/// self-contained with no CDN and no runtime asset lookup (SRV-004:
/// bundled, never fetched). Kept in separate files because the project
/// forbids inline CSS/JS.
const APP_CSS: &str = include_str!("../assets/serve/app.css");
const APP_JS: &str = include_str!("../assets/serve/app.js");

/// A read-only doc viewer bound to loopback.
pub struct Server {
    listener: TcpListener,
    root: PathBuf,
}

impl Server {
    /// Bind a loopback-only listener. `port` `0` lets the OS assign a
    /// free port (SRV-006 default). Binds `127.0.0.1` only — never a
    /// routable interface — so the surface stays single-user localhost.
    pub fn bind(root: &Path, port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        Ok(Self {
            listener,
            root: root.to_path_buf(),
        })
    }

    /// The address the listener actually bound — the source of the
    /// runtime port when `--port 0` was requested (SRV-006).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve connections until the process is killed. One detached
    /// thread per connection; a connection or accept error is logged to
    /// stderr and never takes down the accept loop.
    pub fn serve_forever(&self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    let root = self.root.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle_connection(&root, stream) {
                            eprintln!("serve: connection error: {e}");
                        }
                    });
                }
                Err(e) => eprintln!("serve: accept error: {e}"),
            }
        }
    }
}

/// Parse one request, route it, write one response, and close.
fn handle_connection(root: &Path, mut stream: TcpStream) -> std::io::Result<()> {
    match read_request_target(&stream)? {
        Some(target) => {
            eprintln!("serve: GET {target}");
            let response = route(root, &target);
            write_response(&mut stream, &response)
        }
        None => write_response(&mut stream, &Response::method_not_allowed()),
    }
}

/// Read the request line and return the requested path for a `GET`, or
/// `None` for any other method. Headers and body are ignored — the
/// server is read-only and closes after one response.
fn read_request_target(stream: &TcpStream) -> std::io::Result<Option<String>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" {
        return Ok(None);
    }
    // Drop any query string; v1 routes carry no query parameters.
    let path = target.split('?').next().unwrap_or("/");
    Ok(Some(path.to_string()))
}

/// The closed route table (SRV-005). Every branch resolves against the
/// in-process enumeration or a compiled-in asset; nothing maps a URL to
/// a filesystem path, so `/doc/../../etc/passwd` and every other
/// non-route falls through to `404` without touching disk.
fn route(root: &Path, path: &str) -> Response {
    match path {
        "/" => render_index_page(root),
        "/pipeline" => render_pipeline_page(root),
        "/reload" => render_reload(root),
        "/static/app.css" => Response::asset("text/css; charset=utf-8", APP_CSS),
        "/static/app.js" => Response::asset("application/javascript; charset=utf-8", APP_JS),
        _ => {
            if let Some(id) = path.strip_prefix("/doc/") {
                render_doc_page(root, id)
            } else if let Some(location) = path.strip_prefix("/file/") {
                render_file_page(root, location)
            } else {
                Response::not_found()
            }
        }
    }
}

// -- Page rendering ----------------------------------------------------

/// Wrap page-body HTML in the shared chrome: head linking the external
/// stylesheet and live-reload script (no inline CSS/JS), plus the
/// topbar nav.
fn page(title: &str, body_html: &str) -> String {
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <link rel=\"stylesheet\" href=\"/static/app.css\">\n\
         <script src=\"/static/app.js\" defer></script>\n\
         </head>\n\
         <body>\n\
         <header class=\"topbar\"><a class=\"brand\" href=\"/\">ctxgrd</a>\
         <nav><a href=\"/\">Index</a><a href=\"/pipeline\">Pipeline</a></nav></header>\n\
         <main>\n{body_html}</main>\n\
         </body>\n</html>\n",
        title = escape_html(title),
    )
}

/// The namespace-grouped index (SRV-003): one section per namespace,
/// each doc a link to its route plus a status badge. Id-keyed sections
/// reuse [`list::entries`]; file-level (id-less, path-claimed) sections
/// follow them, sourced from the same scan the linter runs (ADR-103 §
/// SRF-003).
fn render_index_page(root: &Path) -> Response {
    let ingested = match run::ingest(root) {
        Ok(result) => result,
        Err(e) => return Response::html(500, &error_page(&e.to_string())),
    };
    let file_scan =
        match agent_guide::scan_file_level(root, &ingested.config, &ingested.path_claims) {
            Ok(scan) => scan,
            Err(e) => return Response::html(500, &error_page(&e.to_string())),
        };
    let documents = ingested.documents;
    // The id-keyed sections stay on `list::entries` untouched: `ctxgrd
    // list` output is not extended here, so the ADR-097 "serve inventory
    // matches list" invariant narrows to these sections; the file-level
    // sections below are additional serve-only surface (ADR-103 §
    // SRF-005).
    let entries = list::entries(&documents, None);

    let mut body = String::from("<h1>Governed documents</h1>\n");
    if entries.is_empty() && file_scan.documents.is_empty() {
        body.push_str("<p class=\"empty\">No governed documents found under this root.</p>\n");
        return Response::html(200, &page("ctxgrd — documents", &body));
    }

    let mut current: Option<&str> = None;
    for e in &entries {
        if current != Some(e.namespace.as_str()) {
            if current.is_some() {
                body.push_str("</ul>\n");
            }
            let _ = writeln!(
                body,
                "<h2>{}</h2>\n<ul class=\"doc-list\">",
                escape_html(&e.namespace)
            );
            current = Some(&e.namespace);
        }
        // Fall back to the body's first heading when frontmatter carries
        // no title/name — e.g. FEEDBACK docs, whose title lives only in
        // their leading `#` H1, not the frontmatter.
        let title = if e.title.is_empty() {
            heading_title(&documents, &e.id)
        } else {
            e.title.clone()
        };
        let _ = writeln!(
            body,
            "<li><a href=\"/doc/{id}\">{id}</a><span class=\"title\">{title}</span>{badge}</li>",
            id = escape_html(&e.id),
            title = escape_html(&title),
            badge = status_badge(&e.status),
        );
    }
    if current.is_some() {
        body.push_str("</ul>\n");
    }

    // File-level sections: the location plays the role the id plays
    // above, linking to the `/file/<location>` route (ADR-103 § SRF-003).
    let mut file_docs: Vec<&Document> = file_scan.documents.iter().collect();
    file_docs.sort_by(|a, b| {
        (a.id.namespace.as_str(), a.location.as_str())
            .cmp(&(b.id.namespace.as_str(), b.location.as_str()))
    });
    let mut current: Option<&str> = None;
    for doc in &file_docs {
        if current != Some(doc.id.namespace.as_str()) {
            if current.is_some() {
                body.push_str("</ul>\n");
            }
            let _ = writeln!(
                body,
                "<h2>{}</h2>\n<ul class=\"doc-list\">",
                escape_html(&doc.id.namespace)
            );
            current = Some(&doc.id.namespace);
        }
        let _ = writeln!(
            body,
            "<li><a href=\"/file/{loc}\">{loc}</a><span class=\"title\">{title}</span>{badge}</li>",
            loc = escape_html(&doc.location),
            title = escape_html(&file_title(doc)),
            badge = status_badge(&meta_str(doc, "status")),
        );
    }
    if current.is_some() {
        body.push_str("</ul>\n");
    }
    Response::html(200, &page("ctxgrd — documents", &body))
}

/// A single doc page: frontmatter chrome (id, title, status badge,
/// `depends_on` as links) plus the server-rendered body (SRV-002/003).
/// The id is resolved against the ingested set only (SRV-005).
fn render_doc_page(root: &Path, id_str: &str) -> Response {
    let documents = match run::ingest(root) {
        Ok(result) => result.documents,
        Err(e) => return Response::html(500, &error_page(&e.to_string())),
    };
    let parsed: Option<DocumentId> = id_str.parse().ok();
    let doc = documents
        .iter()
        .find(|d| d.raw_id == id_str || parsed.as_ref() == Some(&d.id));
    let Some(doc) = doc else {
        return Response::not_found();
    };

    let present: BTreeSet<DocumentId> = documents.iter().map(|d| d.id.clone()).collect();
    let body = render_doc_body(doc, &present);
    Response::html(200, &page(&format!("{} — ctxgrd", doc.raw_id), &body))
}

/// A file-level (id-less, path-claimed) doc page (ADR-103 § SRF-002/004).
/// The repo-relative location is the key: resolved by exact match against
/// the freshly scanned set only, so an unknown or non-enumerated path is
/// a `404` with no filesystem access from the request path (SRV-005
/// preserved). The body renders through the same pipeline as id-keyed
/// docs; cross-ref tokens link into the id-keyed set.
fn render_file_page(root: &Path, location: &str) -> Response {
    let ingested = match run::ingest(root) {
        Ok(result) => result,
        Err(e) => return Response::html(500, &error_page(&e.to_string())),
    };
    let file_scan =
        match agent_guide::scan_file_level(root, &ingested.config, &ingested.path_claims) {
            Ok(scan) => scan,
            Err(e) => return Response::html(500, &error_page(&e.to_string())),
        };
    let Some(doc) = file_scan.documents.iter().find(|d| d.location == location) else {
        return Response::not_found();
    };

    let present: BTreeSet<DocumentId> = ingested.documents.iter().map(|d| d.id.clone()).collect();
    let body = render_doc_body(doc, &present);
    Response::html(200, &page(&format!("{} — ctxgrd", doc.location), &body))
}

/// The work-queue view (SRV-003): what can be picked up now, and what is
/// waiting on what. Reuses [`status::report`] — the same computation
/// `ctxgrd status` prints.
///
/// ADR-118 replaced the stage-ladder columns this drew. The route keeps its
/// path so existing links resolve; what it renders is the per-document
/// queue, since the stage ladder it summarised no longer exists.
fn render_pipeline_page(root: &Path) -> Response {
    let report = match status::report(root) {
        Ok(report) => report,
        Err(e) => return Response::html(500, &error_page(&e.to_string())),
    };

    let ready: Vec<_> = report.ready().collect();
    let blocked: Vec<_> = report.blocked().collect();

    let mut body = String::from("<h1>Work queue</h1>\n");
    let _ = writeln!(
        body,
        "<p class=\"queue-census\">{} documents · {} ready · {} blocked</p>",
        report.documents.len(),
        ready.len(),
        blocked.len(),
    );

    body.push_str("<div class=\"queue\">\n");
    body.push_str("<section class=\"queue-ready\"><h2>Ready</h2>");
    if ready.is_empty() {
        body.push_str("<p class=\"empty\">—</p>");
    } else {
        body.push_str("<ul>");
        for d in &ready {
            let _ = write!(
                body,
                "<li><a href=\"/doc/{id}\">{id}</a> <span class=\"status\">{status}</span></li>",
                id = escape_html(&d.id),
                status = escape_html(d.status.as_deref().unwrap_or("none")),
            );
        }
        body.push_str("</ul>");
    }
    body.push_str("</section>\n");

    body.push_str("<section class=\"queue-blocked\"><h2>Blocked</h2>");
    if blocked.is_empty() {
        body.push_str("<p class=\"empty\">—</p>");
    } else {
        body.push_str("<ul>");
        for d in &blocked {
            let _ = write!(
                body,
                "<li><a href=\"/doc/{id}\">{id}</a> <span class=\"blocked-by\">waiting on {by}</span></li>",
                id = escape_html(&d.id),
                by = escape_html(&d.blocked_by.join(", ")),
            );
        }
        body.push_str("</ul>");
    }
    body.push_str("</section>\n</div>\n");

    Response::html(200, &page("ctxgrd — work queue", &body))
}

/// The live-reload change endpoint (SRV-007): a digest of the governed
/// set's `.md` mtimes as a small JSON object the poller compares.
fn render_reload(root: &Path) -> Response {
    let digest = compute_digest(root);
    Response::asset(
        "application/json; charset=utf-8",
        &format!("{{\"digest\":\"{digest}\"}}"),
    )
}

/// Build the per-doc body HTML: the frontmatter chrome then the rendered
/// markdown. Every interpolated frontmatter value is escaped here.
fn render_doc_body(doc: &Document, present: &BTreeSet<DocumentId>) -> String {
    let status = meta_str(doc, "status");

    // The chrome carries only the graph metadata (id, status, deps). The
    // title is not repeated here: the markdown body leads with its own
    // `#` H1, so re-emitting the frontmatter title would give the page a
    // second H1 and show the title twice.
    // Id-keyed docs show their raw id; file-level synthetic docs have an
    // empty `raw_id`, so the location — their only stable key — takes its
    // place (ADR-103 § SRF-004).
    let id_label = if doc.raw_id.is_empty() {
        &doc.location
    } else {
        &doc.raw_id
    };
    let mut out = String::from("<article class=\"doc\">\n<div class=\"doc-head\">\n");
    let _ = writeln!(
        out,
        "<span class=\"doc-id\">{}</span> {}",
        escape_html(id_label),
        status_badge(&status),
    );
    if !doc.depends_on.is_empty() {
        out.push_str("<div class=\"deps\"><span class=\"deps-label\">depends on:</span>");
        for dep in &doc.depends_on {
            out.push(' ');
            out.push_str(&dep_link(dep, present));
        }
        out.push_str("</div>\n");
    }
    out.push_str("</div>\n<div class=\"doc-body\">\n");
    out.push_str(&render_markdown(&doc.body, present));
    out.push_str("</div>\n</article>\n");
    out
}

/// A `depends_on` entry as a link when it resolves to a governed doc, or
/// a muted, dashed chip when it doesn't (mirroring the `core.dep-resolved`
/// distinction visually).
fn dep_link(dep: &str, present: &BTreeSet<DocumentId>) -> String {
    if let Ok(id) = dep.parse::<DocumentId>() {
        if present.contains(&id) {
            return format!(
                "<a class=\"dep\" href=\"/doc/{d}\">{d}</a>",
                d = escape_html(dep)
            );
        }
    }
    format!(
        "<span class=\"dep dep-unresolved\">{}</span>",
        escape_html(dep)
    )
}

/// Render the markdown body server-side (SRV-002). Strips the
/// frontmatter fence with the same offset `parse_ast` uses, parses with
/// the common GFM extensions (tables, task lists, strikethrough) so a
/// doc renders the way a human or GitHub sees it, linkifies cross-ref
/// tokens that resolve to a governed doc, then emits HTML. The engine is
/// the same `pulldown-cmark` the linter uses — the extensions are a
/// superset of the linter's parse options, not a second markdown engine
/// (the drift SRV-002 rules out). ` ```mermaid ` fences fall through as a
/// code block — source, not a drawing (SRV-004).
fn render_markdown(body: &str, present: &BTreeSet<DocumentId>) -> String {
    let start = frontmatter::body_start_offset(body);
    let markdown = &body[start..];
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let events = linkify_cross_refs(Parser::new_ext(markdown, opts), present);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    html
}

/// Transform a pulldown event stream so `<NS>-<n>` tokens that resolve
/// to a governed doc become links, leaving everything else — and every
/// token inside a code block — untouched. Operating on the event stream
/// (not the rendered HTML) keeps the linkifier from ever rewriting
/// markup or code.
fn linkify_cross_refs<'a>(
    parser: impl Iterator<Item = Event<'a>>,
    present: &BTreeSet<DocumentId>,
) -> Vec<Event<'a>> {
    let mut out = Vec::new();
    let mut code_depth = 0usize;
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                code_depth += 1;
                out.push(event);
            }
            Event::End(TagEnd::CodeBlock) => {
                code_depth = code_depth.saturating_sub(1);
                out.push(event);
            }
            Event::Text(ref text) if code_depth == 0 => linkify_text(text, present, &mut out),
            _ => out.push(event),
        }
    }
    out
}

/// Split one text run into text + inline-link events for every id token
/// that resolves to a governed doc. Non-resolving tokens stay as text.
fn linkify_text<'a>(text: &str, present: &BTreeSet<DocumentId>, out: &mut Vec<Event<'a>>) {
    let re = id_token_regex();
    let mut last = 0;
    for m in re.find_iter(text) {
        let token = m.as_str();
        let resolves = token
            .parse::<DocumentId>()
            .map(|id| present.contains(&id))
            .unwrap_or(false);
        if !resolves {
            continue;
        }
        if m.start() > last {
            out.push(Event::Text(text[last..m.start()].to_string().into()));
        }
        out.push(Event::InlineHtml(
            format!(
                "<a class=\"xref\" href=\"/doc/{t}\">{t}</a>",
                t = escape_html(token)
            )
            .into(),
        ));
        last = m.end();
    }
    if last < text.len() {
        out.push(Event::Text(text[last..].to_string().into()));
    }
}

/// The display title from a document's body when its frontmatter has no
/// `title`/`name`: the first H1 heading, or the first heading of any
/// level if there is no H1. Empty when the doc has no headings (or no
/// AST). Matched by `raw_id` so it works for any namespace.
fn heading_title(documents: &[Document], raw_id: &str) -> String {
    documents
        .iter()
        .find(|d| d.raw_id == raw_id)
        .map(doc_heading_title)
        .unwrap_or_default()
}

/// [`heading_title`] for a document already in hand — the form the
/// file-level index uses, where entries are keyed by location and
/// `raw_id` is empty (ADR-103 § SRF-003).
fn doc_heading_title(doc: &Document) -> String {
    doc.ast
        .as_ref()
        .and_then(|ast| {
            ast.headings
                .iter()
                .find(|h| h.level == 1)
                .or_else(|| ast.headings.first())
        })
        .map(|h| h.text.clone())
        .unwrap_or_default()
}

/// A file-level entry's display title: the `title`/`name` frontmatter
/// chain `list` uses for id-keyed rows, falling back to the body's
/// first heading (ADR-103 § SRF-003).
fn file_title(doc: &Document) -> String {
    ["title", "name"]
        .iter()
        .map(|k| meta_str(doc, k))
        .find(|v| !v.is_empty())
        .unwrap_or_else(|| doc_heading_title(doc))
}

/// A frontmatter value as a display string; non-string scalars render
/// via their JSON form so a numeric or boolean field stays legible.
fn meta_str(doc: &Document, key: &str) -> String {
    match doc.metadata.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// A status pill, or nothing when the doc carries no status.
fn status_badge(status: &str) -> String {
    if status.is_empty() {
        return String::new();
    }
    format!("<span class=\"badge\">{}</span>", escape_html(status))
}

/// The reference-scanner id shape (`src/reference.rs` § REF-002), reused
/// to spot cross-ref tokens in body prose.
fn id_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b[A-Z][A-Z0-9]*-[0-9]+\b").expect("valid id token regex"))
}

/// A minimal error page for the rare case where re-ingest fails while
/// the server is up (e.g. the config was edited into an invalid state).
fn error_page(message: &str) -> String {
    page(
        "ctxgrd — error",
        &format!(
            "<h1>Could not render</h1>\n<pre>{}</pre>\n",
            escape_html(message)
        ),
    )
}

/// HTML-escape the five significant characters. Applied to every value
/// interpolated into the hand-built chrome (SRV-002).
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A repo-wide digest of `.md` mtimes for live-reload (SRV-007). Prunes
/// hidden directories so `.git` and friends don't inflate the walk;
/// over-triggering on a non-governed `.md` edit is acceptable
/// (ADR-097 Open Questions).
fn compute_digest(root: &Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::UNIX_EPOCH;

    let walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        !(e.file_type().is_dir()
            && e
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with('.') && s != "."))
    });

    let mut stamps: Vec<(String, u64)> = Vec::new();
    for entry in walker.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        stamps.push((path.display().to_string(), mtime));
    }
    stamps.sort();

    let mut hasher = DefaultHasher::new();
    stamps.hash(&mut hasher);
    hasher.finish()
}

// -- HTTP response -----------------------------------------------------

/// One fully-buffered HTTP response. Buffered so the `Content-Length`
/// header is exact and the write is a single pass (SRV-001: no chunked,
/// no keep-alive).
struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn html(status: u16, html: &str) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body: html.as_bytes().to_vec(),
        }
    }

    fn asset(content_type: &'static str, s: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: s.as_bytes().to_vec(),
        }
    }

    fn not_found() -> Self {
        Self::html(
            404,
            &page(
                "404 — ctxgrd",
                "<h1>404</h1>\n<p class=\"empty\">No such governed document or route.</p>\n",
            ),
        )
    }

    fn method_not_allowed() -> Self {
        Self {
            status: 405,
            content_type: "text/plain; charset=utf-8",
            body: b"405 method not allowed\n".to_vec(),
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

/// Write status line, headers, and body in one pass, then flush and let
/// the caller drop the stream (`Connection: close`).
fn write_response(stream: &mut TcpStream, resp: &Response) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        resp.status,
        reason_phrase(resp.status),
        resp.content_type,
        resp.body.len(),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Ast;
    use std::collections::BTreeMap;

    fn present(ids: &[&str]) -> BTreeSet<DocumentId> {
        ids.iter().map(|s| s.parse().expect("valid id")).collect()
    }

    #[test]
    fn escape_html_escapes_the_five_significant_chars() {
        assert_eq!(
            escape_html(r#"<b>a & "b" 'c'</b>"#),
            "&lt;b&gt;a &amp; &quot;b&quot; &#39;c&#39;&lt;/b&gt;"
        );
    }

    #[test]
    fn markdown_linkifies_present_cross_ref() {
        let html = render_markdown("See ADR-032 for the rule.", &present(&["ADR-32"]));
        assert!(
            html.contains(r#"<a class="xref" href="/doc/ADR-032">ADR-032</a>"#),
            "present id not linked: {html}"
        );
    }

    #[test]
    fn markdown_leaves_absent_cross_ref_as_text() {
        let html = render_markdown("See ADR-999 for the rule.", &present(&["ADR-1"]));
        assert!(!html.contains("href=\"/doc/ADR-999\""), "absent id linked: {html}");
        assert!(html.contains("ADR-999"), "absent id dropped: {html}");
    }

    #[test]
    fn markdown_does_not_linkify_inside_code_block() {
        let md = "```\nADR-032 in code\n```";
        let html = render_markdown(md, &present(&["ADR-32"]));
        assert!(!html.contains("href=\"/doc/ADR-032\""), "linked in code: {html}");
    }

    #[test]
    fn markdown_renders_gfm_pipe_table_as_html_table() {
        let md = "| Date | Change |\n| --- | --- |\n| 2026-04-26 | RUL-001 added |";
        let html = render_markdown(md, &BTreeSet::new());
        assert!(html.contains("<table>"), "pipe table not rendered: {html}");
        assert!(html.contains("<th>Date</th>"), "table header missing: {html}");
    }

    #[test]
    fn markdown_shows_mermaid_block_as_source_not_drawn() {
        let md = "```mermaid\ngraph TD; A-->B;\n```";
        let html = render_markdown(md, &BTreeSet::new());
        assert!(html.contains("<pre><code"), "mermaid not rendered as a code block: {html}");
        assert!(html.contains("graph TD"), "mermaid source missing: {html}");
    }

    #[test]
    fn dep_link_links_present_and_mutes_absent() {
        let p = present(&["ADR-1"]);
        assert!(dep_link("ADR-1", &p).contains(r#"href="/doc/ADR-1""#));
        let absent = dep_link("PRD-999", &p);
        assert!(absent.contains("dep-unresolved"), "absent dep not muted: {absent}");
        assert!(!absent.contains("href="), "absent dep linked: {absent}");
    }

    #[test]
    fn route_unknown_path_is_404_no_disk_read() {
        // A traversal attempt resolves to no governed doc → 404 by the
        // closed route table, never a filesystem read (SRV-005).
        let resp = route(Path::new("."), "/doc/../../etc/passwd");
        assert_eq!(resp.status, 404);
        // Same guarantee for the file-level route (ADR-103 § SRF-002):
        // a path outside the scanned set never resolves.
        let resp = route(Path::new("."), "/file/../Cargo.toml");
        assert_eq!(resp.status, 404);
        let resp = route(Path::new("."), "/file/src/serve.rs");
        assert_eq!(resp.status, 404);
        let resp = route(Path::new("."), "/nope");
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn route_static_assets_served() {
        assert_eq!(route(Path::new("."), "/static/app.css").status, 200);
        assert_eq!(route(Path::new("."), "/static/app.js").status, 200);
        // A near-miss under /static is not a route (no disk fallback).
        assert_eq!(route(Path::new("."), "/static/../app.css").status, 404);
    }

    #[test]
    fn render_doc_body_escapes_html_bearing_frontmatter() {
        // An HTML-bearing status must be escaped in the hand-built chrome
        // (pulldown escapes the body, but the chrome does not).
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "status".to_string(),
            serde_json::Value::String("<script>alert(1)</script>".to_string()),
        );
        let doc = Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: "ADR-1".to_string(),
            location: "adrs/ADR-1.md".to_string(),
            depends_on: vec![],
            frontmatter_lines: BTreeMap::new(),
            metadata,
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        };
        let html = render_doc_body(&doc, &BTreeSet::new());
        assert!(html.contains("&lt;script&gt;"), "status not escaped: {html}");
        assert!(!html.contains("<script>alert"), "status injected: {html}");
    }

    #[test]
    fn render_doc_body_shows_location_for_file_level_doc() {
        // ADR-103 § SRF-004: a synthetic file-level doc has an empty
        // `raw_id`, so the chrome shows its location; `depends_on` is
        // always empty on synthetic docs, so no deps row appears.
        let doc = Document {
            id: "AGENTS-0".parse().unwrap(),
            raw_id: String::new(),
            location: "docs/guides/getting-started.md".to_string(),
            depends_on: vec![],
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        };
        let html = render_doc_body(&doc, &BTreeSet::new());
        assert!(
            html.contains("docs/guides/getting-started.md"),
            "location missing from chrome: {html}"
        );
        assert!(!html.contains("deps-label"), "unexpected depends_on row: {html}");
    }

    #[test]
    fn file_title_prefers_frontmatter_then_falls_back_to_heading() {
        use crate::ast::Heading;
        let mut doc = Document {
            id: "AGENTS-0".parse().unwrap(),
            raw_id: String::new(),
            location: "CLAUDE.md".to_string(),
            depends_on: vec![],
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings: vec![Heading {
                    level: 1,
                    text: "Heading title".to_string(),
                    line: 1,
                    col: 1,
                }],
                ..Ast::default()
            }),
            body: String::new(),
        };
        assert_eq!(file_title(&doc), "Heading title");
        doc.metadata.insert(
            "title".to_string(),
            serde_json::Value::String("Frontmatter title".to_string()),
        );
        assert_eq!(file_title(&doc), "Frontmatter title");
    }

    #[test]
    fn heading_title_falls_back_to_body_h1_when_frontmatter_has_no_title() {
        use crate::ast::Heading;
        let doc = Document {
            id: "FEEDBACK-1".parse().unwrap(),
            raw_id: "FEEDBACK-1".to_string(),
            location: "feedback/001-x.md".to_string(),
            depends_on: vec![],
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast {
                headings: vec![
                    Heading {
                        level: 1,
                        text: "Field feedback — dogfooding ctxgrd".to_string(),
                        line: 5,
                        col: 1,
                    },
                    Heading {
                        level: 2,
                        text: "Section".to_string(),
                        line: 9,
                        col: 1,
                    },
                ],
                ..Ast::default()
            }),
            body: String::new(),
        };
        assert_eq!(
            heading_title(std::slice::from_ref(&doc), "FEEDBACK-1"),
            "Field feedback — dogfooding ctxgrd"
        );
    }

    #[test]
    fn heading_title_is_empty_when_no_headings() {
        let doc = Document {
            id: "FEEDBACK-1".parse().unwrap(),
            raw_id: "FEEDBACK-1".to_string(),
            location: "feedback/001-x.md".to_string(),
            depends_on: vec![],
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast::default()),
            body: String::new(),
        };
        assert_eq!(heading_title(std::slice::from_ref(&doc), "FEEDBACK-1"), "");
    }

    #[test]
    fn render_doc_body_has_no_h1_in_chrome() {
        // The body owns the sole H1; the chrome must not add a second.
        let doc = Document {
            id: "ADR-1".parse().unwrap(),
            raw_id: "ADR-1".to_string(),
            location: "adrs/ADR-1.md".to_string(),
            depends_on: vec![],
            frontmatter_lines: BTreeMap::new(),
            metadata: BTreeMap::new(),
            pin: None,
            ast: Some(Ast::default()),
            body: "---\nid: ADR-1\n---\n# The one heading\n".to_string(),
        };
        let html = render_doc_body(&doc, &BTreeSet::new());
        assert_eq!(html.matches("<h1>").count(), 1, "expected exactly one H1: {html}");
    }
}
