//! Lowering `AST + CheckResults → HIR` (migration Stage 2).
//!
//! This pass walks the checked AST and the checker's span-keyed side tables and
//! produces the typed [`Hir`] tree. It is **lossless**: every fact the side
//! tables recorded becomes a field on the corresponding HIR node, and every
//! node preserves its source [`Span`]. The checker is untouched; this is an
//! additive translation so the existing pipeline (which still consumes
//! `(ast, CheckResults)`) is unaffected until codegen is repointed (Stage 3).
//!
//! The dispatch decisions here mirror exactly what the backend used to make by
//! re-reading the tables (see `crates/backend/src/gen_call.rs`): the call
//! classification precedence, the operator-overload / coercion / intrinsic
//! folding, and the `for`-driver selection. Monomorphization stays at the
//! HIR→codegen boundary, so generic type arguments are recorded *unresolved*
//! (they may contain `Param`s); codegen substitutes per instance.

use crate::ast;
use crate::ids::{DefId, LocalId, ModId};
use crate::sema::results::ValueRes;
use crate::sema::symbols::DefKind;
use crate::sema::Analysis;
use crate::span::Span;
use crate::ty::Ty;
use std::collections::HashMap;

use super::*;

/// The libraries named by `@Link(lib = "…")` / `@Link("…")` on `extern function`
/// declarations (`docs/19` §13), de-duplicated in first-seen order. Derived
/// straight from the program's attributes — no checker side table. Consumed as
/// [`Hir::link_libs`] (JIT `dlopen`) and by the CLI's native linker (`-l`).
pub fn collect_link_libs(analysis: &Analysis) -> Vec<String> {
    let mut libs: Vec<String> = Vec::new();
    for def in &analysis.program.defs {
        if !matches!(def.item, Some(ast::ItemKind::Extern(ast::ExternItem::Function(_)))) {
            continue;
        }
        for attr in &def.attrs {
            if attr.name.name != "Link" {
                continue;
            }
            for a in &attr.args {
                let value = match a {
                    ast::AttrArg::Named { name, value, .. } if name.name == "lib" => value,
                    ast::AttrArg::Positional(e) => e,
                    _ => continue,
                };
                if let ast::ExprKind::Str(s) = &value.kind {
                    // A library name is a plain (non-interpolated) string literal.
                    let lib = match s.parts.as_slice() {
                        [] => Some(String::new()),
                        [ast::StringPart::Text { text, .. }] => Some(text.clone()),
                        _ => None,
                    };
                    if let Some(lib) = lib {
                        if !lib.is_empty() && !libs.contains(&lib) {
                            libs.push(lib);
                        }
                    }
                }
            }
        }
    }
    libs
}


/// Lower a fully analysed program into HIR.
pub fn lower_program(analysis: &Analysis) -> Hir {
    let span2def = build_span_index(analysis);
    let mut hir = Hir::new();

    // Program-level (def-keyed) facts copy across directly.
    hir.structs = analysis.results.struct_fields.clone();
    hir.iface_impls = analysis.results.iface_impls.clone();
    hir.link_libs = collect_link_libs(analysis);
    hir.local_decls = analysis.results.local_decls.clone();
    // Extern C-ABI signatures are already the HIR `ExternSig` (checker-built).
    hir.extern_sigs = analysis.results.extern_sigs.clone();

    // Signatures: the checker already built every function/method's `FnSig`
    // directly (Stage 5), so they copy across without rebuilding from side
    // tables.
    hir.fn_sigs = analysis.results.fn_sigs.clone();

    // Bodies for every function / extend method that has one.
    for id in 0..analysis.program.defs.len() {
        let def = DefId(id as u32);
        let d = analysis.program.def(def);
        if !matches!(d.kind, DefKind::Function | DefKind::ExtendMethod) {
            continue;
        }
        let Some(ast::ItemKind::Function(f)) = &d.item else { continue };
        let Some(body) = &f.body else { continue };
        let sig = analysis.results.fn_sigs.get(&def);
        let params: Vec<LocalId> =
            sig.map(|s| s.params.iter().map(|(l, _)| *l).collect()).unwrap_or_default();
        let ret = sig.map(|s| s.ret).unwrap_or(analysis.tcx.null);
        let async_output = sig.and_then(|s| s.async_output);
        let mut bl = BodyLower {
            a: analysis,
            span2def: &span2def,
            module: d.module,
            locals: HashMap::new(),
        };
        // Pre-record parameter locals so their types are present even if a
        // parameter is never referenced in the body.
        for &l in &params {
            bl.record_local(l);
        }
        // Stage 5: the checker emits the whole body's HIR `Block` directly; use
        // it (recording its locals into `Body.locals`). Fall back to lowering the
        // block here only for an error program where the checker could not build
        // it (the same graceful recovery `lower_expr` uses for unbuilt nodes).
        let block = match analysis.results.fn_bodies.get(&def) {
            Some(b) => {
                bl.record_block_locals(b);
                b.clone()
            }
            None => bl.lower_block(body),
        };
        hir.bodies.insert(
            def,
            Body { def, params, locals: bl.locals, ret, async_output, block, span: body.span },
        );
    }

    hir
}

