//! A stable, deterministic pretty-printer for the [`Hir`] — the `--emit=hir`
//! surface and a debugging aid.
//!
//! Output is deterministic (definitions are emitted in `DefId` order) and
//! self-describing: every expression is annotated with its resolved type, every
//! name with its resolution, every call with its dispatch kind. This is the
//! human-readable witness that the HIR carries everything codegen needs.

use crate::ids::DefId;
use crate::sema::symbols::Program;
use crate::ty::{Ty, TyCtxt};

use super::*;

/// Render a whole program's HIR to a stable string.
pub fn print_program(hir: &Hir, tcx: &TyCtxt, program: &Program) -> String {
    print_program_with_filter(hir, tcx, program, |_| true)
}

/// Render the HIR for definitions originating in real source files below
/// `file_count`. Bundled toolchain sources use high synthetic file ids, so this
/// keeps `otter_fusion emit hir <file>` focused on the user's program while the
/// full HIR remains available to codegen and tests through [`print_program`].
pub fn print_program_for_files(
    hir: &Hir,
    tcx: &TyCtxt,
    program: &Program,
    file_count: usize,
) -> String {
    print_program_with_filter(hir, tcx, program, |def| {
        program.def(def).span.file.0 < file_count as u32
    })
}

fn print_program_with_filter(
    hir: &Hir,
    tcx: &TyCtxt,
    program: &Program,
    include_def: impl Fn(DefId) -> bool,
) -> String {
    let mut p = Printer {
        hir,
        tcx,
        program,
        out: String::new(),
        indent: 0,
        include_def: &include_def,
    };
    p.program();
    p.out
}

struct Printer<'a> {
    hir: &'a Hir,
    tcx: &'a TyCtxt,
    program: &'a Program,
    out: String,
    indent: usize,
    include_def: &'a dyn Fn(DefId) -> bool,
}

impl<'a> Printer<'a> {
    fn ty(&self, ty: Ty) -> String {
        self.tcx.display(ty, &|d| self.name_of(d))
    }

    fn name_of(&self, d: DefId) -> String {
        if d.index() < self.program.defs.len() {
            self.program.def(d).name.clone()
        } else {
            format!("def{}", d.0)
        }
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn program(&mut self) {
        // Structs, in DefId order.
        let mut struct_defs: Vec<DefId> = self.hir.structs.keys().copied().collect();
        struct_defs.retain(|def| (self.include_def)(*def));
        struct_defs.sort();
        for def in struct_defs {
            let layout = match &self.hir.structs[&def] {
                StructFields::Unit => "unit".to_string(),
                StructFields::Tuple(ts) => {
                    let parts: Vec<String> = ts.iter().map(|t| self.ty(*t)).collect();
                    format!("tuple({})", parts.join(", "))
                }
                StructFields::Record(fs) => {
                    let parts: Vec<String> = fs
                        .iter()
                        .map(|(n, t)| format!("{n}: {}", self.ty(*t)))
                        .collect();
                    format!("record({})", parts.join(", "))
                }
            };
            self.line(&format!(
                "struct {} {} = {}",
                def_tag(def),
                self.name_of(def),
                layout
            ));
        }

        // Extern signatures, in DefId order.
        let mut extern_defs: Vec<DefId> = self.hir.extern_sigs.keys().copied().collect();
        extern_defs.retain(|def| (self.include_def)(*def));
        extern_defs.sort();
        for def in extern_defs {
            let sig = &self.hir.extern_sigs[&def];
            let ps: Vec<String> = sig.params.iter().map(|t| self.ty(*t)).collect();
            self.line(&format!(
                "extern fn {} {}({}): {}",
                def_tag(def),
                self.name_of(def),
                ps.join(", "),
                self.ty(sig.ret)
            ));
        }

        // Bodies, in DefId order.
        let mut body_defs: Vec<DefId> = self.hir.bodies.keys().copied().collect();
        body_defs.retain(|def| (self.include_def)(*def));
        body_defs.sort();
        for def in body_defs {
            self.body(def);
        }
    }

    fn body(&mut self, def: DefId) {
        let body = &self.hir.bodies[&def];
        let params: Vec<String> = body
            .params
            .iter()
            .map(|l| {
                format!(
                    "{}: {}",
                    local_tag(*l),
                    self.ty(body.local_ty(*l).unwrap_or(self.tcx.error))
                )
            })
            .collect();
        let async_marker = if body.async_output.is_some() {
            "async "
        } else {
            ""
        };
        self.line(&format!(
            "{}fn {} {}({}): {} {{",
            async_marker,
            def_tag(def),
            self.name_of(def),
            params.join(", "),
            self.ty(body.ret)
        ));
        self.indent += 1;
        self.block(&body.block);
        self.indent -= 1;
        self.line("}");
    }

    fn block(&mut self, b: &Block) {
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.trailing {
            let s = self.expr(t);
            self.line(&format!("trailing {s}"));
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pattern, init } => {
                let pat = self.pattern(pattern);
                let e = self.expr(init);
                self.line(&format!("let {pat} = {e}"));
            }
            StmtKind::Assign { target, value } => {
                let t = self.expr(target);
                let v = self.expr(value);
                self.line(&format!("assign {t} = {v}"));
            }
            StmtKind::Expr(e) => {
                let s = self.expr(e);
                self.line(&format!("expr {s}"));
            }
            StmtKind::Item(d) => self.line(&format!("item {}", def_tag(*d))),
        }
    }

