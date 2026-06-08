//! The definition table and module tree.
//!
//! [`Program::collect`] walks the parsed modules once and assigns a [`DefId`]
//! to every nameable entity — items, struct fields, generic parameters, and
//! modules — recording each in a [`Def`] and registering names into the
//! owning module's two namespaces (types and values). This is phase 1 of
//! semantic analysis; reference resolution and type checking build on the
//! tables it produces.
//!
//! The language has exactly two namespaces, matching the spec's grammar: a
//! *type* namespace (`struct`, `interface`, `type` alias, `extern type`/`struct`)
//! and a *value* namespace (`function`, module-level `var`, `extern function`/
//! `var`). Unit/tuple structs occupy both — the bare name is a value
//! constructor and a type — so they are registered in each.

use crate::ast::*;
use crate::ids::{DefId, ModId};
use crate::imports::Scheme;
use crate::sema::diag::{SemaError, SemaErrorKind};
use crate::sema::resolve_ctx::ResolveContext;
use crate::sema::stdlib::StdProvider;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

/// The diagnostic for a scheme used without project context (`docs/17` §17.13).
fn no_project_message(scheme: Scheme) -> String {
    match scheme {
        Scheme::Pkg => "`pkg:` import requires a project manifest (global package resolution \
                        is not yet available)"
            .to_string(),
        // Both `self:` forms.
        _ => "`self:` import requires a project: run inside a package, or add a project.toml"
            .to_string(),
    }
}

/// The diagnostic for a `self:` relative path that climbs above the package
/// (source) root (`docs/17` §17.4).
fn escape_message(parsed: &crate::imports::ImportPath, ctx: &ResolveContext) -> String {
    let pkg = ctx.package_name.as_deref().unwrap_or("this package");
    format!("`{}` escapes package `{pkg}`", parsed.display_source())
}

/// Whether `target` is authorized by one `[file-imports] allow` entry, resolved
/// relative to the package `root`. A glob entry (`assets/**`, `gen/*`) authorizes
/// everything under its non-glob prefix; a plain entry authorizes its subtree.
fn path_matches_allow(root: &std::path::Path, entry: &str, target: &std::path::Path) -> bool {
    use crate::sema::resolve_ctx::normalize;
    // Take the literal prefix up to the first glob component.
    let prefix: std::path::PathBuf = entry
        .split('/')
        .take_while(|seg| !seg.contains('*'))
        .collect();
    let allowed = normalize(&root.join(prefix));
    target.starts_with(&allowed)
}

/// Parsed bodies of file-backed submodules, keyed by their module path relative
/// to the crate root (e.g. `["util", "helpers"]`). The driver builds this by
/// discovering `mod` declarations and loading the corresponding `.otter` files;
/// single-file builds pass an empty map.
pub type Externals = HashMap<Vec<String>, Module>;

/// The literal text of an import path string (`import … from "util/helpers"`).
/// Paths are plain string literals, so only `Text` parts contribute.
/// Whether a method's parameter list begins with (or contains) a `self`
/// parameter. A method without `self` is a *static* method (`docs/09`/`docs/10`).
fn has_self_param(params: &[Param]) -> bool {
    params
        .iter()
        .any(|p| matches!(p.kind, ParamKind::SelfParam))
}

fn import_path_string(lit: &StringLit) -> String {
    let mut s = String::new();
    for part in &lit.parts {
        if let StringPart::Text { text, .. } = part {
            s.push_str(text);
        }
    }
    s
}

/// What kind of entity a [`DefId`] names.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum DefKind {
    Function,
    Struct,
    Interface,
    TypeAlias,
    Module,
    ModuleVar,
    /// A field of a record or tuple struct.
    Field,
    /// A generic type parameter on a function/struct/interface/extend/alias.
    GenericParam,
    /// A method declared inside an `interface` body.
    InterfaceMethod,
    /// A method (or static fn) declared inside an `extend` block.
    ExtendMethod,
    /// An `extend` block itself (anonymous; carries methods + impl target).
    Extend,
    ExternFunction,
    ExternStruct,
    ExternType,
    ExternVar,
    /// A `test "name" { … }` declaration (`docs/23`): a zero-argument unit body
    /// run by `otter_fusion test`.
    Test,
}

impl DefKind {
    pub fn describe(self) -> &'static str {
        use DefKind::*;
        match self {
            Function => "function",
            Struct => "struct",
            Interface => "interface",
            TypeAlias => "type alias",
            Module => "module",
            ModuleVar => "variable",
            Field => "field",
            GenericParam => "generic parameter",
            InterfaceMethod => "interface method",
            ExtendMethod => "method",
            Extend => "extension",
            ExternFunction => "extern function",
            ExternStruct => "extern struct",
            ExternType => "extern type",
            ExternVar => "extern variable",
            Test => "test",
        }
    }

    /// Does this kind occupy the type namespace?
    fn in_type_ns(self) -> bool {
        use DefKind::*;
        matches!(
            self,
            Struct | Interface | TypeAlias | ExternStruct | ExternType
        )
    }

    /// Does this kind occupy the value namespace (a bare-name expression)?
    fn in_value_ns(self) -> bool {
        use DefKind::*;
        matches!(self, Function | ModuleVar | ExternFunction | ExternVar)
    }
}

/// One entry in the definition table.
#[derive(Clone, Debug)]
pub struct Def {
    pub kind: DefKind,
    pub name: String,
    /// The module this definition lives in.
    pub module: ModId,
    /// The enclosing definition, when nested: a field's struct, a generic
    /// param's owner, a method's `extend`/`interface`.
    pub parent: Option<DefId>,
    pub public: bool,
    pub span: Span,
    /// For item-level defs, a clone of the AST so later phases reach the
    /// signature and body without borrowing the original tree.
    pub item: Option<ItemKind>,
    /// The decorators written above the item (`@Align(N)`, `@Packed`, …).
    /// FFI layout decorators (`docs/19` §3) are read off here by the backend.
    pub attrs: Vec<Attribute>,
    /// Generic parameter defs declared directly on this def, in order.
    pub generics: Vec<DefId>,
    /// For a `GenericParam` def, its interface bounds (`T: A + B`) as written.
    /// Lowered on demand by the checker against the owner's type environment.
    pub param_bounds: Vec<Type>,
    /// For an `ExtendMethod`/`InterfaceMethod` def, whether it is a *static*
    /// method — it takes no `self` and is called as `Type.method(...)`
    /// (`docs/09` §6, `docs/10`). Computed from the absence of a `self` param.
    pub is_static: bool,
}

#[derive(Clone, Debug)]
pub struct ModuleImport {
    pub item: ImportItem,
    pub public: bool,
    pub target: Option<ModId>,
    pub toolchain: bool,
}

/// A node in the module tree.
#[derive(Clone, Debug)]
pub struct ModuleInfo {
    pub id: ModId,
    pub name: String,
    pub parent: Option<ModId>,
    pub public: bool,
    /// This module's path from the crate root (root = `[]`, a child `util` =
    /// `["util"]`). Used to resolve relative `self:` imports against the
    /// importing module's position and to invert file paths.
    pub path: Vec<String>,
    /// `true` for a file-backed `mod foo` whose body has not been loaded yet
    /// (filled by the driver in multi-file builds).
    pub external_unloaded: bool,
    /// Child modules, by name.
    pub children: HashMap<String, ModId>,
    /// Name → def in the type namespace.
    pub types: HashMap<String, DefId>,
    /// Name → def in the value namespace.
    pub values: HashMap<String, DefId>,
    /// All `extend` blocks declared directly in this module.
    pub extends: Vec<DefId>,
    /// `import` declarations in this module, in source order.
    pub imports: Vec<ModuleImport>,
    /// Names brought into scope by `import { … } from "…"`, in the type
    /// namespace (resolved by [`Program::resolve_imports`]).
    pub imported_types: HashMap<String, DefId>,
    /// As [`Self::imported_types`], for the value namespace.
    pub imported_values: HashMap<String, DefId>,
    /// Publicly re-exported named imports in the type namespace.
    pub public_imported_types: HashMap<String, DefId>,
    /// Publicly re-exported named imports in the value namespace.
    pub public_imported_values: HashMap<String, DefId>,
    /// `import "path" as M` aliases: `M` → the aliased module. Member access
    /// `M.foo` resolves against that module's public definitions.
    pub namespace_imports: HashMap<String, ModId>,
    /// Publicly re-exported namespace imports (`pub import "path" as M`).
    pub public_namespace_imports: HashMap<String, ModId>,
    /// Modules imported by any source import form. Their `extend` blocks are
    /// visible for method/interface resolution in this module (`docs/17` §17.5).
    pub extension_imports: Vec<ModId>,
    /// Public imports whose extension activation is re-exported by this module
    /// (`docs/17` §17.10), so importers see those `extend` blocks transitively.
    pub public_extension_imports: Vec<ModId>,
}

impl ModuleInfo {
    fn new(id: ModId, name: String, parent: Option<ModId>, public: bool) -> Self {
        ModuleInfo {
            id,
            name,
            parent,
            public,
            path: Vec::new(),
            external_unloaded: false,
            children: HashMap::new(),
            types: HashMap::new(),
            values: HashMap::new(),
            extends: Vec::new(),
            imports: Vec::new(),
            imported_types: HashMap::new(),
            imported_values: HashMap::new(),
            public_imported_types: HashMap::new(),
            public_imported_values: HashMap::new(),
            namespace_imports: HashMap::new(),
            public_namespace_imports: HashMap::new(),
            extension_imports: Vec::new(),
            public_extension_imports: Vec::new(),
        }
    }
}

