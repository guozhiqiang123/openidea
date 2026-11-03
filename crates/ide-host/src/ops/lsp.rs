use crate::Host;
use crate::apply_workspace_edit as apply_edit;
use crate::plugins::{ProviderKind, editor_providers_for as editor_providers};
use crate::rpc::Dispatcher;
use ide_core::{DocId, IdeResult, Location, Pos, Range, RequestId, ServerId, TextEdit, WorkspaceEdit};
use ide_lsp::types::*;
use ide_plugin::ProviderDocument;

pub async fn lsp_completion(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    pos: Pos,
    trigger: Option<String>,
) -> IdeResult<CompletionList> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.completion(doc, pos, trigger.clone(), cancel).await;
    host.end_request(request);
    // Plugin providers (`[[contributes.providers]]`) add theirs.
    let Some((plugins, d)) = plugin_providers(host, window, doc, ProviderKind::Completion) else { return result };
    let items = blocking(move || plugins.provide_completion(&d, pos, trigger.as_deref())).await.unwrap_or_default();
    if items.is_empty() {
        return result;
    }
    let mut list = result.unwrap_or(CompletionList { is_incomplete: false, items: Vec::new() });
    list.items.extend(items.into_iter().map(|(_, item)| plugin_completion(item)));
    Ok(list)
}

/// The plugin host and the document, when some plugin provides `kind`.
fn plugin_providers(
    host: &Host,
    window: &str,
    doc: DocId,
    kind: ProviderKind,
) -> Option<(std::sync::Arc<crate::plugins::PluginHost>, ProviderDocument)> {
    let plugins = editor_providers(kind)?;
    let snap = host.project(window).ok()?.documents.snapshot(doc).ok()?;
    let d = ProviderDocument {
        path: snap.path.0,
        language: snap.language.0,
        version: u64::from(snap.version),
        text: snap.text,
    };
    Some((plugins, d))
}

/// Plugin calls block (the plugin's actor thread does the work).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// Items from plugins carry no server (resolve and commands don't apply).
const PLUGIN_SERVER: ServerId = ServerId(0);

fn plugin_completion(item: ide_plugin::CompletionItem) -> CompletionItem {
    let kind = item.kind.as_deref().and_then(|k| {
        Some(match k {
            "text" => CompletionKind::Text,
            "method" => CompletionKind::Method,
            "function" => CompletionKind::Function,
            "constructor" => CompletionKind::Constructor,
            "field" => CompletionKind::Field,
            "variable" => CompletionKind::Variable,
            "class" => CompletionKind::Class,
            "interface" => CompletionKind::Interface,
            "module" => CompletionKind::Module,
            "property" => CompletionKind::Property,
            "keyword" => CompletionKind::Keyword,
            "snippet" => CompletionKind::Snippet,
            "value" => CompletionKind::Value,
            "constant" => CompletionKind::Constant,
            "file" => CompletionKind::File,
            "reference" => CompletionKind::Reference,
            _ => return None,
        })
    });
    CompletionItem {
        label: item.label,
        label_detail: None,
        detail: item.detail,
        kind,
        documentation: item.documentation.map(|value| Markup { kind: "markdown".into(), value }),
        sort_text: item.sort_text,
        filter_text: None,
        preselect: false,
        deprecated: false,
        insert_text: item.insert_text,
        is_snippet: false,
        replace_range: item.replace,
        replace_range_tab: None,
        additional_edits: Vec::new(),
        commit_characters: Vec::new(),
        command: None,
        data: None,
        server: PLUGIN_SERVER,
    }
}

