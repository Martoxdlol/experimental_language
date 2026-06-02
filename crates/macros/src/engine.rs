//! The procedural-macro expansion driver (`docs/22` §4).
//!
//! Macro expansion is phase 2 of the pipeline — after parse, before type
//! checking and module resolution. This module:
//!
//!   1. Finds every `@ProcMacro` function in the program.
//!   2. Builds a self-contained *macro sub-program* (the macro functions, their
//!      helper closure, the `core:compiler` import, and a thin `i64`-ABI entry
//!      shim per macro), runs the normal front-end over it, and JIT-compiles it
//!      with the host functions ([`crate::host`]) registered.
//!   3. Walks the user program and, for every `@MacroName` decorator that names
//!      a defined macro, runs that macro with the annotated item as input and
//!      splices its output back in — to a fixed point.
//!
//! Programs with no `@ProcMacro` definitions pay nothing: [`expand_user_macros`]
//! returns immediately.

use backend::macro_host::arena::{self, DiagLevel, MacroCtx, Node};
use backend::macro_host::host;
use compiler::ast::*;
use compiler::sema::{self, SemaError};
use compiler::span::Span;
use std::collections::HashSet;

/// Built-in decorator names that are *not* user macros and must be left for
/// their dedicated handlers (`@Derive` is the built-in derive desugaring;
/// `@ProcMacro` marks a definition; the rest are layout/ABI decorators).
const RESERVED_ATTRS: &[&str] = &[
    "Derive",
    "derive",
    "ProcMacro",
    "Transparent",
    "Packed",
    "Align",
    "Union",
    "RefCounted",
    "CallConv",
    "Link",
];

/// Default macro-expansion recursion limit (`docs/22` §10). Overridable via
/// `[macros] recursion_limit` in the project manifest.
const DEFAULT_RECURSION_LIMIT: usize = 128;

/// The configured recursion limit for this build, or the default.
fn recursion_limit(ctx: &sema::ResolveContext) -> usize {
    ctx.macro_recursion_limit.unwrap_or(DEFAULT_RECURSION_LIMIT)
}

/// Expand every user procedural-macro invocation in `root` (and `externals`) to
/// a fixed point, mutating the modules in place. Returns the diagnostics macros
/// emitted plus any macro-definition / expansion errors.
pub fn expand_user_macros(
    root: &mut Module,
    externals: &mut sema::symbols::Externals,
    ctx: &sema::ResolveContext,
) -> Vec<SemaError> {
    // 1. Gather macro definitions across the whole program.
    let mut macro_names: HashSet<String> = HashSet::new();
    collect_macro_names(root, &mut macro_names);
    for m in externals.values() {
        collect_macro_names(m, &mut macro_names);
    }
    if macro_names.is_empty() {
        return Vec::new();
    }

    // 2. Enforce the sandbox before doing any work: a macro that uses a `std:`
    //    name is rejected up front (`docs/22` §6).
    let sandbox = sandbox_violations(root, externals);
    if !sandbox.is_empty() {
        return sandbox;
    }

    arena::reset();

    // 3. Build + compile the macro sub-program.
    let jit = match build_macro_jit(root, externals, &macro_names, ctx) {
        Ok(jit) => jit,
        Err(errs) => return errs,
    };

    // 4. Expand to a fixed point. Each invocation site is expanded recursively
    //    (a macro's output is re-expanded immediately), so the recursion depth
    //    of self-emitting macros is bounded by `limit` (`docs/22` §10) and a
    //    runaway macro is rejected with its invocation chain. Decorator items
    //    are expanded first, then value (expression/block) macros in all bodies.
    let limit = recursion_limit(ctx);
    let mut exp = Expander {
        names: &macro_names,
        jit: &jit,
        limit,
        errors: Vec::new(),
        chain: Vec::new(),
    };
    exp.expand_items(&mut root.items);
    for m in externals.values_mut() {
        exp.expand_items(&mut m.items);
    }
    for it in &mut root.items {
        exp.walk_item(it);
    }
    for m in externals.values_mut() {
        for it in &mut m.items {
            exp.walk_item(it);
        }
    }
    let errors = exp.errors;

    // Macro definitions are compile-time only (like Rust proc-macro crates):
    // strip them and their now-orphaned `core:compiler` imports so they never
    // reach type checking or codegen of the runtime program.
    strip_macro_definitions(root);
    for m in externals.values_mut() {
        strip_macro_definitions(m);
    }
    errors
}