    /// Render an expression to a one-line s-expression annotated with its type.
    /// Nested blocks/control flow render their sub-structure inline but stably.
    fn expr(&mut self, e: &Expr) -> String {
        let inner = self.expr_kind(&e.kind);
        format!("{inner}:{}", self.ty(e.ty))
    }

    fn expr_kind(&mut self, k: &ExprKind) -> String {
        match k {
            ExprKind::Int(v) => format!("(int {v})"),
            ExprKind::Float(v) => format!("(float {v})"),
            ExprKind::Bool(v) => format!("(bool {v})"),
            ExprKind::Null => "(null)".to_string(),
            ExprKind::Char(v) => format!("(char {v})"),
            ExprKind::Str(parts) => {
                let ps: Vec<String> = parts
                    .iter()
                    .map(|p| match p {
                        StrPart::Text(t) => format!("{t:?}"),
                        StrPart::Interp {
                            expr, stringify, ..
                        } => {
                            let inner = self.expr(expr);
                            match stringify {
                                Some(m) => format!("${{{inner} via {}}}", def_tag(*m)),
                                None => format!("${{{inner}}}"),
                            }
                        }
                    })
                    .collect();
                format!("(str {})", ps.join(" "))
            }
            ExprKind::Name(res) => format!("(name {})", res_str(*res)),
            ExprKind::Tuple(xs) => format!("(tuple {})", self.exprs(xs)),
            ExprKind::List(xs) => format!("(list {})", self.exprs(xs)),
            ExprKind::Map(items) => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|it| match it {
                        MapEntry::Kv { key, value } => {
                            format!("{} => {}", self.expr(key), self.expr(value))
                        }
                        MapEntry::Spread(e) => format!("..{}", self.expr(e)),
                    })
                    .collect();
                format!("(map {})", parts.join(" "))
            }
            ExprKind::Struct {
                def,
                fields,
                spread,
                ..
            } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}#{}={}", f.name, f.index, self.expr(&f.value)))
                    .collect();
                let sp = spread
                    .as_ref()
                    .map(|s| format!(" ..{}", self.expr(s)))
                    .unwrap_or_default();
                format!("(struct {} {}{})", def_tag(*def), fs.join(" "), sp)
            }
            ExprKind::Field { receiver, field } => {
                format!(
                    "(field {} .{}#{})",
                    self.expr(receiver),
                    field.name,
                    field.index
                )
            }
            ExprKind::TupleIndex { receiver, index } => {
                format!("(tupidx {} .{index})", self.expr(receiver))
            }
            ExprKind::Index { receiver, index } => {
                format!("(index {} {})", self.expr(receiver), self.expr(index))
            }
            ExprKind::Call { kind, args, .. } => {
                format!("(call {} {})", self.call_kind(kind), self.exprs(args))
            }
            ExprKind::Intrinsic { intrinsic, args } => {
                format!(
                    "(intrinsic {} {})",
                    intrinsic_str(intrinsic),
                    self.exprs(args)
                )
            }
            ExprKind::Unary {
                op,
                operand,
                overload,
            } => {
                let ov = overload
                    .as_ref()
                    .map(|o| format!(" via {}", def_tag(o.method)))
                    .unwrap_or_default();
                format!("(unary {op:?} {}{ov})", self.expr(operand))
            }
            ExprKind::Binary {
                op,
                left,
                right,
                overload,
            } => {
                let ov = overload
                    .as_ref()
                    .map(|o| format!(" via {}", def_tag(o.method)))
                    .unwrap_or_default();
                format!(
                    "(binary {op:?} {} {}{ov})",
                    self.expr(left),
                    self.expr(right)
                )
            }
            ExprKind::Cast { op, expr, target } => {
                format!("(cast {op:?} {} to {})", self.expr(expr), self.ty(*target))
            }
            ExprKind::Ref(e) => format!("(ref {})", self.expr(e)),
            ExprKind::Deref(e) => format!("(deref {})", self.expr(e)),
            ExprKind::Try { expr, branch, .. } => {
                let b = if branch.is_some() { " +branch" } else { "" };
                format!("(try {}{b})", self.expr(expr))
            }
            ExprKind::Await { expr, output } => {
                format!("(await {} -> {})", self.expr(expr), self.ty(*output))
            }
            ExprKind::Spawn { expr, output } => {
                format!("(spawn {} -> {})", self.expr(expr), self.ty(*output))
            }
            ExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                let c = self.expr(cond);
                let t = self.block_str(then_block);
                let e = else_branch
                    .as_ref()
                    .map(|e| format!(" else {}", self.expr(e)))
                    .unwrap_or_default();
                format!("(if {c} then {t}{e})")
            }
            ExprKind::Match { scrutinee, arms } => {
                let s = self.expr(scrutinee);
                let arms: Vec<String> = arms
                    .iter()
                    .map(|a| {
                        let pat = self.pattern(&a.pattern);
                        let body = self.expr(&a.body);
                        format!("[{pat} => {body}]")
                    })
                    .collect();
                format!("(match {s} {})", arms.join(" "))
            }
            ExprKind::Block(b) => format!("(block {})", self.block_str(b)),
            ExprKind::Loop(b) => format!("(loop {})", self.block_str(b)),
            ExprKind::While { cond, body } => {
                format!("(while {} {})", self.expr(cond), self.block_str(body))
            }
            ExprKind::For {
                pattern,
                iter,
                body,
                driver,
                in_async,
            } => {
                let pat = self.pattern(pattern);
                let aw = if *in_async { "await " } else { "" };
                format!(
                    "(for{} {aw}{pat} in {} {})",
                    driver_str(driver),
                    self.expr(iter),
                    self.block_str(body)
                )
            }
            ExprKind::Return(v) => {
                format!(
                    "(return {})",
                    v.as_ref().map(|e| self.expr(e)).unwrap_or_default()
                )
            }
            ExprKind::Break(v) => {
                format!(
                    "(break {})",
                    v.as_ref().map(|e| self.expr(e)).unwrap_or_default()
                )
            }
            ExprKind::Continue => "(continue)".to_string(),
            ExprKind::Closure {
                params,
                captures,
                ret,
                is_async,
                body,
            } => {
                let ps: Vec<String> = params.iter().map(|(l, _)| local_tag(*l)).collect();
                let cs: Vec<String> = captures.iter().map(|(l, _)| local_tag(*l)).collect();
                let a = if *is_async { "async " } else { "" };
                format!(
                    "({a}closure ({}) caps[{}] -> {} {})",
                    ps.join(" "),
                    cs.join(" "),
                    self.ty(*ret),
                    self.expr(body)
                )
            }
            ExprKind::AsyncBlock {
                output,
                captures,
                body,
                ..
            } => {
                let cs: Vec<String> = captures.iter().map(|(l, _)| local_tag(*l)).collect();
                format!(
                    "(async-block caps[{}] -> {} {})",
                    cs.join(" "),
                    self.ty(*output),
                    self.block_str(body)
                )
            }
            ExprKind::Adjust { adjust, expr } => {
                format!("({} {})", adjust_str(adjust), self.expr(expr))
            }
            ExprKind::Discard => "(discard)".to_string(),
            ExprKind::Error => "(error)".to_string(),
        }
    }

    fn exprs(&mut self, xs: &[Expr]) -> String {
        let parts: Vec<String> = xs.iter().map(|e| self.expr(e)).collect();
        parts.join(" ")
    }

    fn block_str(&mut self, b: &Block) -> String {
        let mut parts = Vec::new();
        for s in &b.stmts {
            parts.push(match &s.kind {
                StmtKind::Let { pattern, init } => {
                    format!("let {} = {}", self.pattern(pattern), self.expr(init))
                }
                StmtKind::Assign { target, value } => {
                    format!("assign {} = {}", self.expr(target), self.expr(value))
                }
                StmtKind::Expr(e) => format!("expr {}", self.expr(e)),
                StmtKind::Item(d) => format!("item {}", def_tag(*d)),
            });
        }
        if let Some(t) = &b.trailing {
            parts.push(format!("=> {}", self.expr(t)));
        }
        format!("{{ {} }}", parts.join("; "))
    }

    fn call_kind(&self, k: &CallKind) -> String {
        match k {
            CallKind::Direct { def, type_args } => {
                format!("direct {}{}", def_tag(*def), targs(type_args, self))
            }
            CallKind::Method {
                def,
                type_args,
                is_static,
                ..
            } => {
                let s = if *is_static { "static-" } else { "" };
                format!("{s}method {}{}", def_tag(*def), targs(type_args, self))
            }
            CallKind::Builtin(b) => format!("builtin {b:?}"),
            CallKind::Closure { .. } => "closure".to_string(),
            CallKind::Extern { def } => format!("extern {}", def_tag(*def)),
            CallKind::BuiltinMethod { name } => format!("builtin-method .{name}"),
            CallKind::TupleCtor { def, .. } => format!("ctor {}", def_tag(*def)),
        }
    }

    fn pattern(&mut self, p: &Pattern) -> String {
        let inner = match &p.kind {
            PatternKind::Wildcard => "_".to_string(),
            PatternKind::Bind(l) => format!("bind {}", local_tag(*l)),
            PatternKind::Literal(e) => format!("lit {}", self.expr(e)),
            PatternKind::TypeBind { test_ty, bind } => {
                let b = bind
                    .map(|l| format!(" {}", local_tag(l)))
                    .unwrap_or_default();
                format!("typebind {}{b}", self.ty(*test_ty))
            }
            PatternKind::UnitPath { def, .. } => format!("unit {}", def_tag(*def)),
            PatternKind::TupleStruct { def, fields, .. } => {
                let fs: Vec<String> = fields.iter().map(|f| self.pattern(f)).collect();
                format!("tuplestruct {} ({})", def_tag(*def), fs.join(" "))
            }
            PatternKind::RecordStruct {
                def,
                fields,
                has_rest,
            } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}#{}: {}", f.name, f.index, self.pattern(&f.pattern)))
                    .collect();
                let r = if *has_rest { " .." } else { "" };
                format!("recordstruct {} ({}{r})", def_tag(*def), fs.join(" "))
            }
            PatternKind::Tuple { elems, .. } => {
                let es: Vec<String> = elems.iter().map(|e| self.pattern(e)).collect();
                format!("tuple ({})", es.join(" "))
            }
            PatternKind::List { elems, .. } => {
                let es: Vec<String> = elems.iter().map(|e| self.pattern(e)).collect();
                format!("list ({})", es.join(" "))
            }
            PatternKind::Or(ps) => {
                let es: Vec<String> = ps.iter().map(|e| self.pattern(e)).collect();
                format!("or ({})", es.join(" | "))
            }
        };
        format!("<{inner}>")
    }
}

