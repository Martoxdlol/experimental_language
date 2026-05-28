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
    builtin_signature, keyword_texts, offset_at, span_to_range, Compiled, LineIndex, TokenClass,
    DOC_FILE,
};

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

    /// Compile a document's current text, if it is open.
    fn compile(&self, uri: &Url) -> Option<Compiled> {
        let text = self.documents.get(uri)?.clone();
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
                source: Some("lang".into()),
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
                name: "lang-lsp".into(),
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
            .log_message(MessageType::INFO, "lang language server ready")
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
                        .analysis
                        .results
                        .local_decls
                        .get(&id)
                        .map(|s| c.map.slice(*s).to_string())
                        .unwrap_or_else(|| c.map.slice(span).to_string());
                    let ty = c
                        .analysis
                        .results
                        .local_types
                        .get(&id)
                        .map(|t| c.display_ty(*t))
                        .unwrap_or_else(|| "?".into());
                    lines.push(format!("```lang\n{name}: {ty}\n```"));
                }
                ValueRes::Function(d)
                | ValueRes::Method(d)
                | ValueRes::Global(d)
                | ValueRes::StructCtor(d) => {
                    let def = c.analysis.program.def(d);
                    lines.push(format!("```lang\n{}\n```", c.def_label(def)));
                    if let Some(ret) = c.analysis.results.fn_return.get(&d) {
                        lines.push(format!("returns `{}`", c.display_ty(*ret)));
                    }
                }
                ValueRes::Builtin(b) => {
                    lines.push(format!("```lang\n{}\n```", builtin_signature(b)));
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
        let loc = Location { uri, range: span_to_range(&c.text, def_span) };
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
        let mut spans: Vec<Span> = c
            .analysis
            .results
            .resolutions
            .iter()
            .filter(|(s, r)| **r == target && s.file == DOC_FILE)
            .map(|(s, _)| *s)
            .collect();

        // The declaration span of a local is itself in `resolutions` (bind()
        // records it), so locals are already covered. For def targets, add the
        // definition's name span if requested and in-file.
        if include_decl {
            if let Some(dspan) = c.definition_span(target) {
                if !spans.contains(&dspan) {
                    spans.push(dspan);
                }
            }
        } else {
            // Exclude the binding occurrence of a local.
            if let ValueRes::Local(id) = target {
                if let Some(dspan) = c.analysis.results.local_decls.get(&id) {
                    spans.retain(|s| s != dspan);
                }
            }
        }

        spans.sort_by_key(|s| (s.lo.0, s.hi.0));
        spans.dedup();
        let locs = spans
            .into_iter()
            .map(|s| Location { uri: uri.clone(), range: span_to_range(&c.text, s) })
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

        // Collect every use site plus the declaration.
        let mut spans: HashSet<Span> = c
            .analysis
            .results
            .resolutions
            .iter()
            .filter(|(s, r)| **r == target && s.file == DOC_FILE)
            .map(|(s, _)| *s)
            .collect();
        if let Some(dspan) = c.definition_span(target) {
            spans.insert(dspan);
        }
        if spans.is_empty() {
            return Ok(None);
        }

        let edits: Vec<TextEdit> = spans
            .into_iter()
            .map(|s| TextEdit {
                range: span_to_range(&c.text, s),
                new_text: params.new_name.clone(),
            })
            .collect();
        let mut changes = std::collections::HashMap::new();
        changes.insert(uri, edits);
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

        let mut items: Vec<CompletionItem> = Vec::new();

        // Keywords.
        for kw in keyword_texts() {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
        // Builtins.
        for b in ["print", "println", "panic", "panic_with", "exit", "abort"] {
            items.push(CompletionItem {
                label: b.into(),
                kind: Some(CompletionItemKind::FUNCTION),
                ..Default::default()
            });
        }
        // Declared top-level types and functions.
        let (types, fns) = c.declared_names();
        for (name, class) in &types {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(match class {
                    TokenClass::Interface => CompletionItemKind::INTERFACE,
                    _ => CompletionItemKind::STRUCT,
                }),
                ..Default::default()
            });
        }
        for name in &fns {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                ..Default::default()
            });
        }
        // In-scope local names (over-approximated across the file).
        let mut locals: HashSet<String> = HashSet::new();
        for s in c.analysis.results.local_decls.values() {
            if s.file == DOC_FILE {
                locals.insert(c.map.slice(*s).to_string());
            }
        }
        for name in locals {
            items.push(CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::VARIABLE),
                ..Default::default()
            });
        }

        Ok(Some(CompletionResponse::Array(items)))
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
}