/// Map every item def's span to its [`DefId`], so block-level `Item` statements
/// can be re-linked to their definition (whose body lives in [`Hir::bodies`]).
fn build_span_index(analysis: &Analysis) -> HashMap<Span, DefId> {
    let mut m = HashMap::new();
    for (i, d) in analysis.program.defs.iter().enumerate() {
        m.entry(d.span).or_insert(DefId(i as u32));
    }
    m
}

/// Per-body lowering state.
struct BodyLower<'a> {
    a: &'a Analysis,
    span2def: &'a HashMap<Span, DefId>,
    module: ModId,
    /// Every local referenced/bound in this body, with its type.
    locals: HashMap<LocalId, Ty>,
}

impl<'a> BodyLower<'a> {
    // -- small helpers -------------------------------------------------------

    fn expr_ty(&self, span: Span) -> Ty {
        self.a.results.expr_ty(span).unwrap_or(self.a.tcx.error)
    }

    /// Note a local's type into the body's local map (idempotent).
    fn record_local(&mut self, id: LocalId) {
        if let Some(ty) = self.a.results.local_ty(id) {
            self.locals.insert(id, ty);
        } else {
            self.locals.entry(id).or_insert(self.a.tcx.error);
        }
    }

    /// Record every local referenced by a checker-built HIR node (and its
    /// subtree) into `Body.locals`. Lowering normally records a local as a side
    /// effect of resolving its `Name` (`res_at`); when a node comes prebuilt
    /// from `node_hir` that resolution is skipped, so we recover the locals by
    /// walking the node here.
    fn record_node_locals(&mut self, e: &Expr) {
        use ExprKind as K;
        if let K::Name(ValueRes::Local(id)) = &e.kind {
            self.record_local(*id);
        }
        match &e.kind {
            K::Tuple(xs) | K::List(xs) => xs.iter().for_each(|x| self.record_node_locals(x)),
            K::Unary { operand, .. } => self.record_node_locals(operand),
            K::Binary { left, right, .. } => {
                self.record_node_locals(left);
                self.record_node_locals(right);
            }
            K::Cast { expr, .. }
            | K::Ref(expr)
            | K::Deref(expr)
            | K::Adjust { expr, .. }
            | K::Try { expr, .. }
            | K::Await { expr, .. }
            | K::Spawn { expr, .. }
            | K::Field { receiver: expr, .. }
            | K::TupleIndex { receiver: expr, .. } => self.record_node_locals(expr),
            K::Index { receiver, index } => {
                self.record_node_locals(receiver);
                self.record_node_locals(index);
            }
            K::Return(v) | K::Break(v) => {
                if let Some(e) = v {
                    self.record_node_locals(e);
                }
            }
            K::Call { args, kind, .. } => {
                if let CallKind::Closure { callee } = kind {
                    self.record_node_locals(callee);
                }
                args.iter().for_each(|a| self.record_node_locals(a));
            }
            K::Intrinsic { args, .. } => args.iter().for_each(|a| self.record_node_locals(a)),
            K::Struct { fields, spread, .. } => {
                fields.iter().for_each(|f| self.record_node_locals(&f.value));
                if let Some(s) = spread {
                    self.record_node_locals(s);
                }
            }
            K::Str(parts) => parts.iter().for_each(|p| {
                if let StrPart::Interp { expr, .. } = p {
                    self.record_node_locals(expr);
                }
            }),
            K::Map(items) => items.iter().for_each(|it| match it {
                MapEntry::Kv { key, value } => {
                    self.record_node_locals(key);
                    self.record_node_locals(value);
                }
                MapEntry::Spread(e) => self.record_node_locals(e),
            }),
            K::If { cond, then_block, else_branch } => {
                self.record_node_locals(cond);
                self.record_block_locals(then_block);
                if let Some(e) = else_branch {
                    self.record_node_locals(e);
                }
            }
            K::Match { scrutinee, arms } => {
                self.record_node_locals(scrutinee);
                for a in arms {
                    self.record_pattern_locals(&a.pattern);
                    if let Some(g) = &a.guard {
                        self.record_node_locals(g);
                    }
                    self.record_node_locals(&a.body);
                }
            }
            K::Block(b) | K::Loop(b) => self.record_block_locals(b),
            K::While { cond, body } => {
                self.record_node_locals(cond);
                self.record_block_locals(body);
            }
            K::For { pattern, iter, body, .. } => {
                self.record_pattern_locals(pattern);
                self.record_node_locals(iter);
                self.record_block_locals(body);
            }
            K::Closure { params, captures, body, .. } => {
                for (id, _) in params.iter().chain(captures) {
                    self.record_local(*id);
                }
                self.record_node_locals(body);
            }
            K::AsyncBlock { params, captures, body, .. } => {
                for (id, _) in params.iter().chain(captures) {
                    self.record_local(*id);
                }
                self.record_block_locals(body);
            }
            // Leaves and not-yet-migrated kinds (the latter never reach
            // `node_hir`, so they cannot appear as a prebuilt subtree here).
            _ => {}
        }
    }

