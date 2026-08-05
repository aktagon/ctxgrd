//! Language Server Protocol (LSP) adapter.
//!
//! Translates between LSP requests (stdio) and core analysis logic.
//! Reuses [`run::IngestResult`] and [`run::lint`] logic where possible
//! while maintaining an in-memory index for fast navigation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::run;

pub(crate) struct Backend {
    client: Client,
    workspace_state: Arc<RwLock<WorkspaceState>>,
}

#[derive(Default)]
struct WorkspaceState {
    /// Root path of the workspace.
    root_path: Option<Arc<PathBuf>>,
    /// Loaded configuration.
    config: Option<Arc<crate::config::Config>>,
    /// In-memory index of documents and their IDs.
    index: WorkspaceIndex,
}

#[derive(Default)]
struct WorkspaceIndex {
    /// URI -> Parsed document
    documents: HashMap<Url, Arc<crate::document::Document>>,
    /// Document ID -> Location of definition
    definitions: HashMap<String, Location>,
    /// Document ID -> List of reference locations
    references: HashMap<String, Vec<Location>>,
}

impl WorkspaceIndex {
    fn clear(&mut self) {
        self.documents.clear();
        self.definitions.clear();
        self.references.clear();
    }

    fn remove_document(&mut self, uri: &Url) {
        if let Some(doc) = self.documents.remove(uri) {
            self.definitions.remove(&doc.raw_id);
            for refs in self.references.values_mut() {
                refs.retain(|l| l.uri != *uri);
            }
        }
    }

    fn index_document(&mut self, uri: Url, doc: crate::document::Document) {
        let raw_id = doc.raw_id.clone();

        let definition_location = Location {
            uri: uri.clone(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
        };
        self.definitions.insert(raw_id, definition_location);

        if let Some(ast) = &doc.ast {
            for token in &ast.cross_ref_tokens {
                if token.in_code || token.in_strikethrough {
                    continue;
                }
                let ref_loc = Location {
                    uri: uri.clone(),
                    range: Range {
                        start: Position {
                            line: token.line.saturating_sub(1),
                            character: token.col.saturating_sub(1),
                        },
                        end: Position {
                            line: token.line.saturating_sub(1),
                            character: token.col.saturating_sub(1) + token.token.len() as u32,
                        },
                    },
                };
                self.references
                    .entry(token.token.clone())
                    .or_default()
                    .push(ref_loc);
            }
        }

        for dep in &doc.depends_on {
            let line = doc
                .frontmatter_lines
                .get("depends_on")
                .copied()
                .unwrap_or(0);
            let ref_loc = Location {
                uri: uri.clone(),
                range: Range {
                    start: Position {
                        line: line.saturating_sub(1),
                        character: 0,
                    },
                    end: Position {
                        line: line.saturating_sub(1),
                        character: 0,
                    },
                },
            };
            self.references
                .entry(dep.clone())
                .or_default()
                .push(ref_loc);
        }

        self.documents.insert(uri, Arc::new(doc));
    }
}

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            workspace_state: Arc::new(RwLock::default()),
        }
    }

    async fn scan_workspace(&self) -> Result<()> {
        let root = {
            let state = self.workspace_state.read().await;
            match &state.root_path {
                Some(p) => Arc::clone(p),
                None => return Ok(()),
            }
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!("Scanning workspace: {}", root.display()),
            )
            .await;

        match run::ingest(&root) {
            Ok(ingest) => {
                let mut state = self.workspace_state.write().await;
                state.config = Some(Arc::new(ingest.config));
                state.index.clear();
                let mut diagnostics_by_uri: HashMap<Url, Vec<tower_lsp::lsp_types::Diagnostic>> =
                    HashMap::new();

                for doc in ingest.documents {
                    let full_path = root.join(&doc.location);
                    if let Ok(uri) = Url::from_file_path(full_path) {
                        state.index.index_document(uri.clone(), doc);
                    }
                }

                // Run linting to get diagnostics
                let outcome = match run::lint(&root) {
                    Ok(o) => o,
                    Err(_) => return Ok(()),
                };

                for diag in outcome.diagnostics {
                    let full_path = root.join(&diag.location);
                    if let Ok(uri) = Url::from_file_path(full_path) {
                        let lsp_diag = to_lsp_diagnostic(diag);
                        diagnostics_by_uri.entry(uri).or_default().push(lsp_diag);
                    }
                }

                for (uri, diags) in diagnostics_by_uri {
                    self.publish_diagnostics(uri, diags).await;
                }
            }
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("Workspace scan failed: {}", e))
                    .await;
            }
        }

        Ok(())
    }

    async fn on_document_changed(&self, uri: Url, text: &str) {
        let (root, config) = {
            let state = self.workspace_state.read().await;
            match (&state.root_path, &state.config) {
                (Some(r), Some(c)) => (Arc::clone(r), Arc::clone(c)),
                _ => return,
            }
        };

        let path_claims = crate::path_claims::PathClaims::from_config(&config);
        let location = uri
            .to_file_path()
            .ok()
            .and_then(|p| {
                p.strip_prefix(&*root)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_default();

        {
            let mut state = self.workspace_state.write().await;
            match crate::source::markdown::parse_one(text, location, &path_claims) {
                crate::source::markdown::ParseOutcome::Document(doc) => {
                    state.index.remove_document(&uri);
                    state.index.index_document(uri.clone(), doc);
                }
                _ => {
                    state.index.remove_document(&uri);
                }
            }
        }

        // For now, re-run full lint to update diagnostics
        if let Ok(outcome) = run::lint(&root) {
            let mut diagnostics_by_uri: HashMap<Url, Vec<tower_lsp::lsp_types::Diagnostic>> =
                HashMap::new();
            for diag in outcome.diagnostics {
                let full_path = root.join(&diag.location);
                if let Ok(u) = Url::from_file_path(full_path) {
                    let lsp_diag = to_lsp_diagnostic(diag);
                    diagnostics_by_uri.entry(u).or_default().push(lsp_diag);
                }
            }

            // Collect URIs to clear while holding the read lock
            let all_uris: Vec<Url> = {
                let state = self.workspace_state.read().await;
                state.index.documents.keys().cloned().collect()
            };

            for existing_uri in all_uris {
                if !diagnostics_by_uri.contains_key(&existing_uri) {
                    self.publish_diagnostics(existing_uri, Vec::new()).await;
                }
            }
            for (u, diags) in diagnostics_by_uri {
                self.publish_diagnostics(u, diags).await;
            }
        }
    }

    async fn publish_diagnostics(
        &self,
        uri: Url,
        diagnostics: Vec<tower_lsp::lsp_types::Diagnostic>,
    ) {
        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }

    async fn inner_goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let state = self.workspace_state.read().await;
        let doc = match state.index.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        if let Some(token) = find_token_at_position(&doc.body, position) {
            if let Some(location) = state.index.definitions.get(&token) {
                return Ok(Some(GotoDefinitionResponse::Scalar(location.clone())));
            }
        }

        Ok(None)
    }
}