/// Remove `@ProcMacro` function items and `core:compiler` imports from a module
/// (recursing into inline submodules).
fn strip_macro_definitions(module: &mut Module) {
    module.items.retain(|it| {
        if is_proc_macro(it) {
            return false;
        }
        if let ItemKind::Import(imp) = &it.kind {
            if imports_compiler_surface(imp) {
                return false;
            }
        }
        true
    });
    for it in &mut module.items {
        if let ItemKind::Module(ModuleItem {
            kind: ModuleKind::Inline { items, .. },
            ..
        }) = &mut it.kind
        {
            let mut sub = Module {
                inner_docs: Vec::new(),
                items: std::mem::take(items),
                span: it.span,
            };
            strip_macro_definitions(&mut sub);
            *items = sub.items;
        }
    }
}

/// The literal text of an import path (`"core:compiler"` → `core:compiler`).
fn import_path(imp: &ImportItem) -> String {
    imp.path
        .parts
        .iter()
        .map(|p| match p {
            StringPart::Text { text, .. } => text.as_str(),
            _ => "",
        })
        .collect()
}

/// Whether an import targets the `core:compiler` macro-authoring module.
fn imports_compiler_surface(imp: &ImportItem) -> bool {
    import_path(imp) == "core:compiler"
}

/// Whether an import targets an OS-backed `std:` module (`docs/22` §6: a
/// procedural macro is sandboxed and may not use one).
fn is_std_import(imp: &ImportItem) -> bool {
    import_path(imp).starts_with("std:")
}

// ---------------------------------------------------------------------------
// Sandbox (`docs/22` §6)
// ---------------------------------------------------------------------------

/// Enforce the macro sandbox: a `@ProcMacro` function may not use any name
/// brought in from a `std:` module (OS-backed I/O, threads, etc.). Returns one
/// error per offending (macro, name). The check is by reference, so a program
/// where only non-macro code (e.g. `main`) uses `std:` is unaffected.
fn sandbox_violations(root: &Module, externals: &sema::symbols::Externals) -> Vec<SemaError> {
    // Local name (alias or original) → its import span, for every `std:` import.
    let mut std_names: Vec<(String, Span)> = Vec::new();
    let mut collect_std = |m: &Module| {
        for it in &m.items {
            if let ItemKind::Import(imp) = &it.kind {
                if is_std_import(imp) {
                    if let ImportKind::Named(specs) = &imp.kind {
                        for s in specs {
                            let local = s.alias.as_ref().unwrap_or(&s.name);
                            std_names.push((local.name.clone(), s.span));
                        }
                    }
                }
            }
        }
    };
    collect_std(root);
    for m in externals.values() {
        collect_std(m);
    }
    if std_names.is_empty() {
        return Vec::new();
    }

    let mut errors = Vec::new();
    let mut check = |m: &Module| {
        for it in &m.items {
            if !is_proc_macro(it) {
                continue;
            }
            let ItemKind::Function(f) = &it.kind else {
                continue;
            };
            let refs = referenced_names(it);
            for (name, span) in &std_names {
                if refs.contains(name) {
                    errors.push(SemaError::message(
                        *span,
                        format!(
                            "procedural macro `{}` may not use `{}` from a `std:` module — \
                             macros are sandboxed and cannot perform I/O or use OS-backed \
                             modules (`docs/22` §6)",
                            f.name.name, name
                        ),
                    ));
                }
            }
        }
    };
    check(root);
    for m in externals.values() {
        check(m);
    }
    errors
}

// ---------------------------------------------------------------------------
// Definition discovery
// ---------------------------------------------------------------------------

fn is_proc_macro(item: &Item) -> bool {
    matches!(item.kind, ItemKind::Function(_))
        && item.attrs.iter().any(|a| a.name.name == "ProcMacro")
}