    /// Record every local bound or referenced inside a checker-built block (its
    /// statements + trailing expression), mirroring the recording lowering would
    /// otherwise perform while constructing the block.
    fn record_block_locals(&mut self, b: &Block) {
        for s in &b.stmts {
            match &s.kind {
                StmtKind::Let { pattern, init } => {
                    self.record_pattern_locals(pattern);
                    self.record_node_locals(init);
                }
                StmtKind::Assign { target, value } => {
                    self.record_node_locals(target);
                    self.record_node_locals(value);
                }
                StmtKind::Expr(e) => self.record_node_locals(e),
                StmtKind::Item(_) => {}
            }
        }
        if let Some(t) = &b.trailing {
            self.record_node_locals(t);
        }
    }

    /// Record every local bound by a checker-built pattern subtree.
    fn record_pattern_locals(&mut self, p: &Pattern) {
        use PatternKind as P;
        match &p.kind {
            P::Bind(id) => self.record_local(*id),
            P::TypeBind { bind, .. } => {
                if let Some(id) = bind {
                    self.record_local(*id);
                }
            }
            P::Literal(e) => self.record_node_locals(e),
            P::TupleStruct { fields, rest, .. } => {
                fields.iter().for_each(|f| self.record_pattern_locals(f));
                if let Some(r) = rest {
                    if let Some(id) = r.bind {
                        self.record_local(id);
                    }
                }
            }
            P::RecordStruct { fields, .. } => {
                fields.iter().for_each(|f| self.record_pattern_locals(&f.pattern))
            }
            P::Tuple { elems, rest } | P::List { elems, rest } => {
                elems.iter().for_each(|e| self.record_pattern_locals(e));
                if let Some((_, r)) = rest {
                    if let Some(id) = r.bind {
                        self.record_local(id);
                    }
                }
            }
            P::Or(ps) => ps.iter().for_each(|p| self.record_pattern_locals(p)),
            P::Wildcard | P::UnitPath { .. } => {}
        }
    }


    /// The local bound at a binding-occurrence `span`.
    fn local_at(&mut self, span: Span) -> LocalId {
        match self.a.results.resolution(span) {
            Some(ValueRes::Local(id)) => {
                self.record_local(id);
                id
            }
            // A binding always resolves to a local; if not, fall back to a
            // sentinel so lowering stays total (never reached on valid input).
            _ => LocalId(u32::MAX),
        }
    }

    // -- blocks & statements -------------------------------------------------

    fn lower_block(&mut self, b: &ast::Block) -> Block {
        let stmts = b.stmts.iter().filter_map(|s| self.lower_stmt(s)).collect();
        let trailing = b.trailing.as_ref().map(|e| Box::new(self.lower_expr(e)));
        let ty = b
            .trailing
            .as_ref()
            .map(|e| self.expr_ty(e.span))
            .unwrap_or(self.a.tcx.null);
        Block { stmts, trailing, ty, span: b.span }
    }

