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
    "Derive", "derive", "ProcMacro", "Transparent", "Packed", "Align", "Union", "RefCounted",
    "CallConv", "Link",
];

/// A safety cap on total expansion rounds (the per-chain recursion limit of
/// `docs/22` §10 is refined in a later slice).
const MAX_ROUNDS: usize = 256;

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

    // 2. Are there any invocations at all? If not, still compile the macro
    //    sub-program so definition errors surface, but skip the expansion loop.
    arena::reset();

    // 3. Build + compile the macro sub-program.
    let jit = match build_macro_jit(root, externals, &macro_names, ctx) {
        Ok(jit) => jit,
        Err(errs) => return errs,
    };

    // 4. Expansion loop to a fixed point.
    let mut errors = Vec::new();
    let mut rounds = 0;
    loop {
        let mut changed = false;
        changed |= expand_module(root, &macro_names, &jit, &mut errors);
        changed |= expand_value_macros_in_module(root, &macro_names, &jit, &mut errors);
        for m in externals.values_mut() {
            changed |= expand_module(m, &macro_names, &jit, &mut errors);
            changed |= expand_value_macros_in_module(m, &macro_names, &jit, &mut errors);
        }
        if !changed {
            break;
        }
        rounds += 1;
        if rounds >= MAX_ROUNDS {
            errors.push(SemaError::message(
                Span::dummy(),
                format!(
                    "macro expansion did not terminate after {MAX_ROUNDS} rounds \
                     (a macro likely re-emits its own invocation; see `docs/22` §10)"
                ),
            ));
            break;
        }
    }

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
        if let ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) =
            &mut it.kind
        {
            let mut sub = Module { inner_docs: Vec::new(), items: std::mem::take(items), span: it.span };
            strip_macro_definitions(&mut sub);
            *items = sub.items;
        }
    }
}

/// Whether an import targets the `core:compiler` macro-authoring module.
fn imports_compiler_surface(imp: &ImportItem) -> bool {
    let path: String = imp
        .path
        .parts
        .iter()
        .map(|p| match p {
            StringPart::Text { text, .. } => text.as_str(),
            _ => "",
        })
        .collect();
    path == "core:compiler"
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
        if let ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) = &it.kind
        {
            let sub = Module { inner_docs: Vec::new(), items: items.clone(), span: it.span };
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

    let synthetic = Module { inner_docs: Vec::new(), items, span: Span::dummy() };
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
        vec![SemaError::message(e.span, format!("macro code generation failed: {}", e.message))]
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
                    if pool.iter().any(|p| {
                        item_decl_name(p).as_deref() == Some(r.as_str())
                    }) && needed.insert(r)
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
        return Err(SemaError::message(e.span, format!("macro shim lex error: {e:?}")));
    }
    let (module, parse_errs) = compiler::parse(&src, &tokens);
    if let Some(e) = parse_errs.first() {
        return Err(SemaError::message(e.span, format!("macro shim parse error: {}", e.kind)));
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

/// Expand all matching decorator invocations in `module` (recursing into inline
/// submodules). Returns whether any expansion happened.
fn expand_module(
    module: &mut Module,
    macro_names: &HashSet<String>,
    jit: &backend::Jit,
    errors: &mut Vec<SemaError>,
) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < module.items.len() {
        // Recurse into inline submodules first.
        if let ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) =
            &mut module.items[i].kind
        {
            let mut sub = Module { inner_docs: Vec::new(), items: std::mem::take(items), span: module.items[i].span };
            changed |= expand_module(&mut sub, macro_names, jit, errors);
            if let ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) =
                &mut module.items[i].kind
            {
                *items = sub.items;
            }
            i += 1;
            continue;
        }

        // The bottom-most matching decorator (last in source) applies first
        // (`docs/22` §2: stacked decorators apply bottom-up).
        let attr_pos = module.items[i]
            .attrs
            .iter()
            .rposition(|a| macro_names.contains(&a.name.name) && !RESERVED_ATTRS.contains(&a.name.name.as_str()));
        let Some(ap) = attr_pos else {
            i += 1;
            continue;
        };

        let attr = module.items[i].attrs.remove(ap);
        let macro_name = attr.name.name.clone();
        // The triggering attribute was already removed from this vec element by
        // the `remove` above, so the input the macro sees excludes its own
        // decorator (`docs/22` §2).
        let input_item = module.items[i].clone();

        match run_decorator(jit, &macro_name, attr, input_item, errors) {
            Some(replacement) => {
                let count = replacement.len();
                module.items.splice(i..=i, replacement);
                // Re-examine starting at the same index so freshly-produced
                // decorators are expanded too.
                changed = true;
                let _ = count;
            }
            None => {
                // Macro produced an error marker or a diagnostic: leave the
                // (attr-stripped) item in place and move on.
                i += 1;
            }
        }
    }
    changed
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
    let mut mctx = MacroCtx { invocation_span, ..Default::default() };
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
        return MacroOutcome { node: None, had_error: true };
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
        had_error |= d.level == DiagLevel::Error;
        errors.push(diag_to_sema(d));
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
    let outcome = invoke_macro(jit, macro_name, &attr.args, Node::Item(input_item), attr.span, errors);
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