/// Inline (ghost text) suggestions at `pos`; `automatic` when triggered by typing.
pub async fn lsp_inline_completion(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    pos: Pos,
    automatic: bool,
) -> IdeResult<Vec<InlineCompletionItem>> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.inline_completion(doc, pos, automatic, cancel.clone()).await;
    // Plugins: cancelled (cooperatively) when the editor cancels the request.
    let plugin = match plugin_providers(host, window, doc, ProviderKind::InlineCompletion) {
        Some((plugins, d)) if !matches!(&result, Ok(items) if !items.is_empty()) => {
            let cancel = cancel.clone();
            blocking(move || plugins.provide_inline_completion(&d, pos, &cancel)).await.flatten()
        }
        _ => None,
    };
    host.end_request(request);
    match plugin {
        Some((_, s)) => Ok(vec![InlineCompletionItem {
            insert_text: s.insert_text,
            range: s.replace,
            filter_text: None,
            command: None,
            server: PLUGIN_SERVER,
        }]),
        None => result,
    }
}

pub async fn lsp_resolve_completion(
    host: &Host,
    window: &str,
    doc: DocId,
    item: CompletionItem,
) -> IdeResult<CompletionItem> {
    host.project(window)?.lsp.resolve_completion(doc, item).await
}

pub async fn lsp_hover(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    pos: Pos,
) -> IdeResult<Option<Hover>> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.hover(doc, pos, cancel).await;
    host.end_request(request);
    result
}

pub async fn lsp_signature_help(host: &Host, window: &str, doc: DocId, pos: Pos) -> IdeResult<Option<SignatureHelp>> {
    host.project(window)?.lsp.signature_help(doc, pos).await
}

pub async fn lsp_goto(host: &Host, window: &str, doc: DocId, pos: Pos, kind: GotoKind) -> IdeResult<Vec<Location>> {
    let result = host.project(window)?.lsp.goto(doc, pos, kind).await;
    if kind != GotoKind::Definition {
        return result;
    }
    let Some((plugins, d)) = plugin_providers(host, window, doc, ProviderKind::Definition) else { return result };
    let found = blocking(move || plugins.provide_definition(&d, pos)).await.unwrap_or_default();
    if found.is_empty() {
        return result;
    }
    let mut out = result.unwrap_or_default();
    out.extend(found.into_iter().map(|l| Location { path: l.path.into(), range: l.range, uri: None }));
    Ok(out)
}

/// Gutter markers from plugins (`line-markers` providers) for `doc`.
pub async fn plugin_line_markers(
    host: &Host,
    window: &str,
    doc: DocId,
) -> IdeResult<Vec<crate::plugins::LineMarkerInfo>> {
    let Some((plugins, d)) = plugin_providers(host, window, doc, ProviderKind::LineMarkers) else {
        return Ok(Vec::new());
    };
    let markers = blocking(move || plugins.provide_line_markers(&d)).await.unwrap_or_default();
    Ok(markers
        .into_iter()
        .map(|(plugin_id, m)| crate::plugins::LineMarkerInfo {
            plugin_id,
            line: m.line,
            icon: m.icon,
            tooltip: m.tooltip,
            target: m.target.map(|l| Location { path: l.path.into(), range: l.range, uri: None }),
        })
        .collect())
}

/// Implementations of the symbols at `positions`, one list per position (the
/// editor's "is implemented / overridden" gutter markers). Cancel with
/// `cancel_request(request)`.
pub async fn lsp_implementations(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    positions: Vec<Pos>,
) -> IdeResult<Vec<Vec<Location>>> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.implementations(doc, &positions, cancel).await;
    host.end_request(request);
    result
}

/// Contents of a virtual document (library class, decompiled code) that a
/// language server returned a non-`file:` URI for. Cached per project.
pub async fn lsp_virtual_document(host: &Host, window: &str, uri: String) -> IdeResult<VirtualDocument> {
    host.project(window)?.lsp.virtual_document(&uri).await
}

/// References of the symbol at `pos`. `project_wide` also searches project files
/// outside the language server's analysis scope; bulk scans over many symbols
/// leave it off. Cancel with `cancel_request(request)`.
pub async fn lsp_references(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    pos: Pos,
    include_declaration: bool,
    project_wide: bool,
) -> IdeResult<References> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.references(doc, pos, include_declaration, project_wide, cancel).await;
    host.end_request(request);
    result
}

pub async fn lsp_document_highlights(host: &Host, window: &str, doc: DocId, pos: Pos) -> IdeResult<Vec<Highlight>> {
    host.project(window)?.lsp.document_highlights(doc, pos).await
}