/// The whole program under analysis: the definition table, module tree, and
/// accumulated diagnostics.
pub struct Program {
    pub defs: Vec<Def>,
    pub modules: Vec<ModuleInfo>,
    pub errors: Vec<SemaError>,
    /// The compiler-injected `List<T>` type definition.
    pub list_def: DefId,
    /// The compiler-injected `Map<K, V>` type definition.
    pub map_def: DefId,
    /// The builtin `Set<T>` type definition (from `core:collections`).
    pub set_def: DefId,
    /// `core:prelude` `Item<T>` — the iterator protocol's element wrapper.
    pub item_def: DefId,
    /// `core:prelude` `Done` — the iterator protocol's end marker.
    pub done_def: DefId,
    /// `core:prelude` `Iterator<T>`.
    pub iterator_def: DefId,
    /// `core:collections::Entry<K, V>` — yielded by `for entry in map`.
    pub entry_def: DefId,
    /// `core:prelude` `FromResidual<R>` — error conversion for `?` (`docs/13`).
    pub from_residual_def: DefId,
    /// `core:prelude` `Try<Output, Residual>` — lets a non-union wrapper type
    /// participate in `?` (`docs/13` §3): `branch(self)` splits the wrapper
    /// into its success and failure variants.
    pub try_def: DefId,
    /// `core:prelude` `Clone` — deep-copy entry point (`docs/10`/`docs/15`).
    pub clone_def: DefId,
    /// `core:prelude` `Drop` — finalizer run before reclamation (`docs/16` §8).
    pub drop_def: DefId,
    /// `std:thread` `JoinHandle<R>` — `Thread.spawn`'s result (`docs/20`).
    pub join_handle_def: DefId,
    /// `std:task` `JoinHandle<R>` — `Task.spawn`'s result (`docs/20`, `docs/21`).
    pub task_join_handle_def: DefId,
    /// `std:thread` `Joined<R>` — a worker's value after `join` (`docs/20`).
    pub joined_def: DefId,
    /// `std:thread` `Panicked` — a worker that panicked (`docs/20`).
    pub panicked_def: DefId,
    /// `std:task` `Cancelled` — a cooperatively cancelled executor task.
    pub cancelled_def: DefId,
    /// `std:sync` `Sender<T>` — a channel's sending end (`docs/20` §2).
    pub sender_def: DefId,
    /// `std:sync` `Receiver<T>` — a channel's receiving end (`docs/20` §2).
    pub receiver_def: DefId,
    /// `std:sync` `ChannelClosed` — returned by a closed channel.
    pub channel_closed_def: DefId,
    /// `std:sync` `Shared<T>` — a mutex handle (`docs/20` §4).
    pub shared_def: DefId,
    /// `std:sync` `LockBusy` — `try_lock` failure.
    pub lock_busy_def: DefId,
    /// `core:async` `Pending` — a future that is not yet ready (`docs/21` §1).
    pub pending_def: DefId,
    /// `core:async` `Ready<T>` — a completed future's value (`docs/21` §1).
    pub ready_def: DefId,
    /// `core:async` `Future<Output>` — the async state-machine shape.
    pub future_def: DefId,
    /// `core:async` `Context` — carries the waker (`docs/21` §2).
    pub context_def: DefId,
    /// `core:async` `AsyncIterator<T>` — async streams (`docs/21` §10).
    pub async_iterator_def: DefId,
    /// `std:async::TimedOut` — `timeout` loser marker (`docs/21` §9).
    pub timed_out_def: DefId,
    /// `core:prelude` `Eq` — structural equality (`docs/15`); the `T: Eq`
    /// bound for `@Derive(Eq)` on generic structs.
    pub eq_def: DefId,
    /// `core:prelude` `Ord` — total ordering (`docs/15`); the `T: Ord` bound for
    /// `@Derive(Ord)` on generic structs.
    pub ord_def: DefId,
    /// `core:prelude` `ToStr` — string rendering (`docs/15`/`docs/01` §8); the
    /// `T: ToStr` bound for `@Derive(ToStr)` on generic structs.
    pub to_str_def: DefId,
    /// `core:prelude` `Hash` — structural hashing (`docs/15` §7); the
    /// `T: Hash` bound for `@Derive(Hash)` on generic structs and for
    /// `Map<K, V>` keys.
    pub hash_def: DefId,
    /// `std:fmt::Debug` — diagnostic rendering. Primitive/`str` impls are
    /// compiler intrinsics; user and stdlib values satisfy it with normal
    /// `extend … : Debug` blocks.
    pub debug_def: DefId,
    /// Toolchain-private `MapKeys<K>` — the `Iterator<K>` returned by `Map.keys()`
    /// (`docs/18` §6). Holds a snapshot `List<K>` of the keys at call time.
    pub map_keys_def: DefId,
    /// Toolchain-private `MapValues<V>` — the `Iterator<V>` returned by
    /// `Map.values()`. Holds a snapshot `List<V>` of the values at call time.
    pub map_values_def: DefId,
    /// Toolchain-private `MapEntries<K, V>` — the `Iterator<Entry<K, V>>`
    /// returned by `Map.entries()`. Holds a reference to the map plus a snapshot
    /// of its keys; values are looked up lazily as each `next()` runs.
    pub map_entries_def: DefId,
    /// Toolchain-private `ListIter<T>` — the `Iterator<T>` returned by
    /// `List.iter()` (`docs/18` §5). Holds the live list plus a cursor (reads
    /// through to the list, so it is a view, not a snapshot).
    pub list_iter_def: DefId,
    /// Toolchain-private `StrChars` — the `Iterator<char>` returned by
    /// `str.chars()` (`docs/18` §4). Holds a snapshot `List<char>` of the
    /// string's Unicode scalars at call time.
    pub str_chars_def: DefId,
    /// Toolchain-private `StrBytes` — the `Iterator<u8>` returned by
    /// `str.bytes()`. Holds a snapshot `List<u8>` of the string's UTF-8 bytes at
    /// call time.
    pub str_bytes_def: DefId,
    /// Names that are implicitly visible in every module.
    ///
    /// This is intentionally empty today. `core:prelude` is a compiler
    /// desugaring table and an explicit import target, not a bag of names made
    /// visible by default (`docs/17` §17.8). Keep these maps empty unless the
    /// language design deliberately adds a real universal prelude.
    pub prelude_types: HashMap<String, DefId>,
    pub prelude_values: HashMap<String, DefId>,
    /// A synthetic module aggregating the toolchain (`core:`/`std:`) definitions
    /// before the catalog builds curated public import views over them.
    pub builtin_module: ModId,
    /// The named toolchain modules: `["core","collections"]` → its module, etc.
    /// Each is a curated view over `__builtins__` exposing exactly the names
    /// that module publishes (`docs/17` §17.8). `core:`/`std:` imports resolve
    /// against these.
    pub builtin_modules: HashMap<Vec<String>, ModId>,
    /// Hidden modules that own the bundled Otter-authored toolchain source.
    ///
    /// The previous bootstrap collected every source file into `__builtins__`.
    /// That made all `std:*` modules share one namespace, so ordinary names like
    /// `sleep` could not exist in both `std:async` and `std:time`. Source files
    /// now live in module-local hidden owners and [`Self::builtin_modules`]
    /// exposes provider-selected public views over those owners.
    pub builtin_source_modules: HashMap<Vec<String>, ModId>,
    /// Name of the provider that built [`Self::builtin_modules`]. Used for
    /// diagnostics when a known toolchain module is absent from that provider.
    pub std_provider_name: String,
    /// Toolchain marker functions (`print`/`println`/`panic`/…) → the builtin
    /// they dispatch to. A call whose callee resolves to one of these `DefId`s
    /// lowers to the builtin intrinsic, so the names are ordinary importable
    /// symbols (`docs/17` §17.8) rather than magic.
    pub builtin_fns: HashMap<DefId, crate::sema::results::Builtin>,
    /// Resolved dependency packages: package-instance key → the root module of
    /// that dependency's collected subtree. `pkg:<name>` imports resolve against
    /// the key selected by the importing package's dependency context
    /// (`docs/17` §17.4).
    pub package_roots: HashMap<String, ModId>,
    /// `file:` import targets: normalized target file → its collected module.
    /// A `file:` import resolves into this module's public surface.
    pub file_modules: HashMap<std::path::PathBuf, ModId>,
}

impl Default for Program {
    fn default() -> Self {
        Self::new()
    }
}

impl Program {
    pub fn new() -> Self {
        let root = ModuleInfo::new(ModId::ROOT, "crate".into(), None, true);
        Program {
            defs: Vec::new(),
            modules: vec![root],
            errors: Vec::new(),
            list_def: DefId(0),
            map_def: DefId(0),
            set_def: DefId(0),
            item_def: DefId(0),
            done_def: DefId(0),
            iterator_def: DefId(0),
            entry_def: DefId(0),
            from_residual_def: DefId(0),
            try_def: DefId(0),
            clone_def: DefId(0),
            drop_def: DefId(0),
            join_handle_def: DefId(0),
            task_join_handle_def: DefId(0),
            joined_def: DefId(0),
            panicked_def: DefId(0),
            cancelled_def: DefId(0),
            sender_def: DefId(0),
            receiver_def: DefId(0),
            channel_closed_def: DefId(0),
            shared_def: DefId(0),
            lock_busy_def: DefId(0),
            pending_def: DefId(0),
            ready_def: DefId(0),
            future_def: DefId(0),
            context_def: DefId(0),
            async_iterator_def: DefId(0),
            timed_out_def: DefId(0),
            eq_def: DefId(0),
            ord_def: DefId(0),
            to_str_def: DefId(0),
            hash_def: DefId(0),
            debug_def: DefId(0),
            map_keys_def: DefId(0),
            map_values_def: DefId(0),
            map_entries_def: DefId(0),
            list_iter_def: DefId(0),
            str_chars_def: DefId(0),
            str_bytes_def: DefId(0),
            prelude_types: HashMap::new(),
            prelude_values: HashMap::new(),
            builtin_module: ModId::ROOT,
            builtin_modules: HashMap::new(),
            builtin_source_modules: HashMap::new(),
            std_provider_name: "builtin".into(),
            builtin_fns: HashMap::new(),
            package_roots: HashMap::new(),
            file_modules: HashMap::new(),
        }
    }

    /// Collect every definition from a parsed root module (single-file build).
    /// Inline `mod { .. }` blocks are descended into; file-backed `mod foo`
    /// declarations register an empty child module marked `external_unloaded`.
    pub fn collect(module: &Module) -> Program {
        Self::collect_multi(module, &Externals::new())
    }

    /// Collect a whole multi-file program: the root module plus the parsed
    /// bodies of every file-backed submodule, keyed by their module path
    /// relative to the crate root (e.g. `["util", "helpers"]`). A file-backed
    /// `mod foo` whose body is present in `externals` is descended into; one
    /// that is absent stays `external_unloaded` (single-file builds pass none).
    pub fn collect_multi(root: &Module, externals: &Externals) -> Program {
        Self::collect_multi_ctx(root, externals, &ResolveContext::direct())
    }

    /// As [`Self::collect_multi`], with an explicit [`ResolveContext`] (run mode
    /// + project) governing which import schemes are available and how `self:`
    /// relative paths resolve (`docs/17` §17.13).
    pub fn collect_multi_ctx(
        root: &Module,
        externals: &Externals,
        ctx: &ResolveContext,
    ) -> Program {
        let provider = crate::sema::stdlib::active_provider();
        Self::collect_multi_ctx_with_provider(root, externals, ctx, &provider)
    }

