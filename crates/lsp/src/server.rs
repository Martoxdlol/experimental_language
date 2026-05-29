//! The language server: capabilities, document lifecycle, and one handler per
//! LSP feature. Each handler recompiles the open document (the front-end is
//! fast and side-effect-free) and answers from the resulting [`Compiled`].

use std::collections::HashSet;

use compiler::ast::{ExternItem, ItemKind, ModuleKind, StructKind};
use compiler::sema::ValueRes;
use compiler::span::Span;
use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{
    builtin_signature, dot_completion_context, float_instance_methods, float_static_methods,
    int_instance_methods, keyword_texts, list_intrinsic_methods, map_intrinsic_methods,
    offset_at, primitive_static_methods, span_to_range, str_intrinsic_methods, Compiled,
    LineIndex, TokenClass, DOC_FILE,
};
use compiler::sema::symbols::DefKind;
use compiler::ty::TyKind;

/// Map any analysis [`Span`] to an editor [`Location`]: the open document, a
/// loaded submodule file (resolved through the `SourceMap`), or `None` for a
/// virtual file (prelude / synthesized code, beyond the real file count). Shared
/// by go-to-definition and cross-file references/rename.
fn span_to_location(c: &Compiled, span: Span, doc_uri: &Url) -> Option<Location> {
    if span.file == DOC_FILE {
        return Some(Location { uri: doc_uri.clone(), range: span_to_range(&c.text, span) });
    }
    if (span.file.0 as usize) < c.map.file_count() {
        let sf = c.map.file(span.file);
        if let Ok(u) = Url::from_file_path(&sf.name) {
            return Some(Location { uri: u, range: span_to_range(&sf.src, span) });
        }
    }
    None
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
}

impl Backend {
    pub fn new(client: Client) -> Backend {
        Backend { client, documents: DashMap::new() }
    }

    /// Compile a document's current text, if it is open. When the document has
    /// a filesystem path, its file-backed submodules are loaded too (preferring
    /// open editor buffers over disk) so cross-module imports resolve.
    fn compile(&self, uri: &Url) -> Option<Compiled> {
        let text = self.documents.get(uri)?.clone();
        if let Ok(path) = uri.to_file_path() {
            if let (Some(parent), Some(stem)) =
                (path.parent(), path.file_stem().and_then(|s| s.to_str()))
            {
                let docs = &self.documents;
                let read = |p: &std::path::Path| -> Option<String> {
                    if let Ok(u) = Url::from_file_path(p) {
                        if let Some(t) = docs.get(&u) {
                            return Some(t.clone());
                        }
                    }
                    std::fs::read_to_string(p).ok()
                };
                return Some(Compiled::new_multi(
                    text,
                    parent.to_path_buf(),
                    stem.to_string(),
                    &read,
                ));
            }
        }
        Some(Compiled::new(text))
    }