pub async fn lsp_workspace_symbols(
    host: &Host,
    window: &str,
    request: RequestId,
    query: String,
) -> IdeResult<Vec<WorkspaceSymbol>> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.workspace_symbols(&query, cancel).await;
    host.end_request(request);
    result
}

pub async fn lsp_code_actions(
    host: &Host,
    window: &str,
    doc: DocId,
    range: Range,
    only: Vec<String>,
) -> IdeResult<Vec<CodeAction>> {
    let result = host.project(window)?.lsp.code_actions(doc, range, only.clone()).await;
    let Some((plugins, d)) = plugin_providers(host, window, doc, ProviderKind::CodeActions) else { return result };
    let actions = blocking(move || plugins.provide_code_actions(&d, range)).await.unwrap_or_default();
    let actions: Vec<CodeAction> = actions
        .into_iter()
        .map(|(_, a)| plugin_code_action(a))
        .filter(|a| {
            only.is_empty()
                || a.kind.as_ref().is_some_and(|k| only.iter().any(|o| k == o || k.starts_with(&format!("{o}."))))
        })
        .collect();
    if actions.is_empty() {
        return result;
    }
    let mut out = result.unwrap_or_default();
    out.extend(actions);
    Ok(out)
}

fn plugin_code_action(a: ide_plugin::CodeAction) -> CodeAction {
    let files =
        a.edits.into_iter().map(|(path, edits)| ide_core::FileEdit::Edit { path: path.into(), edits }).collect();
    CodeAction {
        title: a.title,
        kind: a.kind,
        is_preferred: a.preferred,
        disabled_reason: None,
        edit: Some(WorkspaceEdit { files }),
        command: None,
        data: None,
        server: PLUGIN_SERVER,
    }
}

/// Cancellable code actions with the kinds the servers advertise (Refactor This, refactoring availability).
pub async fn lsp_code_action_query(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    range: Range,
    only: Vec<String>,
) -> IdeResult<CodeActionQuery> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.code_action_query(doc, range, only, cancel).await;
    host.end_request(request);
    result
}

/// Resolves a code action's edit without applying it (intention preview).
pub async fn lsp_resolve_code_action(
    host: &Host,
    window: &str,
    doc: DocId,
    action: CodeAction,
) -> IdeResult<CodeAction> {
    if action.edit.is_some() {
        return Ok(action);
    }
    host.project(window)?.lsp.resolve_code_action(doc, action).await
}

/// Resolves (if needed), applies the edit, then runs the command of a code action.
pub async fn lsp_apply_code_action(host: &Host, window: &str, doc: DocId, action: CodeAction) -> IdeResult<()> {
    let project = host.project(window)?;
    let action = if action.edit.is_none() && action.command.is_none() {
        project.lsp.resolve_code_action(doc, action).await?
    } else {
        action
    };
    if let Some(edit) = &action.edit {
        apply_edit(&project.documents, edit)?;
    }
    if let Some(command) = action.command {
        project.lsp.execute_command(doc, command).await?;
    }
    Ok(())
}

pub async fn lsp_prepare_rename(host: &Host, window: &str, doc: DocId, pos: Pos) -> IdeResult<Option<Range>> {
    host.project(window)?.lsp.prepare_rename(doc, pos).await
}

/// Computes the rename edit without applying it, so the UI can preview conflicts.
pub async fn lsp_rename(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    pos: Pos,
    new_name: String,
) -> IdeResult<Renamed> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.rename(doc, pos, new_name, cancel).await;
    host.end_request(request);
    result
}

pub async fn lsp_format(
    host: &Host,
    window: &str,
    doc: DocId,
    range: Option<Range>,
    options: FormatOptions,
) -> IdeResult<Vec<TextEdit>> {
    host.project(window)?.lsp.format(doc, range, options).await
}

pub async fn lsp_inlay_hints(host: &Host, window: &str, doc: DocId, range: Range) -> IdeResult<Vec<InlayHint>> {
    host.project(window)?.lsp.inlay_hints(doc, range).await
}