/// Expand every `@Name(args)` / `@Name { … }` macro call appearing in the item
/// bodies of `module` (recursing into inline submodules). Returns whether any
/// expansion happened.
fn expand_value_macros_in_module(
    module: &mut Module,
    macro_names: &HashSet<String>,
    jit: &backend::Jit,
    errors: &mut Vec<SemaError>,
) -> bool {
    let mut changed = false;
    for it in &mut module.items {
        walk_item(it, macro_names, jit, errors, &mut changed);
    }
    changed
}

fn walk_item(
    item: &mut Item,
    names: &HashSet<String>,
    jit: &backend::Jit,
    errors: &mut Vec<SemaError>,
    changed: &mut bool,
) {
    match &mut item.kind {
        ItemKind::Var(v) => walk_expr(&mut v.init, names, jit, errors, changed),
        ItemKind::Function(f) => {
            if let Some(b) = &mut f.body {
                walk_block(b, names, jit, errors, changed);
            }
        }
        ItemKind::Extend(e) => {
            for m in &mut e.members {
                if let Some(b) = &mut m.function.body {
                    walk_block(b, names, jit, errors, changed);
                }
            }
        }
        ItemKind::Interface(i) => {
            for m in &mut i.members {
                if let Some(b) = &mut m.default_body {
                    walk_block(b, names, jit, errors, changed);
                }
            }
        }
        ItemKind::Test(t) => walk_block(&mut t.body, names, jit, errors, changed),
        ItemKind::Module(ModuleItem { kind: ModuleKind::Inline { items, .. }, .. }) => {
            for sub in items {
                walk_item(sub, names, jit, errors, changed);
            }
        }
        _ => {}
    }
}

fn walk_block(
    b: &mut Block,
    names: &HashSet<String>,
    jit: &backend::Jit,
    errors: &mut Vec<SemaError>,
    changed: &mut bool,
) {
    for s in &mut b.stmts {
        walk_stmt(s, names, jit, errors, changed);
    }
    if let Some(t) = &mut b.trailing {
        walk_expr(t, names, jit, errors, changed);
    }
}

fn walk_stmt(
    s: &mut Stmt,
    names: &HashSet<String>,
    jit: &backend::Jit,
    errors: &mut Vec<SemaError>,
    changed: &mut bool,
) {
    match &mut s.kind {
        StmtKind::Var(v) => walk_expr(&mut v.init, names, jit, errors, changed),
        StmtKind::Assign { target, value } => {
            walk_expr(target, names, jit, errors, changed);
            walk_expr(value, names, jit, errors, changed);
        }
        StmtKind::Expr(e) => walk_expr(e, names, jit, errors, changed),
        StmtKind::Item(it) => walk_item(it, names, jit, errors, changed),
    }
}