    /// Recompile and publish diagnostics for `uri`.
    async fn publish(&self, uri: Url, version: Option<i32>) {
        let Some(c) = self.compile(&uri) else { return };
        let diags = c
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
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into()]),
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
        self.publish(doc.uri, Some(doc.version)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last change carries the entire new document text.
        if let Some(change) = params.content_changes.into_iter().last() {
            self.documents
                .insert(params.text_document.uri.clone(), change.text);
        }
        self.publish(params.text_document.uri, Some(params.text_document.version))
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
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
                    lines.push(format!("```otter-fusion\n{}\n```", c.def_label(def)));
                    if let Some(ret) = c.analysis.hir.fn_sigs.get(&d).map(|s| s.ret) {
                        lines.push(format!("returns `{}`", c.display_ty(ret)));
                    }
                }
                ValueRes::Builtin(b) => {
                    lines.push(format!("```otter-fusion\n{}\n```", builtin_signature(b)));
                    lines.push("builtin function".into());
                }
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
        let Some((_, res)) = c.resolution_at(off) else {
            return Ok(None);
        };
        let Some(def_span) = c.definition_span(res) else {
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

        spans.sort_by_key(|s| (s.file.0, s.lo.0, s.hi.0));
        spans.dedup();
        // Map each use site to its own file's location (a virtual-file span — a
        // synthesized node — has no editor location and is dropped).
        let locs = spans
            .into_iter()
            .filter_map(|s| span_to_location(&c, s, &uri))
            .collect();
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
        if spans.is_empty() {
            return Ok(None);
        }

        // Group edits by the file (URI) each span belongs to, so a cross-module
        // rename updates every affected document in one `WorkspaceEdit`.
        let mut changes: std::collections::HashMap<Url, Vec<TextEdit>> = std::collections::HashMap::new();
        for s in spans {
            if let Some(loc) = span_to_location(&c, s, &uri) {
                changes.entry(loc.uri).or_default().push(TextEdit {
                    range: loc.range,
                    new_text: params.new_name.clone(),
                });
            }
        }
        if changes.is_empty() {
            return Ok(None);
        }
        Ok(Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }))
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
            return Ok(Some(CompletionResponse::Array(member_completions(&c, &ctx))));
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

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let pos = params.text_document_position_params;
        let Some(c) = self.compile(&pos.text_document.uri) else {
            return Ok(None);
        };
        let off = offset_at(&c.text, pos.position);
        let Some((callee_off, active)) = find_active_call(&c.text, off) else {
            return Ok(None);
        };
        let Some((_, res)) = c.resolution_at(callee_off) else {
            return Ok(None);
        };
        let sig = match res {
            compiler::sema::ValueRes::Function(d)
            | compiler::sema::ValueRes::Method(d)
            | compiler::sema::ValueRes::StructCtor(d) => {
                build_signature_info(&c, c.analysis.program.def(d))
            }
            compiler::sema::ValueRes::Builtin(b) => Some(builtin_signature_info(b)),
            _ => None,
        };
        let Some(sig) = sig else { return Ok(None) };
        let active = active.min(sig.parameters.as_ref().map_or(0, |p| p.len().saturating_sub(1)) as u32);
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
    let mk = |name: String, detail: Option<String>, kind: SymbolKind, span: Span, sel: Span, children: Vec<DocumentSymbol>| {
        DocumentSymbol {
            name,
            detail,
            kind,
            tags: None,
            deprecated: None,
            range: span_to_range(text, span),
            selection_range: span_to_range(text, sel),
            children: if children.is_empty() { None } else { Some(children) },
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
                out.push(mk(m.name.name.clone(), None, kind, item.span, m.name.span, vec![]));
            }
            ItemKind::Extern(ext) => {
                let (name, sel, kind) = match ext {
                    ExternItem::Function(f) => (f.name.name.clone(), f.name.span, SymbolKind::FUNCTION),
                    ExternItem::Struct(s) => (s.name.name.clone(), s.name.span, SymbolKind::STRUCT),
                    ExternItem::OpaqueType(n) => (n.name.clone(), n.span, SymbolKind::TYPE_PARAMETER),
                    ExternItem::Var { name, .. } => (name.name.clone(), name.span, SymbolKind::VARIABLE),
                };
                out.push(mk(name, None, kind, item.span, sel, vec![]));
            }
            ItemKind::Import(_) => {}
        }
    }
    out
}