fn to_lsp_diagnostic(diag: crate::diagnostic::Diagnostic) -> tower_lsp::lsp_types::Diagnostic {
    let severity = match diag.severity {
        crate::diagnostic::Severity::Error => Some(DiagnosticSeverity::ERROR),
        crate::diagnostic::Severity::Warning => Some(DiagnosticSeverity::WARNING),
        crate::diagnostic::Severity::Info => Some(DiagnosticSeverity::INFORMATION),
    };

    // ctxgrd uses 1-indexed lines/cols (None when unknown). LSP uses
    // 0-indexed and has no "unknown", so an absent position anchors at 0.
    let start_pos = Position {
        line: diag.line.map_or(0, |l| l.saturating_sub(1)),
        character: diag.col.map_or(0, |c| c.saturating_sub(1)),
    };
    let end_pos = Position {
        line: diag.line.map_or(0, |l| l.saturating_sub(1)),
        character: diag.col.unwrap_or(0),
    };

    let mut message = diag.message;
    if let Some(help) = diag.help {
        message.push_str(&format!("\n\nHelp: {}", help));
    }
    if let Some(note) = diag.note {
        message.push_str(&format!("\n\nNote: {}", note));
    }

    tower_lsp::lsp_types::Diagnostic {
        range: Range {
            start: start_pos,
            end: end_pos,
        },
        severity,
        code: Some(NumberOrString::String(diag.code)),
        source: Some("ctxgrd".to_string()),
        message,
        ..Default::default()
    }
}

fn cross_ref_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[A-Z][A-Z0-9]*-[0-9]+").unwrap())
}