fn targs(ts: &[Ty], p: &Printer) -> String {
    if ts.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = ts.iter().map(|t| p.ty(*t)).collect();
        format!("<{}>", parts.join(", "))
    }
}

fn def_tag(d: DefId) -> String {
    format!("def{}", d.0)
}

fn local_tag(l: crate::ids::LocalId) -> String {
    format!("local{}", l.0)
}

fn res_str(res: Res) -> String {
    match res {
        Res::Local(l) => format!("local {}", local_tag(l)),
        Res::Function(d) => format!("fn {}", def_tag(d)),
        Res::Method(d) => format!("method {}", def_tag(d)),
        Res::Global(d) => format!("global {}", def_tag(d)),
        Res::StructCtor(d) => format!("ctor {}", def_tag(d)),
        Res::Builtin(b) => format!("builtin {b:?}"),
    }
}

fn adjust_str(a: &Adjust) -> &'static str {
    match a {
        Adjust::Widen(_) => "widen",
        Adjust::Unbox(_) => "unbox",
        Adjust::WidenDyn(_) => "widen-dyn",
    }
}

fn driver_str(d: &ForDriver) -> &'static str {
    match d {
        ForDriver::ListFast { .. } => "/list",
        ForDriver::Iter(_) => "/iter",
        ForDriver::Map { .. } => "/map",
        ForDriver::AsyncIter(_) => "/async-iter",
        ForDriver::StrChars => "/str-chars",
        ForDriver::Channel { .. } => "/channel",
    }
}