pub async fn lsp_semantic_tokens(host: &Host, window: &str, doc: DocId) -> IdeResult<Option<SemanticTokens>> {
    host.project(window)?.lsp.semantic_tokens(doc).await
}

pub async fn lsp_code_lens(host: &Host, window: &str, doc: DocId) -> IdeResult<Vec<CodeLens>> {
    host.project(window)?.lsp.code_lens(doc).await
}

/// Links in a document (`href`s, Markdown links, `#include`s…) from its language servers.
pub async fn lsp_document_links(host: &Host, window: &str, doc: DocId) -> IdeResult<Vec<DocumentLink>> {
    host.project(window)?.lsp.document_links(doc).await
}

pub async fn lsp_call_hierarchy(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    pos: Pos,
    direction: HierarchyDirection,
) -> IdeResult<CallHierarchy> {
    let lsp = host.project(window)?.lsp.clone();
    let cancel = host.begin_request(request);
    let result = lsp.call_hierarchy(doc, pos, direction, cancel).await;
    host.end_request(request);
    result
}

pub async fn lsp_type_hierarchy(
    host: &Host,
    window: &str,
    doc: DocId,
    pos: Pos,
    direction: HierarchyDirection,
) -> IdeResult<Vec<HierarchyItem>> {
    host.project(window)?.lsp.type_hierarchy(doc, pos, direction).await
}

pub fn lsp_servers(host: &Host, window: &str) -> IdeResult<Vec<ServerInfo>> {
    Ok(host.project(window)?.lsp.servers())
}

pub fn lsp_resend_diagnostics(host: &Host, window: &str) -> IdeResult<()> {
    host.project(window)?.lsp.resend_diagnostics();
    Ok(())
}

pub async fn lsp_restart(host: &Host, window: &str, server: ServerId) -> IdeResult<()> {
    host.project(window)?.lsp.restart(server).await
}

/// Applies a (previewed) workspace edit, e.g. after rename confirmation.
pub fn apply_workspace_edit(host: &Host, window: &str, edit: WorkspaceEdit) -> IdeResult<()> {
    let project = host.project(window)?;
    // Multi-file refactorings leave a Local History point to go back to.
    let paths: Vec<&std::path::Path> = edit
        .files
        .iter()
        .filter_map(|f| match f {
            ide_core::FileEdit::Edit { path, .. } => Some(&**path),
            _ => None,
        })
        .collect();
    if edit.files.len() > 1 {
        project.record_before_refactoring(&paths, "Apply changes");
    }
    apply_edit(&project.documents, &edit)
}

/// Runs a server command (`workspace/executeCommand`), e.g. a completion item's
/// follow-up command. Edits the server requests arrive via `workspace/applyEdit`.
pub async fn lsp_execute_command(host: &Host, window: &str, doc: DocId, command: Command) -> IdeResult<()> {
    host.project(window)?.lsp.execute_command(doc, command).await
}

