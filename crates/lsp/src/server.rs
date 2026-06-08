//! The language server: capabilities, document lifecycle, and one handler per
//! LSP feature. Each handler recompiles the open document (the front-end is
//! fast and side-effect-free) and answers from the resulting [`Compiled`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use compiler::ast::{
    self, ExprKind as AstExprKind, ExternItem, ImportKind, ItemKind, ModuleKind,
    PatternKind as AstPatternKind, StmtKind as AstStmtKind, StructKind,
};
use compiler::ids::{DefId, ModId};
use compiler::sema::resolve_ctx::normalize;
use compiler::sema::{Lowerer, TypeEnv, ValueRes};
use compiler::span::Span;
use compiler::ty::{Ty, TyCtxt};
use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::stdlib_marker_signature;
use crate::analysis::{
    Compiled, DOC_FILE, LineIndex, TokenClass, builtin_signature, dot_completion_context,
    float_instance_methods, float_static_methods, int_instance_methods, keyword_texts,
    list_intrinsic_methods, map_intrinsic_methods, offset_at, position_at,
    primitive_static_methods, span_to_range, str_intrinsic_methods, top_named_name,
};
use compiler::sema::symbols::DefKind;
use compiler::ty::TyKind;

const MAX_WORKSPACE_SCAN_FILES: usize = 2048;

#[derive(Clone, Eq, PartialEq, Hash, Debug)]
struct SourceSpanKey {
    path: PathBuf,
    lo: usize,
    hi: usize,
}

/// Map any analysis [`Span`] to an editor [`Location`]: the open document, a
/// loaded submodule file (resolved through the `SourceMap`), or `None` for a
/// virtual file (prelude / synthesized code, beyond the real file count). Shared
/// by go-to-definition and cross-file references/rename.
fn span_to_location(c: &Compiled, span: Span, doc_uri: &Url) -> Option<Location> {
    if span.file == DOC_FILE {
        return Some(Location {
            uri: doc_uri.clone(),
            range: span_to_range(&c.text, span),
        });
    }
    if (span.file.0 as usize) < c.map.file_count() {
        let sf = c.map.file(span.file);
        if let Ok(u) = Url::from_file_path(&sf.name) {
            return Some(Location {
                uri: u,
                range: span_to_range(&sf.src, span),
            });
        }
    }
    None
}

fn span_file_path(c: &Compiled, span: Span, doc_uri: &Url) -> Option<PathBuf> {
    if span.file == DOC_FILE {
        return doc_uri.to_file_path().ok().map(|p| normalize(&p));
    }
    if (span.file.0 as usize) < c.map.file_count() {
        return Some(normalize(Path::new(&c.map.file(span.file).name)));
    }
    None
}

fn span_source_key(c: &Compiled, span: Span, doc_uri: &Url) -> Option<SourceSpanKey> {
    Some(SourceSpanKey {
        path: span_file_path(c, span, doc_uri)?,
        lo: span.lo.to_usize(),
        hi: span.hi.to_usize(),
    })
}

fn resolution_def_key(c: &Compiled, res: ValueRes, doc_uri: &Url) -> Option<SourceSpanKey> {
    let span = c.definition_span(res)?;
    span_source_key(c, span, doc_uri)
}

fn resolution_def_name<'a>(c: &'a Compiled, res: ValueRes) -> Option<&'a str> {
    match res {
        ValueRes::Function(d)
        | ValueRes::Method(d)
        | ValueRes::Global(d)
        | ValueRes::StructCtor(d) => Some(c.analysis.program.def(d).name.as_str()),
        ValueRes::Local(_) | ValueRes::Builtin(_) => None,
    }
}

fn location_key(loc: &Location) -> (String, u32, u32, u32, u32) {
    (
        loc.uri.to_string(),
        loc.range.start.line,
        loc.range.start.character,
        loc.range.end.line,
        loc.range.end.character,
    )
}

fn dedup_locations(locs: &mut Vec<Location>) {
    let mut seen = HashSet::new();
    locs.retain(|loc| seen.insert(location_key(loc)));
    locs.sort_by_key(location_key);
}

fn target_is_workspace_wide(res: ValueRes) -> bool {
    matches!(
        res,
        ValueRes::Function(_) | ValueRes::Method(_) | ValueRes::Global(_) | ValueRes::StructCtor(_)
    )
}

fn def_key(c: &Compiled, def: DefId, doc_uri: &Url) -> Option<SourceSpanKey> {
    let span = c.def_name_span(def)?;
    span_source_key(c, span, doc_uri)
}

fn import_name_spans_for_target(
    c: &Compiled,
    doc_uri: &Url,
    target_key: &SourceSpanKey,
) -> Vec<Span> {
    let mut out = Vec::new();
    let root = c.analysis.program.module(ModId::ROOT);
    for item in &c.module.items {
        let ItemKind::Import(import) = &item.kind else {
            continue;
        };
        let ImportKind::Named(names) = &import.kind else {
            continue;
        };
        for name in names {
            let bound = name.alias.as_ref().unwrap_or(&name.name).name.as_str();
            let imported = root
                .imported_values
                .get(bound)
                .or_else(|| root.imported_types.get(bound));
            if imported.and_then(|def| def_key(c, *def, doc_uri)).as_ref() == Some(target_key) {
                out.push(name.name.span);
            }
        }
    }
    out
}

fn dedup_changes(changes: &mut HashMap<Url, Vec<TextEdit>>) {
    for edits in changes.values_mut() {
        let mut seen = HashSet::new();
        edits.retain(|edit| {
            seen.insert((
                edit.range.start.line,
                edit.range.start.character,
                edit.range.end.line,
                edit.range.end.character,
            ))
        });
        edits.sort_by_key(|edit| {
            (
                edit.range.start.line,
                edit.range.start.character,
                edit.range.end.line,
                edit.range.end.character,
            )
        });
    }
}

fn read_path_with_documents(documents: &DashMap<Url, String>, path: &Path) -> Option<String> {
    if let Ok(uri) = Url::from_file_path(path) {
        if let Some(text) = documents.get(&uri) {
            return Some(text.clone());
        }
    }
    std::fs::read_to_string(path).ok()
}

fn compile_text_at_path_with_documents(
    documents: &DashMap<Url, String>,
    text: String,
    path: &Path,
) -> Compiled {
    if let (Some(parent), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str())) {
        let read = |p: &Path| -> Option<String> { read_path_with_documents(documents, p) };
        return Compiled::new_multi(text, parent.to_path_buf(), stem.to_string(), &read);
    }
    Compiled::new(text)
}

fn compile_path_with_documents(documents: &DashMap<Url, String>, path: &Path) -> Option<Compiled> {
    let text = read_path_with_documents(documents, path)?;
    Some(compile_text_at_path_with_documents(documents, text, path))
}

#[derive(Clone)]
struct CachedCompiled {
    text: String,
    compiled: Arc<Compiled>,
}

fn compile_document_with_cache(
    documents: &DashMap<Url, String>,
    cache: &DashMap<Url, CachedCompiled>,
    uri: &Url,
) -> Option<Arc<Compiled>> {
    let text = documents.get(uri)?.clone();
    if let Some(cached) = cache.get(uri) {
        if cached.text == text {
            return Some(cached.compiled.clone());
        }
    }

    let compiled = match uri.to_file_path() {
        Ok(path) => compile_text_at_path_with_documents(documents, text.clone(), &path),
        Err(_) => Compiled::new(text.clone()),
    };
    let compiled = Arc::new(compiled);
    cache.insert(
        uri.clone(),
        CachedCompiled {
            text,
            compiled: compiled.clone(),
        },
    );
    Some(compiled)
}

fn apply_document_changes(
    mut text: String,
    changes: Vec<TextDocumentContentChangeEvent>,
) -> std::result::Result<String, String> {
    for change in changes {
        let Some(range) = change.range else {
            text = change.text;
            continue;
        };
        let start = offset_at(&text, range.start);
        let end = offset_at(&text, range.end);
        if start > end {
            return Err(format!(
                "invalid incremental edit range: start {:?} is after end {:?}",
                range.start, range.end
            ));
        }
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return Err(format!(
                "invalid incremental edit range: {:?} is not on UTF-8 character boundaries",
                range
            ));
        }
        text.replace_range(start..end, &change.text);
    }
    Ok(text)
}