fn find_token_at_position(text: &str, position: Position) -> Option<String> {
    let line = text.lines().nth(position.line as usize)?;
    let char_idx = position.character as usize;

    if char_idx >= line.len() {
        return None;
    }

    // Broad pattern for ID: [A-Z][A-Z0-9]*-[0-9]+
    let re = cross_ref_regex();
    for mat in re.find_iter(line) {
        if char_idx >= mat.start() && char_idx <= mat.end() {
            return Some(mat.as_str().to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_token_at_position() {
        let text = "See ADR-001 for details.\nAnd PRD-123 too.";

        // At 'A' in ADR-001
        assert_eq!(
            find_token_at_position(
                text,
                Position {
                    line: 0,
                    character: 4
                }
            ),
            Some("ADR-001".to_string())
        );

        // At 'R' in ADR-001
        assert_eq!(
            find_token_at_position(
                text,
                Position {
                    line: 0,
                    character: 6
                }
            ),
            Some("ADR-001".to_string())
        );

        // At '1' in ADR-001
        assert_eq!(
            find_token_at_position(
                text,
                Position {
                    line: 0,
                    character: 10
                }
            ),
            Some("ADR-001".to_string())
        );

        // Outside
        assert_eq!(
            find_token_at_position(
                text,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            None
        );

        // Second line
        assert_eq!(
            find_token_at_position(
                text,
                Position {
                    line: 1,
                    character: 4
                }
            ),
            Some("PRD-123".to_string())
        );
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let mut state = self.workspace_state.write().await;

        if let Some(uri) = params.root_uri {
            if let Ok(path) = uri.to_file_path() {
                state.root_path = Some(Arc::new(path));
            }
        } else {
            #[allow(deprecated)]
            if let Some(path_str) = params.root_path {
                state.root_path = Some(Arc::new(PathBuf::from(path_str)));
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["-".to_string()]),
                    ..Default::default()
                }),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "ctxgrd lsp initialized")
            .await;

        let _ = self.scan_workspace().await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn symbol(
        &self,
        _params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let state = self.workspace_state.read().await;
        let mut symbols = Vec::new();

        for (id, loc) in &state.index.definitions {
            #[allow(deprecated)]
            symbols.push(SymbolInformation {
                name: id.clone(),
                kind: SymbolKind::FILE,
                location: loc.clone(),
                container_name: None,
                tags: None,
                deprecated: None,
            });
        }

        Ok(Some(symbols))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.inner_goto_definition(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let state = self.workspace_state.read().await;
        let doc = match state.index.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        if let Some(token) = find_token_at_position(&doc.body, position) {
            if let Some(refs) = state.index.references.get(&token) {
                return Ok(Some(refs.clone()));
            }
        }

        Ok(None)
    }

    async fn completion(&self, _params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let state = self.workspace_state.read().await;
        let mut items = Vec::new();

        for doc in state.index.documents.values() {
            let label = doc.raw_id.clone();
            let detail = doc
                .metadata
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let documentation = doc
                .metadata
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("Status: {}", s),
                    })
                });

            items.push(CompletionItem {
                label,
                kind: Some(CompletionItemKind::FILE),
                detail,
                documentation,
                ..Default::default()
            });
        }

        items.sort_by(|a, b| a.label.cmp(&b.label));

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let state = self.workspace_state.read().await;
        let doc = match state.index.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        if let Some(token) = find_token_at_position(&doc.body, position) {
            if let Some(target_doc) = state
                .index
                .definitions
                .get(&token)
                .and_then(|loc| state.index.documents.get(&loc.uri))
            {
                let mut contents = format!("**{}**", target_doc.raw_id);
                if let Some(title) = target_doc.metadata.get("title").and_then(|v| v.as_str()) {
                    contents.push_str(&format!("\n\n{}", title));
                }
                if let Some(status) = target_doc.metadata.get("status").and_then(|v| v.as_str()) {
                    contents.push_str(&format!("\n\nStatus: {}", status));
                }

                return Ok(Some(Hover {
                    contents: HoverContents::Scalar(MarkedString::String(contents)),
                    range: None,
                }));
            }
        }

        Ok(None)
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_document_changed(params.text_document.uri, &params.text_document.text)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().next() {
            self.on_document_changed(params.text_document.uri, &change.text)
                .await;
        }
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        let _ = self.scan_workspace().await;
    }

    async fn did_close(&self, _params: DidCloseTextDocumentParams) {}
}

pub async fn run_server() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = tower_lsp::LspService::new(Backend::new);
    tower_lsp::Server::new(stdin, stdout, socket)
        .serve(service)
        .await;
}