/// Commands of this module the remote server answers.
pub(crate) fn register(d: &mut Dispatcher) {
    d.add("lsp_completion", |c| async move {
        lsp_completion(&c.host, &c.window, c.arg("request")?, c.arg("doc")?, c.arg("pos")?, c.arg("trigger")?).await
    });
    d.add("lsp_inline_completion", |c| async move {
        lsp_inline_completion(&c.host, &c.window, c.arg("request")?, c.arg("doc")?, c.arg("pos")?, c.arg("automatic")?)
            .await
    });
    d.add("lsp_resolve_completion", |c| async move {
        lsp_resolve_completion(&c.host, &c.window, c.arg("doc")?, c.arg("item")?).await
    });
    d.add("lsp_hover", |c| async move {
        lsp_hover(&c.host, &c.window, c.arg("request")?, c.arg("doc")?, c.arg("pos")?).await
    });
    d.add("lsp_signature_help", |c| async move {
        lsp_signature_help(&c.host, &c.window, c.arg("doc")?, c.arg("pos")?).await
    });
    d.add(
        "lsp_goto",
        |c| async move { lsp_goto(&c.host, &c.window, c.arg("doc")?, c.arg("pos")?, c.arg("kind")?).await },
    );
    d.add("lsp_implementations", |c| async move {
        lsp_implementations(&c.host, &c.window, c.arg("request")?, c.arg("doc")?, c.arg("positions")?).await
    });
    d.add("lsp_virtual_document", |c| async move { lsp_virtual_document(&c.host, &c.window, c.arg("uri")?).await });
    d.add("lsp_references", |c| async move {
        lsp_references(
            &c.host,
            &c.window,
            c.arg("request")?,
            c.arg("doc")?,
            c.arg("pos")?,
            c.arg("includeDeclaration")?,
            c.arg("projectWide")?,
        )
        .await
    });
    d.add("lsp_document_highlights", |c| async move {
        lsp_document_highlights(&c.host, &c.window, c.arg("doc")?, c.arg("pos")?).await
    });
    d.add("lsp_workspace_symbols", |c| async move {
        lsp_workspace_symbols(&c.host, &c.window, c.arg("request")?, c.arg("query")?).await
    });
    d.add("lsp_code_actions", |c| async move {
        lsp_code_actions(&c.host, &c.window, c.arg("doc")?, c.arg("range")?, c.arg("only")?).await
    });
    d.add("lsp_code_action_query", |c| async move {
        lsp_code_action_query(&c.host, &c.window, c.arg("request")?, c.arg("doc")?, c.arg("range")?, c.arg("only")?)
            .await
    });
    d.add("lsp_resolve_code_action", |c| async move {
        lsp_resolve_code_action(&c.host, &c.window, c.arg("doc")?, c.arg("action")?).await
    });
    d.add("lsp_apply_code_action", |c| async move {
        lsp_apply_code_action(&c.host, &c.window, c.arg("doc")?, c.arg("action")?).await
    });
    d.add("lsp_prepare_rename", |c| async move {
        lsp_prepare_rename(&c.host, &c.window, c.arg("doc")?, c.arg("pos")?).await
    });
    d.add("lsp_rename", |c| async move {
        lsp_rename(&c.host, &c.window, c.arg("request")?, c.arg("doc")?, c.arg("pos")?, c.arg("newName")?).await
    });
    d.add("lsp_format", |c| async move {
        lsp_format(&c.host, &c.window, c.arg("doc")?, c.arg("range")?, c.arg("options")?).await
    });
    d.add(
        "lsp_inlay_hints",
        |c| async move { lsp_inlay_hints(&c.host, &c.window, c.arg("doc")?, c.arg("range")?).await },
    );
    d.add("lsp_semantic_tokens", |c| async move { lsp_semantic_tokens(&c.host, &c.window, c.arg("doc")?).await });
    d.add("lsp_code_lens", |c| async move { lsp_code_lens(&c.host, &c.window, c.arg("doc")?).await });
    d.add("lsp_document_links", |c| async move { lsp_document_links(&c.host, &c.window, c.arg("doc")?).await });
    d.add("lsp_call_hierarchy", |c| async move {
        lsp_call_hierarchy(&c.host, &c.window, c.arg("request")?, c.arg("doc")?, c.arg("pos")?, c.arg("direction")?)
            .await
    });
    d.add("lsp_type_hierarchy", |c| async move {
        lsp_type_hierarchy(&c.host, &c.window, c.arg("doc")?, c.arg("pos")?, c.arg("direction")?).await
    });
    d.add("lsp_servers", |c| async move { lsp_servers(&c.host, &c.window) });
    d.add("lsp_resend_diagnostics", |c| async move { lsp_resend_diagnostics(&c.host, &c.window) });
    d.add("lsp_restart", |c| async move { lsp_restart(&c.host, &c.window, c.arg("server")?).await });
    d.add("apply_workspace_edit", |c| async move { apply_workspace_edit(&c.host, &c.window, c.arg("edit")?) });
    d.add("lsp_execute_command", |c| async move {
        lsp_execute_command(&c.host, &c.window, c.arg("doc")?, c.arg("command")?).await
    });
}