    /// As [`Self::collect_multi_ctx`], but with an explicit stdlib provider.
    ///
    /// The CLI still selects the built-in provider today, but keeping semantic
    /// collection parameterized here lets future target/sysroot configuration
    /// replace, omit, or extend `std:*` views without changing import syntax or
    /// the rest of module resolution.
    pub fn collect_multi_ctx_with_provider(
        root: &Module,
        externals: &Externals,
        ctx: &ResolveContext,
        provider: &dyn StdProvider,
    ) -> Program {
        let mut p = Program::new();
        p.std_provider_name = if crate::sema::stdlib::valid_provider_name(provider.name()) {
            provider.name().to_string()
        } else {
            "<invalid>".to_string()
        };
        // Toolchain definitions live in `__builtins__` — *not* `ROOT` — so
        // their names do not pollute user scope (`docs/17` §17.8: every named
        // symbol requires an import). Built-in *syntax* still resolves via the
        // stored core `DefId`s, and toolchain `extend` blocks are scanned
        // program-wide by method resolution.
        let builtins = p.new_module("__builtins__".into(), ModId::ROOT, true);
        p.builtin_module = builtins;
        p.inject_builtins(builtins);
        p.collect_toolchain_source(builtins);
        p.build_toolchain_internal_imports();
        p.build_builtin_views(provider);
        // Near-empty prelude (`docs/17` §17.8): built-in *syntax* resolves via
        // the stored toolchain `DefId`s, but no built-in *name* is universally
        // visible — every named symbol (`List`, `Map`, `print`, `panic`, …) must
        // be imported. The universal-visibility maps stay empty.
        p.collect_items(ModId::ROOT, &root.items, externals, &[]);
        // Collect each resolved dependency package instance as a standalone
        // subtree (not reachable through the user `mod` tree). Multiple versions
        // of the same package name are distinct keys; `pkg:<name>` resolution
        // chooses the key from the importing package's dependency context.
        let mut package_keys: HashMap<String, Vec<String>> = HashMap::new();
        for key in ctx.packages.values() {
            if let Some(id) = package_instance_id(key) {
                package_keys.insert(id.to_string(), key.clone());
            }
        }
        for deps in ctx.package_dependencies.values() {
            for key in deps.values() {
                if let Some(id) = package_instance_id(key) {
                    package_keys.insert(id.to_string(), key.clone());
                }
            }
        }
        let mut pkgs: Vec<(String, Vec<String>)> = package_keys.into_iter().collect();
        pkgs.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, key) in pkgs {
            if let Some(entry) = externals.get(&key) {
                let pkg_mod = p.new_module(format!("__pkg__{id}"), ModId::ROOT, true);
                p.modules[pkg_mod.index()].path = key.clone();
                p.collect_items(pkg_mod, &entry.items, externals, &key);
                p.package_roots.insert(id, pkg_mod);
            }
        }
        // Collect each `file:` import target as a standalone module.
        let mut files: Vec<(&std::path::PathBuf, &Vec<String>)> = ctx.file_targets.iter().collect();
        files.sort_by(|a, b| a.1.cmp(b.1));
        for (target, key) in files {
            if let Some(entry) = externals.get(key) {
                let m = p.new_module(format!("__file__{}", key.join("_")), ModId::ROOT, true);
                p.modules[m.index()].path = key.clone();
                p.collect_items(m, &entry.items, externals, key);
                p.file_modules.insert(target.clone(), m);
            }
        }
        p.resolve_imports(ctx);
        p
    }

    /// Lex, parse, and collect the bundled toolchain source files into hidden
    /// module-local owners under `__builtins__`. The toolchain source uses high
    /// synthetic `FileId`s so its spans never collide with user source or
    /// macro-generated files.
    fn collect_toolchain_source(&mut self, target: ModId) {
        for (index, spec) in crate::sema::stdlib::TOOLCHAIN_SOURCES.iter().enumerate() {
            let file = crate::span::FileId(u32::MAX - index as u32);
            let (tokens, lex_errs) = crate::lexer::lex(spec.source, file);
            debug_assert!(
                lex_errs.is_empty(),
                "toolchain stdlib lex errors in {:?}: {lex_errs:?}",
                spec.path
            );
            let (module, parse_errs) = crate::parser::parse(spec.source, &tokens);
            debug_assert!(
                parse_errs.is_empty(),
                "toolchain stdlib parse errors in {:?}: {parse_errs:?}",
                spec.path
            );
            // The bundled toolchain source has no file-backed submodules yet.
            let source_module =
                self.new_module(format!("__src_{}", spec.path.join("_")), target, true);
            let path: Vec<String> = spec.path.iter().map(|s| s.to_string()).collect();
            self.modules[source_module.index()].path = path.clone();
            self.collect_items(source_module, &module.items, &Externals::new(), &path);
            self.builtin_source_modules.insert(path, source_module);
        }
        self.item_def = self.toolchain_type("Item").unwrap_or(DefId(0));
        self.done_def = self.toolchain_type("Done").unwrap_or(DefId(0));
        self.iterator_def = self.toolchain_type("Iterator").unwrap_or(DefId(0));
        self.entry_def = self.toolchain_type("Entry").unwrap_or(DefId(0));
        self.from_residual_def = self.toolchain_type("FromResidual").unwrap_or(DefId(0));
        self.try_def = self.toolchain_type("Try").unwrap_or(DefId(0));
        self.clone_def = self.toolchain_type("Clone").unwrap_or(DefId(0));
        self.drop_def = self.toolchain_type("Drop").unwrap_or(DefId(0));
        self.join_handle_def = self
            .toolchain_source_type(&["std", "thread"], "JoinHandle")
            .unwrap_or(DefId(0));
        self.task_join_handle_def = self
            .toolchain_source_type(&["std", "task"], "JoinHandle")
            .unwrap_or(DefId(0));
        self.joined_def = self.toolchain_type("Joined").unwrap_or(DefId(0));
        self.panicked_def = self.toolchain_type("Panicked").unwrap_or(DefId(0));
        self.cancelled_def = self
            .toolchain_source_type(&["std", "task"], "Cancelled")
            .unwrap_or(DefId(0));
        self.sender_def = self.toolchain_type("Sender").unwrap_or(DefId(0));
        self.receiver_def = self.toolchain_type("Receiver").unwrap_or(DefId(0));
        self.channel_closed_def = self.toolchain_type("ChannelClosed").unwrap_or(DefId(0));
        self.shared_def = self.toolchain_type("Shared").unwrap_or(DefId(0));
        self.lock_busy_def = self.toolchain_type("LockBusy").unwrap_or(DefId(0));
        self.pending_def = self.toolchain_type("Pending").unwrap_or(DefId(0));
        self.ready_def = self.toolchain_type("Ready").unwrap_or(DefId(0));
        self.future_def = self.toolchain_type("Future").unwrap_or(DefId(0));
        self.context_def = self.toolchain_type("Context").unwrap_or(DefId(0));
        self.async_iterator_def = self.toolchain_type("AsyncIterator").unwrap_or(DefId(0));
        self.timed_out_def = self.toolchain_type("TimedOut").unwrap_or(DefId(0));
        self.eq_def = self.toolchain_type("Eq").unwrap_or(DefId(0));
        self.ord_def = self.toolchain_type("Ord").unwrap_or(DefId(0));
        self.to_str_def = self.toolchain_type("ToStr").unwrap_or(DefId(0));
        self.hash_def = self.toolchain_type("Hash").unwrap_or(DefId(0));
        self.debug_def = self
            .toolchain_source_type(&["std", "fmt"], "Debug")
            .unwrap_or(DefId(0));
        self.set_def = self.toolchain_type("Set").unwrap_or(DefId(0));
        self.map_keys_def = self.toolchain_type("MapKeys").unwrap_or(DefId(0));
        self.map_values_def = self.toolchain_type("MapValues").unwrap_or(DefId(0));
        self.map_entries_def = self.toolchain_type("MapEntries").unwrap_or(DefId(0));
        self.list_iter_def = self.toolchain_type("ListIter").unwrap_or(DefId(0));
        self.str_chars_def = self.toolchain_type("StrChars").unwrap_or(DefId(0));
        self.str_bytes_def = self.toolchain_type("StrBytes").unwrap_or(DefId(0));
        // Map the marker functions to their builtin intrinsics. A call resolving
        // to one of these defs lowers to the builtin (`docs/14`, `docs/24`).
        // `std:io` print helpers are deliberately not here: public stream writes
        // are async stdlib functions, not immediate compiler intrinsics.
        use crate::sema::results::Builtin;
        for (name, b) in [
            ("panic", Builtin::Panic),
            ("panic_with", Builtin::PanicWith),
            ("exit", Builtin::Exit),
            ("abort", Builtin::Abort),
        ] {
            if let Some(d) = self.toolchain_value(name) {
                self.builtin_fns.insert(d, b);
            }
        }
    }

    /// Make bundled stdlib source modules see the unique toolchain symbols they
    /// depend on (`Eq`, `List`, `IoError`, `Debug`, …) without merging all source
    /// files into one namespace. Ambiguous duplicate names are deliberately not
    /// imported; a source file that needs one must use its own local definition
    /// or grow explicit internal imports later.
    fn build_toolchain_internal_imports(&mut self) {
        fn record(table: &mut HashMap<String, Option<DefId>>, name: &str, def: DefId) {
            match table.get_mut(name) {
                Some(slot) => {
                    if slot.is_some_and(|existing| existing != def) {
                        *slot = None;
                    }
                }
                None => {
                    table.insert(name.to_string(), Some(def));
                }
            }
        }

        let mut types: HashMap<String, Option<DefId>> = HashMap::new();
        let mut values: HashMap<String, Option<DefId>> = HashMap::new();
        for (name, def) in &self.modules[self.builtin_module.index()].types {
            record(&mut types, name, *def);
        }
        for (name, def) in &self.modules[self.builtin_module.index()].values {
            record(&mut values, name, *def);
        }
        let source_modules: Vec<ModId> = self.builtin_source_modules.values().copied().collect();
        for module in &source_modules {
            for (name, def) in &self.modules[module.index()].types {
                record(&mut types, name, *def);
            }
            for (name, def) in &self.modules[module.index()].values {
                record(&mut values, name, *def);
            }
        }
        let unique_types: Vec<(String, DefId)> = types
            .into_iter()
            .filter_map(|(name, def)| def.map(|def| (name, def)))
            .collect();
        let unique_values: Vec<(String, DefId)> = values
            .into_iter()
            .filter_map(|(name, def)| def.map(|def| (name, def)))
            .collect();
        for module in source_modules {
            for (name, def) in &unique_types {
                if !self.modules[module.index()].types.contains_key(name) {
                    self.modules[module.index()]
                        .imported_types
                        .entry(name.clone())
                        .or_insert(*def);
                }
            }
            for (name, def) in &unique_values {
                if !self.modules[module.index()].values.contains_key(name) {
                    self.modules[module.index()]
                        .imported_values
                        .entry(name.clone())
                        .or_insert(*def);
                }
            }
        }
    }

    fn toolchain_type(&self, name: &str) -> Option<DefId> {
        self.modules[self.builtin_module.index()]
            .types
            .get(name)
            .copied()
            .or_else(|| self.unique_source_type(name))
    }

    fn toolchain_value(&self, name: &str) -> Option<DefId> {
        self.modules[self.builtin_module.index()]
            .values
            .get(name)
            .copied()
            .or_else(|| self.unique_source_value(name))
    }

    fn toolchain_source_type(&self, path: &[&str], name: &str) -> Option<DefId> {
        let path: Vec<String> = path.iter().map(|seg| (*seg).to_string()).collect();
        let module = self.builtin_source_modules.get(&path).copied()?;
        self.modules[module.index()].types.get(name).copied()
    }

    fn unique_source_type(&self, name: &str) -> Option<DefId> {
        let mut found = None;
        for module in self.builtin_source_modules.values().copied() {
            if let Some(def) = self.modules[module.index()].types.get(name).copied() {
                if found.is_some_and(|existing| existing != def) {
                    return None;
                }
                found = Some(def);
            }
        }
        found
    }

    fn unique_source_value(&self, name: &str) -> Option<DefId> {
        let mut found = None;
        for module in self.builtin_source_modules.values().copied() {
            if let Some(def) = self.modules[module.index()].values.get(name).copied() {
                if found.is_some_and(|existing| existing != def) {
                    return None;
                }
                found = Some(def);
            }
        }
        found
    }

    /// Whether `def` is a method of the `core:compiler` macro-authoring surface
    /// (`extend ASTNode/MacroContext/Span`, `docs/22`). These methods call the
    /// `__ast_*`/`__mctx_*` host externs, which only exist inside the macro JIT;
    /// they are therefore compiled on demand (when a macro actually calls them)
    /// and must never be eagerly seeded into a normal program's code generation
    /// — doing so would emit unresolved references in native object output.
    pub fn is_macro_surface_method(&self, def: DefId) -> bool {
        let d = self.def(def);
        if d.kind != DefKind::ExtendMethod || !self.is_builtin_def(def) {
            return false;
        }
        let Some(parent) = d.parent else { return false };
        let Some(crate::ast::ItemKind::Extend(e)) = &self.def(parent).item else {
            return false;
        };
        matches!(
            &e.target.kind,
            crate::ast::TypeKind::Named { name, .. }
                if matches!(name.name.as_str(), "ASTNode" | "MacroContext" | "Span")
        )
    }

    /// The builtin a marker-function `DefId` dispatches to, if any.
    pub fn builtin_of_def(&self, def: DefId) -> Option<crate::sema::results::Builtin> {
        self.builtin_fns.get(&def).copied()
    }

    /// Whether `def` is a toolchain (`core:`/`std:`) definition — it lives in
    /// `__builtins__`. Used to tell an *imported* builtin name apart from a
    /// *user* shadow when recognizing built-in intrinsics.
    pub fn is_builtin_def(&self, def: DefId) -> bool {
        self.is_builtin_module(self.defs[def.index()].module)
    }

    fn is_builtin_module(&self, module: ModId) -> bool {
        module == self.builtin_module
            || self
                .builtin_source_modules
                .values()
                .any(|source| *source == module)
    }

    fn is_builtin_view_module(&self, module: ModId) -> bool {
        self.builtin_modules.values().any(|view| *view == module)
    }

    /// Build the curated `core:`/`std:` module views over `__builtins__`
    /// (`docs/17` §17.8). Each view exposes exactly the names that module
    /// publishes; internal iterator adapters (`ListIter`, `MapKeys`, …) are not
    /// exposed. Module metadata lives in `sema::stdlib` so future target std
    /// providers can replace or extend this catalog in one place.
    fn build_builtin_views(&mut self, provider: &dyn StdProvider) {
        let span = Span::new(
            crate::span::FileId(0),
            crate::span::BytePos(0),
            crate::span::BytePos(0),
        );
        let provider_name = provider.name();
        if !crate::sema::stdlib::valid_provider_name(provider_name) {
            let display_name = if provider_name.is_empty() {
                "<empty>"
            } else {
                provider_name
            };
            self.errors.push(SemaError::new(
                SemaErrorKind::Message(format!(
                    "stdlib provider name `{display_name}` is invalid; provider names must be \
                     non-empty ASCII identifiers using letters, digits, `.`, `_`, or `-`"
                )),
                span,
            ));
            return;
        }
        let mut seen_paths = std::collections::HashSet::new();
        for spec in provider.modules() {
            let path_vec: Vec<String> = spec.path.iter().map(|s| s.to_string()).collect();
            let display_path = crate::sema::stdlib::display_module_path(spec.path);
            if !seen_paths.insert(path_vec.clone()) {
                self.errors.push(SemaError::new(
                    SemaErrorKind::Message(format!(
                        "stdlib provider `{}` defines duplicate module `{}`",
                        provider_name, display_path
                    )),
                    span,
                ));
                continue;
            }
            if spec.path.len() < 2 {
                self.errors.push(SemaError::new(
                    SemaErrorKind::Message(format!(
                        "stdlib provider `{}` module `{}` must include a scheme and module path",
                        provider_name, display_path
                    )),
                    span,
                ));
                continue;
            }
            if let Some(seg) =
                spec.path.iter().skip(1).find(|seg| {
                    seg.is_empty() || **seg == "." || **seg == ".." || seg.contains('/')
                })
            {
                let display_segment = if seg.is_empty() { "<empty>" } else { *seg };
                self.errors.push(SemaError::new(
                    SemaErrorKind::Message(format!(
                        "stdlib provider `{}` module `{}` contains invalid path segment `{}`",
                        provider_name, display_path, display_segment
                    )),
                    span,
                ));
                continue;
            }
            let expected_root = match spec.tier {
                crate::sema::stdlib::StdTier::Core => "core",
                crate::sema::stdlib::StdTier::Std => "std",
            };
            if spec.path.first().copied() != Some(expected_root) {
                self.errors.push(SemaError::new(
                    SemaErrorKind::Message(format!(
                        "stdlib provider `{}` module `{}` has tier `{:?}` but path root is not `{}`",
                        provider_name,
                        display_path,
                        spec.tier,
                        expected_root
                    )),
                    span,
                ));
                continue;
            }
            let mut seen_exports = std::collections::HashSet::new();
            let mut resolved_exports = Vec::new();
            let mut valid_exports = true;
            let source_module = self.builtin_source_modules.get(&path_vec).copied();
            for name in spec.exports {
                if !seen_exports.insert(*name) {
                    self.errors.push(SemaError::new(
                        SemaErrorKind::Message(format!(
                            "stdlib provider `{}` module `{}` defines duplicate export `{}`",
                            provider_name, display_path, name
                        )),
                        span,
                    ));
                    valid_exports = false;
                    continue;
                }
                let type_def = source_module
                    .and_then(|module| self.modules[module.index()].types.get(*name).copied())
                    .or_else(|| {
                        self.modules[self.builtin_module.index()]
                            .types
                            .get(*name)
                            .copied()
                    })
                    .or_else(|| self.unique_source_type(name));
                let value_def = source_module
                    .and_then(|module| self.modules[module.index()].values.get(*name).copied())
                    .or_else(|| {
                        self.modules[self.builtin_module.index()]
                            .values
                            .get(*name)
                            .copied()
                    })
                    .or_else(|| self.unique_source_value(name));
                if type_def.is_none() && value_def.is_none() {
                    self.errors.push(SemaError::new(
                        SemaErrorKind::Message(format!(
                            "stdlib provider `{}` module `{}` exports `{}`, but the bundled \
                             toolchain source does not define that symbol",
                            provider_name, display_path, name
                        )),
                        span,
                    ));
                    valid_exports = false;
                }
                resolved_exports.push((*name, type_def, value_def));
            }
            if !valid_exports {
                continue;
            }
            let view =
                self.new_module(format!("__view_{}", spec.path.join("_")), ModId::ROOT, true);
            for (name, type_def, value_def) in resolved_exports {
                if let Some(d) = type_def {
                    self.modules[view.index()].types.insert(name.to_string(), d);
                }
                if let Some(d) = value_def {
                    self.modules[view.index()]
                        .values
                        .insert(name.to_string(), d);
                }
            }
            self.modules[view.index()].path = path_vec.clone();
            self.builtin_modules.insert(path_vec, view);
        }
    }

    /// Inject compiler-provided collection types (currently `List<T>` and
    /// `Map<K, V>`). These have no AST item; their behavior is special-cased in
    /// the checker and code generator. Injected before user items so they get
    /// stable low ids.
    fn inject_builtins(&mut self, target: ModId) {
        let span = Span::new(
            crate::span::FileId(0),
            crate::span::BytePos(0),
            crate::span::BytePos(0),
        );
        let list = self.add_def(DefKind::Struct, "List".into(), target, None, true, span);
        let t = self.add_def(
            DefKind::GenericParam,
            "T".into(),
            target,
            Some(list),
            false,
            span,
        );
        self.defs[list.index()].generics = vec![t];
        self.modules[target.index()]
            .types
            .insert("List".into(), list);
        self.list_def = list;

        let map = self.add_def(DefKind::Struct, "Map".into(), target, None, true, span);
        let k = self.add_def(
            DefKind::GenericParam,
            "K".into(),
            target,
            Some(map),
            false,
            span,
        );
        let v = self.add_def(
            DefKind::GenericParam,
            "V".into(),
            target,
            Some(map),
            false,
            span,
        );
        self.defs[map.index()].generics = vec![k, v];
        self.modules[target.index()].types.insert("Map".into(), map);
        self.map_def = map;
    }

    #[inline]
    pub fn def(&self, id: DefId) -> &Def {
        &self.defs[id.index()]
    }

    #[inline]
    pub fn module(&self, id: ModId) -> &ModuleInfo {
        &self.modules[id.index()]
    }

    /// Resolve a type-namespace name visible in `module`: the module's own
    /// definitions, then names it imported, then the universal prelude map
    /// (currently empty by design). Parent modules are *not* searched — names
    /// cross module boundaries only via `import` (`docs/17` §3).
    pub fn resolve_type_in(&self, module: ModId, name: &str) -> Option<DefId> {
        let m = &self.modules[module.index()];
        m.types
            .get(name)
            .or_else(|| m.imported_types.get(name))
            .or_else(|| self.prelude_types.get(name))
            .copied()
    }

    /// As [`Self::resolve_type_in`], for the value namespace.
    pub fn resolve_value_in(&self, module: ModId, name: &str) -> Option<DefId> {
        let m = &self.modules[module.index()];
        m.values
            .get(name)
            .or_else(|| m.imported_values.get(name))
            .or_else(|| self.prelude_values.get(name))
            .copied()
    }

    /// Resolve a type exported by `module`, including public named re-exports.
    pub fn resolve_pub_type_in(&self, module: ModId, name: &str) -> Option<DefId> {
        let mut seen = HashSet::new();
        self.resolve_pub_type_in_inner(module, name, &mut seen)
    }

    fn resolve_pub_type_in_inner(
        &self,
        module: ModId,
        name: &str,
        seen: &mut HashSet<(ModId, String)>,
    ) -> Option<DefId> {
        if !seen.insert((module, name.to_string())) {
            return None;
        }
        let m = &self.modules[module.index()];
        if let Some(def) = m.types.get(name).copied() {
            return (self.is_builtin_view_module(module) || self.defs[def.index()].public)
                .then_some(def);
        }
        if let Some(def) = m.public_imported_types.get(name).copied() {
            return Some(def);
        }
        for module_import in &m.imports {
            if !module_import.public {
                continue;
            }
            let Some(target) = module_import.target else {
                continue;
            };
            let ImportKind::Named(names) = &module_import.item.kind else {
                continue;
            };
            for import_name in names {
                let bind = import_name
                    .alias
                    .as_ref()
                    .unwrap_or(&import_name.name)
                    .name
                    .as_str();
                if bind == name {
                    return self.resolve_pub_type_in_inner(target, &import_name.name.name, seen);
                }
            }
        }
        None
    }

    /// Resolve a value exported by `module`, including public named re-exports.
    pub fn resolve_pub_value_in(&self, module: ModId, name: &str) -> Option<DefId> {
        let mut seen = HashSet::new();
        self.resolve_pub_value_in_inner(module, name, &mut seen)
    }

    fn resolve_pub_value_in_inner(
        &self,
        module: ModId,
        name: &str,
        seen: &mut HashSet<(ModId, String)>,
    ) -> Option<DefId> {
        if !seen.insert((module, name.to_string())) {
            return None;
        }
        let m = &self.modules[module.index()];
        if let Some(def) = m.values.get(name).copied() {
            return (self.is_builtin_view_module(module) || self.defs[def.index()].public)
                .then_some(def);
        }
        if let Some(def) = m.public_imported_values.get(name).copied() {
            return Some(def);
        }
        for module_import in &m.imports {
            if !module_import.public {
                continue;
            }
            let Some(target) = module_import.target else {
                continue;
            };
            let ImportKind::Named(names) = &module_import.item.kind else {
                continue;
            };
            for import_name in names {
                let bind = import_name
                    .alias
                    .as_ref()
                    .unwrap_or(&import_name.name)
                    .name
                    .as_str();
                if bind == name {
                    return self.resolve_pub_value_in_inner(target, &import_name.name.name, seen);
                }
            }
        }
        None
    }

    /// Resolve a value exported by an importable toolchain view.
    pub fn toolchain_export_value(&self, path: &[&str], name: &str) -> Option<DefId> {
        let key: Vec<String> = path.iter().map(|segment| (*segment).to_string()).collect();
        let module = self.builtin_modules.get(&key).copied()?;
        self.modules[module.index()].values.get(name).copied()
    }

    /// The `extend` blocks visible from `module` for method resolution: the
    /// module's own, every directly imported source module, plus all toolchain
    /// source modules. Built-in protocol impls — `List`/`Map`/`str` iterators,
    /// etc. — are program-wide (`docs/17` §17.8, orphan rule). User-authored
    /// extensions stay module-bound and become visible through explicit imports
    /// (`docs/17` §17.5).
    pub fn visible_extends(&self, module: ModId) -> Vec<DefId> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.push_extension_module(&mut out, &mut seen, module);
        for imported in self.modules[module.index()]
            .extension_imports
            .iter()
            .copied()
        {
            self.push_extension_module(&mut out, &mut seen, imported);
        }
        self.push_extension_module(&mut out, &mut seen, self.builtin_module);
        for source in self.builtin_source_modules.values().copied() {
            self.push_extension_module(&mut out, &mut seen, source);
        }
        out
    }

    fn push_extension_module(
        &self,
        out: &mut Vec<DefId>,
        seen: &mut HashSet<ModId>,
        module: ModId,
    ) {
        if !seen.insert(module) {
            return;
        }
        out.extend(self.modules[module.index()].extends.iter().copied());
        for reexport in self.modules[module.index()]
            .public_extension_imports
            .iter()
            .copied()
        {
            self.push_extension_module(out, seen, reexport);
        }
    }

    /// The module an `import … as alias` brings into scope in `module`.
    pub fn namespace_target(&self, module: ModId, alias: &str) -> Option<ModId> {
        self.modules[module.index()]
            .namespace_imports
            .get(alias)
            .copied()
    }

    /// The module a public namespace re-export exposes from `module`.
    pub fn public_namespace_target(&self, module: ModId, alias: &str) -> Option<ModId> {
        let mut seen = HashSet::new();
        self.public_namespace_target_inner(module, alias, &mut seen)
    }

    fn public_namespace_target_inner(
        &self,
        module: ModId,
        alias: &str,
        seen: &mut HashSet<(ModId, String)>,
    ) -> Option<ModId> {
        if !seen.insert((module, alias.to_string())) {
            return None;
        }
        let m = &self.modules[module.index()];
        if let Some(target) = m.public_namespace_imports.get(alias).copied() {
            return Some(target);
        }
        for module_import in &m.imports {
            if !module_import.public {
                continue;
            }
            let Some(target) = module_import.target else {
                continue;
            };
            let ImportKind::Namespace(import_alias) = &module_import.item.kind else {
                continue;
            };
            if import_alias.name == alias {
                return Some(target);
            }
        }
        None
    }

    /// Walk the module tree from the root along `segments` (e.g.
    /// `["util", "helpers"]`), returning the target module if it exists.
    fn module_by_path(&self, segments: &[String]) -> Option<ModId> {
        let mut cur = ModId::ROOT;
        for seg in segments {
            cur = self.modules[cur.index()].children.get(seg).copied()?;
        }
        Some(cur)
    }

    /// Resolve every module's `import` declarations (`docs/17` §17.3–§17.4,
    /// §17.13). Each path carries an explicit scheme; resolution and the schemes
    /// *available* depend on the [`ResolveContext`] (run mode + project).
    fn resolve_imports(&mut self, ctx: &ResolveContext) {
        let file_to_module = ctx.file_to_module();
        for mid in 0..self.modules.len() {
            let imports = self.modules[mid].imports.clone();
            let mod_path = self.modules[mid].path.clone();
            for (import_index, module_import) in imports.iter().enumerate() {
                let imp = &module_import.item;
                let raw = import_path_string(&imp.path);
                let span = imp.path.span;
                let parsed = match crate::imports::classify(&raw) {
                    Ok(p) => p,
                    Err(e) => {
                        self.errors
                            .push(SemaError::new(SemaErrorKind::Message(e.to_string()), span));
                        continue;
                    }
                };
                // Project-context gating (`docs/17` §17.13).
                if parsed.scheme.requires_project_context() && !ctx.project {
                    self.errors.push(SemaError::new(
                        SemaErrorKind::Message(no_project_message(parsed.scheme)),
                        span,
                    ));
                    continue;
                }
                // `no-std`: `std:` is unavailable.
                if parsed.scheme == Scheme::Std && ctx.no_std {
                    self.errors.push(SemaError::new(
                        SemaErrorKind::Message(format!(
                            "`{}` import: this package is `no-std`, so `std:` is unavailable",
                            parsed.display_source()
                        )),
                        span,
                    ));
                    continue;
                }

                match parsed.scheme {
                    // Toolchain modules, resolved against the curated named view
                    // for this exact path (`docs/17` §17.8).
                    Scheme::Core | Scheme::Std => {
                        let mut key = vec![parsed.scheme.keyword().to_string()];
                        key.extend(parsed.segments.iter().cloned());
                        match self.builtin_modules.get(&key).copied() {
                            Some(target) => self.record_resolved_import(
                                mid,
                                import_index,
                                target,
                                /* toolchain = */ true,
                                module_import.public,
                            ),
                            None => {
                                let message =
                                    if crate::sema::stdlib::TOOLCHAIN_MODULES.iter().any(|spec| {
                                        spec.path.iter().copied().eq(key.iter().map(String::as_str))
                                    }) {
                                        format!(
                                            "stdlib provider `{}` does not provide module `{}`",
                                            self.std_provider_name,
                                            parsed.display_source()
                                        )
                                    } else {
                                        format!("no built-in module `{}`", parsed.display_source())
                                    };
                                self.errors
                                    .push(SemaError::new(SemaErrorKind::Message(message), span));
                            }
                        }
                    }
                    Scheme::SelfRoot => match self.resolve_self_root(&parsed.segments) {
                        Ok(target) => self.record_resolved_import(
                            mid,
                            import_index,
                            target,
                            false,
                            module_import.public,
                        ),
                        Err(msg) => self
                            .errors
                            .push(SemaError::new(SemaErrorKind::Message(msg), span)),
                    },
                    Scheme::SelfRel => {
                        match self.resolve_self_rel(&mod_path, &parsed, ctx, &file_to_module) {
                            Ok(target) => self.record_resolved_import(
                                mid,
                                import_index,
                                target,
                                false,
                                module_import.public,
                            ),
                            Err(msg) => self
                                .errors
                                .push(SemaError::new(SemaErrorKind::Message(msg), span)),
                        }
                    }
                    Scheme::Pkg => {
                        let name = parsed.package_name().unwrap_or("").to_string();
                        let package_key = if let Some(owner) = package_instance_id(&mod_path) {
                            ctx.package_dependencies
                                .get(owner)
                                .and_then(|deps| deps.get(&name))
                        } else {
                            ctx.packages.get(&name)
                        };
                        let declared = if package_instance_id(&mod_path).is_some() {
                            package_key.is_some()
                        } else {
                            ctx.dependencies.contains(&name)
                        };
                        if !declared {
                            self.errors.push(SemaError::new(
                                SemaErrorKind::Message(format!(
                                    "no dependency named `{name}` in the manifest \
                                     (add it under `[dependencies]`)"
                                )),
                                span,
                            ));
                            continue;
                        }
                        // Resolve `pkg:<name>[/<sub>…]` into the collected
                        // dependency subtree, honoring `pub`/`pub mod` visibility.
                        let Some(package_key) = package_key else {
                            self.errors.push(SemaError::new(
                                SemaErrorKind::Message(format!(
                                    "dependency `{name}` is declared but could not be loaded \
                                     (run `otter_fusion lock` to resolve it)"
                                )),
                                span,
                            ));
                            continue;
                        };
                        let Some(package_id) = package_instance_id(package_key) else {
                            self.errors.push(SemaError::new(
                                SemaErrorKind::Message(format!(
                                    "dependency `{name}` has an invalid package instance key"
                                )),
                                span,
                            ));
                            continue;
                        };
                        let Some(&pkg_root) = self.package_roots.get(package_id) else {
                            self.errors.push(SemaError::new(
                                SemaErrorKind::Message(format!(
                                    "dependency `{name}` is declared but could not be loaded \
                                     (run `otter_fusion lock` to resolve it)"
                                )),
                                span,
                            ));
                            continue;
                        };
                        match self.resolve_pkg_subpath(pkg_root, parsed.package_subpath(), &name) {
                            Ok(target) => self.record_resolved_import(
                                mid,
                                import_index,
                                target,
                                false,
                                module_import.public,
                            ),
                            Err(msg) => self
                                .errors
                                .push(SemaError::new(SemaErrorKind::Message(msg), span)),
                        }
                    }
                    Scheme::File => {
                        // Enforce the allowlist/escape gate, then bind names from
                        // the loaded target module (`docs/17` §17.4).
                        match self.check_file_import(&mod_path, &parsed, ctx) {
                            Ok(target) => match self.file_modules.get(&target).copied() {
                                Some(m) => self.record_resolved_import(
                                    mid,
                                    import_index,
                                    m,
                                    false,
                                    module_import.public,
                                ),
                                None => self.errors.push(SemaError::new(
                                    SemaErrorKind::Message(format!(
                                        "`{}` could not be loaded (expected file `{}`)",
                                        parsed.display_source(),
                                        target.display()
                                    )),
                                    span,
                                )),
                            },
                            Err(msg) => self
                                .errors
                                .push(SemaError::new(SemaErrorKind::Message(msg), span)),
                        }
                    }
                }
            }
        }
        for mid in 0..self.modules.len() {
            let imports = self.modules[mid].imports.clone();
            for module_import in &imports {
                let Some(target) = module_import.target else {
                    continue;
                };
                self.bind_import(
                    mid,
                    target,
                    &module_import.item,
                    module_import.toolchain,
                    module_import.public,
                );
            }
        }
    }

    fn record_resolved_import(
        &mut self,
        mid: usize,
        import_index: usize,
        target: ModId,
        toolchain: bool,
        public: bool,
    ) {
        let module_import = &mut self.modules[mid].imports[import_index];
        module_import.target = Some(target);
        module_import.toolchain = toolchain;
        if !self.modules[mid].extension_imports.contains(&target) {
            self.modules[mid].extension_imports.push(target);
        }
        if public && !self.modules[mid].public_extension_imports.contains(&target) {
            self.modules[mid].public_extension_imports.push(target);
        }
    }

    /// Resolve a `self:` root path to a module in this package's tree.
    fn resolve_self_root(&self, segments: &[String]) -> Result<ModId, String> {
        let target = self
            .module_by_path(segments)
            .ok_or_else(|| format!("cannot find module `self:{}`", segments.join("/")))?;
        if self.modules[target.index()].external_unloaded {
            return Err(format!(
                "module `self:{}` was not loaded",
                segments.join("/")
            ));
        }
        Ok(target)
    }

    /// Resolve `pkg:<name>/<sub>…` into a dependency's subtree, walking `pub mod`
    /// children. A consumer reaches a submodule only when every `mod` on the
    /// path is `pub mod` (`docs/17` §17.5).
    fn resolve_pkg_subpath(
        &self,
        pkg_root: ModId,
        subpath: &[String],
        name: &str,
    ) -> Result<ModId, String> {
        let mut cur = pkg_root;
        for seg in subpath {
            let child = self.modules[cur.index()]
                .children
                .get(seg)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "`pkg:{name}/{}` is not a module in `{name}`",
                        subpath.join("/")
                    )
                })?;
            if !self.modules[child.index()].public {
                return Err(format!(
                    "`pkg:{name}/{}` is not publicly exported (its module is not `pub mod`)",
                    subpath.join("/")
                ));
            }
            cur = child;
        }
        Ok(cur)
    }

    /// Resolve a relative `self:` path (`self:./`, `self:../`) against the
    /// importing module's *file location*, enforcing the package-escape rule
    /// (`docs/17` §17.4). The resolved file must be a declared module.
    fn resolve_self_rel(
        &self,
        importing_mod_path: &[String],
        parsed: &crate::imports::ImportPath,
        ctx: &ResolveContext,
        file_to_module: &std::collections::HashMap<std::path::PathBuf, Vec<String>>,
    ) -> Result<ModId, String> {
        use crate::sema::resolve_ctx::normalize;
        let importing_file = ctx
            .file_of
            .get(importing_mod_path)
            .ok_or_else(|| "relative `self:` import has no source location".to_string())?;
        let mut dir = importing_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        for _ in 0..parsed.up {
            dir = dir
                .parent()
                .map(|p| p.to_path_buf())
                .ok_or_else(|| escape_message(parsed, ctx))?;
        }
        // The resolved file = dir / segments... + `.otter`.
        let mut target_file = dir;
        for (i, seg) in parsed.segments.iter().enumerate() {
            if i + 1 == parsed.segments.len() {
                target_file.push(format!("{seg}.otter"));
            } else {
                target_file.push(seg);
            }
        }
        let target_file = normalize(&target_file);
        // Escape rule: the target must stay within the source root.
        if let Some(root) = &ctx.source_root {
            let root = normalize(root);
            if !target_file.starts_with(&root) {
                return Err(escape_message(parsed, ctx));
            }
        }
        match file_to_module.get(&target_file) {
            Some(mp) => self.resolve_self_root(mp).or_else(|_| {
                // `mp == []` is the root entry, which `module_by_path` returns
                // directly.
                self.module_by_path(mp).ok_or_else(|| {
                    format!(
                        "`{}` does not resolve to a declared module",
                        parsed.display_source()
                    )
                })
            }),
            None => Err(format!(
                "`{}` does not resolve to a declared module (it is not in the `mod` tree)",
                parsed.display_source()
            )),
        }
    }

    /// Enforce the `file:` allowlist/escape gate (`docs/17` §17.4). A `file:`
    /// path that resolves *outside* the source root must match a `[file-imports]
    /// allow` entry (project mode); inside the source root, or in direct mode
    /// (no source root), it is unrestricted.
    fn check_file_import(
        &self,
        importing_mod_path: &[String],
        parsed: &crate::imports::ImportPath,
        ctx: &ResolveContext,
    ) -> Result<std::path::PathBuf, String> {
        use crate::sema::resolve_ctx::normalize;
        // Resolve the target location relative to the importing file. `.otter` is
        // appended when no extension is given (must match the loader).
        let importing_file = ctx
            .file_of
            .get(importing_mod_path)
            .ok_or_else(|| "`file:` import has no source location".to_string())?;
        let mut dir = importing_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        for _ in 0..parsed.up {
            dir = dir
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| dir.clone());
        }
        for seg in &parsed.segments {
            dir.push(seg);
        }
        if dir.extension().is_none() {
            dir.set_extension("otter");
        }
        let target = normalize(&dir);

        // Direct mode (no source root): `file:` is unrestricted.
        let Some(source_root) = &ctx.source_root else {
            return Ok(target);
        };
        let source_root = normalize(source_root);
        if target.starts_with(&source_root) {
            return Ok(target); // inside the package — always allowed
        }
        // Escaping the package: must match an allowlist entry.
        let root = ctx
            .package_root
            .clone()
            .unwrap_or_else(|| source_root.clone());
        for entry in &ctx.file_import_allow {
            if path_matches_allow(&root, entry, &target) {
                return Ok(target);
            }
        }
        Err(format!(
            "`{}` resolves outside the package and is not authorized by `[file-imports] allow`",
            parsed.display_source()
        ))
    }

    /// Bind an import's names/namespace into module `mid` from `target`. When
    /// `toolchain` is set the visibility gate is skipped (toolchain modules
    /// export their documented catalog surface; those definitions are not
    /// user-authored `pub` items).
    fn bind_import(
        &mut self,
        mid: usize,
        target: ModId,
        imp: &ImportItem,
        toolchain: bool,
        public: bool,
    ) {
        if !self.modules[mid].extension_imports.contains(&target) {
            self.modules[mid].extension_imports.push(target);
        }
        if public && !self.modules[mid].public_extension_imports.contains(&target) {
            self.modules[mid].public_extension_imports.push(target);
        }
        match &imp.kind {
            ImportKind::Named(names) => {
                for n in names {
                    self.resolve_named_import(mid, target, n, toolchain, public);
                }
            }
            // `import "path" as M` — bind the alias to the module so `M.foo`
            // resolves against its public definitions.
            ImportKind::Namespace(alias) => {
                if self.namespace_import_collides(mid, &alias.name) {
                    self.errors.push(SemaError::new(
                        SemaErrorKind::Message(format!(
                            "namespace import `{}` collides with an existing binding; alias it with a different name",
                            alias.name
                        )),
                        alias.span,
                    ));
                    return;
                }
                self.modules[mid]
                    .namespace_imports
                    .insert(alias.name.clone(), target);
                if public {
                    self.modules[mid]
                        .public_namespace_imports
                        .insert(alias.name.clone(), target);
                }
            }
            // Ambient (extension-only) imports: extensions are module-bound and
            // already active program-wide; no names are bound.
            ImportKind::Ambient => {}
        }
    }

    /// Bind one `import { name as alias }` entry from `target` into module
    /// `mid`. `toolchain` skips the `pub` gate for built-in (`core:`/`std:`)
    /// modules, whose exported surface is not marked user-`pub`.
    fn resolve_named_import(
        &mut self,
        mid: usize,
        target: ModId,
        n: &ImportName,
        toolchain: bool,
        public: bool,
    ) {
        let src = n.name.name.clone();
        let bind = n.alias.as_ref().unwrap_or(&n.name).name.clone();
        let tmod = &self.modules[target.index()];
        let as_type = if toolchain {
            tmod.types.get(&src).copied()
        } else {
            self.resolve_pub_type_in(target, &src)
        };
        let as_value = if toolchain {
            tmod.values.get(&src).copied()
        } else {
            self.resolve_pub_value_in(target, &src)
        };
        if as_type.is_none() && as_value.is_none() {
            let private = !toolchain
                && tmod
                    .types
                    .get(&src)
                    .or_else(|| tmod.values.get(&src))
                    .is_some_and(|d| !self.defs[d.index()].public);
            if private {
                self.errors.push(SemaError::new(
                    SemaErrorKind::Message(format!("`{src}` is private")),
                    n.span,
                ));
                return;
            }
            self.errors.push(SemaError::new(
                SemaErrorKind::Message(format!("no `{src}` in the imported module")),
                n.span,
            ));
            return;
        }
        if self.named_import_collides(mid, &bind) {
            self.errors.push(SemaError::new(
                SemaErrorKind::Message(format!(
                    "imported name `{bind}` collides with another import; alias one with `as`"
                )),
                n.span,
            ));
            return;
        }
        if let Some(d) = as_type {
            self.modules[mid].imported_types.insert(bind.clone(), d);
            if public {
                self.modules[mid]
                    .public_imported_types
                    .insert(bind.clone(), d);
            }
        }
        if let Some(d) = as_value {
            self.modules[mid].imported_values.insert(bind.clone(), d);
            if public {
                self.modules[mid].public_imported_values.insert(bind, d);
            }
        }
    }

    fn named_import_collides(&self, mid: usize, bind: &str) -> bool {
        let module = &self.modules[mid];
        module.types.contains_key(bind)
            || module.values.contains_key(bind)
            || module.imported_types.contains_key(bind)
            || module.imported_values.contains_key(bind)
            || module.namespace_imports.contains_key(bind)
    }

    fn namespace_import_collides(&self, mid: usize, bind: &str) -> bool {
        let module = &self.modules[mid];
        module.types.contains_key(bind)
            || module.values.contains_key(bind)
            || module.children.contains_key(bind)
            || module.imported_types.contains_key(bind)
            || module.imported_values.contains_key(bind)
            || module.namespace_imports.contains_key(bind)
    }

    fn new_module(&mut self, name: String, parent: ModId, public: bool) -> ModId {
        let id = ModId(self.modules.len() as u32);
        self.modules
            .push(ModuleInfo::new(id, name, Some(parent), public));
        id
    }

    /// Push a `Def` and return its fresh id.
    fn add_def(
        &mut self,
        kind: DefKind,
        name: String,
        module: ModId,
        parent: Option<DefId>,
        public: bool,
        span: Span,
    ) -> DefId {
        let id = DefId(self.defs.len() as u32);
        self.defs.push(Def {
            kind,
            name,
            module,
            parent,
            public,
            span,
            item: None,
            attrs: Vec::new(),
            generics: Vec::new(),
            param_bounds: Vec::new(),
            is_static: false,
        });
        id
    }

    /// Register a name in the appropriate namespace(s) of `module`, reporting a
    /// duplicate if one already exists.
    fn register_name(&mut self, module: ModId, name: &str, def: DefId, kind: DefKind, span: Span) {
        if kind.in_type_ns() {
            self.bind(module, name, def, kind, span, true);
        }
        if kind.in_value_ns() {
            self.bind(module, name, def, kind, span, false);
        }
        // Unit and tuple structs are also value constructors; record them in
        // the value namespace too so `Red` / `Pair(..)` resolve as values.
        if matches!(kind, DefKind::Struct) {
            self.bind(module, name, def, kind, span, false);
        }
    }

    fn bind(
        &mut self,
        module: ModId,
        name: &str,
        def: DefId,
        kind: DefKind,
        span: Span,
        type_ns: bool,
    ) {
        let m = &mut self.modules[module.index()];
        let table = if type_ns { &mut m.types } else { &mut m.values };
        if table.insert(name.to_string(), def).is_some() {
            self.errors.push(SemaError::new(
                SemaErrorKind::DuplicateDefinition {
                    name: name.to_string(),
                    kind: kind.describe(),
                },
                span,
            ));
        }
    }

    fn collect_items(
        &mut self,
        module: ModId,
        items: &[Item],
        externals: &Externals,
        path: &[String],
    ) {
        for item in items {
            self.collect_item(module, item, externals, path);
        }
    }

    fn collect_item(&mut self, module: ModId, item: &Item, externals: &Externals, path: &[String]) {
        let public = item.visibility.is_public();
        match &item.kind {
            ItemKind::Function(f) => {
                let def = self.add_def(
                    DefKind::Function,
                    f.name.name.clone(),
                    module,
                    None,
                    public,
                    item.span,
                );
                self.register_name(module, &f.name.name, def, DefKind::Function, f.name.span);
                self.attach_item(def, item, &f.generics, None);
            }
            ItemKind::Struct(s) => {
                let kind = if s.is_extern {
                    DefKind::ExternStruct
                } else {
                    DefKind::Struct
                };
                let def = self.add_def(kind, s.name.name.clone(), module, None, public, item.span);
                self.register_name(module, &s.name.name, def, kind, s.name.span);
                self.attach_item(def, item, &s.generics, None);
                self.collect_struct_fields(module, def, &s.kind);
            }
            ItemKind::Interface(i) => {
                let def = self.add_def(
                    DefKind::Interface,
                    i.name.name.clone(),
                    module,
                    None,
                    public,
                    item.span,
                );
                self.register_name(module, &i.name.name, def, DefKind::Interface, i.name.span);
                self.attach_item(def, item, &i.generics, None);
                self.collect_interface_members(module, def, &i.members);
            }
            ItemKind::TypeAlias(a) => {
                let def = self.add_def(
                    DefKind::TypeAlias,
                    a.name.name.clone(),
                    module,
                    None,
                    public,
                    item.span,
                );
                self.register_name(module, &a.name.name, def, DefKind::TypeAlias, a.name.span);
                self.attach_item(def, item, &a.generics, None);
            }
            ItemKind::Var(v) => {
                let def = self.add_def(
                    DefKind::ModuleVar,
                    v.name.name.clone(),
                    module,
                    None,
                    public,
                    item.span,
                );
                self.register_name(module, &v.name.name, def, DefKind::ModuleVar, v.name.span);
                self.defs[def.index()].item = Some(item.kind.clone());
            }
            ItemKind::Module(m) => {
                let child = self.new_module(m.name.name.clone(), module, public);
                let def = self.add_def(
                    DefKind::Module,
                    m.name.name.clone(),
                    module,
                    None,
                    public,
                    item.span,
                );
                // Modules register only a child link; they are not values/types.
                let parent = &mut self.modules[module.index()];
                if parent.children.insert(m.name.name.clone(), child).is_some() {
                    self.errors.push(SemaError::new(
                        SemaErrorKind::DuplicateDefinition {
                            name: m.name.name.clone(),
                            kind: "module",
                        },
                        m.name.span,
                    ));
                }
                self.defs[def.index()].item = Some(item.kind.clone());
                let mut child_path = path.to_vec();
                child_path.push(m.name.name.clone());
                self.modules[child.index()].path = child_path.clone();
                match &m.kind {
                    ModuleKind::Inline { items, .. } => {
                        self.collect_items(child, items, externals, &child_path)
                    }
                    ModuleKind::External => match externals.get(&child_path) {
                        // The driver loaded this file-backed submodule's body.
                        Some(loaded) => {
                            self.collect_items(child, &loaded.items, externals, &child_path)
                        }
                        None => self.modules[child.index()].external_unloaded = true,
                    },
                }
            }
            ItemKind::Extend(e) => {
                let def = self.add_def(
                    DefKind::Extend,
                    String::new(),
                    module,
                    None,
                    public,
                    item.span,
                );
                self.attach_item(def, item, &e.generics, None);
                self.modules[module.index()].extends.push(def);
                self.collect_extend_members(module, def, &e.members);
            }
            ItemKind::Extern(ext) => self.collect_extern(module, item, ext, public),
            ItemKind::Import(imp) => {
                self.modules[module.index()].imports.push(ModuleImport {
                    item: imp.clone(),
                    public,
                    target: None,
                    toolchain: false,
                });
            }
            ItemKind::Test(t) => {
                // A test is a zero-arg unit body run by `otter_fusion test`. It is
                // not referenceable by name, so it gets a unique internal symbol
                // (its display name lives on the `Test` item); registered only as
                // a def so the checker/codegen process it and the runner finds it.
                let sym = format!("test#{}", self.defs.len());
                let def = self.add_def(DefKind::Test, sym, module, None, false, item.span);
                self.attach_item(def, item, &None, None);
                let _ = t;
            }
        }
    }

    /// Store the item AST and assign generic-parameter defs to `owner`.
    fn attach_item(
        &mut self,
        owner: DefId,
        item: &Item,
        generics: &Option<GenericParams>,
        explicit_module: Option<ModId>,
    ) {
        let module = explicit_module.unwrap_or(self.defs[owner.index()].module);
        self.defs[owner.index()].item = Some(item.kind.clone());
        self.defs[owner.index()].attrs = item.attrs.clone();
        let gen_defs = self.collect_generics(module, owner, generics);
        self.defs[owner.index()].generics = gen_defs;
    }

    fn collect_generics(
        &mut self,
        module: ModId,
        owner: DefId,
        generics: &Option<GenericParams>,
    ) -> Vec<DefId> {
        let mut out = Vec::new();
        if let Some(gp) = generics {
            for p in &gp.params {
                let def = self.add_def(
                    DefKind::GenericParam,
                    p.name.name.clone(),
                    module,
                    Some(owner),
                    false,
                    p.span,
                );
                self.defs[def.index()].param_bounds = p.bounds.clone();
                out.push(def);
            }
        }
        out
    }

    fn collect_struct_fields(&mut self, module: ModId, owner: DefId, kind: &StructKind) {
        match kind {
            StructKind::Unit => {}
            StructKind::Tuple(fields) => {
                for (i, f) in fields.iter().enumerate() {
                    self.add_def(
                        DefKind::Field,
                        i.to_string(),
                        module,
                        Some(owner),
                        f.visibility.is_public(),
                        f.span,
                    );
                }
            }
            StructKind::Record(fields) => {
                for f in fields {
                    self.add_def(
                        DefKind::Field,
                        f.name.name.clone(),
                        module,
                        Some(owner),
                        f.visibility.is_public(),
                        f.span,
                    );
                }
            }
        }
    }

    fn collect_interface_members(
        &mut self,
        module: ModId,
        owner: DefId,
        members: &[InterfaceMember],
    ) {
        for m in members {
            let def = self.add_def(
                DefKind::InterfaceMethod,
                m.function.name.name.clone(),
                module,
                Some(owner),
                true,
                m.span,
            );
            // Store the signature (and any default body) as a plain function so
            // signature lowering reuses the function machinery, exactly like
            // `extend` methods. `self` and interface generics layer in via the
            // method's parent interface.
            self.defs[def.index()].item = Some(ItemKind::Function(FunctionItem {
                name: m.function.name.clone(),
                generics: m.function.generics.clone(),
                params: m.function.params.clone(),
                return_type: m.function.return_type.clone(),
                is_async: m.function.is_async,
                body: m.default_body.clone(),
            }));
            // A method with no `self` parameter is static (`docs/10`).
            self.defs[def.index()].is_static = !has_self_param(&m.function.params);
            self.collect_generics(module, def, &m.function.generics);
        }
    }

    fn collect_extend_members(&mut self, module: ModId, owner: DefId, members: &[ExtendMember]) {
        for m in members {
            let def = self.add_def(
                DefKind::ExtendMethod,
                m.function.name.name.clone(),
                module,
                Some(owner),
                m.visibility.is_public(),
                m.span,
            );
            // Store the method's signature/body as a plain function so the
            // checker and code generator reuse the function machinery; `self`
            // and the extend's generics are layered in via the method's parent.
            self.defs[def.index()].item = Some(ItemKind::Function(m.function.clone()));
            // A method with no `self` parameter is static (`docs/09` §6).
            self.defs[def.index()].is_static = !has_self_param(&m.function.params);
            // Method-level generic params (`function map<U>(...)`): store on the
            // method def so `fn_env` can layer them on top of the extend's
            // generics and `Self`. Without this, `<U>` collected the param defs
            // but they never reached the method's `generics` vector, so type
            // lowering of the signature failed with "cannot find type `U`".
            let gen_defs = self.collect_generics(module, def, &m.function.generics);
            self.defs[def.index()].generics = gen_defs;
        }
    }

    fn collect_extern(&mut self, module: ModId, item: &Item, ext: &ExternItem, public: bool) {
        match ext {
            ExternItem::Function(f) => {
                let def = self.add_def(
                    DefKind::ExternFunction,
                    f.name.name.clone(),
                    module,
                    None,
                    public,
                    item.span,
                );
                self.register_name(
                    module,
                    &f.name.name,
                    def,
                    DefKind::ExternFunction,
                    f.name.span,
                );
                self.attach_item(def, item, &f.generics, None);
            }
            ExternItem::Struct(s) => {
                let def = self.add_def(
                    DefKind::ExternStruct,
                    s.name.name.clone(),
                    module,
                    None,
                    public,
                    item.span,
                );
                self.register_name(
                    module,
                    &s.name.name,
                    def,
                    DefKind::ExternStruct,
                    s.name.span,
                );
                // Store the bare `StructItem` (not the `Extern(..)` wrapper) so the
                // existing struct machinery — `record_fields`, `tuple_fields`,
                // `collect_struct_layouts` — works on extern structs transparently;
                // the `ExternStruct` def kind + decorators (`attrs`) carry the C-ABI
                // distinction (`docs/19` §3).
                self.defs[def.index()].item = Some(ItemKind::Struct(s.clone()));
                self.defs[def.index()].attrs = item.attrs.clone();
                let gen_defs = self.collect_generics(module, def, &s.generics);
                self.defs[def.index()].generics = gen_defs;
                self.collect_struct_fields(module, def, &s.kind);
            }
            ExternItem::OpaqueType(name) => {
                let def = self.add_def(
                    DefKind::ExternType,
                    name.name.clone(),
                    module,
                    None,
                    public,
                    name.span,
                );
                self.register_name(module, &name.name, def, DefKind::ExternType, name.span);
            }
            ExternItem::Var { name, .. } => {
                let def = self.add_def(
                    DefKind::ExternVar,
                    name.name.clone(),
                    module,
                    None,
                    public,
                    name.span,
                );
                self.register_name(module, &name.name, def, DefKind::ExternVar, name.span);
                self.defs[def.index()].item = Some(item.kind.clone());
            }
        }
    }
}