fn intrinsic_str(i: &Intrinsic) -> &'static str {
    match i {
        Intrinsic::Num(_) => "num",
        Intrinsic::CollectionCtor => "collection-ctor",
        Intrinsic::Clone(_) => "clone",
        Intrinsic::SharedNew => "shared-new",
        Intrinsic::ChannelNew => "channel-new",
        Intrinsic::ThreadSpawn { .. } => "thread-spawn",
        Intrinsic::TaskSpawn { .. } => "task-spawn",
        Intrinsic::ThreadJoin { .. } => "thread-join",
        Intrinsic::ThreadDetach => "thread-detach",
        Intrinsic::TaskJoin { .. } => "task-join",
        Intrinsic::TaskDetach => "task-detach",
        Intrinsic::TaskCancel => "task-cancel",
        Intrinsic::YieldNow => "yield-now",
        Intrinsic::AsyncSleep => "async-sleep",
        Intrinsic::AsyncTimeout { .. } => "async-timeout",
        Intrinsic::TimeMonotonicNanos => "time-monotonic-nanos",
        Intrinsic::TimeSystemNanos => "time-system-nanos",
        Intrinsic::FutureCancel => "future-cancel",
        Intrinsic::ForeignAlloc { .. } => "foreign-alloc",
        Intrinsic::ForeignFree => "foreign-free",
        Intrinsic::ForeignRealloc => "foreign-realloc",
        Intrinsic::ForeignFlex { .. } => "foreign-flex",
    }
}