    fn lower_stmt(&mut self, s: &ast::Stmt) -> Option<Stmt> {
        let kind = match &s.kind {
            ast::StmtKind::Var(lv) => StmtKind::Let {
                pattern: self.lower_pattern(&lv.pattern),
                init: self.lower_expr(&lv.init),
            },
            ast::StmtKind::Assign { target, value } => StmtKind::Assign {
                target: self.lower_expr(target),
                value: self.lower_expr(value),
            },
            ast::StmtKind::Expr(e) => StmtKind::Expr(self.lower_expr(e)),
            ast::StmtKind::Item(item) => match self.span2def.get(&item.span) {
                Some(&d) => StmtKind::Item(d),
                // No def for this item span (should not happen); drop it — a
                // block-level item has no runtime statement effect anyway.
                None => return None,
            },
        };
        Some(Stmt { kind, span: s.span })
    }

    // -- expressions ---------------------------------------------------------

    /// Lower an expression — the checker already built its HIR node (with any
    /// coercion baked in) into `node_hir`, so this just clones it (recording its
    /// locals). The `Error` fallback is reached only for an expression the
    /// checker could not resolve in an already-erroring program (e.g. the LSP
    /// analysing half-typed code).
    fn lower_expr(&mut self, e: &ast::Expr) -> Expr {
        // Parentheses are pure grouping — transparent in the HIR.
        if let ast::ExprKind::Paren(inner) = &e.kind {
            return self.lower_expr(inner);
        }
        match self.a.results.node_hir.get(&e.span) {
            Some(h) => {
                let h = h.clone();
                // `res_at`'s local-recording side effect is skipped for prebuilt
                // nodes; recover it by walking the node.
                self.record_node_locals(&h);
                h
            }
            None => Expr { kind: ExprKind::Error, ty: self.expr_ty(e.span), span: e.span },
        }
    }

    fn field_index(&self, def: DefId, name: &str) -> u32 {
        match self.a.results.struct_fields.get(&def) {
            Some(StructFields::Record(fs)) => {
                fs.iter().position(|(n, _)| n == name).unwrap_or(0) as u32
            }
            Some(StructFields::Tuple(_)) => name.parse().unwrap_or(0),
            _ => name.parse().unwrap_or(0),
        }
    }

    fn pat_ty(&self, span: Span) -> Ty {
        // Error-recovery fallback only (the checker builds bodies for well-formed
        // programs); a matched-variant test type isn't recovered here.
        self.a.results.expr_ty(span).unwrap_or(self.a.tcx.error)
    }

    fn path_def(&self, path: &ast::TypePath) -> DefId {
        self.a
            .program
            .resolve_type_in(self.module, &path.name.name)
            .unwrap_or(DefId(0))
    }

    fn lower_rest(&mut self, r: &ast::RestPattern) -> RestPattern {
        RestPattern {
            bind: r.name.as_ref().map(|i| self.local_at(i.span)),
            span: r.span,
        }
    }

    fn lower_pattern(&mut self, p: &ast::Pattern) -> Pattern {
        let ty = self.pat_ty(p.span);
        let kind = match &p.kind {
            ast::PatternKind::Wildcard => PatternKind::Wildcard,
            ast::PatternKind::Binding(id) => PatternKind::Bind(self.local_at(id.span)),
            ast::PatternKind::Literal(e) => PatternKind::Literal(Box::new(self.lower_expr(e))),
            ast::PatternKind::TypeBinding { binding, .. } => PatternKind::TypeBind {
                test_ty: self.pat_ty(p.span),
                bind: binding.as_ref().map(|i| self.local_at(i.span)),
            },
            ast::PatternKind::UnitPath(tp) => {
                PatternKind::UnitPath { def: self.path_def(tp), test_ty: self.pat_ty(p.span) }
            }
            ast::PatternKind::TupleStruct { path, fields, rest } => PatternKind::TupleStruct {
                def: self.path_def(path),
                fields: fields.iter().map(|f| self.lower_pattern(f)).collect(),
                rest: rest.as_ref().map(|r| self.lower_rest(r)),
            },
            ast::PatternKind::RecordStruct { path, fields, has_rest } => {
                let def = self.path_def(path);
                PatternKind::RecordStruct {
                    def,
                    fields: fields.iter().map(|f| self.lower_field_pattern(def, f)).collect(),
                    has_rest: *has_rest,
                }
            }
            ast::PatternKind::Tuple { elems, rest } => PatternKind::Tuple {
                elems: elems.iter().map(|e| self.lower_pattern(e)).collect(),
                rest: rest.as_ref().map(|(i, r)| (*i, self.lower_rest(r))),
            },
            ast::PatternKind::List { elems, rest } => PatternKind::List {
                elems: elems.iter().map(|e| self.lower_pattern(e)).collect(),
                rest: rest.as_ref().map(|(i, r)| (*i, self.lower_rest(r))),
            },
            ast::PatternKind::Or(ps) => {
                PatternKind::Or(ps.iter().map(|p| self.lower_pattern(p)).collect())
            }
        };
        Pattern { kind, ty, span: p.span }
    }