fn package_instance_id(key: &[String]) -> Option<&str> {
    match key {
        [prefix, id, ..] if prefix == "__pkg__" => Some(id.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ModId;
    use crate::lexer::lex;
    use crate::parser::parse;
    use crate::span::FileId;

    fn program(src: &str) -> Program {
        let (tokens, lex_errs) = lex(src, FileId(0));
        assert!(lex_errs.is_empty(), "lex errors: {lex_errs:?}");
        let (module, parse_errs) = parse(src, &tokens);
        assert!(parse_errs.is_empty(), "parse errors: {parse_errs:?}");
        Program::collect(&module)
    }

    fn lookup_value(p: &Program, m: ModId, name: &str) -> Option<DefId> {
        p.module(m).values.get(name).copied()
    }
    fn lookup_type(p: &Program, m: ModId, name: &str) -> Option<DefId> {
        p.module(m).types.get(name).copied()
    }

    #[test]
    fn collects_functions_and_structs() {
        let p = program(
            "pub struct Person { pub name: str, age: i32 }\n\
             function main() { }\n",
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let person = lookup_type(&p, ModId::ROOT, "Person").unwrap();
        assert_eq!(p.def(person).kind, DefKind::Struct);
        assert!(p.def(person).public);
        // The struct is also a value constructor.
        assert_eq!(lookup_value(&p, ModId::ROOT, "Person"), Some(person));
        let main = lookup_value(&p, ModId::ROOT, "main").unwrap();
        assert_eq!(p.def(main).kind, DefKind::Function);
        // Two record fields became defs.
        let fields: Vec<_> = p
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Field && d.parent == Some(person))
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(fields, vec!["name", "age"]);
    }

    #[test]
    fn detects_duplicate_definition() {
        let p = program("function f() {}\nfunction f() {}\n");
        assert_eq!(p.errors.len(), 1);
        assert!(matches!(
            p.errors[0].kind,
            SemaErrorKind::DuplicateDefinition { .. }
        ));
    }

    #[test]
    fn generic_params_get_defs() {
        let p = program("function id<T>(x: T): T { x }\n");
        let id = lookup_value(&p, ModId::ROOT, "id").unwrap();
        assert_eq!(p.def(id).generics.len(), 1);
        let t = p.def(id).generics[0];
        assert_eq!(p.def(t).kind, DefKind::GenericParam);
        assert_eq!(p.def(t).name, "T");
    }

    #[test]
    fn inline_modules_nest() {
        let p = program("mod math {\n  pub function add(a: i64, b: i64): i64 { a + b }\n}\n");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let math = p.module(ModId::ROOT).children.get("math").copied().unwrap();
        assert!(lookup_value(&p, math, "add").is_some());
        // Not visible from the root namespace.
        assert!(lookup_value(&p, ModId::ROOT, "add").is_none());
    }

    #[test]
    fn stdlib_catalog_exports_materialize_in_builtin_views() {
        let p = program("");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let provider = crate::sema::stdlib::active_provider();

        for spec in provider.modules() {
            let path: Vec<String> = spec.path.iter().map(|s| s.to_string()).collect();
            let module = p
                .builtin_modules
                .get(&path)
                .copied()
                .unwrap_or_else(|| panic!("missing builtin view for {:?}", spec.path));
            for export in spec.exports {
                assert!(
                    p.module(module).types.contains_key(*export)
                        || p.module(module).values.contains_key(*export),
                    "{:?} exports `{}` but __builtins__ does not define it",
                    spec.path,
                    export
                );
            }
        }
    }

    #[test]
    fn stdlib_source_modules_keep_duplicate_export_names_distinct() {
        let p = program("");
        assert!(p.errors.is_empty(), "{:?}", p.errors);

        let async_sleep = p
            .toolchain_export_value(&["std", "async"], "sleep")
            .expect("std:async exports sleep");
        let time_sleep = p
            .toolchain_export_value(&["std", "time"], "sleep")
            .expect("std:time exports sleep");

        assert_ne!(
            async_sleep, time_sleep,
            "duplicate export names in different stdlib modules must not alias"
        );
        assert_ne!(p.def(async_sleep).module, p.def(time_sleep).module);
        assert_eq!(
            p.module(p.def(async_sleep).module).path,
            vec!["std".to_string(), "async".to_string()]
        );
        assert_eq!(
            p.module(p.def(time_sleep).module).path,
            vec!["std".to_string(), "time".to_string()]
        );
    }

    #[test]
    fn implicit_prelude_maps_stay_empty() {
        let p = program("");
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(
            p.prelude_types.is_empty(),
            "core:prelude names must require explicit imports"
        );
        assert!(
            p.prelude_values.is_empty(),
            "core:prelude values must require explicit imports"
        );
    }

    #[test]
    fn no_std_rejects_std_imports() {
        let root = parse_module("import { println } from \"std:io\";\nfunction main() {}");
        let ctx = ResolveContext {
            project: true,
            no_std: true,
            ..Default::default()
        };
        let p = Program::collect_multi_ctx(&root, &Externals::new(), &ctx);
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("no-std") && msg.contains("std:` is unavailable")
            }),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn no_std_keeps_core_imports_available() {
        let root = parse_module("import { List } from \"core:collections\";\nfunction main() {}");
        let ctx = ResolveContext {
            project: true,
            no_std: true,
            ..Default::default()
        };
        let p = Program::collect_multi_ctx(&root, &Externals::new(), &ctx);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(p.module(ModId::ROOT).imported_types.contains_key("List"));
    }

    #[test]
    fn duplicate_named_imports_are_rejected() {
        let root = parse_module(
            "import { sleep } from \"std:async\";\n\
             import { sleep } from \"std:time\";\n\
             function main() {}",
        );
        let p = Program::collect(&root);
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("imported name `sleep` collides with another import")
            }),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn aliased_named_imports_do_not_collide() {
        let root = parse_module(
            "import { sleep as async_sleep } from \"std:async\";\n\
             import { sleep as time_sleep } from \"std:time\";\n\
             function main() {}",
        );
        let p = Program::collect(&root);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(
            p.module(ModId::ROOT)
                .imported_values
                .contains_key("async_sleep")
        );
        assert!(
            p.module(ModId::ROOT)
                .imported_values
                .contains_key("time_sleep")
        );
    }

    #[test]
    fn local_definition_still_shadows_named_import() {
        let root = parse_module(
            "import { sleep } from \"std:async\";\n\
             function sleep(): i64 { 1 }\n\
             function main() {}",
        );
        let p = Program::collect(&root);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let local = p.module(ModId::ROOT).values.get("sleep").copied().unwrap();
        assert_eq!(p.resolve_value_in(ModId::ROOT, "sleep"), Some(local));
    }

    #[test]
    fn namespace_import_collides_with_existing_import_binding() {
        let root = parse_module(
            "import { Duration as Time } from \"std:time\";\n\
             import \"std:time\" as Time;\n\
             function main() {}",
        );
        let p = Program::collect(&root);
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("namespace import `Time` collides with an existing binding")
            }),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn namespace_import_collides_with_local_definition() {
        let root = parse_module(
            "struct Time {}\n\
             import \"std:time\" as Time;\n\
             function main() {}",
        );
        let p = Program::collect(&root);
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("namespace import `Time` collides with an existing binding")
            }),
            "{:?}",
            p.errors
        );
    }

    struct CoreCollectionsOnlyProvider;

    static CORE_COLLECTIONS_ONLY_MODULES: &[crate::sema::stdlib::StdModuleSpec] =
        &[crate::sema::stdlib::StdModuleSpec {
            path: &["core", "collections"],
            tier: crate::sema::stdlib::StdTier::Core,
            implementation: crate::sema::stdlib::StdImplementation::Mixed,
            exports: &["List", "Map", "Set", "Entry"],
        }];

    static BROKEN_PROVIDER_MODULES: &[crate::sema::stdlib::StdModuleSpec] =
        &[crate::sema::stdlib::StdModuleSpec {
            path: &["std", "broken"],
            tier: crate::sema::stdlib::StdTier::Std,
            implementation: crate::sema::stdlib::StdImplementation::Otter,
            exports: &["DefinitelyMissingStdSymbol"],
        }];

    static DUPLICATE_PROVIDER_MODULES: &[crate::sema::stdlib::StdModuleSpec] = &[
        crate::sema::stdlib::StdModuleSpec {
            path: &["core", "collections"],
            tier: crate::sema::stdlib::StdTier::Core,
            implementation: crate::sema::stdlib::StdImplementation::Mixed,
            exports: &["List"],
        },
        crate::sema::stdlib::StdModuleSpec {
            path: &["core", "collections"],
            tier: crate::sema::stdlib::StdTier::Core,
            implementation: crate::sema::stdlib::StdImplementation::Mixed,
            exports: &["Map"],
        },
    ];

    static WRONG_TIER_PROVIDER_MODULES: &[crate::sema::stdlib::StdModuleSpec] =
        &[crate::sema::stdlib::StdModuleSpec {
            path: &["core", "collections"],
            tier: crate::sema::stdlib::StdTier::Std,
            implementation: crate::sema::stdlib::StdImplementation::Mixed,
            exports: &["List"],
        }];

    static DUPLICATE_EXPORT_PROVIDER_MODULES: &[crate::sema::stdlib::StdModuleSpec] =
        &[crate::sema::stdlib::StdModuleSpec {
            path: &["core", "collections"],
            tier: crate::sema::stdlib::StdTier::Core,
            implementation: crate::sema::stdlib::StdImplementation::Mixed,
            exports: &["List", "List"],
        }];

    static ROOT_ONLY_PROVIDER_MODULES: &[crate::sema::stdlib::StdModuleSpec] =
        &[crate::sema::stdlib::StdModuleSpec {
            path: &["std"],
            tier: crate::sema::stdlib::StdTier::Std,
            implementation: crate::sema::stdlib::StdImplementation::Otter,
            exports: &[],
        }];

    static INVALID_SEGMENT_PROVIDER_MODULES: &[crate::sema::stdlib::StdModuleSpec] =
        &[crate::sema::stdlib::StdModuleSpec {
            path: &["std", ""],
            tier: crate::sema::stdlib::StdTier::Std,
            implementation: crate::sema::stdlib::StdImplementation::Otter,
            exports: &[],
        }];

    static CUSTOM_STD_PROVIDER_MODULES: &[crate::sema::stdlib::StdModuleSpec] =
        &[crate::sema::stdlib::StdModuleSpec {
            path: &["std", "target_error"],
            tier: crate::sema::stdlib::StdTier::Std,
            implementation: crate::sema::stdlib::StdImplementation::Otter,
            exports: &["Error"],
        }];

    impl crate::sema::stdlib::StdProvider for CoreCollectionsOnlyProvider {
        fn name(&self) -> &'static str {
            "core-collections-only"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            CORE_COLLECTIONS_ONLY_MODULES
        }
    }

    struct BrokenProvider;

    impl crate::sema::stdlib::StdProvider for BrokenProvider {
        fn name(&self) -> &'static str {
            "broken-provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            BROKEN_PROVIDER_MODULES
        }
    }

    struct DuplicateProvider;

    impl crate::sema::stdlib::StdProvider for DuplicateProvider {
        fn name(&self) -> &'static str {
            "duplicate-provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            DUPLICATE_PROVIDER_MODULES
        }
    }

    struct WrongTierProvider;

    impl crate::sema::stdlib::StdProvider for WrongTierProvider {
        fn name(&self) -> &'static str {
            "wrong-tier-provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            WRONG_TIER_PROVIDER_MODULES
        }
    }

    struct DuplicateExportProvider;

    impl crate::sema::stdlib::StdProvider for DuplicateExportProvider {
        fn name(&self) -> &'static str {
            "duplicate-export-provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            DUPLICATE_EXPORT_PROVIDER_MODULES
        }
    }

    struct RootOnlyProvider;

    impl crate::sema::stdlib::StdProvider for RootOnlyProvider {
        fn name(&self) -> &'static str {
            "root-only-provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            ROOT_ONLY_PROVIDER_MODULES
        }
    }

    struct InvalidSegmentProvider;

    impl crate::sema::stdlib::StdProvider for InvalidSegmentProvider {
        fn name(&self) -> &'static str {
            "invalid-segment-provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            INVALID_SEGMENT_PROVIDER_MODULES
        }
    }

    struct EmptyNameProvider;

    impl crate::sema::stdlib::StdProvider for EmptyNameProvider {
        fn name(&self) -> &'static str {
            ""
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            CORE_COLLECTIONS_ONLY_MODULES
        }
    }

    struct PathLikeNameProvider;

    impl crate::sema::stdlib::StdProvider for PathLikeNameProvider {
        fn name(&self) -> &'static str {
            "bad/provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            CORE_COLLECTIONS_ONLY_MODULES
        }
    }

    struct CustomStdProvider;

    impl crate::sema::stdlib::StdProvider for CustomStdProvider {
        fn name(&self) -> &'static str {
            "custom-std-provider"
        }

        fn modules(&self) -> &'static [crate::sema::stdlib::StdModuleSpec] {
            CUSTOM_STD_PROVIDER_MODULES
        }
    }

    #[test]
    fn explicit_provider_rejects_invalid_provider_names() {
        let root = parse_module("import { List } from \"core:collections\";\nfunction main() {}");
        for (provider, expected_name) in [
            (
                &EmptyNameProvider as &dyn crate::sema::stdlib::StdProvider,
                "<empty>",
            ),
            (
                &PathLikeNameProvider as &dyn crate::sema::stdlib::StdProvider,
                "bad/provider",
            ),
        ] {
            let p = Program::collect_multi_ctx_with_provider(
                &root,
                &Externals::new(),
                &ResolveContext::direct(),
                provider,
            );
            assert!(
                p.errors.iter().any(|e| {
                    let msg = e.kind.to_string();
                    msg.contains(&format!(
                        "stdlib provider name `{expected_name}` is invalid"
                    )) && msg.contains("non-empty ASCII identifiers")
                }),
                "{:?}",
                p.errors
            );
            assert!(
                !p.module(ModId::ROOT).imported_types.contains_key("List"),
                "provider with invalid identity must not expose module views"
            );
            assert_eq!(p.std_provider_name, "<invalid>");
            assert!(
                p.errors.iter().all(|e| {
                    !e.kind
                        .to_string()
                        .contains(&format!("stdlib provider `{expected_name}`"))
                }),
                "invalid provider identity must not leak into downstream provider diagnostics: {:?}",
                p.errors
            );
        }
    }

    #[test]
    fn explicit_provider_can_omit_std_modules() {
        let root = parse_module("import { println } from \"std:io\";\nfunction main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &CoreCollectionsOnlyProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("stdlib provider `core-collections-only`")
                    && msg.contains("does not provide module `std:io`")
            }),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn explicit_provider_keeps_listed_core_modules_available() {
        let root = parse_module("import { List } from \"core:collections\";\nfunction main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &CoreCollectionsOnlyProvider,
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(p.module(ModId::ROOT).imported_types.contains_key("List"));
    }

    #[test]
    fn explicit_provider_rejects_exports_missing_from_toolchain_source() {
        let root = parse_module("function main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &BrokenProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("stdlib provider `broken-provider` module `std:broken`")
                    && msg.contains("exports `DefinitelyMissingStdSymbol`")
                    && msg.contains("does not define that symbol")
            }),
            "{:?}",
            p.errors
        );
        assert!(
            !p.builtin_modules
                .contains_key(&vec!["std".to_string(), "broken".to_string()]),
            "provider module with missing exports must not materialize a view"
        );
    }

    #[test]
    fn explicit_provider_rejects_duplicate_module_paths() {
        let root = parse_module("function main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &DuplicateProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("stdlib provider `duplicate-provider`")
                    && msg.contains("duplicate module `core:collections`")
            }),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn explicit_provider_rejects_tier_path_mismatches() {
        let root = parse_module("import { List } from \"core:collections\";\nfunction main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &WrongTierProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("stdlib provider `wrong-tier-provider`")
                    && msg.contains("module `core:collections` has tier `Std`")
                    && msg.contains("path root is not `std`")
            }),
            "{:?}",
            p.errors
        );
        assert!(
            !p.module(ModId::ROOT).imported_types.contains_key("List"),
            "wrong-tier provider must not expose an invalid module view"
        );
    }

    #[test]
    fn explicit_provider_rejects_duplicate_exports() {
        let root = parse_module("function main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &DuplicateExportProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("stdlib provider `duplicate-export-provider`")
                    && msg.contains("module `core:collections`")
                    && msg.contains("duplicate export `List`")
            }),
            "{:?}",
            p.errors
        );
        assert!(
            !p.builtin_modules
                .contains_key(&vec!["core".to_string(), "collections".to_string()]),
            "provider module with duplicate exports must not materialize a view"
        );
    }

    #[test]
    fn explicit_provider_rejects_root_only_module_paths() {
        let root = parse_module("function main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &RootOnlyProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("stdlib provider `root-only-provider`")
                    && msg.contains("module `std`")
                    && msg.contains("must include a scheme and module path")
            }),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn explicit_provider_rejects_invalid_module_path_segments() {
        let root = parse_module("function main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &InvalidSegmentProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("stdlib provider `invalid-segment-provider`")
                    && msg.contains("module `std:`")
                    && msg.contains("invalid path segment `<empty>`")
            }),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn explicit_provider_can_add_custom_std_modules() {
        let root = parse_module("import { Error } from \"std:target_error\";\nfunction main() {}");
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ResolveContext::direct(),
            &CustomStdProvider,
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert!(p.module(ModId::ROOT).imported_types.contains_key("Error"));
    }

    #[test]
    fn no_std_rejects_provider_added_std_modules() {
        let root = parse_module("import { Error } from \"std:target_error\";\nfunction main() {}");
        let ctx = ResolveContext {
            project: true,
            no_std: true,
            ..Default::default()
        };
        let p = Program::collect_multi_ctx_with_provider(
            &root,
            &Externals::new(),
            &ctx,
            &CustomStdProvider,
        );
        assert!(
            p.errors.iter().any(|e| {
                let msg = e.kind.to_string();
                msg.contains("std:target_error")
                    && msg.contains("no-std")
                    && msg.contains("std:` is unavailable")
            }),
            "{:?}",
            p.errors
        );
        assert!(!p.module(ModId::ROOT).imported_types.contains_key("Error"));
    }

    #[test]
    fn interface_and_extend_methods_collected() {
        let p = program(
            "interface Named { function name(self): str; }\n\
             struct P { x: i64 }\n\
             extend P: Named { function name(self): str { \"p\" } }\n",
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        // The user's `Named.name` interface method is collected (the toolchain
        // also contributes `Iterator.next`, so filter by name).
        let iface_methods = p
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::InterfaceMethod && d.name == "name")
            .count();
        assert_eq!(iface_methods, 1);
        // The user's `extend P: Named` contributes one method; the toolchain
        // also contributes `extend MapKeys/MapValues/MapEntries: Iterator<…>`
        // impls plus the `core:compiler` macro surface (`extend ASTNode { name }`),
        // so restrict the count to non-toolchain (user) defs.
        let user_extend_methods = p
            .defs
            .iter()
            .enumerate()
            .filter(|(i, d)| {
                d.kind == DefKind::ExtendMethod
                    && d.name == "name"
                    && !p.is_builtin_def(DefId(*i as u32))
            })
            .count();
        assert_eq!(user_extend_methods, 1);
        let user_extends = p
            .module(ModId::ROOT)
            .extends
            .iter()
            .filter(|&&e| {
                p.def(e).item.as_ref().is_some_and(|it| {
                    matches!(
                        it,
                        ItemKind::Extend(e) if matches!(
                            &e.target.kind,
                            TypeKind::Named { name, .. } if name.name == "P"
                        )
                    )
                })
            })
            .count();
        assert_eq!(user_extends, 1);
    }

    #[test]
    fn unit_struct_is_value_and_type() {
        let p = program("struct Red;\ntype Color = Red;\n");
        let red = lookup_type(&p, ModId::ROOT, "Red").unwrap();
        assert_eq!(lookup_value(&p, ModId::ROOT, "Red"), Some(red));
        assert!(lookup_type(&p, ModId::ROOT, "Color").is_some());
    }

    /// Parse `src` as a module (no submodules).
    fn parse_module(src: &str) -> Module {
        let (tokens, _) = lex(src, FileId(0));
        let (module, errs) = parse(src, &tokens);
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        module
    }

    /// A project context with one resolved dependency package.
    fn pkg_ctx(dep: &str) -> ResolveContext {
        let mut packages = HashMap::new();
        packages.insert(
            dep.to_string(),
            vec!["__pkg__".to_string(), dep.to_string()],
        );
        let mut dependencies = std::collections::HashSet::new();
        dependencies.insert(dep.to_string());
        ResolveContext {
            project: true,
            dependencies,
            packages,
            ..Default::default()
        }
    }

    #[test]
    fn pkg_import_binds_public_dependency_items() {
        let root = parse_module("import { greet } from \"pkg:greeter\";\nfunction main() {}");
        let dep = parse_module("pub function greet(): i64 { 42 }\nfunction hidden() {}");
        let mut externals = Externals::new();
        externals.insert(vec!["__pkg__".into(), "greeter".into()], dep);
        let p = Program::collect_multi_ctx(&root, &externals, &pkg_ctx("greeter"));
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        // `greet` is imported into the root module's value scope.
        assert!(p.module(ModId::ROOT).imported_values.contains_key("greet"));
        // The dependency package was collected and registered.
        assert!(p.package_roots.contains_key("greeter"));
    }

    #[test]
    fn pkg_import_of_a_private_dependency_item_is_rejected() {
        let root = parse_module("import { hidden } from \"pkg:greeter\";\nfunction main() {}");
        let dep = parse_module("pub function greet(): i64 { 1 }\nfunction hidden(): i64 { 2 }");
        let mut externals = Externals::new();
        externals.insert(vec!["__pkg__".into(), "greeter".into()], dep);
        let p = Program::collect_multi_ctx(&root, &externals, &pkg_ctx("greeter"));
        assert!(
            p.errors
                .iter()
                .any(|e| e.kind.to_string().contains("private")),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn public_named_imports_are_reexported() {
        let root = parse_module(
            "mod model;\n\
             mod facade;\n\
             import { CrateBox, make_box } from \"self:facade\";\n\
             function main() {}\n",
        );
        let model = parse_module(
            "pub struct Box { pub value: i64 }\n\
             pub function make_box(v: i64): Box { Box { value: v } }\n",
        );
        let facade =
            parse_module("pub import { Box as CrateBox, make_box } from \"self:model\";\n");
        let mut externals = Externals::new();
        externals.insert(vec!["model".into()], model);
        externals.insert(vec!["facade".into()], facade);
        let ctx = ResolveContext {
            project: true,
            ..Default::default()
        };
        let p = Program::collect_multi_ctx(&root, &externals, &ctx);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let root_mod = p.module(ModId::ROOT);
        assert!(root_mod.imported_types.contains_key("CrateBox"));
        assert!(root_mod.imported_values.contains_key("make_box"));
    }

    #[test]
    fn private_named_imports_are_not_reexported() {
        let root = parse_module(
            "mod model;\n\
             mod facade;\n\
             import { Box } from \"self:facade\";\n\
             function main() {}\n",
        );
        let model = parse_module("pub struct Box { pub value: i64 }\n");
        let facade = parse_module("import { Box } from \"self:model\";\n");
        let mut externals = Externals::new();
        externals.insert(vec!["model".into()], model);
        externals.insert(vec!["facade".into()], facade);
        let ctx = ResolveContext {
            project: true,
            ..Default::default()
        };
        let p = Program::collect_multi_ctx(&root, &externals, &ctx);
        assert!(
            p.errors.iter().any(|e| e
                .kind
                .to_string()
                .contains("no `Box` in the imported module")),
            "{:?}",
            p.errors
        );
    }

    #[test]
    fn pkg_import_of_undeclared_dependency_is_rejected() {
        let root = parse_module("import { x } from \"pkg:unknown\";\nfunction main() {}");
        let ctx = ResolveContext {
            project: true,
            ..Default::default()
        };
        let p = Program::collect_multi_ctx(&root, &Externals::new(), &ctx);
        assert!(
            p.errors
                .iter()
                .any(|e| e.kind.to_string().contains("no dependency named")),
            "{:?}",
            p.errors
        );
    }
}