/// Recurse into `e`'s sub-expressions, then — if `e` is itself a `@Name(...)` /
/// `@Name { … }` call to a defined macro — run the macro and replace `e` with
/// its output expression. Nested macros in the output are expanded in a later
/// round of the fixed-point loop.
fn walk_expr(
    e: &mut Expr,
    names: &HashSet<String>,
    jit: &backend::Jit,
    errors: &mut Vec<SemaError>,
    changed: &mut bool,
) {
    match &mut e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Null
        | ExprKind::Char(_) | ExprKind::Str(_) | ExprKind::SelfExpr | ExprKind::Underscore
        | ExprKind::Ident(_) | ExprKind::Continue => {}
        ExprKind::Tuple(es) | ExprKind::List(es) => {
            for x in es {
                walk_expr(x, names, jit, errors, changed);
            }
        }
        ExprKind::Paren(x) => walk_expr(x, names, jit, errors, changed),
        ExprKind::MapLit(items) => {
            for it in items {
                match it {
                    MapItem::Entry { key, value, .. } => {
                        walk_expr(key, names, jit, errors, changed);
                        walk_expr(value, names, jit, errors, changed);
                    }
                    MapItem::Spread(x) => walk_expr(x, names, jit, errors, changed),
                }
            }
        }
        ExprKind::StructLit { fields, spread, .. } => {
            for f in fields {
                if let Some(v) = &mut f.value {
                    walk_expr(v, names, jit, errors, changed);
                }
            }
            if let Some(s) = spread {
                walk_expr(s, names, jit, errors, changed);
            }
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, names, jit, errors, changed),
        ExprKind::Binary { left, right, .. } => {
            walk_expr(left, names, jit, errors, changed);
            walk_expr(right, names, jit, errors, changed);
        }
        ExprKind::Cast { expr, .. } => walk_expr(expr, names, jit, errors, changed),
        ExprKind::Field { receiver, .. } => walk_expr(receiver, names, jit, errors, changed),
        ExprKind::TupleIndex { receiver, .. } => walk_expr(receiver, names, jit, errors, changed),
        ExprKind::Call { callee, args, trailing_closure, .. } => {
            walk_expr(callee, names, jit, errors, changed);
            for a in args {
                walk_expr(a, names, jit, errors, changed);
            }
            if let Some(tc) = trailing_closure {
                walk_expr(tc, names, jit, errors, changed);
            }
        }
        ExprKind::Index { receiver, index } => {
            walk_expr(receiver, names, jit, errors, changed);
            walk_expr(index, names, jit, errors, changed);
        }
        ExprKind::Try { expr, .. } | ExprKind::Ref { expr, .. } | ExprKind::Deref { expr, .. }
        | ExprKind::Await { expr, .. } | ExprKind::Spawn { expr, .. } => {
            walk_expr(expr, names, jit, errors, changed)
        }
        ExprKind::If { cond, then_block, else_branch } => {
            walk_expr(cond, names, jit, errors, changed);
            walk_block(then_block, names, jit, errors, changed);
            if let Some(eb) = else_branch {
                match eb {
                    ElseBranch::If(x) => walk_expr(x, names, jit, errors, changed),
                    ElseBranch::Block(b) => walk_block(b, names, jit, errors, changed),
                }
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, names, jit, errors, changed);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    walk_expr(g, names, jit, errors, changed);
                }
                walk_expr(&mut arm.body, names, jit, errors, changed);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => walk_block(b, names, jit, errors, changed),
        ExprKind::While { cond, body } => {
            walk_expr(cond, names, jit, errors, changed);
            walk_block(body, names, jit, errors, changed);
        }
        ExprKind::For { iter, body, .. } => {
            walk_expr(iter, names, jit, errors, changed);
            walk_block(body, names, jit, errors, changed);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => {
            if let Some(x) = v {
                walk_expr(x, names, jit, errors, changed);
            }
        }
        ExprKind::Closure { body, .. } => walk_expr(body, names, jit, errors, changed),
        ExprKind::AnonFn(f) => {
            if let Some(b) = &mut f.body {
                walk_block(b, names, jit, errors, changed);
            }
        }
        ExprKind::AsyncBlock(b) => walk_block(b, names, jit, errors, changed),
        ExprKind::MacroCall { .. } => {
            // Children (args, block) walked below by the dedicated branch.
        }
    }

    // After children, expand this node if it is a macro call to a defined macro.
    let ExprKind::MacroCall { name, args, block, at_span } = &mut e.kind else { return };
    if !names.contains(&name.name) {
        return; // unknown macro — left for the checker to report.
    }
    // Walk the call's own arguments/block (children) before invoking.
    for a in args.iter_mut() {
        match a {
            AttrArg::Positional(x) => walk_expr(x, names, jit, errors, changed),
            AttrArg::Named { value, .. } => walk_expr(value, names, jit, errors, changed),
        }
    }
    if let Some(b) = block.as_mut() {
        walk_block(b, names, jit, errors, changed);
    }

    let macro_name = name.name.clone();
    let span = *at_span;
    let args_owned = std::mem::take(args);
    let input = match block.take() {
        Some(b) => Node::Block(*b),
        None => Node::Args(positional_exprs(&args_owned)),
    };
    let outcome = invoke_macro(jit, &macro_name, &args_owned, input, span, errors);
    *changed = true;
    match (outcome.had_error, outcome.node) {
        (false, Some(Node::Expr(out))) => *e = out,
        (false, Some(Node::Block(b))) => e.kind = ExprKind::Block(b),
        (false, Some(Node::ErrorMarker)) | (true, _) => {
            // Reported its own error: leave a typed placeholder so the checker
            // does not additionally complain about an undefined macro.
            e.kind = ExprKind::Null;
        }
        (false, Some(other)) => {
            errors.push(SemaError::message(
                span,
                format!(
                    "macro `@{macro_name}` in expression position must return an expression \
                     or block, but returned a `{}` node",
                    arena::node_kind(&other)
                ),
            ));
            e.kind = ExprKind::Null;
        }
        (false, None) => e.kind = ExprKind::Null,
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

fn diag_to_sema(d: arena::MacroDiag) -> SemaError {
    let prefix = match d.level {
        DiagLevel::Error => "",
        DiagLevel::Warn => "warning: ",
        DiagLevel::Note => "note: ",
    };
    SemaError::message(d.span, format!("{prefix}{}", d.message))
}