#[cfg(test)]
mod tests {
    use super::print_program;
    use crate::lexer::lex;
    use crate::parser::parse;
    use crate::sema::analyze;
    use crate::span::FileId;

    fn print(src: &str) -> String {
        // Near-empty prelude (`docs/17` §17.8): import the names the test uses.
        let src = &format!(
            "import {{ println }} from \"std:io\";\nimport {{ List, Map }} from \"core:collections\";\n{src}"
        );
        let (tokens, le) = lex(src, FileId(0));
        assert!(le.is_empty(), "lex: {le:?}");
        let (module, pe) = parse(src, &tokens);
        assert!(pe.is_empty(), "parse: {pe:?}");
        let a = analyze(&module);
        assert!(a.errors.is_empty(), "analysis: {:?}", a.errors);
        print_program(&a.hir, &a.tcx, &a.program)
    }

    #[test]
    fn prints_function_with_typed_nodes() {
        let out = print("function add(x: i64, y: i64): i64 { x + y }\nfunction main() {}");
        // Signature line with parameter and return types.
        assert!(out.contains("fn "), "no fn header:\n{out}");
        assert!(out.contains("add(local"), "no add params:\n{out}");
        // The body's `x + y` is a typed primitive Add.
        assert!(out.contains("(binary Add"), "no binary:\n{out}");
        assert!(out.contains(":i64"), "no type annotations:\n{out}");
        assert!(out.contains("trailing"), "no trailing expr:\n{out}");
    }

    #[test]
    fn output_is_deterministic() {
        let src = r#"
            struct Point { x: i64, y: i64 }
            function dist(p: Point): i64 { p.x + p.y }
            function main() {
                var q = Point { x: 1, y: 2 };
                println("hi");
            }
        "#;
        let a = print(src);
        let b = print(src);
        assert_eq!(a, b, "pretty-printer output must be deterministic");
    }

    #[test]
    fn shows_call_dispatch_and_resolution() {
        let out = print(
            r#"
            function helper(n: i64): i64 { n }
            function main() {
                var x = helper(7);
                println("hi");
            }
            "#,
        );
        assert!(out.contains("call direct"), "no direct call:\n{out}");
        assert!(
            out.contains("call builtin Println"),
            "no builtin call:\n{out}"
        );
        assert!(out.contains("name local"), "no resolved local name:\n{out}");
    }

    #[test]
    fn shows_struct_layout_and_field_indices() {
        let out = print(
            r#"
            struct Point { x: i64, y: i64 }
            function main() { var p = Point { y: 2, x: 1 }; }
            "#,
        );
        assert!(
            out.contains("record(x: i64, y: i64)"),
            "no struct layout:\n{out}"
        );
        // Field initializers carry their resolved declaration index.
        assert!(out.contains("x#0="), "no x index:\n{out}");
        assert!(out.contains("y#1="), "no y index:\n{out}");
    }
}