fn collect_macro_names(module: &Module, out: &mut HashSet<String>) {
    for it in &module.items {
        if is_proc_macro(it) {
            if let ItemKind::Function(f) = &it.kind {
                out.insert(f.name.name.clone());
            }
        }
        if let ItemKind::Module(ModuleItem {
            kind: ModuleKind::Inline { items, .. },
            ..
        }) = &it.kind
        {
            let sub = Module {
                inner_docs: Vec::new(),
                items: items.clone(),
                span: it.span,
            };
            collect_macro_names(&sub, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Macro sub-program construction + compilation
// ---------------------------------------------------------------------------

/// Build, analyse, and JIT-compile the macro sub-program. On any front-end or
/// codegen error, returns those errors (so the user sees what's wrong with the
/// macro definitions) instead of a `Jit`.
fn build_macro_jit(
    root: &Module,
    externals: &sema::symbols::Externals,
    macro_names: &HashSet<String>,
    ctx: &sema::ResolveContext,
) -> Result<backend::Jit, Vec<SemaError>> {
    // Flatten every module's top-level items into one pool (slice-1 limitation:
    // macros + helpers are resolved in a single synthetic module).
    let mut pool: Vec<Item> = Vec::new();
    let mut imports: Vec<Item> = Vec::new();
    let mut seen_imports: HashSet<String> = HashSet::new();
    let mut gather = |m: &Module| {
        for it in &m.items {
            match &it.kind {
                // Sandbox (`docs/22` §6): never give the macro sub-program access
                // to `std:` — defence in depth alongside `sandbox_violations`.
                ItemKind::Import(imp) if is_std_import(imp) => {}
                ItemKind::Import(_) => {
                    let key = compiler::ast_print::print_item(it);
                    if seen_imports.insert(key) {
                        imports.push(it.clone());
                    }
                }
                ItemKind::Module(_) | ItemKind::Test(_) | ItemKind::Var(_) => {}
                _ => pool.push(it.clone()),
            }
        }
    };
    gather(root);
    for m in externals.values() {
        gather(m);
    }

    // Dependency closure: start from the macro functions, pull in every pooled
    // item whose declared name is referenced, transitively; always include
    // `extend` blocks that target an included type.
    let closure = dependency_closure(&pool, macro_names);

    // Synthesise an `i64`-ABI entry shim per macro so the host can call it with
    // plain handles (constructing the `MacroContext`/`ASTNode` wrappers on the
    // language side, where their layout is known).
    let mut items: Vec<Item> = Vec::new();
    items.extend(imports);
    items.extend(closure);
    let mut entry_errors = Vec::new();
    for name in macro_names {
        match shim_item(name) {
            Ok(it) => items.push(it),
            Err(e) => entry_errors.push(e),
        }
    }
    if !entry_errors.is_empty() {
        return Err(entry_errors);
    }

    let synthetic = Module {
        inner_docs: Vec::new(),
        items,
        span: Span::dummy(),
    };
    let analysis = sema::analyze_multi_ctx(&synthetic, &sema::symbols::Externals::new(), ctx);
    if !analysis.errors.is_empty() {
        return Err(analysis
            .errors
            .iter()
            .map(|e| {
                SemaError::message(
                    e.span,
                    format!("in procedural-macro definition: {}", error_text(e)),
                )
            })
            .collect());
    }

    // Macros run at compile time; never collect during the short macro run.
    backend::set_gc_enabled(false);
    let syms = host::symbols();
    let syms_ref: Vec<(&str, *const u8)> = syms.iter().map(|(n, a)| (*n, *a)).collect();
    backend::compile_with_symbols(&analysis, &syms_ref).map_err(|e| {
        vec![SemaError::message(
            e.span,
            format!("macro code generation failed: {}", e.message),
        )]
    })
}

fn error_text(e: &SemaError) -> String {
    e.kind.to_string()
}

/// Names referenced anywhere in an item's printed source (a safe over-
/// approximation used to compute the macro dependency closure).
fn referenced_names(item: &Item) -> HashSet<String> {
    let text = compiler::ast_print::print_item(item);
    let mut out = HashSet::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '_' || ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.insert(cur);
    }
    out
}

fn item_decl_name(item: &Item) -> Option<String> {
    match &item.kind {
        ItemKind::Function(f) => Some(f.name.name.clone()),
        ItemKind::Struct(s) => Some(s.name.name.clone()),
        ItemKind::Interface(i) => Some(i.name.name.clone()),
        ItemKind::TypeAlias(t) => Some(t.name.name.clone()),
        ItemKind::Extern(_) => None,
        _ => None,
    }
}

fn extend_target_name(item: &Item) -> Option<String> {
    match &item.kind {
        ItemKind::Extend(e) => match &e.target.kind {
            TypeKind::Named { name, .. } => Some(name.name.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn dependency_closure(pool: &[Item], seeds: &HashSet<String>) -> Vec<Item> {
    let mut needed: HashSet<String> = seeds.clone();
    // Iterate to a fixed point over named items.
    loop {
        let mut grew = false;
        for it in pool {
            let include = item_decl_name(it).is_some_and(|n| needed.contains(&n))
                || extend_target_name(it).is_some_and(|n| needed.contains(&n));
            if include {
                for r in referenced_names(it) {
                    if pool
                        .iter()
                        .any(|p| item_decl_name(p).as_deref() == Some(r.as_str()))
                        && needed.insert(r)
                    {
                        grew = true;
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    pool.iter()
        .filter(|it| {
            item_decl_name(it).is_some_and(|n| needed.contains(&n))
                || extend_target_name(it).is_some_and(|n| needed.contains(&n))
        })
        .cloned()
        .collect()
}

/// Parse the `i64`-ABI entry shim for macro `name`.
fn shim_item(name: &str) -> Result<Item, SemaError> {
    let src = format!(
        "function __macro_entry_{name}(__c: i64, __i: i64): i64 {{ \
         {name}(MacroContext {{ ctx: __c }}, ASTNode {{ handle: __i }}).handle }}"
    );
    let file = arena::with(|s| s.new_gen_file(src.clone()));
    let (tokens, lex_errs) = compiler::lex(&src, file);
    if let Some(e) = lex_errs.first() {
        return Err(SemaError::message(
            e.span,
            format!("macro shim lex error: {e:?}"),
        ));
    }
    let (module, parse_errs) = compiler::parse(&src, &tokens);
    if let Some(e) = parse_errs.first() {
        return Err(SemaError::message(
            e.span,
            format!("macro shim parse error: {}", e.kind),
        ));
    }
    module
        .items
        .into_iter()
        .next()
        .ok_or_else(|| SemaError::message(Span::dummy(), "macro shim produced no item"))
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// Drives recursive macro expansion: each invocation's output is re-expanded
/// immediately, so `chain` records the active expansion path and its length is
/// the recursion depth. Exceeding `limit` rejects a runaway (self-emitting)
/// macro with its invocation chain (`docs/22` §10).
struct Expander<'a> {
    names: &'a HashSet<String>,
    jit: &'a backend::Jit,
    limit: usize,
    errors: Vec<SemaError>,
    chain: Vec<String>,
}

impl Expander<'_> {
    fn is_user_macro(&self, name: &str) -> bool {
        self.names.contains(name) && !RESERVED_ATTRS.contains(&name)
    }

    /// If adding one more expansion of `name` would exceed the depth limit,
    /// record a chain error and return `true`.
    fn limit_exceeded(&mut self, at: Span, name: &str) -> bool {
        if self.chain.len() < self.limit {
            return false;
        }
        let mut chain = self.chain.clone();
        chain.push(name.to_string());
        let total = chain.len();
        // Truncate a long (typically self-recursive) chain so the message stays
        // readable: first few → … → last few.
        let path = if total > 10 {
            let head = chain[..5]
                .iter()
                .map(|n| format!("@{n}"))
                .collect::<Vec<_>>()
                .join(" → ");
            let tail = chain[total - 3..]
                .iter()
                .map(|n| format!("@{n}"))
                .collect::<Vec<_>>()
                .join(" → ");
            format!("{head} → … ({} more) → {tail}", total - 8)
        } else {
            chain
                .iter()
                .map(|n| format!("@{n}"))
                .collect::<Vec<_>>()
                .join(" → ")
        };
        self.errors.push(SemaError::message(
            at,
            format!(
                "macro expansion exceeded the recursion limit of {} (a macro likely \
                 re-emits its own invocation); chain: {path} (`docs/22` §10)",
                self.limit
            ),
        ));
        true
    }

    /// Expand every decorator macro in `items` (recursing into inline submodules
    /// and into each macro's own output).
    fn expand_items(&mut self, items: &mut Vec<Item>) {
        let mut i = 0;
        while i < items.len() {
            if let ItemKind::Module(ModuleItem {
                kind: ModuleKind::Inline { items: sub, .. },
                ..
            }) = &mut items[i].kind
            {
                let mut taken = std::mem::take(sub);
                self.expand_items(&mut taken);
                if let ItemKind::Module(ModuleItem {
                    kind: ModuleKind::Inline { items: sub, .. },
                    ..
                }) = &mut items[i].kind
                {
                    *sub = taken;
                }
                i += 1;
                continue;
            }

            // Bottom-most matching decorator applies first (`docs/22` §2).
            let attr_pos = items[i]
                .attrs
                .iter()
                .rposition(|a| self.is_user_macro(&a.name.name));
            let Some(ap) = attr_pos else {
                i += 1;
                continue;
            };

            let attr = items[i].attrs.remove(ap);
            let macro_name = attr.name.name.clone();
            if self.limit_exceeded(attr.span, &macro_name) {
                // Attribute already removed, so we won't loop on it again.
                i += 1;
                continue;
            }
            let input_item = items[i].clone();
            let replacement =
                run_decorator(self.jit, &macro_name, attr, input_item, &mut self.errors);
            match replacement {
                Some(mut repl) => {
                    // Re-expand the output at one deeper level (catches a macro
                    // re-emitting itself, and remaining stacked decorators).
                    self.chain.push(macro_name);
                    self.expand_items(&mut repl);
                    self.chain.pop();
                    let n = repl.len();
                    items.splice(i..=i, repl);
                    i += n;
                }
                None => i += 1,
            }
        }
    }
}

/// Outcome of invoking a macro: the returned node (if any) and whether the
/// macro reported an error (via `ctx.error`) or an error marker.
struct MacroOutcome {
    node: Option<Node>,
    had_error: bool,
}

/// Run macro `macro_name` with `args` (positional + keyword) and the given
/// `input` node at `invocation_span`. Drains the macro's diagnostics into
/// `errors`. Shared by every invocation form.
fn invoke_macro(
    jit: &backend::Jit,
    macro_name: &str,
    args: &[AttrArg],
    input: Node,
    invocation_span: Span,
    errors: &mut Vec<SemaError>,
) -> MacroOutcome {
    let mut mctx = MacroCtx {
        invocation_span,
        ..Default::default()
    };
    for a in args {
        match a {
            AttrArg::Positional(e) => mctx.args.push(host::intern_arg_expr(e.clone())),
            AttrArg::Named { name, value, .. } => {
                let h = host::intern_arg_expr(value.clone());
                mctx.kwargs.push((name.name.clone(), h));
            }
        }
    }
    let input_h = arena::with(|s| s.push_node(input));
    let ctx_h = arena::with(|s| {
        s.contexts.push(mctx);
        (s.contexts.len() - 1) as i64
    });

    let entry = format!("__macro_entry_{macro_name}");
    let Some(ptr) = jit.func_ptr(&entry) else {
        errors.push(SemaError::message(
            invocation_span,
            format!("internal error: macro entry `{entry}` was not compiled"),
        ));
        return MacroOutcome {
            node: None,
            had_error: true,
        };
    };
    // SAFETY: the shim has signature `(i64, i64) -> i64` by construction.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    let out_h = f(ctx_h, input_h);

    let diags = arena::with(|s| {
        usize::try_from(ctx_h)
            .ok()
            .and_then(|i| s.contexts.get_mut(i))
            .map(|c| std::mem::take(&mut c.diags))
            .unwrap_or_default()
    });
    let mut had_error = false;
    for d in diags {
        match d.level {
            // Errors are fatal and join the diagnostic stream.
            DiagLevel::Error => {
                had_error = true;
                errors.push(SemaError::message(d.span, d.message));
            }
            // `warn`/`note` are informational and must not fail compilation;
            // surface them on stderr immediately (`docs/22` §7).
            DiagLevel::Warn => eprintln!("warning: [@{macro_name}] {}", d.message),
            DiagLevel::Note => eprintln!("note: [@{macro_name}] {}", d.message),
        }
    }
    let node = arena::with(|s| s.node(out_h).cloned());
    if matches!(node, Some(Node::ErrorMarker)) {
        had_error = true;
    }
    MacroOutcome { node, had_error }
}

/// Run one decorator macro. Returns the replacement items, or `None` if the
/// macro yielded an error marker / a non-item result (an error is recorded).
fn run_decorator(
    jit: &backend::Jit,
    macro_name: &str,
    attr: Attribute,
    input_item: Item,
    errors: &mut Vec<SemaError>,
) -> Option<Vec<Item>> {
    let outcome = invoke_macro(
        jit,
        macro_name,
        &attr.args,
        Node::Item(input_item),
        attr.span,
        errors,
    );
    if outcome.had_error {
        return None;
    }
    match outcome.node {
        None | Some(Node::ErrorMarker) => None,
        Some(Node::Item(it)) => Some(vec![it]),
        Some(Node::Items(v)) => Some(v),
        Some(other) => {
            errors.push(SemaError::message(
                attr.span,
                format!(
                    "macro `@{macro_name}` in decorator position must return an item or items, \
                     but returned a `{}` node",
                    arena::node_kind(&other)
                ),
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Expression- and block-form expansion (`docs/22` §2)
// ---------------------------------------------------------------------------

impl Expander<'_> {
    /// Recurse into the bodies of an item, expanding any expression/block macro
    /// calls (`docs/22` §2). Inline submodules are descended into.
    fn walk_item(&mut self, item: &mut Item) {
        match &mut item.kind {
            ItemKind::Var(v) => self.walk_expr(&mut v.init),
            ItemKind::Function(f) => {
                if let Some(b) = &mut f.body {
                    self.walk_block(b);
                }
            }
            ItemKind::Extend(e) => {
                for m in &mut e.members {
                    if let Some(b) = &mut m.function.body {
                        self.walk_block(b);
                    }
                }
            }
            ItemKind::Interface(i) => {
                for m in &mut i.members {
                    if let Some(b) = &mut m.default_body {
                        self.walk_block(b);
                    }
                }
            }
            ItemKind::Test(t) => self.walk_block(&mut t.body),
            ItemKind::Module(ModuleItem {
                kind: ModuleKind::Inline { items, .. },
                ..
            }) => {
                for sub in items {
                    self.walk_item(sub);
                }
            }
            _ => {}
        }
    }

    fn walk_block(&mut self, b: &mut Block) {
        for s in &mut b.stmts {
            self.walk_stmt(s);
        }
        if let Some(t) = &mut b.trailing {
            self.walk_expr(t);
        }
    }

    fn walk_stmt(&mut self, s: &mut Stmt) {
        match &mut s.kind {
            StmtKind::Var(v) => self.walk_expr(&mut v.init),
            StmtKind::Assign { target, value } => {
                self.walk_expr(target);
                self.walk_expr(value);
            }
            StmtKind::Expr(e) => self.walk_expr(e),
            StmtKind::Item(it) => self.walk_item(it),
        }
    }

    /// Recurse into `e`'s sub-expressions, then — if `e` is itself a `@Name(...)`
    /// / `@Name { … }` call to a defined macro — run the macro and replace `e`
    /// with its output, re-expanding that output one level deeper (so nested and
    /// recursive macros expand, bounded by the depth limit).
    fn walk_expr(&mut self, e: &mut Expr) {
        match &mut e.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Null
            | ExprKind::Char(_)
            | ExprKind::Str(_)
            | ExprKind::SelfExpr
            | ExprKind::Underscore
            | ExprKind::Ident(_)
            | ExprKind::Continue => {}
            ExprKind::Tuple(es) | ExprKind::List(es) => {
                for x in es {
                    self.walk_expr(x);
                }
            }
            ExprKind::Paren(x) => self.walk_expr(x),
            ExprKind::MapLit(items) => {
                for it in items {
                    match it {
                        MapItem::Entry { key, value, .. } => {
                            self.walk_expr(key);
                            self.walk_expr(value);
                        }
                        MapItem::Spread(x) => self.walk_expr(x),
                    }
                }
            }
            ExprKind::StructLit { fields, spread, .. } => {
                for f in fields {
                    if let Some(v) = &mut f.value {
                        self.walk_expr(v);
                    }
                }
                if let Some(s) = spread {
                    self.walk_expr(s);
                }
            }
            ExprKind::Unary { operand, .. } => self.walk_expr(operand),
            ExprKind::Binary { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            ExprKind::Cast { expr, .. } => self.walk_expr(expr),
            ExprKind::Field { receiver, .. } => self.walk_expr(receiver),
            ExprKind::TupleIndex { receiver, .. } => self.walk_expr(receiver),
            ExprKind::Call {
                callee,
                args,
                trailing_closure,
                ..
            } => {
                self.walk_expr(callee);
                for a in args {
                    self.walk_expr(a);
                }
                if let Some(tc) = trailing_closure {
                    self.walk_expr(tc);
                }
            }
            ExprKind::Index { receiver, index } => {
                self.walk_expr(receiver);
                self.walk_expr(index);
            }
            ExprKind::Try { expr, .. }
            | ExprKind::Ref { expr, .. }
            | ExprKind::Deref { expr, .. }
            | ExprKind::Await { expr, .. }
            | ExprKind::Spawn { expr, .. } => self.walk_expr(expr),
            ExprKind::If {
                cond,
                then_block,
                else_branch,
            } => {
                self.walk_expr(cond);
                self.walk_block(then_block);
                if let Some(eb) = else_branch {
                    match eb {
                        ElseBranch::If(x) => self.walk_expr(x),
                        ElseBranch::Block(b) => self.walk_block(b),
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &mut arm.guard {
                        self.walk_expr(g);
                    }
                    self.walk_expr(&mut arm.body);
                }
            }
            ExprKind::Block(b) | ExprKind::Loop(b) => self.walk_block(b),
            ExprKind::While { cond, body } => {
                self.walk_expr(cond);
                self.walk_block(body);
            }
            ExprKind::For { iter, body, .. } => {
                self.walk_expr(iter);
                self.walk_block(body);
            }
            ExprKind::Return(v) | ExprKind::Break(v) => {
                if let Some(x) = v {
                    self.walk_expr(x);
                }
            }
            ExprKind::Closure { body, .. } => self.walk_expr(body),
            ExprKind::AnonFn(f) => {
                if let Some(b) = &mut f.body {
                    self.walk_block(b);
                }
            }
            ExprKind::AsyncBlock(b) => self.walk_block(b),
            ExprKind::MacroCall { .. } => { /* expanded below */ }
        }

        let ExprKind::MacroCall {
            name,
            args,
            block,
            at_span,
        } = &mut e.kind
        else {
            return;
        };
        if !self.is_user_macro(&name.name) {
            return; // unknown macro — left for the checker to report.
        }
        // Expand the call's own arguments/block first.
        for a in args.iter_mut() {
            match a {
                AttrArg::Positional(x) => self.walk_expr(x),
                AttrArg::Named { value, .. } => self.walk_expr(value),
            }
        }
        if let Some(b) = block.as_mut() {
            self.walk_block(b);
        }

        let macro_name = name.name.clone();
        let span = *at_span;
        if self.limit_exceeded(span, &macro_name) {
            e.kind = ExprKind::Null;
            return;
        }
        let args_owned = std::mem::take(args);
        let input = match block.take() {
            Some(b) => Node::Block(*b),
            None => Node::Args(positional_exprs(&args_owned)),
        };
        let outcome = invoke_macro(
            self.jit,
            &macro_name,
            &args_owned,
            input,
            span,
            &mut self.errors,
        );
        match (outcome.had_error, outcome.node) {
            (false, Some(Node::Expr(out))) => *e = out,
            (false, Some(Node::Block(b))) => e.kind = ExprKind::Block(b),
            (false, Some(Node::ErrorMarker)) | (true, _) => {
                e.kind = ExprKind::Null;
                return;
            }
            (false, Some(other)) => {
                self.errors.push(SemaError::message(
                    span,
                    format!(
                        "macro `@{macro_name}` in expression position must return an expression \
                         or block, but returned a `{}` node",
                        arena::node_kind(&other)
                    ),
                ));
                e.kind = ExprKind::Null;
                return;
            }
            (false, None) => {
                e.kind = ExprKind::Null;
                return;
            }
        }
        // Re-expand the macro's output one level deeper (nested/recursive macros).
        self.chain.push(macro_name);
        self.walk_expr(e);
        self.chain.pop();
    }
}

/// The positional argument expressions of an invocation (keyword args dropped —
/// they remain reachable via `ctx.kwargs`).
fn positional_exprs(args: &[AttrArg]) -> Vec<Expr> {
    args.iter()
        .filter_map(|a| match a {
            AttrArg::Positional(e) => Some(e.clone()),
            AttrArg::Named { .. } => None,
        })
        .collect()
}