/// The completion set when the cursor is *not* after a `.` — keywords,
/// builtins, declared top-level items, and locals visible anywhere in the
/// file (a single-file LSP cannot do precise lexical scoping yet).
fn default_completions(c: &Compiled) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let push = |items: &mut Vec<CompletionItem>,
                seen: &mut HashSet<String>,
                item: CompletionItem| {
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
            .map(|d| c.def_signature(c.analysis.program.def(d)));
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

    // 3. Builtins.
    let builtins: &[(&str, &str)] = &[
        ("print", "(str)"),
        ("println", "(str)"),
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
fn member_completions(
    c: &Compiled,
    ctx: &crate::analysis::DotContext,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |items: &mut Vec<CompletionItem>,
                    seen: &mut HashSet<String>,
                    item: CompletionItem| {
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
    if let TyKind::Named { def, .. } = kind {
        let d = prog.def(def);
        match d.kind {
            DefKind::Struct | DefKind::ExternStruct => {
                for f in c.struct_fields(def) {
                    push(
                        items,
                        seen,
                        CompletionItem {
                            label: f.name.clone(),
                            kind: Some(CompletionItemKind::FIELD),
                            sort_text: Some("1".into()),
                            ..Default::default()
                        },
                    );
                }
            }
            DefKind::Interface => {
                for m in c.interface_methods(def, false) {
                    push(items, seen, def_to_completion(c, m, CompletionItemKind::METHOD));
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

        // Any user `extend` block whose target's head name matches this type's
        // name contributes instance methods.
        let type_name = d.name.clone();
        for m in c.extend_methods_for(&type_name, false) {
            push(items, seen, def_to_completion(c, m, CompletionItemKind::METHOD));
        }
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
            push(items, seen, intrinsic_completion(n, sig, CompletionItemKind::FUNCTION));
        }
        return;
    }
    if FLOAT_NAMES.contains(&name) {
        for (n, sig) in float_static_methods() {
            push(items, seen, intrinsic_completion(n, sig, CompletionItemKind::CONSTANT));
        }
        return;
    }

    // User type namespace: static methods on `extend Type`.
    if let Some(_def) = c.lookup_type_def(name) {
        for m in c.extend_methods_for(name, true) {
            push(items, seen, def_to_completion(c, m, CompletionItemKind::FUNCTION));
        }
    }
}

fn intrinsic_completion(
    name: &str,
    signature: &str,
    kind: CompletionItemKind,
) -> CompletionItem {
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
        detail: Some(c.def_signature(def)),
        sort_text: Some("1".into()),
        ..Default::default()
    }
}

/// Build the Run/Build CodeLenses for a compiled document. Emits lenses above
/// every top-level `function main` (sync or async — the runtime resolves async
/// main through `block_on`, so the same CLI invocation works).
fn collect_code_lenses(c: &Compiled, uri: &str) -> Vec<CodeLens> {
    let mut lenses = Vec::new();
    for item in &c.module.items {
        let ItemKind::Function(f) = &item.kind else { continue };
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
    def: &compiler::sema::symbols::Def,
) -> Option<SignatureInformation> {
    use compiler::ast::ParamKind;
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

fn builtin_signature_info(b: compiler::sema::Builtin) -> SignatureInformation {
    use compiler::sema::Builtin;
    let (label, parts): (&str, &[(&str, &str)]) = match b {
        Builtin::Print => ("print(value: str)", &[("value", "str")]),
        Builtin::Println => ("println(value: str)", &[("value", "str")]),
        Builtin::Panic => ("panic(message: str): never", &[("message", "str")]),
        Builtin::PanicWith => (
            "panic_with(value: dynamic): never",
            &[("value", "dynamic")],
        ),
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

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
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
        let titles: Vec<&str> = lenses.iter().map(|l| l.command.as_ref().unwrap().title.as_str()).collect();
        assert!(titles.iter().any(|t| t.contains("Run") && !t.contains("release")));
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
    fn code_lens_empty_without_main() {
        let c = Compiled::new("function helper() {}\n".into());
        let lenses = collect_code_lenses(&c, "file:///tmp/x.otter");
        assert!(lenses.is_empty());
    }

    #[test]
    fn default_completion_includes_locals_and_keywords() {
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
        assert!(names.contains(&"println"));
        assert!(names.contains(&"function"));
        // Each label appears only once even when several sources contribute it.
        let count = names.iter().filter(|n| **n == "greet").count();
        assert_eq!(count, 1);
    }
}