fn workspace_source_root(file: &Path) -> PathBuf {
    let mut dir = file.parent();
    while let Some(d) = dir {
        let manifest = d.join("project.toml");
        if manifest.is_file() {
            let src = d.join("src");
            return if src.is_dir() { src } else { d.to_path_buf() };
        }
        dir = d.parent();
    }
    file.parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn workspace_candidate_paths(
    documents: &DashMap<Url, String>,
    current_path: &Path,
) -> Vec<PathBuf> {
    let root = workspace_source_root(current_path);
    let current = normalize(current_path);
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) == Some("otter") {
                let normalized = normalize(&path);
                if normalized != current {
                    out.push(normalized);
                }
                if out.len() >= MAX_WORKSPACE_SCAN_FILES {
                    return out;
                }
            }
        }
    }
    for entry in documents.iter() {
        let Ok(path) = entry.key().to_file_path() else {
            continue;
        };
        let normalized = normalize(&path);
        if normalized != current && !out.contains(&normalized) {
            out.push(normalized);
            if out.len() >= MAX_WORKSPACE_SCAN_FILES {
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn workspace_reference_locations(
    documents: &DashMap<Url, String>,
    current_uri: &Url,
    target_key: &SourceSpanKey,
) -> Vec<Location> {
    let Ok(current_path) = current_uri.to_file_path() else {
        return Vec::new();
    };
    let mut locs = Vec::new();
    for path in workspace_candidate_paths(documents, &current_path) {
        let Some(compiled) = compile_path_with_documents(documents, &path) else {
            continue;
        };
        let Ok(uri) = Url::from_file_path(&path) else {
            continue;
        };
        if !compiled.diagnostics.is_empty() {
            continue;
        }
        for (span, res) in &compiled.index.resolutions {
            if resolution_def_key(&compiled, *res, &uri).as_ref() == Some(target_key) {
                if let Some(loc) = span_to_location(&compiled, *span, &uri) {
                    locs.push(loc);
                }
            }
        }
        for span in import_name_spans_for_target(&compiled, &uri, target_key) {
            if let Some(loc) = span_to_location(&compiled, span, &uri) {
                locs.push(loc);
            }
        }
    }
    dedup_locations(&mut locs);
    locs
}

fn workspace_rename_edits(
    documents: &DashMap<Url, String>,
    current_uri: &Url,
    target_key: &SourceSpanKey,
    old_name: &str,
    new_name: &str,
) -> HashMap<Url, Vec<TextEdit>> {
    let Ok(current_path) = current_uri.to_file_path() else {
        return HashMap::new();
    };
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for path in workspace_candidate_paths(documents, &current_path) {
        let Some(compiled) = compile_path_with_documents(documents, &path) else {
            continue;
        };
        let Ok(uri) = Url::from_file_path(&path) else {
            continue;
        };
        if !compiled.diagnostics.is_empty() {
            continue;
        }
        for (span, res) in &compiled.index.resolutions {
            if resolution_def_key(&compiled, *res, &uri).as_ref() == Some(target_key)
                && compiled.map.slice(*span) == old_name
            {
                if let Some(loc) = span_to_location(&compiled, *span, &uri) {
                    changes.entry(loc.uri).or_default().push(TextEdit {
                        range: loc.range,
                        new_text: new_name.to_string(),
                    });
                }
            }
        }
        for span in import_name_spans_for_target(&compiled, &uri, target_key) {
            if let Some(loc) = span_to_location(&compiled, span, &uri) {
                changes.entry(loc.uri).or_default().push(TextEdit {
                    range: loc.range,
                    new_text: new_name.to_string(),
                });
            }
        }
    }
    dedup_changes(&mut changes);
    changes
}

/// The semantic-token legend, in the exact order of [`TokenClass`]'s numeric
/// values (the handler emits `class as u32` as the token-type index).
const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::TYPE,      // 0 Type
    SemanticTokenType::STRUCT,    // 1 Struct
    SemanticTokenType::INTERFACE, // 2 Interface
    SemanticTokenType::FUNCTION,  // 3 Function
    SemanticTokenType::METHOD,    // 4 Method
    SemanticTokenType::VARIABLE,  // 5 Variable
    SemanticTokenType::PARAMETER, // 6 Parameter
    SemanticTokenType::PROPERTY,  // 7 Property
    SemanticTokenType::KEYWORD,   // 8 Keyword
    SemanticTokenType::NUMBER,    // 9 Number
    SemanticTokenType::STRING,    // 10 String
    SemanticTokenType::COMMENT,   // 11 Comment
    SemanticTokenType::OPERATOR,  // 12 Operator
];

pub struct Backend {
    client: Client,
    /// Latest text of every open document, keyed by URI.
    documents: DashMap<Url, String>,
    /// Cached front-end result for open documents. Cleared on any document
    /// mutation because open submodules can affect another document's analysis.
    compiled: DashMap<Url, CachedCompiled>,
}

impl Backend {
    pub fn new(client: Client) -> Backend {
        Backend {
            client,
            documents: DashMap::new(),
            compiled: DashMap::new(),
        }
    }

    /// Compile a document's current text, if it is open. When the document has
    /// a filesystem path, its file-backed submodules are loaded too (preferring
    /// open editor buffers over disk) so cross-module imports resolve.
    fn compile(&self, uri: &Url) -> Option<Arc<Compiled>> {
        compile_document_with_cache(&self.documents, &self.compiled, uri)
    }

    fn workspace_reference_locations(
        &self,
        current_uri: &Url,
        target_key: &SourceSpanKey,
    ) -> Vec<Location> {
        workspace_reference_locations(&self.documents, current_uri, target_key)
    }

    fn workspace_rename_edits(
        &self,
        current_uri: &Url,
        target_key: &SourceSpanKey,
        old_name: &str,
        new_name: &str,
    ) -> HashMap<Url, Vec<TextEdit>> {
        workspace_rename_edits(&self.documents, current_uri, target_key, old_name, new_name)
    }

    /// Recompile and publish diagnostics for `uri`.
    async fn publish(&self, uri: Url, version: Option<i32>) {
        let Some(c) = self.compile(&uri) else { return };
        let mut diags: Vec<Diagnostic> = c
            .diagnostics
            .iter()
            .map(|(span, msg)| Diagnostic {
                range: span_to_range(&c.text, *span),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("otter-fusion".into()),
                message: msg.clone(),
                ..Default::default()
            })
            .collect();
        // Lint warnings (unused variables / unused private fns / unreachable
        // code) — only when the program is error-free, so the HIR is complete,
        // and only for spans in the open document.
        if c.diagnostics.is_empty() {
            for (span, msg) in compiler::lint::collect_lints(&c.analysis, &c.map) {
                if span.file == DOC_FILE {
                    diags.push(Diagnostic {
                        range: span_to_range(&c.text, span),
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("otter-fusion".into()),
                        message: msg,
                        ..Default::default()
                    });
                }
            }
        }
        self.client.publish_diagnostics(uri, diags, version).await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        let token_legend = SemanticTokensLegend {
            token_types: TOKEN_TYPES.to_vec(),
            token_modifiers: vec![],
        };
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "otter_fusion_lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into(), "@".into()]),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), ",".into()]),
                    retrigger_characters: Some(vec![",".into()]),
                    ..Default::default()
                }),
                document_highlight_provider: Some(OneOf::Left(true)),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: token_legend,
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "otter-fusion language server ready")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        self.documents.insert(doc.uri.clone(), doc.text);
        self.compiled.clear();
        self.publish(doc.uri, Some(doc.version)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let Some(current) = self.documents.get(&uri).map(|text| text.clone()) else {
            return;
        };
        let new_text = match apply_document_changes(current, params.content_changes) {
            Ok(text) => text,
            Err(msg) => {
                self.client.log_message(MessageType::WARNING, msg).await;
                return;
            }
        };
        self.documents.insert(uri.clone(), new_text);
        self.compiled.clear();
        self.publish(params.text_document.uri, Some(params.text_document.version))
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
        self.compiled.clear();
        // Clear diagnostics for the closed file.
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pos = params.text_document_position_params;
        let Some(c) = self.compile(&pos.text_document.uri) else {
            return Ok(None);
        };
        let off = offset_at(&c.text, pos.position);

        let mut lines: Vec<String> = Vec::new();
        let mut range: Option<Range> = None;

        if let Some((span, res)) = c.resolution_at(off) {
            range = Some(span_to_range(&c.text, span));
            match res {
                ValueRes::Local(id) => {
                    let name = c
                        .index
                        .local_decls
                        .get(&id)
                        .map(|s| c.map.slice(*s).to_string())
                        .unwrap_or_else(|| c.map.slice(span).to_string());
                    let ty = c
                        .index
                        .local_types
                        .get(&id)
                        .map(|t| c.display_ty(*t))
                        .unwrap_or_else(|| "?".into());
                    lines.push(format!("```otter-fusion\n{name}: {ty}\n```"));
                }
                ValueRes::Function(d)
                | ValueRes::Method(d)
                | ValueRes::Global(d)
                | ValueRes::StructCtor(d) => {
                    let def = c.analysis.program.def(d);
                    if let Some(sig) = stdlib_marker_signature(&c.analysis.program, d) {
                        let display_name = c.map.slice(span);
                        lines.push(format!(
                            "```otter-fusion\nfunction {}\n```",
                            sig.label(display_name)
                        ));
                        lines.push(format!("returns `{}`", sig.ret));
                    } else {
                        lines.push(format!("```otter-fusion\n{}\n```", c.def_label(def)));
                        if let Some(ret) = c.analysis.hir.fn_sigs.get(&d).map(|s| s.ret) {
                            lines.push(format!("returns `{}`", c.display_ty(ret)));
                        }
                    }
                }
                ValueRes::Builtin(b) => {
                    lines.push(format!("```otter-fusion\n{}\n```", builtin_signature(b)));
                    lines.push("builtin function".into());
                }
            }
        }

        // A type name written in a type annotation (no value resolution there):
        // show what kind of type it names (`struct Point`, `interface Show`, …).
        if lines.is_empty() {
            if let Some((tspan, def)) = c.type_def_at(off) {
                range = Some(span_to_range(&c.text, tspan));
                let d = c.analysis.program.def(def);
                lines.push(format!("```otter-fusion\n{}\n```", c.def_label(d)));
            }
        }

        // Append the expression's resolved type when available and distinct.
        if let Some((espan, ty)) = c.expr_ty_at(off) {
            if range.is_none() {
                range = Some(span_to_range(&c.text, espan));
            }
            let tystr = c.display_ty(ty);
            let already = lines.iter().any(|l| l.contains(&tystr));
            if !already {
                lines.push(format!("type: `{tystr}`"));
            }
        }

        if lines.is_empty() {
            return Ok(None);
        }
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: lines.join("\n\n"),
            }),
            range,
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params;
        let uri = pos.text_document.uri.clone();
        let Some(c) = self.compile(&uri) else {
            return Ok(None);
        };
        let off = offset_at(&c.text, pos.position);
        // A value-position name (function / local / global / ctor / method).
        let def_span = c
            .resolution_at(off)
            .and_then(|(_, res)| c.definition_span(res))
            // Else a type name written in a type annotation (type-position goto).
            .or_else(|| c.type_def_span_at(off));
        let Some(def_span) = def_span else {
            return Ok(None);
        };
        // The definition may live in another file (a loaded submodule); a virtual
        // file (prelude / synthesised code) has no editor location.
        let Some(loc) = span_to_location(&c, def_span, &uri) else {
            return Ok(None);
        };
        Ok(Some(GotoDefinitionResponse::Scalar(loc)))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position;
        let uri = pos.text_document.uri.clone();
        let Some(c) = self.compile(&uri) else {
            return Ok(None);
        };
        let off = offset_at(&c.text, pos.position);
        let Some((_, target)) = c.resolution_at(off) else {
            return Ok(None);
        };
        let target_key = target_is_workspace_wide(target)
            .then(|| resolution_def_key(&c, target, &uri))
            .flatten();

        let include_decl = params.context.include_declaration;
        // Use sites across *every* analyzed file (the open document plus its
        // loaded submodules), not just the open one — the HIR index spans them
        // all. A resolution target (`Function`/`Local`/… by def-id or unique
        // local-id) is file-independent, so the same `target` matches uses
        // wherever they occur.
        let mut spans: Vec<Span> = c
            .index
            .resolutions
            .iter()
            .filter(|(_, r)| *r == target)
            .map(|(s, _)| *s)
            .collect();

        // The declaration span of a local is itself in `resolutions` (bind()
        // records it), so locals are already covered. For def targets, add the
        // definition's name span if requested.
        if include_decl {
            if let Some(dspan) = c.definition_span(target) {
                if !spans.contains(&dspan) {
                    spans.push(dspan);
                }
            }
        } else {
            // Exclude the binding occurrence of a local.
            if let ValueRes::Local(id) = target {
                if let Some(dspan) = c.index.local_decls.get(&id) {
                    spans.retain(|s| s != dspan);
                }
            }
        }
        if let Some(target_key) = &target_key {
            spans.extend(import_name_spans_for_target(&c, &uri, target_key));
        }

        spans.sort_by_key(|s| (s.file.0, s.lo.0, s.hi.0));
        spans.dedup();
        // Map each use site to its own file's location (a virtual-file span — a
        // synthesized node — has no editor location and is dropped).
        let mut locs: Vec<Location> = spans
            .into_iter()
            .filter_map(|s| span_to_location(&c, s, &uri))
            .collect();
        if let Some(target_key) = &target_key {
            locs.extend(self.workspace_reference_locations(&uri, target_key));
        }
        dedup_locations(&mut locs);
        Ok(Some(locs))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let pos = params.text_document_position;
        let uri = pos.text_document.uri.clone();
        let Some(c) = self.compile(&uri) else {
            return Ok(None);
        };
        let off = offset_at(&c.text, pos.position);
        let Some((_, target)) = c.resolution_at(off) else {
            return Ok(None);
        };
        let target_key = target_is_workspace_wide(target)
            .then(|| resolution_def_key(&c, target, &uri))
            .flatten();
        let old_name = resolution_def_name(&c, target)
            .map(str::to_string)
            .unwrap_or_else(|| c.map.slice(c.resolution_at(off).unwrap().0).to_string());

        // Collect every use site plus the declaration, across all analyzed files.
        let mut spans: HashSet<Span> = c
            .index
            .resolutions
            .iter()
            .filter(|(_, r)| *r == target)
            .map(|(s, _)| *s)
            .collect();
        if let Some(dspan) = c.definition_span(target) {
            spans.insert(dspan);
        }
        if let Some(target_key) = &target_key {
            spans.extend(import_name_spans_for_target(&c, &uri, target_key));
        }
        if spans.is_empty() {
            return Ok(None);
        }

        // Group edits by the file (URI) each span belongs to, so a cross-module
        // rename updates every affected document in one `WorkspaceEdit`.
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for s in spans {
            if target_key.is_some() && c.map.slice(s) != old_name {
                continue;
            }
            if let Some(loc) = span_to_location(&c, s, &uri) {
                changes.entry(loc.uri).or_default().push(TextEdit {
                    range: loc.range,
                    new_text: params.new_name.clone(),
                });
            }
        }
        if let Some(target_key) = &target_key {
            let extra = self.workspace_rename_edits(&uri, target_key, &old_name, &params.new_name);
            for (uri, edits) in extra {
                changes.entry(uri).or_default().extend(edits);
            }
            dedup_changes(&mut changes);
        }
        if changes.is_empty() {
            return Ok(None);
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(c) = self.compile(&params.text_document.uri) else {
            return Ok(None);
        };
        let symbols = document_symbols(&c);
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        let Some(c) = self.compile(&uri) else {
            return Ok(None);
        };
        // Quick-fix: rename an unused local to `_name` (silences the lint without
        // removing code). Offered for any unused-variable binding overlapping the
        // requested range.
        let req = &params.range;
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();
        for (span, name) in compiler::lint::analyze(&c.analysis, &c.map).unused_locals {
            if span.file != DOC_FILE {
                continue;
            }
            let range = span_to_range(&c.text, span);
            // Overlap test against the requested range (line-level is enough).
            if range.end.line < req.start.line || range.start.line > req.end.line {
                continue;
            }
            // A zero-width insert of `_` at the binding's start.
            let at = Range {
                start: range.start,
                end: range.start,
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: at,
                    new_text: "_".into(),
                }],
            );
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Prefix `_` to silence unused `{name}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                ..Default::default()
            }));
        }
        Ok(Some(actions))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let Some(c) = self.compile(&params.text_document.uri) else {
            return Ok(None);
        };
        let formatted = compiler::fmt::format_source(&c.text);
        if formatted == c.text {
            return Ok(Some(Vec::new())); // already formatted — no edits
        }
        // Safety: a reformat may only change whitespace, never code. If the token
        // stream would differ, decline to format rather than risk corruption.
        if !compiler::fmt::token_stream_preserved(&c.text, &formatted) {
            return Ok(None);
        }
        // Replace the whole document in one edit.
        let end = position_at(&c.text, c.text.len());
        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(0, 0),
                end,
            },
            new_text: formatted,
        }]))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let Some(c) = self.compile(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(Some(collect_folding_ranges(&c.text)))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let Some(c) = self.compile(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(Some(collect_inlay_hints(&c, params.range)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let Some(c) = self.compile(&uri) else {
            return Ok(None);
        };
        let pos = params.text_document_position.position;
        let off = offset_at(&c.text, pos);

        // Dot completion: `recv.foo|` — restrict suggestions to members of
        // the receiver's type. The same path handles trigger-by-`.` and
        // re-trigger after typing letters.
        if let Some(ctx) = dot_completion_context(&c.text, off) {
            return Ok(Some(CompletionResponse::Array(member_completions(
                &c, &ctx,
            ))));
        }

        // Macro completion: just after `@` (with an optional partial name),
        // offer the defined procedural macros plus the built-ins (`docs/22`).
        if at_macro_context(&c.text, off) {
            return Ok(Some(CompletionResponse::Array(macro_completions(&c))));
        }

        Ok(Some(CompletionResponse::Array(default_completions(&c))))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let pos = params.text_document_position_params;
        let Some(c) = self.compile(&pos.text_document.uri) else {
            return Ok(None);
        };
        let off = offset_at(&c.text, pos.position);
        let Some((_, target)) = c.resolution_at(off) else {
            return Ok(None);
        };

        // Collect every occurrence of the same resolution in this document.
        let mut spans: Vec<Span> = c
            .index
            .resolutions
            .iter()
            .filter(|(s, r)| *r == target && s.file == DOC_FILE)
            .map(|(s, _)| *s)
            .collect();
        if let Some(dspan) = c.definition_span(target) {
            if !spans.contains(&dspan) {
                spans.push(dspan);
            }
        }
        spans.sort_by_key(|s| (s.lo.0, s.hi.0));
        spans.dedup();

        let highlights = spans
            .into_iter()
            .map(|s| DocumentHighlight {
                range: span_to_range(&c.text, s),
                kind: Some(DocumentHighlightKind::READ),
            })
            .collect();
        Ok(Some(highlights))
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri.clone();
        let Some(c) = self.compile(&uri) else {
            return Ok(None);
        };
        Ok(Some(collect_code_lenses(&c, uri.as_ref())))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let pos = params.text_document_position_params;
        let Some(c) = self.compile(&pos.text_document.uri) else {
            return Ok(None);
        };
        let off = offset_at(&c.text, pos.position);
        let Some((callee_off, active)) = find_active_call(&c.text, off) else {
            return Ok(None);
        };
        let Some((span, res)) = c.resolution_at(callee_off) else {
            return Ok(None);
        };
        let display_name = c.map.slice(span);
        let sig = match res {
            compiler::sema::ValueRes::Function(d)
            | compiler::sema::ValueRes::Method(d)
            | compiler::sema::ValueRes::StructCtor(d) => build_signature_info(&c, d, display_name),
            compiler::sema::ValueRes::Builtin(b) => Some(builtin_signature_info(b)),
            _ => None,
        };
        let Some(sig) = sig else { return Ok(None) };
        let active = active.min(
            sig.parameters
                .as_ref()
                .map_or(0, |p| p.len().saturating_sub(1)) as u32,
        );
        Ok(Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                active_parameter: Some(active),
                ..sig
            }],
            active_signature: Some(0),
            active_parameter: Some(active),
        }))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some(c) = self.compile(&params.text_document.uri) else {
            return Ok(None);
        };
        let index = LineIndex::new(&c.text);
        let mut data: Vec<SemanticToken> = Vec::new();
        let (mut prev_line, mut prev_start) = (0u32, 0u32);
        for (span, class) in c.semantic_tokens() {
            let start = index.position(&c.text, span.lo.to_usize());
            let end = index.position(&c.text, span.hi.to_usize());
            if start.line != end.line || end.character <= start.character {
                continue; // semantic tokens cannot span lines
            }
            let length = end.character - start.character;
            let delta_line = start.line - prev_line;
            let delta_start = if delta_line == 0 {
                start.character - prev_start
            } else {
                start.character
            };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type: class as u32,
                token_modifiers_bitset: 0,
            });
            prev_line = start.line;
            prev_start = start.character;
        }
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }
}

/// Build a flat-ish tree of `DocumentSymbol`s from the parsed module: top-level
/// items, with struct fields and `extend`/`interface` methods as children.
#[allow(deprecated)] // DocumentSymbol::deprecated is a required (deprecated) field
fn document_symbols(c: &Compiled) -> Vec<DocumentSymbol> {
    let text = &c.text;
    let mk = |name: String,
              detail: Option<String>,
              kind: SymbolKind,
              span: Span,
              sel: Span,
              children: Vec<DocumentSymbol>| {
        DocumentSymbol {
            name,
            detail,
            kind,
            tags: None,
            deprecated: None,
            range: span_to_range(text, span),
            selection_range: span_to_range(text, sel),
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        }
    };

    let mut out = Vec::new();
    for item in &c.module.items {
        match &item.kind {
            ItemKind::Function(f) => {
                let detail = f.is_async.then(|| "async".to_string());
                out.push(mk(
                    f.name.name.clone(),
                    detail,
                    SymbolKind::FUNCTION,
                    item.span,
                    f.name.span,
                    vec![],
                ));
            }
            ItemKind::Struct(s) => {
                let fields = match &s.kind {
                    StructKind::Record(fs) => fs
                        .iter()
                        .map(|fld| {
                            mk(
                                fld.name.name.clone(),
                                None,
                                SymbolKind::FIELD,
                                fld.span,
                                fld.name.span,
                                vec![],
                            )
                        })
                        .collect(),
                    _ => vec![],
                };
                out.push(mk(
                    s.name.name.clone(),
                    None,
                    SymbolKind::STRUCT,
                    item.span,
                    s.name.span,
                    fields,
                ));
            }
            ItemKind::Interface(i) => {
                let methods = i
                    .members
                    .iter()
                    .map(|m| {
                        mk(
                            m.function.name.name.clone(),
                            None,
                            SymbolKind::METHOD,
                            m.span,
                            m.function.name.span,
                            vec![],
                        )
                    })
                    .collect();
                out.push(mk(
                    i.name.name.clone(),
                    None,
                    SymbolKind::INTERFACE,
                    item.span,
                    i.name.span,
                    methods,
                ));
            }
            ItemKind::TypeAlias(a) => {
                out.push(mk(
                    a.name.name.clone(),
                    None,
                    SymbolKind::TYPE_PARAMETER,
                    item.span,
                    a.name.span,
                    vec![],
                ));
            }
            ItemKind::Extend(e) => {
                let target = c.map.slice(e.target.span).to_string();
                let methods = e
                    .members
                    .iter()
                    .map(|m| {
                        mk(
                            m.function.name.name.clone(),
                            None,
                            SymbolKind::METHOD,
                            m.span,
                            m.function.name.span,
                            vec![],
                        )
                    })
                    .collect();
                out.push(mk(
                    format!("extend {target}"),
                    None,
                    SymbolKind::NAMESPACE,
                    item.span,
                    e.target.span,
                    methods,
                ));
            }
            ItemKind::Var(v) => {
                out.push(mk(
                    v.name.name.clone(),
                    None,
                    SymbolKind::VARIABLE,
                    item.span,
                    v.name.span,
                    vec![],
                ));
            }
            ItemKind::Module(m) => {
                let kind = match &m.kind {
                    ModuleKind::External => SymbolKind::MODULE,
                    ModuleKind::Inline { .. } => SymbolKind::MODULE,
                };
                out.push(mk(
                    m.name.name.clone(),
                    None,
                    kind,
                    item.span,
                    m.name.span,
                    vec![],
                ));
            }
            ItemKind::Extern(ext) => {
                let (name, sel, kind) = match ext {
                    ExternItem::Function(f) => {
                        (f.name.name.clone(), f.name.span, SymbolKind::FUNCTION)
                    }
                    ExternItem::Struct(s) => (s.name.name.clone(), s.name.span, SymbolKind::STRUCT),
                    ExternItem::OpaqueType(n) => {
                        (n.name.clone(), n.span, SymbolKind::TYPE_PARAMETER)
                    }
                    ExternItem::Var { name, .. } => {
                        (name.name.clone(), name.span, SymbolKind::VARIABLE)
                    }
                };
                out.push(mk(name, None, kind, item.span, sel, vec![]));
            }
            ItemKind::Test(t) => {
                out.push(mk(
                    format!("test {:?}", t.name),
                    Some("test".to_string()),
                    SymbolKind::FUNCTION,
                    item.span,
                    t.name_span,
                    vec![],
                ));
            }
            ItemKind::Import(_) => {}
        }
    }
    out
}

/// The completion set when the cursor is *not* after a `.` — keywords,
/// builtins, declared top-level items, and locals visible anywhere in the
/// Whether `off` sits just after an `@` (with an optional partial macro name) —
/// the position where a procedural-macro invocation begins (`docs/22`).
fn at_macro_context(text: &str, off: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = off.min(bytes.len());
    while i > 0 {
        let c = bytes[i - 1];
        if c == b'_' || c.is_ascii_alphanumeric() {
            i -= 1;
        } else {
            break;
        }
    }
    i > 0 && bytes[i - 1] == b'@'
}

/// Completions for an `@`-prefixed macro position: the document's defined
/// procedural macros plus the built-in `@Derive` / `@ProcMacro` (`docs/22`).
fn macro_completions(c: &Compiled) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for name in c.proc_macro_names() {
        if seen.insert(name.clone()) {
            items.push(CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("procedural macro".into()),
                sort_text: Some("0".into()),
                ..Default::default()
            });
        }
    }
    for (name, detail) in [
        ("Derive", "built-in derive macro"),
        ("ProcMacro", "marks a procedural-macro definition"),
    ] {
        if seen.insert(name.to_string()) {
            items.push(CompletionItem {
                label: name.into(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some(detail.into()),
                sort_text: Some("1".into()),
                ..Default::default()
            });
        }
    }
    items
}

/// file (a single-file LSP cannot do precise lexical scoping yet).
fn default_completions(c: &Compiled) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push =
        |items: &mut Vec<CompletionItem>, seen: &mut HashSet<String>, item: CompletionItem| {
            if seen.insert(item.label.clone()) {
                items.push(item);
            }
        };

    // 1. Locals (highest priority — most contextually relevant).
    let mut locals: HashSet<String> = HashSet::new();
    for s in c.index.local_decls.values() {
        if s.file == DOC_FILE {
            locals.insert(c.map.slice(*s).to_string());
        }
    }
    for name in locals {
        push(
            &mut items,
            &mut seen,
            CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::VARIABLE),
                sort_text: Some("1".into()),
                ..Default::default()
            },
        );
    }

    // 2. Declared top-level types and functions — surface real signatures
    //    in `detail` so VS Code shows the parameter list inline.
    let (types, fns) = c.declared_names();
    for (name, class) in &types {
        let kind = match class {
            TokenClass::Interface => CompletionItemKind::INTERFACE,
            TokenClass::Type => CompletionItemKind::CLASS,
            _ => CompletionItemKind::STRUCT,
        };
        push(
            &mut items,
            &mut seen,
            CompletionItem {
                label: name.clone(),
                kind: Some(kind),
                sort_text: Some("2".into()),
                ..Default::default()
            },
        );
    }
    for name in &fns {
        let detail = c
            .analysis
            .program
            .resolve_value_in(compiler::ids::ModId::ROOT, name)
            .map(|d| c.def_signature(d));
        push(
            &mut items,
            &mut seen,
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail,
                sort_text: Some("2".into()),
                ..Default::default()
            },
        );
    }

    // 3. Compiler builtins. `std:io` print helpers are ordinary async stdlib
    // functions, so they only appear through imports or `std:io` namespaces.
    let builtins: &[(&str, &str)] = &[
        ("panic", "(str): never"),
        ("panic_with", "(value: dynamic): never"),
        ("exit", "(code: i32): never"),
        ("abort", "(): never"),
    ];
    for (name, sig) in builtins {
        push(
            &mut items,
            &mut seen,
            CompletionItem {
                label: (*name).into(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some((*sig).into()),
                sort_text: Some("3".into()),
                ..Default::default()
            },
        );
    }

    // 4. Keywords last — they're useful but rarely the intended completion in
    //    an expression context, so push them to the bottom of the list.
    for kw in keyword_texts() {
        push(
            &mut items,
            &mut seen,
            CompletionItem {
                label: (*kw).into(),
                kind: Some(CompletionItemKind::KEYWORD),
                sort_text: Some("4".into()),
                ..Default::default()
            },
        );
    }

    items
}

/// Build the completion list for a `recv.|` cursor: members of the receiver
/// type. Falls back to `default_completions` for an empty list when the
/// receiver type cannot be determined (e.g. the receiver expression has not
/// type-checked yet because of an upstream error).
fn member_completions(c: &Compiled, ctx: &crate::analysis::DotContext) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push =
        |items: &mut Vec<CompletionItem>, seen: &mut HashSet<String>, item: CompletionItem| {
            if seen.insert(item.label.clone()) {
                items.push(item);
            }
        };

    // Value-receiver case: `expr.|` — use the inferred type of the receiver.
    if let Some(ty) = c.receiver_type_at_dot(ctx.dot_offset) {
        push_instance_members(c, ty, &mut items, &mut seen, &mut push);
    }

    // Type-receiver case: `TypeName.|` — list static methods and
    // type-namespaced items. This is independent of `expr_types` since a bare
    // type name is not a value expression.
    if let Some((s, e)) = ctx.receiver_ident {
        let name = &c.text[s..e];
        push_type_namespace_members(c, name, &mut items, &mut seen, &mut push);
        push_imported_namespace_members(c, name, &mut items, &mut seen, &mut push);
    }

    items
}

fn push_instance_members(
    c: &Compiled,
    ty: compiler::ty::Ty,
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    push: &mut impl FnMut(&mut Vec<CompletionItem>, &mut HashSet<String>, CompletionItem),
) {
    let prog = &c.analysis.program;
    let tcx = &c.analysis.tcx;
    let kind = tcx.kind(ty).clone();

    // Built-in intrinsics keyed by structural kind.
    match &kind {
        TyKind::Str => {
            for (name, sig) in str_intrinsic_methods() {
                push(
                    items,
                    seen,
                    intrinsic_completion(name, sig, CompletionItemKind::METHOD),
                );
            }
        }
        TyKind::Int(_) => {
            for (name, sig) in int_instance_methods() {
                push(
                    items,
                    seen,
                    intrinsic_completion(name, sig, CompletionItemKind::METHOD),
                );
            }
        }
        TyKind::Float(_) => {
            for (name, sig) in float_instance_methods() {
                push(
                    items,
                    seen,
                    intrinsic_completion(name, sig, CompletionItemKind::METHOD),
                );
            }
        }
        TyKind::Bool | TyKind::Char | TyKind::Null => {
            push(
                items,
                seen,
                intrinsic_completion("clone", "(): Self", CompletionItemKind::METHOD),
            );
        }
        _ => {}
    }

    // Named types: struct fields + interface methods + `extend` methods.
    if let TyKind::Named { def, args } = kind {
        let d = prog.def(def);
        match d.kind {
            DefKind::Struct | DefKind::ExternStruct => {
                for f in c.struct_fields(def) {
                    let detail = c
                        .struct_field_type_detail(def, &args, &f.name)
                        .map(|ty| format!(": {ty}"));
                    push(
                        items,
                        seen,
                        CompletionItem {
                            label: f.name.clone(),
                            kind: Some(CompletionItemKind::FIELD),
                            detail,
                            sort_text: Some("1".into()),
                            ..Default::default()
                        },
                    );
                }
            }
            DefKind::Interface => {
                for m in c.interface_methods(def, false) {
                    push(
                        items,
                        seen,
                        def_to_completion(c, m, CompletionItemKind::METHOD),
                    );
                }
            }
            _ => {}
        }

        // Special-case the std collections — their methods are intrinsic, not
        // in any `extend`. The `def` comparisons against `prog.*_def` are
        // exact so generic instances (`List<i64>`, etc.) all hit.
        if def == prog.list_def {
            for (name, sig) in list_intrinsic_methods() {
                push(
                    items,
                    seen,
                    intrinsic_completion(name, sig, CompletionItemKind::METHOD),
                );
            }
        } else if def == prog.map_def {
            for (name, sig) in map_intrinsic_methods() {
                push(
                    items,
                    seen,
                    intrinsic_completion(name, sig, CompletionItemKind::METHOD),
                );
            }
        }
        push_runtime_handle_intrinsic_methods(c, def, &args, items, seen, push);

        // Any user `extend` block whose target's head name matches this type's
        // name contributes instance methods.
        let type_name = d.name.clone();
        for m in c.extend_methods_for(&type_name, false) {
            push(
                items,
                seen,
                def_to_completion(c, m, CompletionItemKind::METHOD),
            );
        }
    }

    // Aliases lower away before they reach HIR (`type Level = A | B | ...` is
    // just the union), but users attach methods to the source-level alias with
    // `extend Level`. Recover those alias-target methods by comparing the
    // expanded alias type structurally against the receiver type.
    push_alias_extend_methods_for_receiver(c, ty, items, seen, push);
}

fn push_runtime_handle_intrinsic_methods(
    c: &Compiled,
    def: DefId,
    args: &[Ty],
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    push: &mut impl FnMut(&mut Vec<CompletionItem>, &mut HashSet<String>, CompletionItem),
) {
    let prog = &c.analysis.program;
    let first_arg = |fallback: &str| {
        args.first()
            .copied()
            .map(|ty| c.display_ty(ty))
            .unwrap_or_else(|| fallback.to_string())
    };

    if def == prog.join_handle_def {
        let output = first_arg("R");
        for (name, sig) in [
            ("join", format!("(): Future<Joined<{output}> | Panicked>")),
            ("detach", "(): null".to_string()),
        ] {
            push(
                items,
                seen,
                intrinsic_completion(name, &sig, CompletionItemKind::METHOD),
            );
        }
    } else if def == prog.task_join_handle_def {
        let output = first_arg("R");
        for (name, sig) in [
            (
                "join",
                format!("(): Future<Joined<{output}> | Panicked | Cancelled>"),
            ),
            ("detach", "(): null".to_string()),
            ("cancel", "(): null".to_string()),
            ("abort", "(): null".to_string()),
        ] {
            push(
                items,
                seen,
                intrinsic_completion(name, &sig, CompletionItemKind::METHOD),
            );
        }
    } else if def == prog.sender_def {
        let elem = first_arg("T");
        push(
            items,
            seen,
            intrinsic_completion(
                "send",
                &format!("(value: {elem}): null | ChannelClosed"),
                CompletionItemKind::METHOD,
            ),
        );
    } else if def == prog.receiver_def {
        let elem = first_arg("T");
        for (name, sig) in [
            ("recv", format!("(): Future<{elem} | ChannelClosed>")),
            ("try_recv", format!("(): {elem} | null")),
        ] {
            push(
                items,
                seen,
                intrinsic_completion(name, &sig, CompletionItemKind::METHOD),
            );
        }
    }
}

fn push_alias_extend_methods_for_receiver(
    c: &Compiled,
    receiver_ty: Ty,
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    push: &mut impl FnMut(&mut Vec<CompletionItem>, &mut HashSet<String>, CompletionItem),
) {
    let prog = &c.analysis.program;
    for (idx, extend_def) in prog.defs.iter().enumerate() {
        if extend_def.kind != DefKind::Extend {
            continue;
        }
        let Some(ItemKind::Extend(e)) = &extend_def.item else {
            continue;
        };
        let Some(alias_name) = top_named_name(&e.target) else {
            continue;
        };
        let Some(alias_def) = prog.resolve_type_in(extend_def.module, alias_name) else {
            continue;
        };
        if prog.def(alias_def).kind != DefKind::TypeAlias
            || !alias_expands_to_receiver(c, alias_def, receiver_ty)
        {
            continue;
        }

        let extend_id = DefId(idx as u32);
        for method in prog.defs.iter().filter(|d| {
            d.kind == DefKind::ExtendMethod && !d.is_static && d.parent == Some(extend_id)
        }) {
            push(
                items,
                seen,
                def_to_completion(c, method, CompletionItemKind::METHOD),
            );
        }
    }
}

fn alias_expands_to_receiver(c: &Compiled, alias_def: DefId, receiver_ty: Ty) -> bool {
    let prog = &c.analysis.program;
    let def = prog.def(alias_def);
    if !def.generics.is_empty() {
        return false;
    }
    let Some(ItemKind::TypeAlias(alias)) = &def.item else {
        return false;
    };

    let mut alias_tcx = TyCtxt::new();
    let mut errors = Vec::new();
    let alias_ty = {
        let mut lowerer = Lowerer::new(prog, &mut alias_tcx, &mut errors);
        lowerer.lower(&alias.aliased, &TypeEnv::new(def.module))
    };
    errors.is_empty()
        && !alias_tcx.is_error(alias_ty)
        && ty_structurally_eq(&alias_tcx, alias_ty, &c.analysis.tcx, receiver_ty)
}

fn ty_structurally_eq(left_tcx: &TyCtxt, left: Ty, right_tcx: &TyCtxt, right: Ty) -> bool {
    match (left_tcx.kind(left), right_tcx.kind(right)) {
        (TyKind::Bool, TyKind::Bool)
        | (TyKind::Char, TyKind::Char)
        | (TyKind::Str, TyKind::Str)
        | (TyKind::Null, TyKind::Null)
        | (TyKind::Dynamic, TyKind::Dynamic)
        | (TyKind::Never, TyKind::Never)
        | (TyKind::SelfTy, TyKind::SelfTy)
        | (TyKind::Error, TyKind::Error) => true,
        (TyKind::Int(a), TyKind::Int(b)) => a == b,
        (TyKind::Float(a), TyKind::Float(b)) => a == b,
        (TyKind::Named { def: a, args: aa }, TyKind::Named { def: b, args: ba }) => {
            a == b
                && aa.len() == ba.len()
                && aa
                    .iter()
                    .zip(ba)
                    .all(|(a, b)| ty_structurally_eq(left_tcx, *a, right_tcx, *b))
        }
        (TyKind::Tuple(a), TyKind::Tuple(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| ty_structurally_eq(left_tcx, *a, right_tcx, *b))
        }
        (
            TyKind::Func {
                params: ap,
                ret: ar,
                is_extern: ae,
            },
            TyKind::Func {
                params: bp,
                ret: br,
                is_extern: be,
            },
        ) => {
            ae == be
                && ap.len() == bp.len()
                && ap
                    .iter()
                    .zip(bp)
                    .all(|(a, b)| ty_structurally_eq(left_tcx, *a, right_tcx, *b))
                && ty_structurally_eq(left_tcx, *ar, right_tcx, *br)
        }
        (TyKind::Ptr(a), TyKind::Ptr(b)) => ty_structurally_eq(left_tcx, *a, right_tcx, *b),
        (TyKind::Array { elem: ae, len: al }, TyKind::Array { elem: be, len: bl }) => {
            al == bl && ty_structurally_eq(left_tcx, *ae, right_tcx, *be)
        }
        (TyKind::Union(a), TyKind::Union(b)) => {
            if a.len() != b.len() {
                return false;
            }
            let mut used = vec![false; b.len()];
            'outer: for a_member in a {
                for (idx, b_member) in b.iter().enumerate() {
                    if !used[idx] && ty_structurally_eq(left_tcx, *a_member, right_tcx, *b_member) {
                        used[idx] = true;
                        continue 'outer;
                    }
                }
                return false;
            }
            true
        }
        (TyKind::Param(a), TyKind::Param(b)) => a == b,
        (TyKind::Infer(a), TyKind::Infer(b)) => a == b,
        _ => false,
    }
}

fn push_type_namespace_members(
    c: &Compiled,
    name: &str,
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    push: &mut impl FnMut(&mut Vec<CompletionItem>, &mut HashSet<String>, CompletionItem),
) {
    // Primitive integer/float namespaces — `i32.MAX`, `f64.NAN`, etc.
    const INT_NAMES: &[&str] = &[
        "i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize",
    ];
    const FLOAT_NAMES: &[&str] = &["f32", "f64"];

    if INT_NAMES.contains(&name) {
        for (n, sig) in primitive_static_methods() {
            push(
                items,
                seen,
                intrinsic_completion(n, sig, CompletionItemKind::FUNCTION),
            );
        }
        return;
    }
    if FLOAT_NAMES.contains(&name) {
        for (n, sig) in float_static_methods() {
            push(
                items,
                seen,
                intrinsic_completion(n, sig, CompletionItemKind::CONSTANT),
            );
        }
        return;
    }

    // User type namespace: static methods on `extend Type`.
    if let Some(_def) = c.lookup_type_def(name) {
        for m in c.extend_methods_for(name, true) {
            push(
                items,
                seen,
                def_to_completion(c, m, CompletionItemKind::FUNCTION),
            );
        }
    }
}

fn push_imported_namespace_members(
    c: &Compiled,
    alias: &str,
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    push: &mut impl FnMut(&mut Vec<CompletionItem>, &mut HashSet<String>, CompletionItem),
) {
    let prog = &c.analysis.program;
    let Some(module) = prog.namespace_target(compiler::ids::ModId::ROOT, alias) else {
        return;
    };
    let module_info = prog.module(module);

    let mut type_names: Vec<_> = module_info.types.keys().cloned().collect();
    type_names.extend(module_info.public_imported_types.keys().cloned());
    type_names.sort();
    type_names.dedup();
    for name in type_names {
        let Some(def_id) = prog.resolve_pub_type_in(module, &name) else {
            continue;
        };
        let def = prog.def(def_id);
        let kind = match def.kind {
            DefKind::Interface => CompletionItemKind::INTERFACE,
            DefKind::TypeAlias | DefKind::ExternType => CompletionItemKind::CLASS,
            _ => CompletionItemKind::STRUCT,
        };
        push(items, seen, def_id_to_completion(c, def_id, kind));
    }

    let mut value_names: Vec<_> = module_info.values.keys().cloned().collect();
    value_names.extend(module_info.public_imported_values.keys().cloned());
    value_names.sort();
    value_names.dedup();
    for name in value_names {
        let Some(def_id) = prog.resolve_pub_value_in(module, &name) else {
            continue;
        };
        let def = prog.def(def_id);
        let kind = match def.kind {
            DefKind::Function | DefKind::ExternFunction => CompletionItemKind::FUNCTION,
            _ => CompletionItemKind::VARIABLE,
        };
        let mut item = def_id_to_completion(c, def_id, kind);
        if let Some(sig) = stdlib_marker_signature(prog, def_id) {
            item.detail = Some(sig.detail());
        }
        push(items, seen, item);
    }
}

fn intrinsic_completion(name: &str, signature: &str, kind: CompletionItemKind) -> CompletionItem {
    CompletionItem {
        label: name.into(),
        kind: Some(kind),
        detail: Some(signature.into()),
        sort_text: Some("1".into()),
        ..Default::default()
    }
}

fn def_to_completion(
    c: &Compiled,
    def: &compiler::sema::symbols::Def,
    kind: CompletionItemKind,
) -> CompletionItem {
    CompletionItem {
        label: def.name.clone(),
        kind: Some(kind),
        detail: Some(c.def_signature_from_def(def)),
        sort_text: Some("1".into()),
        ..Default::default()
    }
}

fn def_id_to_completion(c: &Compiled, def_id: DefId, kind: CompletionItemKind) -> CompletionItem {
    let def = c.analysis.program.def(def_id);
    CompletionItem {
        label: def.name.clone(),
        kind: Some(kind),
        detail: Some(c.def_signature(def_id)),
        sort_text: Some("1".into()),
        ..Default::default()
    }
}

/// Build the Run/Build CodeLenses for a compiled document. Emits lenses above
/// every top-level `function main`; async `main` is driven by the runtime's
/// private root-future driver, so the same CLI invocation works without a
/// user-visible `block_on` API.
fn collect_code_lenses(c: &Compiled, uri: &str) -> Vec<CodeLens> {
    let mut lenses = Vec::new();
    for item in &c.module.items {
        let ItemKind::Function(f) = &item.kind else {
            continue;
        };
        if f.name.name != "main" {
            continue;
        }
        let range = span_to_range(&c.text, f.name.span);
        let uri = uri.to_string();
        lenses.push(CodeLens {
            range,
            command: Some(Command {
                title: "▶ Run".into(),
                command: "otter-fusion.runFile".into(),
                arguments: Some(vec![
                    serde_json::Value::String(uri.clone()),
                    serde_json::Value::Bool(false),
                ]),
            }),
            data: None,
        });
        lenses.push(CodeLens {
            range,
            command: Some(Command {
                title: "▶ Run (release)".into(),
                command: "otter-fusion.runFile".into(),
                arguments: Some(vec![
                    serde_json::Value::String(uri.clone()),
                    serde_json::Value::Bool(true),
                ]),
            }),
            data: None,
        });
        lenses.push(CodeLens {
            range,
            command: Some(Command {
                title: "🔨 Build".into(),
                command: "otter-fusion.buildFile".into(),
                arguments: Some(vec![serde_json::Value::String(uri)]),
            }),
            data: None,
        });
    }
    lenses
}

fn collect_inlay_hints(c: &Compiled, range: Range) -> Vec<InlayHint> {
    let unannotated = collect_unannotated_var_bindings(&c.module);
    let mut hints = Vec::new();

    for (local, span) in &c.index.local_decls {
        if span.file != DOC_FILE || !unannotated.contains(&span_key(*span)) {
            continue;
        }
        let Some(ty) = c.index.local_types.get(local).copied() else {
            continue;
        };
        let position = span_to_range(&c.text, *span).end;
        if !position_in_range(position, range) {
            continue;
        }
        hints.push(InlayHint {
            position,
            label: InlayHintLabel::String(format!(": {}", c.display_ty(ty))),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        });
    }

    hints.sort_by_key(|h| (h.position.line, h.position.character));
    hints
}

fn position_in_range(position: Position, range: Range) -> bool {
    range.start <= position && position <= range.end
}

fn collect_unannotated_var_bindings(module: &ast::Module) -> HashSet<(u32, usize, usize)> {
    let mut out = HashSet::new();
    for item in &module.items {
        collect_unannotated_var_bindings_in_item(item, &mut out);
    }
    out
}

fn collect_unannotated_var_bindings_in_item(
    item: &ast::Item,
    out: &mut HashSet<(u32, usize, usize)>,
) {
    match &item.kind {
        ItemKind::Function(function) => collect_unannotated_var_bindings_in_function(function, out),
        ItemKind::Module(module) => {
            if let ModuleKind::Inline { items, .. } = &module.kind {
                for item in items {
                    collect_unannotated_var_bindings_in_item(item, out);
                }
            }
        }
        ItemKind::Interface(interface) => {
            for member in &interface.members {
                if let Some(body) = &member.default_body {
                    collect_unannotated_var_bindings_in_block(body, out);
                }
            }
        }
        ItemKind::Extend(extend) => {
            for member in &extend.members {
                collect_unannotated_var_bindings_in_function(&member.function, out);
            }
        }
        ItemKind::Extern(ExternItem::Function(function)) => {
            collect_unannotated_var_bindings_in_function(function, out)
        }
        ItemKind::Test(test) => collect_unannotated_var_bindings_in_block(&test.body, out),
        ItemKind::Var(_)
        | ItemKind::Struct(_)
        | ItemKind::TypeAlias(_)
        | ItemKind::Extern(_)
        | ItemKind::Import(_) => {}
    }
}

fn collect_unannotated_var_bindings_in_function(
    function: &ast::FunctionItem,
    out: &mut HashSet<(u32, usize, usize)>,
) {
    if let Some(body) = &function.body {
        collect_unannotated_var_bindings_in_block(body, out);
    }
}

fn collect_unannotated_var_bindings_in_block(
    block: &ast::Block,
    out: &mut HashSet<(u32, usize, usize)>,
) {
    for stmt in &block.stmts {
        collect_unannotated_var_bindings_in_stmt(stmt, out);
    }
    if let Some(trailing) = &block.trailing {
        collect_unannotated_var_bindings_in_expr(trailing, out);
    }
}

fn collect_unannotated_var_bindings_in_stmt(
    stmt: &ast::Stmt,
    out: &mut HashSet<(u32, usize, usize)>,
) {
    match &stmt.kind {
        AstStmtKind::Var(local) => collect_unannotated_var_bindings_in_local(local, out),
        AstStmtKind::Assign { target, value } => {
            collect_unannotated_var_bindings_in_expr(target, out);
            collect_unannotated_var_bindings_in_expr(value, out);
        }
        AstStmtKind::Expr(expr) => collect_unannotated_var_bindings_in_expr(expr, out),
        AstStmtKind::Item(item) => collect_unannotated_var_bindings_in_item(item, out),
    }
}

fn collect_unannotated_var_bindings_in_local(
    local: &ast::LocalVar,
    out: &mut HashSet<(u32, usize, usize)>,
) {
    if local.ty.is_none() {
        if let AstPatternKind::Binding(name) = &local.pattern.kind {
            out.insert(span_key(name.span));
        }
    }
    collect_unannotated_var_bindings_in_expr(&local.init, out);
}

fn collect_unannotated_var_bindings_in_expr(
    expr: &ast::Expr,
    out: &mut HashSet<(u32, usize, usize)>,
) {
    match &expr.kind {
        AstExprKind::Tuple(exprs) | AstExprKind::List(exprs) => {
            for expr in exprs {
                collect_unannotated_var_bindings_in_expr(expr, out);
            }
        }
        AstExprKind::Paren(expr)
        | AstExprKind::Try { expr, .. }
        | AstExprKind::Ref { expr, .. }
        | AstExprKind::Deref { expr, .. }
        | AstExprKind::Await { expr, .. }
        | AstExprKind::Spawn { expr, .. } => collect_unannotated_var_bindings_in_expr(expr, out),
        AstExprKind::MapLit(entries) => {
            for entry in entries {
                match entry {
                    ast::MapItem::Entry { key, value, .. } => {
                        collect_unannotated_var_bindings_in_expr(key, out);
                        collect_unannotated_var_bindings_in_expr(value, out);
                    }
                    ast::MapItem::Spread(expr) => {
                        collect_unannotated_var_bindings_in_expr(expr, out)
                    }
                }
            }
        }
        AstExprKind::StructLit { fields, spread, .. } => {
            for field in fields {
                if let Some(value) = &field.value {
                    collect_unannotated_var_bindings_in_expr(value, out);
                }
            }
            if let Some(spread) = spread {
                collect_unannotated_var_bindings_in_expr(spread, out);
            }
        }
        AstExprKind::Unary { operand, .. } => {
            collect_unannotated_var_bindings_in_expr(operand, out)
        }
        AstExprKind::Binary { left, right, .. } => {
            collect_unannotated_var_bindings_in_expr(left, out);
            collect_unannotated_var_bindings_in_expr(right, out);
        }
        AstExprKind::Cast { expr, .. } => collect_unannotated_var_bindings_in_expr(expr, out),
        AstExprKind::Field { receiver, .. } | AstExprKind::TupleIndex { receiver, .. } => {
            collect_unannotated_var_bindings_in_expr(receiver, out)
        }
        AstExprKind::Call {
            callee,
            args,
            trailing_closure,
            ..
        } => {
            collect_unannotated_var_bindings_in_expr(callee, out);
            for arg in args {
                collect_unannotated_var_bindings_in_expr(arg, out);
            }
            if let Some(trailing_closure) = trailing_closure {
                collect_unannotated_var_bindings_in_expr(trailing_closure, out);
            }
        }
        AstExprKind::Index { receiver, index } => {
            collect_unannotated_var_bindings_in_expr(receiver, out);
            collect_unannotated_var_bindings_in_expr(index, out);
        }
        AstExprKind::If {
            cond,
            then_block,
            else_branch,
        } => {
            collect_unannotated_var_bindings_in_expr(cond, out);
            collect_unannotated_var_bindings_in_block(then_block, out);
            if let Some(else_branch) = else_branch {
                match else_branch {
                    ast::ElseBranch::If(expr) => {
                        collect_unannotated_var_bindings_in_expr(expr, out)
                    }
                    ast::ElseBranch::Block(block) => {
                        collect_unannotated_var_bindings_in_block(block, out)
                    }
                }
            }
        }
        AstExprKind::Match { scrutinee, arms } => {
            collect_unannotated_var_bindings_in_expr(scrutinee, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_unannotated_var_bindings_in_expr(guard, out);
                }
                collect_unannotated_var_bindings_in_expr(&arm.body, out);
            }
        }
        AstExprKind::Block(block) | AstExprKind::Loop(block) | AstExprKind::AsyncBlock(block) => {
            collect_unannotated_var_bindings_in_block(block, out)
        }
        AstExprKind::While { cond, body } => {
            collect_unannotated_var_bindings_in_expr(cond, out);
            collect_unannotated_var_bindings_in_block(body, out);
        }
        AstExprKind::For { iter, body, .. } => {
            collect_unannotated_var_bindings_in_expr(iter, out);
            collect_unannotated_var_bindings_in_block(body, out);
        }
        AstExprKind::Return(expr) | AstExprKind::Break(expr) => {
            if let Some(expr) = expr {
                collect_unannotated_var_bindings_in_expr(expr, out);
            }
        }
        AstExprKind::Closure { body, .. } => collect_unannotated_var_bindings_in_expr(body, out),
        AstExprKind::AnonFn(function) => {
            collect_unannotated_var_bindings_in_function(function, out)
        }
        AstExprKind::MacroCall { args, block, .. } => {
            for arg in args {
                match arg {
                    ast::AttrArg::Positional(expr) => {
                        collect_unannotated_var_bindings_in_expr(expr, out)
                    }
                    ast::AttrArg::Named { value, .. } => {
                        collect_unannotated_var_bindings_in_expr(value, out)
                    }
                }
            }
            if let Some(block) = block {
                collect_unannotated_var_bindings_in_block(block, out);
            }
        }
        AstExprKind::Int(_)
        | AstExprKind::Float(_)
        | AstExprKind::Bool(_)
        | AstExprKind::Null
        | AstExprKind::Char(_)
        | AstExprKind::Str(_)
        | AstExprKind::Ident(_)
        | AstExprKind::SelfExpr
        | AstExprKind::Underscore
        | AstExprKind::Continue => {}
    }
}

fn span_key(span: Span) -> (u32, usize, usize) {
    (span.file.0, span.lo.to_usize(), span.hi.to_usize())
}

fn collect_folding_ranges(text: &str) -> Vec<FoldingRange> {
    #[derive(Clone, Copy)]
    struct Open {
        line: u32,
        character: u32,
    }

    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut braces: Vec<Open> = Vec::new();
    let mut block_comments: Vec<Open> = Vec::new();
    let mut line = 0u32;
    let mut character = 0u32;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(start) = block_comments.last().copied() {
            if bytes[i] == b'\n' {
                line += 1;
                character = 0;
                i += 1;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                block_comments.push(Open { line, character });
                i += 2;
                character += 2;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                block_comments.pop();
                if block_comments.is_empty() && line > start.line {
                    ranges.push(FoldingRange {
                        start_line: start.line,
                        start_character: Some(start.character),
                        end_line: line,
                        end_character: Some(character + 2),
                        kind: Some(FoldingRangeKind::Comment),
                        collapsed_text: None,
                    });
                }
                i += 2;
                character += 2;
                continue;
            }
            i += 1;
            character += 1;
            continue;
        }

        match bytes[i] {
            b'\n' => {
                line += 1;
                character = 0;
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                    character += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                block_comments.push(Open { line, character });
                i += 2;
                character += 2;
            }
            b'"' => {
                i += 1;
                character += 1;
                while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\n' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        character += 2;
                    } else {
                        i += 1;
                        character += 1;
                    }
                }
                if i < bytes.len() && bytes[i] == b'"' {
                    i += 1;
                    character += 1;
                }
            }
            b'\'' => {
                i += 1;
                character += 1;
                while i < bytes.len() && bytes[i] != b'\'' && bytes[i] != b'\n' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        character += 2;
                    } else {
                        i += 1;
                        character += 1;
                    }
                }
                if i < bytes.len() && bytes[i] == b'\'' {
                    i += 1;
                    character += 1;
                }
            }
            b'{' => {
                braces.push(Open { line, character });
                i += 1;
                character += 1;
            }
            b'}' => {
                if let Some(start) = braces.pop() {
                    if line > start.line {
                        ranges.push(FoldingRange {
                            start_line: start.line,
                            start_character: Some(start.character),
                            end_line: line,
                            end_character: Some(character + 1),
                            kind: Some(FoldingRangeKind::Region),
                            collapsed_text: None,
                        });
                    }
                }
                i += 1;
                character += 1;
            }
            _ => {
                i += 1;
                character += 1;
            }
        }
    }

    ranges.sort_by(|a, b| {
        (
            a.start_line,
            a.start_character.unwrap_or(0),
            a.end_line,
            a.end_character.unwrap_or(0),
        )
            .cmp(&(
                b.start_line,
                b.start_character.unwrap_or(0),
                b.end_line,
                b.end_character.unwrap_or(0),
            ))
    });
    ranges
}

/// Locate the open `(` of the call enclosing `off`, returning the byte offset
/// inside the callee name and the active parameter (0-based comma count). Skips
/// nested braces/brackets/parens and stops at semicolons or unmatched closers.
pub(crate) fn find_active_call(text: &str, off: usize) -> Option<(usize, u32)> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut active: u32 = 0;
    let mut i = off.min(bytes.len());
    let paren_off;
    loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        match bytes[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' => {
                if depth == 0 {
                    paren_off = i;
                    break;
                }
                depth -= 1;
            }
            b'[' | b'{' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            b',' if depth == 0 => active += 1,
            b';' if depth == 0 => return None,
            _ => {}
        }
    }
    // Walk back over whitespace to find the end of the callee name.
    let mut end = paren_off;
    while end > 0 && matches!(bytes[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    // The callee must end in an identifier character (skip generic `>` etc.).
    if !bytes[end - 1].is_ascii_alphanumeric() && bytes[end - 1] != b'_' {
        return None;
    }
    Some((end - 1, active))
}

fn build_signature_info(
    c: &Compiled,
    def_id: DefId,
    display_name: &str,
) -> Option<SignatureInformation> {
    use compiler::ast::ParamKind;
    if let Some(sig) = stdlib_marker_signature(&c.analysis.program, def_id) {
        return Some(signature_info_from_display(display_name, sig));
    }
    let def = c.analysis.program.def(def_id);
    let ItemKind::Function(f) = def.item.as_ref()? else {
        return None;
    };
    let mut label = String::new();
    label.push_str(&def.name);
    label.push('(');
    let mut params: Vec<ParameterInformation> = Vec::new();
    let mut first = true;
    for p in &f.params {
        let (name, ty) = match &p.kind {
            ParamKind::SelfParam => continue,
            ParamKind::Normal { name, ty } => (name, ty),
        };
        if !first {
            label.push_str(", ");
        }
        first = false;
        let start = label.encode_utf16().count() as u32;
        label.push_str(&name.name);
        label.push_str(": ");
        let ty_text = if ty.span.file == DOC_FILE {
            c.map.slice(ty.span).to_string()
        } else {
            // Synthesised types (prelude / derive) don't have document source.
            "_".to_string()
        };
        label.push_str(&ty_text);
        let end = label.encode_utf16().count() as u32;
        params.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([start, end]),
            documentation: None,
        });
    }
    label.push(')');
    if let Some(rt) = &f.return_type {
        label.push_str(": ");
        if rt.span.file == DOC_FILE {
            label.push_str(c.map.slice(rt.span));
        } else {
            label.push('_');
        }
    }
    Some(SignatureInformation {
        label,
        documentation: None,
        parameters: Some(params),
        active_parameter: None,
    })
}

fn signature_info_from_display(
    name: &str,
    sig: crate::analysis::FunctionSignatureDisplay,
) -> SignatureInformation {
    let label = sig.label(name);
    let mut params = Vec::new();
    for (name, ty) in sig.params {
        let needle = format!("{name}: {ty}");
        if let Some(byte_off) = label.find(&needle) {
            let start = label[..byte_off].encode_utf16().count() as u32;
            let end = start + needle.encode_utf16().count() as u32;
            params.push(ParameterInformation {
                label: ParameterLabel::LabelOffsets([start, end]),
                documentation: None,
            });
        }
    }
    SignatureInformation {
        label,
        documentation: None,
        parameters: Some(params),
        active_parameter: None,
    }
}

fn builtin_signature_info(b: compiler::sema::Builtin) -> SignatureInformation {
    use compiler::sema::Builtin;
    let (label, parts): (&str, &[(&str, &str)]) = match b {
        Builtin::Panic => ("panic(message: str): never", &[("message", "str")]),
        Builtin::PanicWith => ("panic_with(value: dynamic): never", &[("value", "dynamic")]),
        Builtin::Exit => ("exit(code: i32): never", &[("code", "i32")]),
        Builtin::Abort => ("abort(): never", &[]),
    };
    let mut params = Vec::new();
    for (name, ty) in parts {
        // Compute UTF-16 offsets of `name: ty` inside `label`.
        let needle = format!("{name}: {ty}");
        if let Some(byte_off) = label.find(&needle) {
            let start = label[..byte_off].encode_utf16().count() as u32;
            let end = start + needle.encode_utf16().count() as u32;
            params.push(ParameterInformation {
                label: ParameterLabel::LabelOffsets([start, end]),
                documentation: None,
            });
        }
    }
    SignatureInformation {
        label: label.into(),
        documentation: None,
        parameters: Some(params),
        active_parameter: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
struct Point { x: i64, y: i64 }
interface Show { function show(self): str }
extend Point: Show {
  function show(self): str { \"p\" }
}
function main() {}
";

    fn names(syms: &[DocumentSymbol]) -> Vec<&str> {
        syms.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn document_symbols_cover_all_top_level_items() {
        let c = Compiled::new(SRC.into());
        let syms = document_symbols(&c);
        let ns = names(&syms);
        assert!(ns.contains(&"Point"));
        assert!(ns.contains(&"Show"));
        assert!(ns.contains(&"main"));
        assert!(ns.iter().any(|n| n.starts_with("extend Point")));
    }

    #[test]
    fn struct_fields_and_methods_are_children() {
        let c = Compiled::new(SRC.into());
        let syms = document_symbols(&c);
        let point = syms.iter().find(|s| s.name == "Point").unwrap();
        let fields: Vec<&str> = point
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(fields, vec!["x", "y"]);

        let ext = syms.iter().find(|s| s.name.starts_with("extend")).unwrap();
        let methods: Vec<&str> = ext
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(methods, vec!["show"]);
    }

    #[test]
    fn selection_range_is_inside_full_range() {
        let c = Compiled::new(SRC.into());
        let syms = document_symbols(&c);
        for s in &syms {
            assert!(s.range.start <= s.selection_range.start);
            assert!(s.selection_range.end <= s.range.end);
        }
    }

    #[test]
    fn incremental_document_changes_apply_in_order_with_utf16_positions() {
        let text = "function main() {\n  var icon = \"🦦\";\n}\n".to_string();
        let otter_start = text.find("🦦").unwrap();
        let first = TextDocumentContentChangeEvent {
            range: Some(Range {
                start: position_at(&text, otter_start),
                end: position_at(&text, otter_start + "🦦".len()),
            }),
            range_length: None,
            text: "otter".into(),
        };
        let after_first = "function main() {\n  var icon = \"otter\";\n}\n";
        let insert_at = after_first.find("otter").unwrap() + "otter".len();
        let second = TextDocumentContentChangeEvent {
            range: Some(Range {
                start: position_at(after_first, insert_at),
                end: position_at(after_first, insert_at),
            }),
            range_length: None,
            text: " fusion".into(),
        };

        let applied = apply_document_changes(text, vec![first, second]).unwrap();
        assert_eq!(
            applied,
            "function main() {\n  var icon = \"otter fusion\";\n}\n"
        );
    }

    #[test]
    fn full_document_change_replaces_text() {
        let applied = apply_document_changes(
            "old".into(),
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "function main() {}\n".into(),
            }],
        )
        .unwrap();
        assert_eq!(applied, "function main() {}\n");
    }

    #[test]
    fn compiled_document_cache_reuses_until_text_changes() {
        let docs = DashMap::new();
        let cache = DashMap::new();
        let uri = Url::parse("file:///tmp/otter_fusion_lsp_cache_reuse.otter").unwrap();
        docs.insert(uri.clone(), "function main() {}\n".to_string());

        let first = compile_document_with_cache(&docs, &cache, &uri).unwrap();
        let second = compile_document_with_cache(&docs, &cache, &uri).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        docs.insert(
            uri.clone(),
            "function main() { var changed = 1; }\n".to_string(),
        );
        let third = compile_document_with_cache(&docs, &cache, &uri).unwrap();
        assert!(!Arc::ptr_eq(&first, &third));
        assert!(third.diagnostics.is_empty(), "{:?}", third.diagnostics);
    }

    #[test]
    fn builtin_signature_help_keeps_stdio_print_helpers_out_of_builtins() {
        use compiler::sema::Builtin;

        assert!(Builtin::from_name("print").is_none());
        assert!(Builtin::from_name("println").is_none());
        assert!(Builtin::from_name("eprint").is_none());
        assert!(Builtin::from_name("eprintln").is_none());

        let info = builtin_signature_info(Builtin::Panic);
        assert_eq!(info.label, "panic(message: str): never");
    }

    #[test]
    fn folding_ranges_cover_multiline_brace_blocks() {
        let src = "\
function main() {
  if true {
    println(\"x\");
  }
}
";
        let ranges = collect_folding_ranges(src);
        let coords = ranges
            .iter()
            .map(|r| (r.start_line, r.end_line, r.kind.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            coords,
            vec![
                (0, 4, Some(FoldingRangeKind::Region)),
                (1, 3, Some(FoldingRangeKind::Region)),
            ]
        );
    }

    #[test]
    fn folding_ranges_include_multiline_block_comments() {
        let src = "\
function main() {
  /*
   * docs
   */
  println(\"done\");
}
";
        let ranges = collect_folding_ranges(src);
        assert!(
            ranges.iter().any(|r| {
                r.start_line == 1 && r.end_line == 3 && r.kind == Some(FoldingRangeKind::Comment)
            }),
            "ranges: {ranges:?}"
        );
        assert!(
            ranges.iter().any(|r| {
                r.start_line == 0 && r.end_line == 5 && r.kind == Some(FoldingRangeKind::Region)
            }),
            "ranges: {ranges:?}"
        );
    }

    #[test]
    fn folding_ranges_ignore_braces_in_strings_and_comments() {
        let src = "\
function main() {
  var s = \"not a fold { }\";
  // not a fold {
  /* also not a code fold { */
}
";
        let ranges = collect_folding_ranges(src);
        let code_ranges = ranges
            .iter()
            .filter(|r| r.kind == Some(FoldingRangeKind::Region))
            .map(|r| (r.start_line, r.end_line))
            .collect::<Vec<_>>();
        assert_eq!(code_ranges, vec![(0, 4)]);
    }

    fn hint_label(hint: &InlayHint) -> &str {
        match &hint.label {
            InlayHintLabel::String(label) => label,
            InlayHintLabel::LabelParts(_) => panic!("expected plain string label"),
        }
    }

    fn pos_after(text: &str, needle: &str) -> Position {
        let start = text.find(needle).expect("needle in source");
        position_at(text, start + needle.len())
    }

    #[test]
    fn inlay_hints_show_inferred_local_types() {
        let src = "\
function main() {
  var answer = 41 + 1;
  var text = \"otter\";
  println(text);
}
";
        let c = Compiled::new(src.into());
        let hints = collect_inlay_hints(
            &c,
            Range {
                start: Position::new(0, 0),
                end: position_at(&c.text, c.text.len()),
            },
        );
        let labels = hints.iter().map(hint_label).collect::<Vec<_>>();
        assert_eq!(labels, vec![": i64", ": str"]);
        assert_eq!(hints[0].position, pos_after(&c.text, "answer"));
        assert_eq!(hints[0].kind, Some(InlayHintKind::TYPE));
        assert_eq!(hints[1].position, pos_after(&c.text, "text"));
    }

    #[test]
    fn inlay_hints_skip_annotated_locals_and_parameters() {
        let src = "\
function main(arg: i64) {
  var typed: i64 = arg;
  var inferred = arg;
  println(\"done\");
}
";
        let c = Compiled::new(src.into());
        let hints = collect_inlay_hints(
            &c,
            Range {
                start: Position::new(0, 0),
                end: position_at(&c.text, c.text.len()),
            },
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hint_label(&hints[0]), ": i64");
        assert_eq!(hints[0].position, pos_after(&c.text, "inferred"));
    }

    #[test]
    fn inlay_hints_cover_nested_var_bindings() {
        let src = "\
function main() {
  var outer = {
    var inner = 7;
    inner
  };
  println(\"done\");
}
";
        let c = Compiled::new(src.into());
        let hints = collect_inlay_hints(
            &c,
            Range {
                start: Position::new(0, 0),
                end: position_at(&c.text, c.text.len()),
            },
        );
        let positions = hints.iter().map(|h| h.position).collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![pos_after(&c.text, "outer"), pos_after(&c.text, "inner")]
        );
        assert_eq!(
            hints.iter().map(hint_label).collect::<Vec<_>>(),
            vec![": i64", ": i64"]
        );
    }

    #[test]
    fn inlay_hints_respect_requested_range() {
        let src = "\
function main() {
  var first = 1;
  var second = 2;
}
";
        let c = Compiled::new(src.into());
        let hints = collect_inlay_hints(
            &c,
            Range {
                start: Position::new(2, 0),
                end: Position::new(2, u32::MAX),
            },
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hint_label(&hints[0]), ": i64");
        assert_eq!(hints[0].position, pos_after(&c.text, "second"));
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    fn completion_detail<'a>(items: &'a [CompletionItem], label: &str) -> &'a str {
        items
            .iter()
            .find(|item| item.label == label)
            .and_then(|item| item.detail.as_deref())
            .unwrap_or_else(|| panic!("{label} completion detail missing in {items:?}"))
    }

    fn completion_items_at(src: &str, marker: &str) -> Vec<CompletionItem> {
        let c = Compiled::new(src.into());
        let dot = src
            .find(marker)
            .unwrap_or_else(|| panic!("{marker} missing"))
            + marker.trim_end_matches(".;").len();
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        member_completions(&c, &ctx)
    }

    fn assert_completion_details(src: &str, marker: &str, expected: &[(&str, &str)]) {
        let items = completion_items_at(src, marker);
        for (label, detail_part) in expected {
            let detail = completion_detail(&items, label);
            assert!(
                detail.contains(detail_part),
                "{label} completion at {marker} must contain `{detail_part}`: {detail:?}"
            );
        }
    }

    #[test]
    fn completion_catalog_keeps_wait_capable_public_surfaces_future_returning() {
        assert_completion_details(
            "\
import \"std:io\" as Io;
function main() { Io.; }
",
            "Io.;",
            &[
                ("print", "Future<null>"),
                ("println", "Future<null>"),
                ("eprint", "Future<null>"),
                ("eprintln", "Future<null>"),
            ],
        );
        assert_completion_details(
            "\
import { stdin, stdout } from \"std:io\";
function main() {
  var input = stdin();
  input.;
  var output = stdout();
  output.;
}
",
            "input.;",
            &[("read", "Future<i64 | IoError>")],
        );
        assert_completion_details(
            "\
import { stdin, stdout } from \"std:io\";
function main() {
  var input = stdin();
  input.;
  var output = stdout();
  output.;
}
",
            "output.;",
            &[("write_all", "Future<null | IoError>")],
        );

        assert_completion_details(
            "\
import \"std:fs\" as Fs;
function main() { Fs.; }
",
            "Fs.;",
            &[
                ("read_to_string", "Future<str | IoError>"),
                ("write", "Future<null | IoError>"),
                ("read_dir", "Future<DirEntries | IoError>"),
            ],
        );
        assert_completion_details(
            "\
import { File } from \"std:fs\";
function main() { File.; }
",
            "File.;",
            &[("open", "Future<File | IoError>")],
        );
        assert_completion_details(
            "\
import { Path } from \"std:fs\";
function main() { Path.new(\".\").; }
",
            "Path.new(\".\").;",
            &[("metadata", "Future<Metadata | IoError>")],
        );

        assert_completion_details(
            "\
import \"std:net\" as Net;
function main() { Net.; }
",
            "Net.;",
            &[("resolve", "Future<List<IpAddr> | IoError>")],
        );
        assert_completion_details(
            "\
import { TcpStream } from \"std:net\";
function main() { TcpStream.; }
",
            "TcpStream.;",
            &[("connect", "Future<TcpStream | IoError>")],
        );
        assert_completion_details(
            "\
import { TcpStream } from \"std:net\";
function inspect(stream: TcpStream) { stream.; }
",
            "stream.;",
            &[
                ("read", "Future<i64 | IoError>"),
                ("peer_addr", "Future<SocketAddr | IoError>"),
            ],
        );
        assert_completion_details(
            "\
import { TcpListener } from \"std:net\";
function inspect(listener: TcpListener) { listener.; }
",
            "listener.;",
            &[("accept", "Future<(TcpStream, SocketAddr) | IoError>")],
        );
        assert_completion_details(
            "\
import { UdpSocket } from \"std:net\";
function inspect(socket: UdpSocket) { socket.; }
",
            "socket.;",
            &[("recv_from", "Future<(i64, SocketAddr) | IoError>")],
        );

        assert_completion_details(
            "\
import \"std:process\" as Process;
function main() { Process.; }
",
            "Process.;",
            &[("args", "Future<List<str>>"), ("env", "Future<str | null>")],
        );
        assert_completion_details(
            "\
import { Command } from \"std:process\";
function inspect(cmd: Command) { cmd.; }
",
            "cmd.;",
            &[
                ("status", "Future<ExitStatus | IoError>"),
                ("spawn", "Future<Child | IoError>"),
            ],
        );
        assert_completion_details(
            "\
import { Child } from \"std:process\";
function inspect(child: Child) { child.; }
",
            "child.;",
            &[
                ("wait", "Future<ExitStatus | IoError>"),
                ("kill", "Future<null | IoError>"),
            ],
        );

        assert_completion_details(
            "\
import \"std:time\" as Time;
import \"std:async\" as Async;
function main() { Time.; Async.; }
",
            "Time.;",
            &[("sleep", "Future<null>")],
        );
        assert_completion_details(
            "\
import \"std:time\" as Time;
import \"std:async\" as Async;
function main() { Time.; Async.; }
",
            "Async.;",
            &[
                ("yield_now", "Future<null>"),
                ("sleep", "Future<null>"),
                ("timeout", "Future<T | TimedOut>"),
            ],
        );
        assert_completion_details(
            "\
import \"std:rand\" as Rand;
function main() { Rand.; }
",
            "Rand.;",
            &[
                ("os_bytes", "Future<Bytes | RandomError>"),
                ("thread_rng", "Future<ThreadRng | RandomError>"),
            ],
        );
        assert_completion_details(
            "\
import \"std:hash\" as Hashing;
function main() { Hashing.; }
",
            "Hashing.;",
            &[("os_keyed_hasher", "Future<KeyedHasher | HashSeedError>")],
        );
        assert_completion_details(
            "\
import \"std:log\" as Log;
function main() { Log.; }
",
            "Log.;",
            &[
                ("record", "Future<Record>"),
                ("log_record", "Future<null>"),
                ("info", "Future<null>"),
                ("info_with", "Future<null>"),
                ("warn", "Future<null>"),
                ("error", "Future<null>"),
            ],
        );

        assert_completion_details(
            "\
import { Receiver } from \"std:sync\";
function inspect(rx: Receiver<i64>) { rx.; }
",
            "rx.;",
            &[("recv", "Future<i64 | ChannelClosed>")],
        );
        assert_completion_details(
            "\
import { JoinHandle } from \"std:thread\";
function inspect(handle: JoinHandle<i64>) { handle.; }
",
            "handle.;",
            &[("join", "Future<Joined<i64> | Panicked>")],
        );
    }

    fn unique_temp_project(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "otter_fusion_lsp_{name}_{}_{}",
            std::process::id(),
            nanos
        ))
    }

    fn write_project(files: &[(&str, &str)]) -> PathBuf {
        let root = unique_temp_project("reverse_refs");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("project.toml"),
            "[package]\nname = \"lsp_reverse_refs\"\nentry = \"src/main.otter\"\n",
        )
        .unwrap();
        for (rel, text) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, text).unwrap();
        }
        root
    }

    fn location_text(loc: &Location) -> String {
        let path = loc.uri.to_file_path().unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let start = offset_at(&text, loc.range.start);
        let end = offset_at(&text, loc.range.end);
        text[start..end].to_string()
    }

    fn location_file_name(loc: &Location) -> String {
        loc.uri
            .to_file_path()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn workspace_references_include_files_that_import_current_document() {
        let root = write_project(&[
            (
                "src/util.otter",
                "pub function answer(): i64 { 42 }\n\
                 function local_use(): i64 { answer() }\n",
            ),
            (
                "src/main.otter",
                "mod util;\n\
                 import { answer } from \"self:util\";\n\
                 function main() { var x = answer(); }\n",
            ),
            (
                "src/alias_user.otter",
                "mod util;\n\
                 import { answer as ans } from \"self:util\";\n\
                 function main() { var x = ans(); }\n",
            ),
        ]);
        let docs = DashMap::new();
        let util_path = root.join("src/util.otter");
        let util_uri = Url::from_file_path(&util_path).unwrap();
        let c = compile_path_with_documents(&docs, &util_path).unwrap();
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
        let off = c.text.rfind("answer()").unwrap();
        let (_, target) = c.resolution_at(off).expect("target resolution");
        let target_key = resolution_def_key(&c, target, &util_uri).expect("target key");

        let locs = workspace_reference_locations(&docs, &util_uri, &target_key);
        let got: Vec<(String, String)> = locs
            .iter()
            .map(|loc| (location_file_name(loc), location_text(loc)))
            .collect();
        assert!(
            got.contains(&("main.otter".into(), "answer".into())),
            "main import/call missing: {got:?}"
        );
        assert!(
            got.contains(&("alias_user.otter".into(), "answer".into())),
            "aliased import specifier missing: {got:?}"
        );
        assert!(
            got.contains(&("alias_user.otter".into(), "ans".into())),
            "aliased use missing: {got:?}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn workspace_rename_updates_importers_without_rewriting_alias_uses() {
        let root = write_project(&[
            (
                "src/util.otter",
                "pub function answer(): i64 { 42 }\n\
                 function local_use(): i64 { answer() }\n",
            ),
            (
                "src/main.otter",
                "mod util;\n\
                 import { answer } from \"self:util\";\n\
                 function main() { var x = answer(); }\n",
            ),
            (
                "src/alias_user.otter",
                "mod util;\n\
                 import { answer as ans } from \"self:util\";\n\
                 function main() { var x = ans(); }\n",
            ),
        ]);
        let docs = DashMap::new();
        let util_path = root.join("src/util.otter");
        let util_uri = Url::from_file_path(&util_path).unwrap();
        let c = compile_path_with_documents(&docs, &util_path).unwrap();
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
        let off = c.text.rfind("answer()").unwrap();
        let (_, target) = c.resolution_at(off).expect("target resolution");
        let target_key = resolution_def_key(&c, target, &util_uri).expect("target key");

        let changes = workspace_rename_edits(&docs, &util_uri, &target_key, "answer", "reply");
        let edits_for = |file: &str| -> Vec<String> {
            changes
                .iter()
                .filter(|(uri, _)| {
                    uri.to_file_path()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        == file
                })
                .flat_map(|(uri, edits)| {
                    let text = std::fs::read_to_string(uri.to_file_path().unwrap()).unwrap();
                    edits.iter().map(move |edit| {
                        let start = offset_at(&text, edit.range.start);
                        let end = offset_at(&text, edit.range.end);
                        text[start..end].to_string()
                    })
                })
                .collect()
        };
        let main_edits = edits_for("main.otter");
        assert_eq!(main_edits, vec!["answer", "answer"]);
        let alias_edits = edits_for("alias_user.otter");
        assert_eq!(
            alias_edits,
            vec!["answer"],
            "the import's source name changes, but the local alias `ans` remains stable"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn member_completion_lists_struct_fields_and_methods() {
        let src = "\
struct Point { x: i64, y: i64 }
extend Point {
  function magnitude(self): i64 { self.x }
}
function main() {
  var p = Point { x: 1, y: 2 };
  var n = p.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("p.;").unwrap() + 1;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        assert!(names.contains(&"x"), "field x missing in {names:?}");
        assert!(names.contains(&"y"), "field y missing in {names:?}");
        assert!(
            names.contains(&"magnitude"),
            "method magnitude missing in {names:?}"
        );
        // Crucially: keywords/builtins are NOT in the member set.
        assert!(!names.contains(&"if"));
        assert!(!names.contains(&"println"));
    }

    #[test]
    fn member_completion_lists_list_intrinsic_methods() {
        let src = "\
function main() {
  var xs = [1, 2, 3];
  var z = xs.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("xs.;").unwrap() + 2;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["push", "size", "is_empty", "get", "map", "filter", "fold"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_std_collections_deque_reverse_methods() {
        let src = "\
import { Deque, deque } from \"std:collections\";
function main() {
  var q = deque<i64>();
  q.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("q.;").unwrap() + 1;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["reverse", "reversed", "push_front", "push_back"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_core_collections_set_helpers() {
        let src = "\
import { Set } from \"core:collections\";
function main() {
  var s = Set<i64>();
  s.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("s.;").unwrap() + 1;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "insert",
            "insert_all",
            "is_subset",
            "is_superset",
            "is_disjoint",
            "to_list",
            "union",
            "intersect",
            "difference",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_std_bytes_insert_method() {
        let src = "\
import { Bytes } from \"std:bytes\";
function main() {
  var b = Bytes.new();
  b.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("b.;").unwrap() + 1;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "insert",
            "index_of",
            "last_index_of",
            "index_of_bytes",
            "last_index_of_bytes",
            "contains",
            "contains_bytes",
            "push",
            "remove_at",
            "resize",
            "fill",
            "truncate",
            "cursor",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_std_json_helpers() {
        let src = "\
import { Json, json_array, json_bool } from \"std:json\";
import { List } from \"core:collections\";
function main() {
  var items = List<Json>();
  items.push(json_bool(true));
  var value = json_array(items);
  value.;
}
";
        let c = Compiled::new(src.into());
        let dot = c.text.find("value.;").unwrap() + 5;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "is_null",
            "is_bool",
            "is_number",
            "is_str",
            "is_array",
            "is_object",
            "len",
            "contains_key",
            "keys",
            "values",
            "as_array",
            "as_object",
            "get",
            "at",
            "append",
            "set_at",
            "with_key",
            "without_key",
            "deep_eq",
            "deep_clone",
            "render",
            "pretty",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_std_http_value_helpers() {
        let src = "\
import { Map } from \"core:collections\";
import { Bytes } from \"std:bytes\";
import { url } from \"std:net/types\";
import { Headers, Status, headers, http_11, http_request, http_response, method_get } from \"std:http\";
function main() {
  var h = Headers.new();
  h.;
  var q = Map.new<str, str>();
  var u = url(\"https\", null, \"example.com\", null, \"/\", q, null);
  var req = http_request(method_get(), u, h, Bytes.new(), http_11());
  req.;
  var resp = http_response(Status.ok(), headers(), Bytes.new(), http_11());
  resp.;
}
";
        let c = Compiled::new(src.into());

        let headers_dot = c.text.find("h.;").unwrap() + 1;
        let headers_ctx =
            crate::analysis::dot_completion_context(&c.text, headers_dot + 1).unwrap();
        let header_items = member_completions(&c, &headers_ctx);
        let header_names = labels(&header_items);
        for must in [
            "get",
            "get_all",
            "entries",
            "names",
            "set",
            "append",
            "remove",
            "contains",
            "size",
            "is_empty",
            "value_count",
            "clear",
        ] {
            assert!(
                header_names.contains(&must),
                "{must} missing in {header_names:?}"
            );
        }

        let req_dot = c.text.find("req.;").unwrap() + 3;
        let req_ctx = crate::analysis::dot_completion_context(&c.text, req_dot + 1).unwrap();
        let req_items = member_completions(&c, &req_ctx);
        let req_names = labels(&req_items);
        for must in [
            "method",
            "url",
            "headers",
            "body",
            "version",
            "with_method",
            "with_url",
            "with_headers",
            "with_body",
            "with_version",
            "with_header",
            "with_set_header",
            "without_header",
            "clear_headers",
        ] {
            assert!(req_names.contains(&must), "{must} missing in {req_names:?}");
        }

        let resp_dot = c.text.find("resp.;").unwrap() + 4;
        let resp_ctx = crate::analysis::dot_completion_context(&c.text, resp_dot + 1).unwrap();
        let resp_items = member_completions(&c, &resp_ctx);
        let resp_names = labels(&resp_items);
        for must in [
            "status",
            "headers",
            "body",
            "version",
            "with_status",
            "with_headers",
            "with_body",
            "with_version",
            "with_header",
            "with_set_header",
            "without_header",
            "clear_headers",
        ] {
            assert!(
                resp_names.contains(&must),
                "{must} missing in {resp_names:?}"
            );
        }
    }

    #[test]
    fn member_completion_lists_std_log_value_helpers() {
        let src = "\
import { Future } from \"core:prelude\";
import { empty_fields, level_info, record } from \"std:log\";
function main(): Future<null> async {
  var level = level_info();
  level.;
  var r = await record(level_info(), \"msg\", \"mod\", empty_fields());
  r.;
  null
}
";
        let c = Compiled::new(src.into());

        let level_dot = c.text.find("level.;").unwrap() + 5;
        let level_ctx = crate::analysis::dot_completion_context(&c.text, level_dot + 1).unwrap();
        let level_items = member_completions(&c, &level_ctx);
        let level_names = labels(&level_items);
        for must in ["rank", "is_at_least", "equals"] {
            assert!(
                level_names.contains(&must),
                "{must} missing in {level_names:?}"
            );
        }

        let record_dot = c.text.find("r.;").unwrap() + 1;
        let record_ctx = crate::analysis::dot_completion_context(&c.text, record_dot + 1).unwrap();
        let record_items = member_completions(&c, &record_ctx);
        let record_names = labels(&record_items);
        for must in [
            "level",
            "message",
            "module",
            "fields",
            "field",
            "has_field",
            "field_count",
            "time",
            "with_level",
            "with_message",
            "with_module",
            "with_fields",
            "with_field",
            "without_field",
            "clear_fields",
            "with_time",
        ] {
            assert!(
                record_names.contains(&must),
                "{must} missing in {record_names:?}"
            );
        }
    }

    #[test]
    fn member_completion_marks_std_fs_file_io_methods_as_futures() {
        let src = "\
import { File, Path } from \"std:fs\";
import { Future } from \"core:prelude\";
function main(): Future<null> async {
  var opened = await File.create(Path.new(\"/tmp/otter_fusion_lsp_file_async_methods.bin\"));
  if opened is File {
    var file = opened as File;
    file.;
  }
  null
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("file.;").unwrap() + 4;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "read_to_string",
            "write_string",
            "append_string",
            "close",
            "read",
            "read_to_end",
            "write",
            "write_all",
            "flush",
            "seek",
            "read_async",
            "read_to_end_async",
            "write_async",
            "write_all_async",
            "flush_async",
            "seek_async",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, ret) in [
            ("read_to_string", "Future<str | IoError>"),
            ("write_string", "Future<null | IoError>"),
            ("append_string", "Future<null | IoError>"),
            ("close", "Future<null | IoError>"),
            ("read", "Future<i64 | IoError>"),
            ("read_to_end", "Future<i64 | IoError>"),
            ("write", "Future<i64 | IoError>"),
            ("write_all", "Future<null | IoError>"),
            ("flush", "Future<null | IoError>"),
            ("seek", "Future<i64 | IoError>"),
            ("read_async", "Future<i64 | IoError>"),
            ("read_to_end_async", "Future<i64 | IoError>"),
            ("write_async", "Future<i64 | IoError>"),
            ("write_all_async", "Future<null | IoError>"),
            ("seek_async", "Future<i64 | IoError>"),
            ("flush_async", "Future<null | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(ret),
                "File.{name} must advertise its async Future return: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn member_completion_marks_concrete_stdio_methods_as_futures() {
        let src = "\
import { stdin, stdout, stderr } from \"std:io\";
function main() {
  var input = stdin();
  input.;
  var output = stdout();
  output.;
  var err = stderr();
  err.;
}
";
        let c = Compiled::new(src.into());
        let input_dot = src.find("input.;").unwrap() + 5;
        let input_ctx = crate::analysis::dot_completion_context(&c.text, input_dot + 1).unwrap();
        let input_items = member_completions(&c, &input_ctx);
        let input_names = labels(&input_items);
        for must in ["read", "read_to_end", "read_async", "read_to_end_async"] {
            assert!(
                input_names.contains(&must),
                "{must} missing in {input_names:?}"
            );
        }
        assert!(
            completion_detail(&input_items, "read").contains("Future<i64 | IoError>"),
            "Stdin.read must show its async Future return: {:?}",
            completion_detail(&input_items, "read")
        );
        assert!(
            completion_detail(&input_items, "read_to_end").contains("Future<i64 | IoError>"),
            "Stdin.read_to_end must show its async Future return: {:?}",
            completion_detail(&input_items, "read")
        );
        assert!(
            completion_detail(&input_items, "read_async").contains("Future<i64 | IoError>"),
            "Stdin.read_async must advertise its async Future return: {:?}",
            completion_detail(&input_items, "read_async")
        );
        assert!(
            completion_detail(&input_items, "read_to_end_async").contains("Future<i64 | IoError>"),
            "Stdin.read_to_end_async must advertise its async Future return: {:?}",
            completion_detail(&input_items, "read_to_end_async")
        );

        for (marker, label) in [("output.;", "Stdout"), ("err.;", "Stderr")] {
            let dot = src.find(marker).unwrap() + marker.trim_end_matches(".;").len();
            let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
            let items = member_completions(&c, &ctx);
            let names = labels(&items);
            for must in [
                "write",
                "write_all",
                "flush",
                "write_async",
                "write_all_async",
                "flush_async",
            ] {
                assert!(names.contains(&must), "{must} missing in {names:?}");
            }
            for (name, expected) in [
                ("write", "Future<i64 | IoError>"),
                ("write_all", "Future<null | IoError>"),
                ("flush", "Future<null | IoError>"),
            ] {
                assert!(
                    completion_detail(&items, name).contains(expected),
                    "{label}.{name} must show its async Future return: {:?}",
                    completion_detail(&items, name)
                );
            }
            for (name, expected) in [
                ("write_async", "Future<i64 | IoError>"),
                ("write_all_async", "Future<null | IoError>"),
                ("flush_async", "Future<null | IoError>"),
            ] {
                assert!(
                    completion_detail(&items, name).contains(expected),
                    "{label}.{name} must advertise its async Future return: {:?}",
                    completion_detail(&items, name)
                );
            }
        }
    }

    #[test]
    fn member_completion_marks_std_io_buffered_adapters_non_awaitable() {
        let src = "\
import { Bytes } from \"std:bytes\";
import { buf_reader, buf_writer } from \"std:io\";
function main() {
  var reader = buf_reader(Bytes.from_str(\"abc\").cursor());
  reader.;
  var writer = buf_writer(Bytes.new());
  writer.;
}
";
        let c = Compiled::new(src.into());

        let reader_dot = src.find("reader.;").unwrap() + 6;
        let reader_ctx = crate::analysis::dot_completion_context(&c.text, reader_dot + 1).unwrap();
        let reader_items = member_completions(&c, &reader_ctx);
        let reader_names = labels(&reader_items);
        for must in ["read", "read_to_end", "read_line", "lines"] {
            assert!(
                reader_names.contains(&must),
                "{must} missing in {reader_names:?}"
            );
        }
        for (name, expected) in [
            ("read", "i64 | IoError"),
            ("read_to_end", "i64 | IoError"),
            ("read_line", "str | IoError"),
            ("lines", "Iterator<str | IoError>"),
        ] {
            assert!(
                completion_detail(&reader_items, name).contains(expected),
                "BufReader.{name} completion must show ordinary return {expected}: {:?}",
                completion_detail(&reader_items, name)
            );
            assert!(
                !completion_detail(&reader_items, name).contains("Future<"),
                "BufReader.{name} completion must not look awaitable: {:?}",
                completion_detail(&reader_items, name)
            );
        }

        let writer_dot = src.find("writer.;").unwrap() + 6;
        let writer_ctx = crate::analysis::dot_completion_context(&c.text, writer_dot + 1).unwrap();
        let writer_items = member_completions(&c, &writer_ctx);
        let writer_names = labels(&writer_items);
        for must in ["write", "write_all", "flush"] {
            assert!(
                writer_names.contains(&must),
                "{must} missing in {writer_names:?}"
            );
        }
        for (name, expected) in [
            ("write", "i64 | IoError"),
            ("write_all", "null | IoError"),
            ("flush", "null | IoError"),
        ] {
            assert!(
                completion_detail(&writer_items, name).contains(expected),
                "BufWriter.{name} completion must show ordinary return {expected}: {:?}",
                completion_detail(&writer_items, name)
            );
            assert!(
                !completion_detail(&writer_items, name).contains("Future<"),
                "BufWriter.{name} completion must not look awaitable: {:?}",
                completion_detail(&writer_items, name)
            );
        }
    }

    #[test]
    fn member_completion_lists_std_net_tcp_listener_options() {
        let src = "\
import { TcpListener } from \"std:net\";
function inspect(listener: TcpListener) {
  listener.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("listener.;").unwrap() + 8;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "accept",
            "local_addr",
            "take_error",
            "set_nonblocking",
            "ttl",
            "set_ttl",
            "close",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, expected) in [
            ("accept", "Future<(TcpStream, SocketAddr) | IoError>"),
            ("local_addr", "Future<SocketAddr | IoError>"),
            ("take_error", "Future<IoError | null>"),
            ("set_nonblocking", "Future<null | IoError>"),
            ("ttl", "Future<u32 | IoError>"),
            ("set_ttl", "Future<null | IoError>"),
            ("close", "Future<null | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "TcpListener.{name} completion must show expected return {expected}: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                completion_detail(&items, name).contains("Future<"),
                "TcpListener.{name} completion must look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn member_completion_lists_std_net_tcp_stream_timeout_options() {
        let src = "\
import { TcpStream } from \"std:net\";
function inspect(stream: TcpStream) {
  stream.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("stream.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "peer_addr",
            "local_addr",
            "take_error",
            "nodelay",
            "set_nodelay",
            "set_nonblocking",
            "read_timeout",
            "set_read_timeout",
            "write_timeout",
            "set_write_timeout",
            "peek",
            "ttl",
            "set_ttl",
            "close",
            "read",
            "read_to_end",
            "write",
            "write_all",
            "flush",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, expected) in [
            ("peer_addr", "Future<SocketAddr | IoError>"),
            ("local_addr", "Future<SocketAddr | IoError>"),
            ("take_error", "Future<IoError | null>"),
            ("nodelay", "Future<bool | IoError>"),
            ("set_nodelay", "Future<null | IoError>"),
            ("set_nonblocking", "Future<null | IoError>"),
            ("read_timeout", "Future<Duration | null | IoError>"),
            ("set_read_timeout", "Future<null | IoError>"),
            ("write_timeout", "Future<Duration | null | IoError>"),
            ("set_write_timeout", "Future<null | IoError>"),
            ("peek", "Future<i64 | IoError>"),
            ("ttl", "Future<u32 | IoError>"),
            ("set_ttl", "Future<null | IoError>"),
            ("close", "Future<null | IoError>"),
            ("read", "Future<i64 | IoError>"),
            ("read_to_end", "Future<i64 | IoError>"),
            ("write", "Future<i64 | IoError>"),
            ("write_all", "Future<null | IoError>"),
            ("flush", "Future<null | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "TcpStream.{name} completion must show expected return {expected}: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                completion_detail(&items, name).contains("Future<"),
                "TcpStream.{name} completion must look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn member_completion_lists_std_net_async_tcp_stream_methods() {
        let src = "\
import { AsyncTcpStream, TcpStream } from \"std:net\";
function inspect(stream: TcpStream) {
  var async_stream = AsyncTcpStream.from_stream(stream);
  async_stream.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("async_stream.;").unwrap() + 12;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "read_async",
            "write_async",
            "peek_async",
            "into_stream",
            "peer_addr",
            "local_addr",
            "close",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        assert!(
            completion_detail(&items, "read_async").contains("Future<i64 | IoError>"),
            "read_async must advertise its async Future return: {:?}",
            completion_detail(&items, "read_async")
        );
        assert!(
            completion_detail(&items, "write_async").contains("Future<i64 | IoError>"),
            "write_async must advertise its async Future return: {:?}",
            completion_detail(&items, "write_async")
        );
        assert!(
            completion_detail(&items, "peek_async").contains("Future<i64 | IoError>"),
            "peek_async must advertise its async Future return: {:?}",
            completion_detail(&items, "peek_async")
        );
        for (name, expected) in [("into_stream", "TcpStream")] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "AsyncTcpStream.{name} completion must show its ordinary conversion return: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                !completion_detail(&items, name).contains("Future<"),
                "AsyncTcpStream.{name} completion must not look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
        for (name, expected) in [
            ("peer_addr", "Future<SocketAddr | IoError>"),
            ("local_addr", "Future<SocketAddr | IoError>"),
            ("close", "Future<null | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "AsyncTcpStream.{name} completion must show its async Future return: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                completion_detail(&items, name).contains("Future<"),
                "AsyncTcpStream.{name} completion must look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn member_completion_rejects_copied_async_network_timeout_method_families() {
        fn member_names(src: &str, marker: &str) -> Vec<String> {
            let c = Compiled::new(src.into());
            let dot = src.find(marker).unwrap() + marker.trim_end_matches(".;").len();
            let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
            member_completions(&c, &ctx)
                .into_iter()
                .map(|item| item.label)
                .collect()
        }

        let stream_src = "\
import { AsyncTcpStream, TcpStream } from \"std:net\";
function inspect(stream: TcpStream) {
  var async_stream = AsyncTcpStream.from_stream(stream);
  async_stream.;
}
";
        let stream_names = member_names(stream_src, "async_stream.;");
        for forbidden in [
            "read_timeout_async",
            "write_timeout_async",
            "peek_timeout_async",
        ] {
            assert!(
                !stream_names.iter().any(|name| name == forbidden),
                "AsyncTcpStream completion must not copy timeout convenience `{forbidden}`: {stream_names:?}"
            );
        }

        let listener_src = "\
import { AsyncTcpListener } from \"std:net\";
function inspect(listener: AsyncTcpListener) {
  listener.;
}
";
        let listener_names = member_names(listener_src, "listener.;");
        assert!(
            !listener_names
                .iter()
                .any(|name| name == "accept_timeout_async"),
            "AsyncTcpListener completion must not copy timeout convenience `accept_timeout_async`: {listener_names:?}"
        );

        let udp_src = "\
import { AsyncUdpSocket } from \"std:net\";
function inspect(socket: AsyncUdpSocket) {
  socket.;
}
";
        let udp_names = member_names(udp_src, "socket.;");
        for forbidden in [
            "send_timeout_async",
            "recv_timeout_async",
            "peek_timeout_async",
            "send_to_timeout_async",
            "recv_from_timeout_async",
            "peek_from_timeout_async",
        ] {
            assert!(
                !udp_names.iter().any(|name| name == forbidden),
                "AsyncUdpSocket completion must not copy timeout convenience `{forbidden}`: {udp_names:?}"
            );
        }
    }

    #[test]
    fn type_namespace_completion_marks_tcp_connectors_awaitable() {
        let plain_src = "\
import { TcpStream } from \"std:net\";
function main() {
  var connected = TcpStream.;
}
";
        let c = Compiled::new(plain_src.into());
        let dot = plain_src.find("TcpStream.;").unwrap() + 9;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["connect", "connect_timeout"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
            assert!(
                completion_detail(&items, must).contains("Future<TcpStream | IoError>"),
                "TcpStream.{must} completion must advertise its Future return: {:?}",
                completion_detail(&items, must)
            );
        }

        let async_src = "\
import { AsyncTcpStream } from \"std:net\";
function main() {
  var connected = AsyncTcpStream.;
}
";
        let c = Compiled::new(async_src.into());
        let dot = async_src.find("AsyncTcpStream.;").unwrap() + 14;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["connect", "connect_timeout"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
            assert!(
                completion_detail(&items, must).contains("Future<AsyncTcpStream | IoError>"),
                "async AsyncTcpStream.{must} completion must advertise its Future return: {:?}",
                completion_detail(&items, must)
            );
        }
    }

    #[test]
    fn type_namespace_completion_marks_async_net_setup_contracts() {
        fn type_items(src: &str, marker: &str) -> Vec<CompletionItem> {
            let c = Compiled::new(src.into());
            let dot = src.find(marker).unwrap() + marker.trim_end_matches(".;").len();
            let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
            member_completions(&c, &ctx)
        }

        let stream_src = "\
import { AsyncTcpStream, TcpStream } from \"std:net\";
function main() {
  AsyncTcpStream.;
}
";
        let stream_items = type_items(stream_src, "AsyncTcpStream.;");
        assert!(
            completion_detail(&stream_items, "from_stream").contains("AsyncTcpStream"),
            "AsyncTcpStream.from_stream completion must show its wrapper return: {:?}",
            completion_detail(&stream_items, "from_stream")
        );
        assert!(
            !completion_detail(&stream_items, "from_stream").contains("Future<"),
            "AsyncTcpStream.from_stream completion must not look awaitable: {:?}",
            completion_detail(&stream_items, "from_stream")
        );

        let listener_src = "\
import { AsyncTcpListener, TcpListener } from \"std:net\";
function main() {
  AsyncTcpListener.;
}
";
        let listener_items = type_items(listener_src, "AsyncTcpListener.;");
        assert!(
            completion_detail(&listener_items, "bind")
                .contains("Future<AsyncTcpListener | IoError>"),
            "AsyncTcpListener.bind completion must advertise its Future return: {:?}",
            completion_detail(&listener_items, "bind")
        );
        assert!(
            completion_detail(&listener_items, "bind").contains("Future<"),
            "AsyncTcpListener.bind completion must look awaitable: {:?}",
            completion_detail(&listener_items, "bind")
        );
        assert!(
            completion_detail(&listener_items, "from_listener").contains("AsyncTcpListener"),
            "AsyncTcpListener.from_listener completion must show its wrapper return: {:?}",
            completion_detail(&listener_items, "from_listener")
        );
        assert!(
            !completion_detail(&listener_items, "from_listener").contains("Future<"),
            "AsyncTcpListener.from_listener completion must not look awaitable: {:?}",
            completion_detail(&listener_items, "from_listener")
        );

        let udp_src = "\
import { AsyncUdpSocket, UdpSocket } from \"std:net\";
function main() {
  AsyncUdpSocket.;
}
";
        let udp_items = type_items(udp_src, "AsyncUdpSocket.;");
        assert!(
            completion_detail(&udp_items, "bind").contains("Future<AsyncUdpSocket | IoError>"),
            "AsyncUdpSocket.bind completion must advertise its Future return: {:?}",
            completion_detail(&udp_items, "bind")
        );
        assert!(
            completion_detail(&udp_items, "from_socket").contains("AsyncUdpSocket"),
            "AsyncUdpSocket.from_socket completion must show its wrapper return: {:?}",
            completion_detail(&udp_items, "from_socket")
        );
        assert!(
            !completion_detail(&udp_items, "from_socket").contains("Future<"),
            "AsyncUdpSocket.from_socket completion must not look awaitable: {:?}",
            completion_detail(&udp_items, "from_socket")
        );
    }

    #[test]
    fn namespace_completion_distinguishes_time_and_async_sleep() {
        let src = "\
import \"std:time\" as Time;
import \"std:async\" as Async;
function main() {
  Time.;
  Async.;
}
";
        let c = Compiled::new(src.into());

        let time_dot = src.find("Time.;").unwrap() + 4;
        let time_ctx = crate::analysis::dot_completion_context(&c.text, time_dot + 1).unwrap();
        let time_items = member_completions(&c, &time_ctx);
        let time_names = labels(&time_items);
        assert!(
            time_names.contains(&"sleep"),
            "Time.sleep missing in {time_names:?}"
        );
        assert!(
            completion_detail(&time_items, "sleep").contains("(duration: Duration): Future<null>"),
            "Time.sleep must advertise its async Future return: {:?}",
            completion_detail(&time_items, "sleep")
        );
        assert!(
            completion_detail(&time_items, "sleep").contains("Future<"),
            "Time.sleep completion must look awaitable: {:?}",
            completion_detail(&time_items, "sleep")
        );

        let async_dot = src.find("Async.;").unwrap() + 5;
        let async_ctx = crate::analysis::dot_completion_context(&c.text, async_dot + 1).unwrap();
        let async_items = member_completions(&c, &async_ctx);
        let async_names = labels(&async_items);
        assert!(
            async_names.contains(&"sleep"),
            "Async.sleep missing in {async_names:?}"
        );
        assert!(
            completion_detail(&async_items, "sleep").contains("(ms: i64): Future<null>"),
            "Async.sleep must advertise its executor-suspending Future return: {:?}",
            completion_detail(&async_items, "sleep")
        );
    }

    #[test]
    fn marker_alias_signatures_stay_future_returning_across_lsp_surfaces() {
        let src = "\
import { Future } from \"core:prelude\";
import { Duration, sleep as time_sleep } from \"std:time\";
import { TimedOut, sleep as nap, timeout as race, yield_now as pause } from \"std:async\";
function fast(): Future<i64> async { 7 }
function main(): Future<null> async {
  await pause();
  await nap(1);
  await time_sleep(Duration.from_millis(1));
  var value: i64 | TimedOut = await race(fast(), 10);
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);

        let cases = [
            ("pause", "(): Future<null>", "pause(): Future<null>"),
            (
                "nap",
                "(ms: i64): Future<null>",
                "nap(ms: i64): Future<null>",
            ),
            (
                "time_sleep",
                "(duration: Duration): Future<null>",
                "time_sleep(duration: Duration): Future<null>",
            ),
            (
                "race",
                "(future: Future<T>, ms: i64): Future<T | TimedOut>",
                "race(future: Future<T>, ms: i64): Future<T | TimedOut>",
            ),
        ];

        for (alias, detail, label) in cases {
            let def = c
                .analysis
                .program
                .resolve_value_in(ModId::ROOT, alias)
                .unwrap_or_else(|| panic!("{alias} alias did not resolve"));
            assert_eq!(
                c.def_signature(def),
                detail,
                "{alias} completion/hover detail must advertise the async contract"
            );
            let sig = build_signature_info(&c, def, alias)
                .unwrap_or_else(|| panic!("{alias} signature help missing"));
            assert_eq!(
                sig.label, label,
                "{alias} signature help must keep the alias name and Future return"
            );
        }
    }

    #[test]
    fn namespace_completion_marks_std_process_helpers_awaitable_where_they_touch_host_state() {
        let src = "\
import \"std:process\" as Process;
function main() {
  Process.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("Process.;").unwrap() + 7;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "command",
            "exit_status",
            "output",
            "args",
            "env",
            "env_all",
            "set_env",
            "exit",
            "abort",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, expected) in [
            ("command", "Command"),
            ("exit_status", "ExitStatus"),
            ("output", "Output"),
            ("args", "Future<List<str>>"),
            ("env", "Future<str | null>"),
            ("env_all", "Future<Map<str, str>>"),
            ("set_env", "Future<null>"),
            ("exit", "never"),
            ("abort", "never"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "Process.{name} completion must show return {expected}: {:?}",
                completion_detail(&items, name)
            );
        }
        for name in ["args", "env", "env_all", "set_env"] {
            assert!(
                completion_detail(&items, name).contains("Future<"),
                "Process.{name} completion must look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
        for name in ["command", "exit_status", "output", "exit", "abort"] {
            assert!(
                !completion_detail(&items, name).contains("Future<"),
                "Process.{name} completion must remain ordinary/non-returning: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn namespace_completion_marks_std_io_helpers_async_where_they_touch_stdio() {
        let src = "\
import \"std:io\" as Io;
function main() {
  Io.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("Io.;").unwrap() + 2;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "stdin",
            "stdout",
            "stderr",
            "print",
            "println",
            "eprint",
            "eprintln",
            "buf_reader",
            "buf_writer",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, expected) in [
            ("stdin", "Stdin"),
            ("stdout", "Stdout"),
            ("stderr", "Stderr"),
            ("buf_reader", "BufReader"),
            ("buf_writer", "BufWriter"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "Io.{name} completion must show ordinary return {expected}: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                !completion_detail(&items, name).contains("Future<"),
                "Io.{name} completion must not look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
        for name in ["print", "println", "eprint", "eprintln"] {
            assert!(
                completion_detail(&items, name).contains("Future<null>"),
                "Io.{name} completion must advertise async output: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                completion_detail(&items, name).contains("Future<"),
                "Io.{name} completion must look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn namespace_completion_marks_std_fs_helpers_awaitable_where_they_touch_storage() {
        let src = "\
import \"std:fs\" as Fs;
function main() {
  Fs.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("Fs.;").unwrap() + 2;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "open_options",
            "read_to_string",
            "write_string",
            "append_string",
            "read",
            "write",
            "remove",
            "rename",
            "create_dir",
            "create_dir_all",
            "canonicalize",
            "read_dir",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, expected) in [
            ("open_options", "OpenOptions"),
            ("read_to_string", "str | IoError"),
            ("write_string", "null | IoError"),
            ("append_string", "null | IoError"),
            ("read", "Bytes | IoError"),
            ("write", "null | IoError"),
            ("remove", "null | IoError"),
            ("rename", "null | IoError"),
            ("create_dir", "null | IoError"),
            ("create_dir_all", "null | IoError"),
            ("canonicalize", "Path | IoError"),
            ("read_dir", "DirEntries | IoError"),
        ] {
            assert!(
                completion_detail(&items, name).contains(&format!("Future<{expected}>")),
                "Fs.{name} completion must show future return {expected}: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                completion_detail(&items, name).contains("Future<"),
                "Fs.{name} completion must look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn member_completion_marks_std_fs_path_storage_queries_awaitable() {
        let src = "\
import { Path } from \"std:fs\";
function main() {
  Path.new(\".\").;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find(".;").unwrap();
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "exists",
            "is_file",
            "is_dir",
            "file_kind",
            "byte_len",
            "permissions",
            "metadata",
            "canonicalize",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, expected) in [
            ("exists", "Future<bool | IoError>"),
            ("is_file", "Future<bool | IoError>"),
            ("is_dir", "Future<bool | IoError>"),
            ("file_kind", "Future<FileKind | IoError>"),
            ("byte_len", "Future<u64 | IoError>"),
            ("permissions", "Future<Permissions | IoError>"),
            ("metadata", "Future<Metadata | IoError>"),
            ("canonicalize", "Future<Path | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "Path.{name} completion must show its future return: {:?}",
                completion_detail(&items, name)
            );
        }
        for (name, expected) in [("join", "Path"), ("as_str", "str"), ("normalize", "Path")] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "Path.{name} completion should keep pure value return {expected}: {:?}",
                completion_detail(&items, name)
            );
            assert!(
                !completion_detail(&items, name).contains("Future<"),
                "Path.{name} completion must not look awaitable: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn namespace_completion_marks_std_net_resolve_awaitable() {
        let src = "\
import \"std:net\" as Net;
function main() {
  Net.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("Net.;").unwrap() + 3;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        assert!(names.contains(&"resolve"), "resolve missing in {names:?}");
        assert!(
            completion_detail(&items, "resolve").contains("Future<List<IpAddr> | IoError>"),
            "Net.resolve completion must show its future return: {:?}",
            completion_detail(&items, "resolve")
        );
    }

    #[test]
    fn type_namespace_completion_marks_std_fs_file_constructors_awaitable() {
        let src = "\
import { File, Path, open_options } from \"std:fs\";
function main() {
  File.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("File.;").unwrap() + 4;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["open", "create", "append", "open_with"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
            let detail = completion_detail(&items, must);
            assert!(
                detail.contains("Future<") && detail.contains("File") && detail.contains("IoError"),
                "File.{must} completion must show its future constructor return: {:?}",
                detail
            );
        }
    }

    #[test]
    fn member_completion_lists_std_net_async_tcp_listener_methods() {
        let src = "\
import { AsyncTcpListener } from \"std:net\";
function inspect(listener: AsyncTcpListener) {
  listener.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("listener.;").unwrap() + 8;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "accept_async",
            "into_listener",
            "local_addr",
            "take_error",
            "ttl",
            "set_ttl",
            "close",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        assert!(
            completion_detail(&items, "accept_async")
                .contains("Future<(AsyncTcpStream, SocketAddr) | IoError>"),
            "accept_async must advertise its async Future return: {:?}",
            completion_detail(&items, "accept_async")
        );
        for (name, expected) in [
            ("into_listener", "TcpListener"),
            ("local_addr", "Future<SocketAddr | IoError>"),
            ("take_error", "Future<IoError | null>"),
            ("ttl", "Future<u32 | IoError>"),
            ("set_ttl", "Future<null | IoError>"),
            ("close", "Future<null | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "AsyncTcpListener.{name} completion must show expected return {expected}: {:?}",
                completion_detail(&items, name)
            );
            if name == "into_listener" {
                assert!(
                    !completion_detail(&items, name).contains("Future<"),
                    "AsyncTcpListener.{name} completion must not look awaitable: {:?}",
                    completion_detail(&items, name)
                );
            } else {
                assert!(
                    completion_detail(&items, name).contains("Future<"),
                    "AsyncTcpListener.{name} completion must look awaitable: {:?}",
                    completion_detail(&items, name)
                );
            }
        }
    }

    #[test]
    fn member_completion_lists_std_net_udp_connected_methods() {
        let src = "\
import { UdpSocket } from \"std:net\";
function inspect(socket: UdpSocket) {
  socket.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("socket.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "local_addr",
            "connect",
            "peer_addr",
            "ttl",
            "take_error",
            "set_nonblocking",
            "read_timeout",
            "set_read_timeout",
            "write_timeout",
            "set_write_timeout",
            "set_ttl",
            "broadcast",
            "set_broadcast",
            "multicast_loop_v4",
            "set_multicast_loop_v4",
            "multicast_loop_v6",
            "set_multicast_loop_v6",
            "multicast_ttl_v4",
            "set_multicast_ttl_v4",
            "join_multicast_v4",
            "leave_multicast_v4",
            "join_multicast_v6",
            "leave_multicast_v6",
            "send_to",
            "send",
            "recv",
            "peek",
            "recv_from",
            "peek_from",
            "close",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, expected) in [
            ("local_addr", "Future<SocketAddr | IoError>"),
            ("connect", "Future<null | IoError>"),
            ("peer_addr", "Future<SocketAddr | IoError>"),
            ("ttl", "Future<u32 | IoError>"),
            ("take_error", "Future<IoError | null>"),
            ("set_nonblocking", "Future<null | IoError>"),
            ("read_timeout", "Future<Duration | null | IoError>"),
            ("set_read_timeout", "Future<null | IoError>"),
            ("write_timeout", "Future<Duration | null | IoError>"),
            ("set_write_timeout", "Future<null | IoError>"),
            ("set_ttl", "Future<null | IoError>"),
            ("broadcast", "Future<bool | IoError>"),
            ("set_broadcast", "Future<null | IoError>"),
            ("multicast_loop_v4", "Future<bool | IoError>"),
            ("set_multicast_loop_v4", "Future<null | IoError>"),
            ("multicast_loop_v6", "Future<bool | IoError>"),
            ("set_multicast_loop_v6", "Future<null | IoError>"),
            ("multicast_ttl_v4", "Future<u32 | IoError>"),
            ("set_multicast_ttl_v4", "Future<null | IoError>"),
            ("join_multicast_v4", "Future<null | IoError>"),
            ("leave_multicast_v4", "Future<null | IoError>"),
            ("join_multicast_v6", "Future<null | IoError>"),
            ("leave_multicast_v6", "Future<null | IoError>"),
            ("send_to", "Future<i64 | IoError>"),
            ("send", "Future<i64 | IoError>"),
            ("recv", "Future<i64 | IoError>"),
            ("peek", "Future<i64 | IoError>"),
            ("recv_from", "Future<(i64, SocketAddr) | IoError>"),
            ("peek_from", "Future<(i64, SocketAddr) | IoError>"),
            ("close", "Future<null | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "UdpSocket.{name} completion must show its awaitable return: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn member_completion_lists_std_net_async_udp_methods() {
        let src = "\
import { AsyncUdpSocket } from \"std:net\";
function inspect(socket: AsyncUdpSocket) {
  socket.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("socket.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "into_socket",
            "local_addr",
            "connect",
            "peer_addr",
            "ttl",
            "take_error",
            "set_ttl",
            "broadcast",
            "set_broadcast",
            "multicast_loop_v4",
            "set_multicast_loop_v4",
            "multicast_loop_v6",
            "set_multicast_loop_v6",
            "multicast_ttl_v4",
            "set_multicast_ttl_v4",
            "join_multicast_v4",
            "leave_multicast_v4",
            "join_multicast_v6",
            "leave_multicast_v6",
            "send_async",
            "recv_async",
            "peek_async",
            "send_to_async",
            "recv_from_async",
            "peek_from_async",
            "close",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        assert!(
            completion_detail(&items, "send_async").contains("Future<i64 | IoError>"),
            "send_async must advertise its async Future return: {:?}",
            completion_detail(&items, "send_async")
        );
        assert!(
            completion_detail(&items, "recv_async").contains("Future<i64 | IoError>"),
            "recv_async must advertise its async Future return: {:?}",
            completion_detail(&items, "recv_async")
        );
        assert!(
            completion_detail(&items, "peek_async").contains("Future<i64 | IoError>"),
            "peek_async must advertise its async Future return: {:?}",
            completion_detail(&items, "peek_async")
        );
        assert!(
            completion_detail(&items, "send_to_async").contains("Future<i64 | IoError>"),
            "send_to_async must advertise its async Future return: {:?}",
            completion_detail(&items, "send_to_async")
        );
        assert!(
            completion_detail(&items, "recv_from_async")
                .contains("Future<(i64, SocketAddr) | IoError>"),
            "recv_from_async must advertise its async Future return: {:?}",
            completion_detail(&items, "recv_from_async")
        );
        assert!(
            completion_detail(&items, "peek_from_async")
                .contains("Future<(i64, SocketAddr) | IoError>"),
            "peek_from_async must advertise its async Future return: {:?}",
            completion_detail(&items, "peek_from_async")
        );
        assert!(
            completion_detail(&items, "into_socket").contains("UdpSocket"),
            "AsyncUdpSocket.into_socket completion must show its wrapper return: {:?}",
            completion_detail(&items, "into_socket")
        );
        assert!(
            !completion_detail(&items, "into_socket").contains("Future<"),
            "AsyncUdpSocket.into_socket completion must not look awaitable: {:?}",
            completion_detail(&items, "into_socket")
        );
        for (name, expected) in [
            ("local_addr", "Future<SocketAddr | IoError>"),
            ("connect", "Future<null | IoError>"),
            ("peer_addr", "Future<SocketAddr | IoError>"),
            ("ttl", "Future<u32 | IoError>"),
            ("take_error", "Future<IoError | null>"),
            ("set_ttl", "Future<null | IoError>"),
            ("broadcast", "Future<bool | IoError>"),
            ("set_broadcast", "Future<null | IoError>"),
            ("multicast_loop_v4", "Future<bool | IoError>"),
            ("set_multicast_loop_v4", "Future<null | IoError>"),
            ("multicast_loop_v6", "Future<bool | IoError>"),
            ("set_multicast_loop_v6", "Future<null | IoError>"),
            ("multicast_ttl_v4", "Future<u32 | IoError>"),
            ("set_multicast_ttl_v4", "Future<null | IoError>"),
            ("join_multicast_v4", "Future<null | IoError>"),
            ("leave_multicast_v4", "Future<null | IoError>"),
            ("join_multicast_v6", "Future<null | IoError>"),
            ("leave_multicast_v6", "Future<null | IoError>"),
            ("close", "Future<null | IoError>"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "AsyncUdpSocket.{name} completion must show its awaitable return: {:?}",
                completion_detail(&items, name)
            );
        }
    }

    #[test]
    fn member_completion_lists_std_process_child_methods() {
        let src = "\
import { Future } from \"core:prelude\";
import { Command, Child } from \"std:process\";
function main(): Future<null> async {
  var spawned = await Command.new(\"true\").spawn();
  if spawned is Child {
    var child = spawned as Child;
    child.;
  }
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("child.;").unwrap() + 5;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["id", "stdin", "stdout", "stderr", "wait", "kill"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        assert!(
            completion_detail(&items, "wait").contains("Future<ExitStatus | IoError>"),
            "Child.wait must show its async value return: {:?}",
            completion_detail(&items, "wait")
        );
        assert!(
            completion_detail(&items, "kill").contains("Future<null | IoError>"),
            "Child.kill must show its async value return: {:?}",
            completion_detail(&items, "kill")
        );
        assert!(
            completion_detail(&items, "id").contains("u32"),
            "Child.id must show its ordinary metadata return: {:?}",
            completion_detail(&items, "id")
        );
        for (name, expected) in [
            ("stdin", "null | AsyncWriter"),
            ("stdout", "null | AsyncReader"),
            ("stderr", "null | AsyncReader"),
        ] {
            assert!(
                completion_detail(&items, name).contains(expected),
                "Child.{name} must show its immediate async-stream-or-null field return: {:?}",
                completion_detail(&items, name)
            );
        }
        assert!(
            !completion_detail(&items, "id").contains("Future<"),
            "Child.id completion must not look awaitable: {:?}",
            completion_detail(&items, "id")
        );
        for must in ["stdin", "stdout", "stderr"] {
            assert!(
                !completion_detail(&items, must).contains("Future<"),
                "Child.{must} completion must not look awaitable: {:?}",
                completion_detail(&items, must)
            );
        }
    }

    #[test]
    fn member_completion_marks_std_process_execution_methods_awaitable() {
        let src = "\
import { Command } from \"std:process\";
function main() {
  var cmd = Command.new(\"true\");
  cmd.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("cmd.;").unwrap() + 3;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["status", "output", "spawn"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        assert!(
            completion_detail(&items, "status").contains("Future<ExitStatus | IoError>"),
            "Command.status must show its async value return: {:?}",
            completion_detail(&items, "status")
        );
        assert!(
            completion_detail(&items, "output").contains("Future<Output | IoError>"),
            "Command.output must show its async value return: {:?}",
            completion_detail(&items, "output")
        );
        assert!(
            completion_detail(&items, "spawn").contains("Future<Child | IoError>"),
            "Command.spawn must show its async child-start return: {:?}",
            completion_detail(&items, "spawn")
        );
    }

    #[test]
    fn member_completion_marks_std_process_command_values_non_awaitable() {
        let src = "\
import { Command } from \"std:process\";
function main() {
  var cmd = Command.new(\"true\");
  cmd.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("cmd.;").unwrap() + 3;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "program",
            "args",
            "arg_count",
            "env",
            "env_var_value",
            "has_env_var",
            "env_count",
            "cwd",
            "with_program",
            "with_args",
            "arg",
            "with_env",
            "env_var",
            "inherit_env",
            "clear_env",
            "with_cwd",
            "clear_cwd",
            "validation_error",
            "validate",
            "is_valid",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        for (name, ret) in [
            ("program", "str"),
            ("args", "List<str>"),
            ("arg_count", "i64"),
            ("env", "Map<str, str> | null"),
            ("env_var_value", "str | null"),
            ("has_env_var", "bool"),
            ("env_count", "i64"),
            ("cwd", "Path | null"),
            ("validation_error", "IoError | null"),
            ("validate", "null | IoError"),
            ("is_valid", "bool"),
        ] {
            assert!(
                completion_detail(&items, name).contains(ret),
                "Command.{name} must show ordinary value return {ret}: {:?}",
                completion_detail(&items, name)
            );
        }
        for name in [
            "with_program",
            "with_args",
            "arg",
            "with_env",
            "env_var",
            "inherit_env",
            "clear_env",
            "with_cwd",
            "clear_cwd",
        ] {
            assert!(
                completion_detail(&items, name).contains("Command"),
                "Command.{name} must show fresh Command return: {:?}",
                completion_detail(&items, name)
            );
        }
        for must in [
            "program",
            "args",
            "arg_count",
            "env",
            "env_var_value",
            "has_env_var",
            "env_count",
            "cwd",
            "with_program",
            "with_args",
            "arg",
            "with_env",
            "env_var",
            "inherit_env",
            "clear_env",
            "with_cwd",
            "clear_cwd",
            "validation_error",
            "validate",
            "is_valid",
        ] {
            assert!(
                !completion_detail(&items, must).contains("Future<"),
                "Command.{must} completion must not look awaitable: {:?}",
                completion_detail(&items, must)
            );
        }
    }

    #[test]
    fn member_completion_lists_joinhandle_intrinsic_methods() {
        let thread_src = "\
import { Thread } from \"std:thread\";
function main() {
  var h = Thread.spawn(() => 7);
  h.;
}
";
        let c = Compiled::new(thread_src.into());
        let dot = thread_src.find("h.;").unwrap() + 1;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["join", "detach"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        assert!(
            completion_detail(&items, "join").contains("Future<Joined<i64> | Panicked>"),
            "thread JoinHandle.join must advertise its async Future return: {:?}",
            completion_detail(&items, "join")
        );
        assert!(
            completion_detail(&items, "detach").contains("(): null"),
            "thread JoinHandle.detach must show its ordinary value return: {:?}",
            completion_detail(&items, "detach")
        );
        assert!(
            !completion_detail(&items, "detach").contains("Future<"),
            "thread JoinHandle.detach completion must not look awaitable: {:?}",
            completion_detail(&items, "detach")
        );

        let task_src = "\
import { Task } from \"std:task\";
function main() {
  var h = Task.spawn(() async => { 7 });
  h.;
}
";
        let c = Compiled::new(task_src.into());
        let dot = task_src.find("h.;").unwrap() + 1;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["join", "detach", "cancel", "abort"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
        assert!(
            completion_detail(&items, "join")
                .contains("Future<Joined<i64> | Panicked | Cancelled>"),
            "task JoinHandle.join must advertise cancellation in its Future return: {:?}",
            completion_detail(&items, "join")
        );
        for must in ["detach", "cancel", "abort"] {
            assert!(
                completion_detail(&items, must).contains("(): null"),
                "task JoinHandle.{must} must show its ordinary value return: {:?}",
                completion_detail(&items, must)
            );
            assert!(
                !completion_detail(&items, must).contains("Future<"),
                "task JoinHandle.{must} completion must not look awaitable: {:?}",
                completion_detail(&items, must)
            );
        }
    }

    #[test]
    fn member_completion_lists_channel_endpoint_intrinsic_methods() {
        let src = "\
import { channel } from \"std:sync\";
function main() {
  var pair = channel<i64>();
  var tx = pair.0;
  tx.;
  var rx = pair.1;
  rx.;
}
";
        let c = Compiled::new(src.into());
        let tx_dot = src.find("tx.;").unwrap() + 2;
        let tx_ctx = crate::analysis::dot_completion_context(&c.text, tx_dot + 1).unwrap();
        let tx_items = member_completions(&c, &tx_ctx);
        let tx_names = labels(&tx_items);
        assert!(tx_names.contains(&"send"), "send missing in {tx_names:?}");
        assert!(
            completion_detail(&tx_items, "send").contains("(value: i64): null | ChannelClosed"),
            "Sender.send must show its ordinary value return: {:?}",
            completion_detail(&tx_items, "send")
        );
        assert!(
            !completion_detail(&tx_items, "send").contains("Future<"),
            "Sender.send completion must not look awaitable: {:?}",
            completion_detail(&tx_items, "send")
        );

        let rx_dot = src.find("rx.;").unwrap() + 2;
        let rx_ctx = crate::analysis::dot_completion_context(&c.text, rx_dot + 1).unwrap();
        let rx_items = member_completions(&c, &rx_ctx);
        let rx_names = labels(&rx_items);
        for must in ["recv", "try_recv"] {
            assert!(rx_names.contains(&must), "{must} missing in {rx_names:?}");
        }
        assert!(
            completion_detail(&rx_items, "recv").contains("Future<i64 | ChannelClosed>"),
            "Receiver.recv must advertise its async Future return: {:?}",
            completion_detail(&rx_items, "recv")
        );
        assert!(
            completion_detail(&rx_items, "try_recv").contains("(): i64 | null"),
            "Receiver.try_recv must show its immediate non-blocking poll return: {:?}",
            completion_detail(&rx_items, "try_recv")
        );
        assert!(
            !completion_detail(&rx_items, "try_recv").contains("Future<"),
            "Receiver.try_recv completion must not look awaitable: {:?}",
            completion_detail(&rx_items, "try_recv")
        );
    }

    #[test]
    fn member_completion_lists_std_process_exit_status_methods() {
        let src = "\
import { ExitStatus } from \"std:process\";
function main() {
  var status = ExitStatus.exited(0i32);
  status.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("status.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "code",
            "signal",
            "core_dumped",
            "stopped_signal",
            "continued",
            "success",
            "exited_normally",
            "was_signaled",
            "was_stopped",
            "was_continued",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_core_atomic_i64_methods() {
        let src = "\
import { AtomicI64 } from \"core:sync/atomic\";
function main() {
  var atomic = AtomicI64.new(0);
  atomic.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("atomic.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "load",
            "store",
            "swap",
            "compare_exchange",
            "fetch_add",
            "fetch_sub",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_core_atomic_i32_methods() {
        let src = "\
import { AtomicI32 } from \"core:sync/atomic\";
function main() {
  var atomic = AtomicI32.new(0);
  atomic.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("atomic.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "load",
            "store",
            "swap",
            "compare_exchange",
            "fetch_add",
            "fetch_sub",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_core_atomic_u64_methods() {
        let src = "\
import { AtomicU64 } from \"core:sync/atomic\";
function main() {
  var atomic = AtomicU64.new(0u64);
  atomic.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("atomic.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "load",
            "store",
            "swap",
            "compare_exchange",
            "fetch_add",
            "fetch_sub",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_core_atomic_u32_methods() {
        let src = "\
import { AtomicU32 } from \"core:sync/atomic\";
function main() {
  var atomic = AtomicU32.new(0u32);
  atomic.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("atomic.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "load",
            "store",
            "swap",
            "compare_exchange",
            "fetch_add",
            "fetch_sub",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_core_atomic_ptr_methods() {
        let src = "\
import { AtomicPtr } from \"core:sync/atomic\";
extern struct Cell { value: i64 }
function main() {
  var value = Cell { value: 0 };
  var atomic = AtomicPtr.new<Cell>((&value) as *Cell);
  atomic.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("atomic.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["load", "store", "swap", "compare_exchange"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_core_atomic_bool_methods() {
        let src = "\
import { AtomicBool } from \"core:sync/atomic\";
function main() {
  var atomic = AtomicBool.new(false);
  atomic.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("atomic.;").unwrap() + 6;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "load",
            "store",
            "swap",
            "compare_exchange",
            "fetch_and",
            "fetch_or",
            "fetch_xor",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn member_completion_lists_str_intrinsic_methods() {
        let src = "\
function main() {
  var s = \"hello\";
  var n = s.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("s.;").unwrap() + 1;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["size", "contains", "to_upper", "trim", "starts_with"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn type_namespace_completion_lists_primitive_static_methods() {
        let src = "function main() { var x = i32.; }\n";
        let c = Compiled::new(src.into());
        let dot = src.find("i32.;").unwrap() + 3;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in ["MIN", "MAX", "wrapping_add", "checked_mul"] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn type_namespace_completion_lists_std_time_datetime_static_methods() {
        let src = "\
import { DateTime } from \"std:time\";
function main() {
  var x = DateTime.;
}
";
        let c = Compiled::new(src.into());
        let dot = src.find("DateTime.;").unwrap() + 8;
        let ctx = crate::analysis::dot_completion_context(&c.text, dot + 1).unwrap();
        let items = member_completions(&c, &ctx);
        let names = labels(&items);
        for must in [
            "new",
            "parse_iso8601",
            "now_utc",
            "now_local",
            "from_system_time_local",
        ] {
            assert!(names.contains(&must), "{must} missing in {names:?}");
        }
    }

    #[test]
    fn analysis_accepts_std_net_resolve_import() {
        let src = "\
import { Future } from \"core:prelude\";
import { List } from \"core:collections\";
import { IpAddr } from \"std:net/types\";
import { resolve } from \"std:net\";
function main(): Future<null> async {
  var resolved = await resolve(\"127.0.0.1\");
  if resolved is List<IpAddr> {
    var addrs = resolved as List<IpAddr>;
  }
  null
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn analysis_accepts_std_fmt_primitive_debug() {
        let src = "\
import { Debug } from \"std:fmt\";
function render(value: Debug): str {
  value.debug()
}
function generic<T: Debug>(value: T): str {
  value.debug()
}
function main() {
  var number: Debug = 42;
  var rendered = render(\"hi\");
  var direct = true.debug();
  var generic_text = generic('Z');
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn analysis_accepts_std_net_tcp_imports_with_awaited_wait_surfaces() {
        let src = "\
import { Future } from \"core:prelude\";
import { AsyncReader, AsyncWriter, IoError } from \"std:io\";
import { Bytes } from \"std:bytes\";
import { TcpStream, AsyncTcpStream, AsyncTcpListener, TcpListener } from \"std:net\";
import { SocketAddr, ip_v4, socket_addr } from \"std:net/types\";
import { Duration } from \"std:time\";
function read_via_async_protocol<T: AsyncReader>(reader: T): Future<i64 | IoError> {
  reader.read_async(Bytes.from_str(\"xxxx\"))
}
function write_via_async_protocol<T: AsyncWriter>(writer: T): Future<i64 | IoError> {
  writer.write_async(Bytes.from_str(\"x\"))
}
function main(): Future<null> async {
  var addr: SocketAddr = socket_addr(ip_v4(127u8, 0u8, 0u8, 1u8), 0u16);
  var listener = await TcpListener.bind(addr);
  if listener is TcpListener {
    var local = await (listener as TcpListener).local_addr();
    var listener_pending_error = await (listener as TcpListener).take_error();
    var listener_nonblocking = await (listener as TcpListener).set_nonblocking(true);
    var listener_ttl = await (listener as TcpListener).ttl();
    var listener_ttl_set = await (listener as TcpListener).set_ttl(64u32);
    var accepted = await (listener as TcpListener).accept();
    var async_listener = AsyncTcpListener.from_listener(listener as TcpListener);
    var async_accept = await async_listener.accept_async();
    var async_local = await async_listener.local_addr();
    var async_pending_error = await async_listener.take_error();
    var async_ttl = await async_listener.ttl();
    var async_ttl_set = await async_listener.set_ttl(64u32);
    var plain_listener = async_listener.into_listener();
  }
  var stream = await TcpStream.connect(addr);
  var timed_stream = await TcpStream.connect_timeout(addr, Duration.from_millis(1));
  if stream is TcpStream {
    var peer = await (stream as TcpStream).peer_addr();
    var stream_pending_error = await (stream as TcpStream).take_error();
    var current = await (stream as TcpStream).nodelay();
    var nodelay = await (stream as TcpStream).set_nodelay(true);
    var stream_nonblocking = await (stream as TcpStream).set_nonblocking(true);
    var stream_read_timeout_set = await (stream as TcpStream).set_read_timeout(Duration.from_millis(1));
    var stream_read_timeout = await (stream as TcpStream).read_timeout();
    var stream_read_timeout_clear = await (stream as TcpStream).set_read_timeout(null);
    var stream_write_timeout_set = await (stream as TcpStream).set_write_timeout(Duration.from_millis(1));
    var stream_write_timeout = await (stream as TcpStream).write_timeout();
    var stream_write_timeout_clear = await (stream as TcpStream).set_write_timeout(null);
    var peeked = await (stream as TcpStream).peek(Bytes.from_str(\"xxxx\"));
    var ttl = await (stream as TcpStream).ttl();
    var ttl_set = await (stream as TcpStream).set_ttl(64u32);
    var async_stream = AsyncTcpStream.from_stream(stream as TcpStream);
    var async_peer = await async_stream.peer_addr();
    var async_read = await async_stream.read_async(Bytes.from_str(\"xxxx\"));
    var async_write = await async_stream.write_async(Bytes.from_str(\"x\"));
    var async_peek = await async_stream.peek_async(Bytes.from_str(\"xxxx\"));
    var async_protocol_read = await read_via_async_protocol(async_stream);
    var async_protocol_write = await write_via_async_protocol(async_stream);
    var plain_stream = async_stream.into_stream();
  }
  var async_connected = await AsyncTcpStream.connect(addr);
  var async_timed_connected = await AsyncTcpStream.connect_timeout(addr, Duration.from_millis(1));
  var async_bound = await AsyncTcpListener.bind(addr);
  if stream is IoError {
    var message = (stream as IoError).message;
  }
  null
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn analysis_accepts_std_net_udp_imports() {
        let src = "\
import { Bytes } from \"std:bytes\";
import { Future } from \"core:prelude\";
import { IoError } from \"std:io\";
import { AsyncUdpSocket, UdpSocket } from \"std:net\";
import { SocketAddr, ip_v4, ip_v6, socket_addr } from \"std:net/types\";
import { Duration } from \"std:time\";
function main(): Future<null> async {
  var addr: SocketAddr = socket_addr(ip_v4(127u8, 0u8, 0u8, 1u8), 0u16);
  var addr6: SocketAddr = socket_addr(ip_v6(0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 1u16), 0u16);
  var socket = await UdpSocket.bind(addr);
  if socket is UdpSocket {
    var local = await (socket as UdpSocket).local_addr();
    var pending_error = await (socket as UdpSocket).take_error();
    var nonblocking = await (socket as UdpSocket).set_nonblocking(true);
    var read_timeout_set = await (socket as UdpSocket).set_read_timeout(Duration.from_millis(1));
    var read_timeout = await (socket as UdpSocket).read_timeout();
    var read_timeout_clear = await (socket as UdpSocket).set_read_timeout(null);
    var write_timeout_set = await (socket as UdpSocket).set_write_timeout(Duration.from_millis(1));
    var write_timeout = await (socket as UdpSocket).write_timeout();
    var write_timeout_clear = await (socket as UdpSocket).set_write_timeout(null);
    var ttl = await (socket as UdpSocket).ttl();
    var ttl_set = await (socket as UdpSocket).set_ttl(64u32);
    var broadcast = await (socket as UdpSocket).broadcast();
    var broadcast_set = await (socket as UdpSocket).set_broadcast(true);
    var loop_v4 = await (socket as UdpSocket).multicast_loop_v4();
    var loop_v4_set = await (socket as UdpSocket).set_multicast_loop_v4(true);
    var multicast_ttl = await (socket as UdpSocket).multicast_ttl_v4();
    var multicast_ttl_set = await (socket as UdpSocket).set_multicast_ttl_v4(32u32);
    var joined = await (socket as UdpSocket).join_multicast_v4(ip_v4(224u8, 0u8, 0u8, 251u8), ip_v4(0u8, 0u8, 0u8, 0u8));
    var left = await (socket as UdpSocket).leave_multicast_v4(ip_v4(224u8, 0u8, 0u8, 251u8), ip_v4(0u8, 0u8, 0u8, 0u8));
    var connected = await (socket as UdpSocket).connect(addr);
    var peer = await (socket as UdpSocket).peer_addr();
    var connected_sent = await (socket as UdpSocket).send(Bytes.from_str(\"x\"));
    var connected_received = await (socket as UdpSocket).recv(Bytes.from_str(\"xxxx\"));
    var connected_peeked = await (socket as UdpSocket).peek(Bytes.from_str(\"xxxx\"));
    var sent = await (socket as UdpSocket).send_to(Bytes.from_str(\"x\"), addr);
    var buf = Bytes.from_str(\"xxxx\");
    var received = await (socket as UdpSocket).recv_from(buf);
    var peeked = await (socket as UdpSocket).peek_from(buf);
    var async_socket = AsyncUdpSocket.from_socket(socket as UdpSocket);
    var async_connected = await async_socket.connect(addr);
    var async_peer = await async_socket.peer_addr();
    var async_sent = await async_socket.send_async(Bytes.from_str(\"x\"));
    var async_received = await async_socket.recv_async(Bytes.from_str(\"xxxx\"));
    var async_peeked = await async_socket.peek_async(Bytes.from_str(\"xxxx\"));
    var async_sent_to = await async_socket.send_to_async(Bytes.from_str(\"x\"), addr);
    var async_received_from = await async_socket.recv_from_async(Bytes.from_str(\"xxxx\"));
    var async_peeked_from = await async_socket.peek_from_async(Bytes.from_str(\"xxxx\"));
    var plain_socket = async_socket.into_socket();
    var closed = await (socket as UdpSocket).close();
  }
  var socket6 = await UdpSocket.bind(addr6);
  if socket6 is UdpSocket {
    var loop_v6 = await (socket6 as UdpSocket).multicast_loop_v6();
    var loop_v6_set = await (socket6 as UdpSocket).set_multicast_loop_v6(true);
    var joined_v6 = await (socket6 as UdpSocket).join_multicast_v6(ip_v6(65282u16, 0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 1u16), 0u32);
    var left_v6 = await (socket6 as UdpSocket).leave_multicast_v6(ip_v6(65282u16, 0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 1u16), 0u32);
  }
  if socket is IoError {
    var message = (socket as IoError).message;
  }
  null
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn analysis_accepts_std_rand_weighted_helpers() {
        let src = "\
import { List } from \"core:collections\";
import { SeededRng, choose_weighted, gen_bates_f64, gen_binomial, gen_irwin_hall_f64, gen_max_f64, gen_median_f64, gen_midrange_f64, gen_min_f64, gen_triangular_f64, sample, sample_indices, shuffled, weighted_index } from \"std:rand\";
function main() {
  var rng = SeededRng.from_seed(7u64);
  var weights = List<u64>();
  weights.push(1u64);
  weights.push(3u64);
  var values = List<i64>();
  values.push(10);
  values.push(20);
  var index = weighted_index(rng, weights);
  var item = choose_weighted(rng, values, weights);
  var successes = gen_binomial(rng, 10, 1, 2);
  var triangular = gen_triangular_f64(rng, 0.0, 1.0);
  var bates = gen_bates_f64(rng, 3, 0.0, 1.0);
  var irwin_hall = gen_irwin_hall_f64(rng, 3, 0.0, 1.0);
  var minimum = gen_min_f64(rng, 3, 0.0, 1.0);
  var maximum = gen_max_f64(rng, 3, 0.0, 1.0);
  var midrange = gen_midrange_f64(rng, 3, 0.0, 1.0);
  var median = gen_median_f64(rng, 3, 0.0, 1.0);
  var indexes = sample_indices(rng, values.size(), 1);
  var picked = sample(rng, values, 1);
  var copy = shuffled(rng, values);
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn analysis_accepts_std_hash_os_keyed_helper() {
        let src = "\
import { Future } from \"core:prelude\";
import { HashSeedError, KeyedHasher, os_keyed_hasher } from \"std:hash\";
function main(): Future<null> async {
  var seeded = await os_keyed_hasher();
  match seeded {
    KeyedHasher => {
      var seed = (seeded as KeyedHasher).seed();
    },
    HashSeedError => {
      var message = (seeded as HashSeedError).message();
    },
  }
  null
}
";
        let c = Compiled::new(src.into());
        assert!(c.diagnostics.is_empty(), "unexpected: {:?}", c.diagnostics);
    }

    #[test]
    fn find_active_call_tracks_param_index() {
        let text = "foo(a, b, c)";
        // Just inside `c` — third parameter (index 2).
        let off = text.find('c').unwrap();
        let (callee_off, active) = find_active_call(text, off + 1).unwrap();
        assert_eq!(active, 2);
        assert_eq!(&text[callee_off..callee_off + 1], "o"); // last char of `foo`
    }

    #[test]
    fn find_active_call_skips_nested_calls() {
        let text = "outer(a, inner(x, y), b";
        // Cursor at end (in the position of `b`) — second arg of outer (index 1).
        let (_, active) = find_active_call(text, text.len()).unwrap();
        assert_eq!(active, 2);
    }

    #[test]
    fn code_lens_targets_main_function() {
        // A program with `main` plus a non-main function — only `main` gets the
        // Run/Build code lenses.
        let src = "\
function helper() {}
function main() { println(\"hi\"); }
";
        let c = Compiled::new(src.into());
        let lenses = collect_code_lenses(&c, "file:///tmp/x.otter");
        let titles: Vec<&str> = lenses
            .iter()
            .map(|l| l.command.as_ref().unwrap().title.as_str())
            .collect();
        assert!(
            titles
                .iter()
                .any(|t| t.contains("Run") && !t.contains("release"))
        );
        assert!(titles.iter().any(|t| t.contains("Run (release)")));
        assert!(titles.iter().any(|t| t.contains("Build")));
        // Every lens anchors at the `main` name occurrence.
        let main_off = src.find("function main").unwrap() + "function ".len();
        let main_line = src[..main_off].matches('\n').count() as u32;
        for l in &lenses {
            assert_eq!(l.range.start.line, main_line);
        }
    }

    #[test]
    fn code_lens_targets_async_main_function() {
        let src = "\
import { Future } from \"core:prelude\";
function main(): Future<null> async { null }
";
        let c = Compiled::new(src.into());
        let lenses = collect_code_lenses(&c, "file:///tmp/x.otter");
        assert!(
            lenses
                .iter()
                .any(|l| l.command.as_ref().unwrap().title.contains("Run")),
            "async main should use the same Run/Build CodeLens path"
        );
        let main_off = src.find("function main").unwrap() + "function ".len();
        let main_line = src[..main_off].matches('\n').count() as u32;
        for l in &lenses {
            assert_eq!(l.range.start.line, main_line);
        }
    }

    #[test]
    fn code_lens_empty_without_main() {
        let c = Compiled::new("function helper() {}\n".into());
        let lenses = collect_code_lenses(&c, "file:///tmp/x.otter");
        assert!(lenses.is_empty());
    }

    #[test]
    fn default_completion_includes_locals_builtins_and_keywords_without_stdio_helpers() {
        let src = "\
function greet(name: str): str { name }
function main() {
  var who = \"world\";
}
";
        let c = Compiled::new(src.into());
        let items = default_completions(&c);
        let names = labels(&items);
        assert!(names.contains(&"greet"));
        assert!(names.contains(&"who"));
        assert!(names.contains(&"panic"));
        assert!(names.contains(&"function"));
        assert!(!names.contains(&"print"));
        assert!(!names.contains(&"println"));
        // Each label appears only once even when several sources contribute it.
        let count = names.iter().filter(|n| **n == "greet").count();
        assert_eq!(count, 1);
    }
}