    fn lower_field_pattern(&mut self, def: DefId, f: &ast::FieldPattern) -> FieldPattern {
        let pattern = match &f.pattern {
            Some(p) => self.lower_pattern(p),
            // Shorthand `Person { name }` binds the field name as a local.
            None => Pattern {
                kind: PatternKind::Bind(self.local_at(f.name.span)),
                ty: self.expr_ty(f.name.span),
                span: f.name.span,
            },
        };
        FieldPattern {
            index: self.field_index(def, &f.name.name),
            name: f.name.name.clone(),
            pattern,
            span: f.span,
        }
    }
}

// ===========================================================================
// Literal parsing (mirrors the backend's parsers so values match exactly)
// ===========================================================================

pub(crate) fn parse_int_lit(lit: &ast::IntLit) -> u128 {
    let digits: String = lit.raw.chars().filter(|c| *c != '_').collect();
    let radix = match lit.base {
        crate::token::IntBase::Dec => 10,
        crate::token::IntBase::Hex => 16,
        crate::token::IntBase::Oct => 8,
        crate::token::IntBase::Bin => 2,
    };
    u128::from_str_radix(&digits, radix).unwrap_or(0)
}

pub(crate) fn parse_float_lit(lit: &ast::FloatLit) -> f64 {
    let raw: String = lit.raw.chars().filter(|c| *c != '_').collect();
    raw.parse().unwrap_or(0.0)
}

/// Parse a char literal to its Unicode scalar value, mirroring the backend's
/// `parse_char` so HIR and codegen agree byte-for-byte.
pub(crate) fn parse_char_lit(raw: &str) -> Option<u32> {
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first != '\\' {
        return if chars.next().is_none() { Some(first as u32) } else { None };
    }
    let esc = chars.next()?;
    let val = match esc {
        'n' => '\n' as u32,
        'r' => '\r' as u32,
        't' => '\t' as u32,
        '\\' => '\\' as u32,
        '\'' => '\'' as u32,
        '"' => '"' as u32,
        '0' => 0,
        'u' => {
            let rest: String = chars.collect();
            let hex = rest.strip_prefix('{')?.strip_suffix('}')?;
            return u32::from_str_radix(hex, 16).ok();
        }
        _ => return None,
    };
    if chars.next().is_none() { Some(val) } else { None }
}

// ===========================================================================
// Operator lowering (AST spelling → HIR spelling)
// ===========================================================================

pub(crate) fn lower_unop(op: ast::UnaryOp) -> UnaryOp {
    match op {
        ast::UnaryOp::Neg => UnaryOp::Neg,
        // `!` and `~` are both the bitwise/logical complement the backend folds
        // into a single `Not` (it picks logical vs bitwise by operand type).
        ast::UnaryOp::Not | ast::UnaryOp::BitNot => UnaryOp::Not,
    }
}

pub(crate) fn lower_binop(op: ast::BinaryOp) -> BinaryOp {
    use ast::BinaryOp as A;
    match op {
        A::Add => BinaryOp::Add,
        A::Sub => BinaryOp::Sub,
        A::Mul => BinaryOp::Mul,
        A::Div => BinaryOp::Div,
        A::Rem => BinaryOp::Rem,
        A::Eq => BinaryOp::Eq,
        A::Ne => BinaryOp::Ne,
        A::Lt => BinaryOp::Lt,
        A::Le => BinaryOp::Le,
        A::Gt => BinaryOp::Gt,
        A::Ge => BinaryOp::Ge,
        A::And => BinaryOp::And,
        A::Or => BinaryOp::Or,
        A::BitAnd => BinaryOp::BitAnd,
        A::BitOr => BinaryOp::BitOr,
        A::BitXor => BinaryOp::BitXor,
        A::Shl => BinaryOp::Shl,
        A::Shr => BinaryOp::Shr,
    }
}

pub(crate) fn lower_castop(op: ast::CastOp) -> CastOp {
    match op {
        ast::CastOp::As => CastOp::As,
        ast::CastOp::Is => CastOp::Is,
    }
}
