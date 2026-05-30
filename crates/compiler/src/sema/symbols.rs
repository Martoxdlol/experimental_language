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
use crate::span::Span;
use std::collections::HashMap;

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
    let prefix: std::path::PathBuf =
        entry.split('/').take_while(|seg| !seg.contains('*')).collect();
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
    params.iter().any(|p| matches!(p.kind, ParamKind::SelfParam))
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
    pub imports: Vec<ImportItem>,
    /// Names brought into scope by `import { … } from "…"`, in the type
    /// namespace (resolved by [`Program::resolve_imports`]).
    pub imported_types: HashMap<String, DefId>,
    /// As [`Self::imported_types`], for the value namespace.
    pub imported_values: HashMap<String, DefId>,
    /// `import "path" as M` aliases: `M` → the aliased module. Member access
    /// `M.foo` resolves against that module's public definitions.
    pub namespace_imports: HashMap<String, ModId>,
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
            namespace_imports: HashMap::new(),
        }
    }
}

/// The whole program under analysis: the definition table, module tree, and
/// accumulated diagnostics.
pub struct Program {
    pub defs: Vec<Def>,
    pub modules: Vec<ModuleInfo>,
    pub errors: Vec<SemaError>,
    /// The builtin `List<T>` type definition (injected by the prelude).
    pub list_def: DefId,
    /// The builtin `Map<K, V>` type definition (injected by the prelude).
    pub map_def: DefId,
    /// Prelude `struct Item<T>` — the iterator protocol's element wrapper.
    pub item_def: DefId,
    /// Prelude `struct Done` — the iterator protocol's end marker.
    pub done_def: DefId,
    /// Prelude `interface Iterator<T>`.
    pub iterator_def: DefId,
    /// Prelude `struct Entry<K, V>` — yielded by `for entry in map`.
    pub entry_def: DefId,
    /// Prelude `interface FromResidual<R>` — error conversion for `?` (`docs/13`).
    pub from_residual_def: DefId,
    /// Prelude `interface Try<Output, Residual>` — lets a non-union wrapper type
    /// participate in `?` (`docs/13` §3): `branch(self)` splits the wrapper into
    /// its success and failure variants.
    pub try_def: DefId,
    /// Prelude `interface Clone` — deep-copy entry point (`docs/10`/`docs/15`).
    pub clone_def: DefId,
    /// Prelude `interface Drop` — finalizer run before reclamation (`docs/16` §8).
    pub drop_def: DefId,
    /// Prelude `struct JoinHandle<R>` — `Thread.spawn`'s result (`docs/20`).
    pub join_handle_def: DefId,
    /// Prelude `struct Joined<R>` — a worker's value after `join` (`docs/20`).
    pub joined_def: DefId,
    /// Prelude `struct Panicked` — a worker that panicked (`docs/20`).
    pub panicked_def: DefId,
    /// Prelude `struct Sender<T>` — a channel's sending end (`docs/20` §2).
    pub sender_def: DefId,
    /// Prelude `struct Receiver<T>` — a channel's receiving end (`docs/20` §2).
    pub receiver_def: DefId,
    /// Prelude `struct ChannelClosed` — returned by a closed channel.
    pub channel_closed_def: DefId,
    /// Prelude `struct Shared<T>` — a mutex handle (`docs/20` §4).
    pub shared_def: DefId,
    /// Prelude `struct LockBusy` — `try_lock` failure.
    pub lock_busy_def: DefId,
    /// Prelude `struct Pending` — a future that is not yet ready (`docs/21` §1).
    pub pending_def: DefId,
    /// Prelude `struct Ready<T>` — a completed future's value (`docs/21` §1).
    pub ready_def: DefId,
    /// Prelude `interface Future<Output>` — the async state-machine shape.
    pub future_def: DefId,
    /// Prelude `extern struct Context` — carries the waker (`docs/21` §2).
    pub context_def: DefId,
    /// Prelude `interface AsyncIterator<T>` — async streams (`docs/21` §10).
    pub async_iterator_def: DefId,
    /// Prelude `struct TimedOut` — `timeout` loser marker (`docs/21` §9).
    pub timed_out_def: DefId,
    /// Prelude `interface Eq` — structural equality (`docs/15`); the `T: Eq`
    /// bound for `@Derive(Eq)` on generic structs.
    pub eq_def: DefId,
    /// Prelude `interface Ord` — total ordering (`docs/15`); the `T: Ord` bound
    /// for `@Derive(Ord)` on generic structs.
    pub ord_def: DefId,
    /// Prelude `interface ToStr` — string rendering (`docs/15`/`docs/01` §8);
    /// the `T: ToStr` bound for `@Derive(ToStr)` on generic structs.
    pub to_str_def: DefId,
    /// Prelude `interface Hash` — structural hashing (`docs/15` §7); the `T: Hash`
    /// bound for `@Derive(Hash)` on generic structs and for `Map<K, V>` keys.
    pub hash_def: DefId,
    /// Prelude `struct MapKeys<K>` — the `Iterator<K>` returned by `Map.keys()`
    /// (`docs/18` §6). Holds a snapshot `List<K>` of the keys at call time.
    pub map_keys_def: DefId,
    /// Prelude `struct MapValues<V>` — the `Iterator<V>` returned by
    /// `Map.values()`. Holds a snapshot `List<V>` of the values at call time.
    pub map_values_def: DefId,
    /// Prelude `struct MapEntries<K, V>` — the `Iterator<Entry<K, V>>` returned
    /// by `Map.entries()`. Holds a reference to the map plus a snapshot of its
    /// keys; values are looked up lazily as each `next()` runs.
    pub map_entries_def: DefId,
    /// Prelude `struct ListIter<T>` — the `Iterator<T>` returned by `List.iter()`
    /// (`docs/18` §5). Holds the live list plus a cursor (reads through to the
    /// list, so it is a view, not a snapshot).
    pub list_iter_def: DefId,
    /// Prelude `struct StrChars` — the `Iterator<char>` returned by `str.chars()`
    /// (`docs/18` §4). Holds a snapshot `List<char>` of the string's Unicode
    /// scalars at call time.
    pub str_chars_def: DefId,
    /// Prelude `struct StrBytes` — the `Iterator<u8>` returned by `str.bytes()`.
    /// Holds a snapshot `List<u8>` of the string's UTF-8 bytes at call time.
    pub str_bytes_def: DefId,
    /// Names the language prelude + builtins put in *every* module's scope
    /// (`List`, `Map`, `Item`, `Done`, `Iterator`, `Entry`, …), so a submodule
    /// resolves them without an `import`. Snapshotted from the root after the
    /// prelude is collected. Type and value namespaces.
    pub prelude_types: HashMap<String, DefId>,
    pub prelude_values: HashMap<String, DefId>,
    /// A synthetic module aggregating the toolchain (`core:`/`std:`) surface,
    /// against which explicit `core:`/`std:` imports resolve. The partition
    /// into named submodules (`core:collections`, `std:io`, …) lands with the
    /// prelude split (`docs/17` §17.8); until then this is the resolution
    /// target for every toolchain import.
    pub builtin_module: ModId,
    /// The named toolchain modules: `["core","collections"]` → its module, etc.
    /// Each is a curated view over `__builtins__` exposing exactly the names
    /// that module publishes (`docs/17` §17.8). `core:`/`std:` imports resolve
    /// against these.
    pub builtin_modules: HashMap<Vec<String>, ModId>,
    /// The prelude marker functions (`print`/`println`/`panic`/…) → the builtin
    /// they dispatch to. A call whose callee resolves to one of these `DefId`s
    /// lowers to the builtin intrinsic, so the names are ordinary importable
    /// symbols (`docs/17` §17.8) rather than magic.
    pub builtin_fns: HashMap<DefId, crate::sema::results::Builtin>,
    /// Resolved dependency packages: `pkg:<name>` → the root module of that
    /// dependency's collected subtree. `pkg:<name>` imports resolve against this
    /// module's public surface (`docs/17` §17.4).
    pub package_roots: HashMap<String, ModId>,
    /// `file:` import targets: normalized target file → its collected module.
    /// A `file:` import resolves into this module's public surface.
    pub file_modules: HashMap<std::path::PathBuf, ModId>,
}

/// Compiler-provided prelude written in the language itself (`docs/18` §7–8).
/// Parsed and collected into the root module before user items. Kept minimal;
/// `List`/`Map` remain special-cased builtins (their storage is intrinsic).
const PRELUDE_SRC: &str = "\
struct Item<T> { value: T }
struct Done {}
interface Iterator<T> {
  function next(self): Item<T> | Done;
}
struct Entry<K, V> { key: K, value: V }
interface FromResidual<R> {
  function from_residual(r: R): Self;
}
interface Try<Output, Residual> {
  function branch(self): Output | Residual;
}
interface Clone {
  function clone(self): Self;
}
interface Eq {
  function eq(self, other: Self): bool;
}
interface Ord {
  function lt(self, other: Self): bool;
  function le(self, other: Self): bool;
  function gt(self, other: Self): bool;
  function ge(self, other: Self): bool;
}
interface ToStr {
  function to_str(self): str;
}
interface Hash {
  function hash(self): u64;
}
struct ListIter<T> { list: List<T>, index: i64 }
extend<T> ListIter<T>: Iterator<T> {
  function next(self): Item<T> | Done {
    if self.index >= self.list.size() {
      Done {}
    } else {
      var v = self.list[self.index];
      self.index = self.index + 1;
      Item { value: v }
    }
  }
}
struct MapKeys<K> { snapshot: List<K>, index: i64 }
struct MapValues<V> { snapshot: List<V>, index: i64 }
struct MapEntries<K, V> { map: Map<K, V>, keys: List<K>, index: i64 }
extend<K> MapKeys<K>: Iterator<K> {
  function next(self): Item<K> | Done {
    if self.index >= self.snapshot.size() {
      Done {}
    } else {
      var v = self.snapshot[self.index];
      self.index = self.index + 1;
      Item { value: v }
    }
  }
}
extend<V> MapValues<V>: Iterator<V> {
  function next(self): Item<V> | Done {
    if self.index >= self.snapshot.size() {
      Done {}
    } else {
      var v = self.snapshot[self.index];
      self.index = self.index + 1;
      Item { value: v }
    }
  }
}
extend<K, V> MapEntries<K, V>: Iterator<Entry<K, V>> {
  function next(self): Item<Entry<K, V>> | Done {
    if self.index >= self.keys.size() {
      Done {}
    } else {
      var k = self.keys[self.index];
      self.index = self.index + 1;
      var v = self.map[k];
      Item { value: Entry { key: k, value: v } }
    }
  }
}
struct StrChars { snapshot: List<char>, index: i64 }
struct StrBytes { snapshot: List<u8>, index: i64 }
extend StrChars: Iterator<char> {
  function next(self): Item<char> | Done {
    if self.index >= self.snapshot.size() {
      Done {}
    } else {
      var v = self.snapshot[self.index];
      self.index = self.index + 1;
      Item { value: v }
    }
  }
}
extend StrBytes: Iterator<u8> {
  function next(self): Item<u8> | Done {
    if self.index >= self.snapshot.size() {
      Done {}
    } else {
      var v = self.snapshot[self.index];
      self.index = self.index + 1;
      Item { value: v }
    }
  }
}
interface Drop {
  function drop(self);
}
struct JoinHandle<R> { id: i64 }
struct Joined<R> { value: R }
struct Panicked { message: str }
struct Sender<T> { chan: i64 }
struct Receiver<T> { chan: i64 }
struct ChannelClosed {}
struct Shared<T> { id: i64 }
struct LockBusy {}
struct Pending {}
struct Ready<T> { value: T }
extern struct Context {
  waker_data: *u8,
  wake_fn: extern (*u8) => null,
}
interface Future<Output> {
  function poll(self, ctx: *Context): Ready<Output> | Pending;
}
interface AsyncIterator<T> {
  function next_async(self): Future<Item<T> | Done>;
}
struct TimedOut {}
struct Set<T> {}
struct MpmcSender<T> { chan: i64 }
struct MpmcReceiver<T> { chan: i64 }
struct Thread {}
struct Foreign {}
struct CString {}
struct CStr {}
function print(s: str) {}
function println(s: str) {}
function panic(msg: str) {}
function panic_with<T>(value: T) {}
function exit(code: i32) {}
function abort() {}
function channel() {}
function channel_bounded() {}
function channel_mpmc() {}
function channel_mpmc_bounded() {}
function yield_now() {}
function sleep(ms: i64) {}
function timeout() {}
";

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
            item_def: DefId(0),
            done_def: DefId(0),
            iterator_def: DefId(0),
            entry_def: DefId(0),
            from_residual_def: DefId(0),
            try_def: DefId(0),
            clone_def: DefId(0),
            drop_def: DefId(0),
            join_handle_def: DefId(0),
            joined_def: DefId(0),
            panicked_def: DefId(0),
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
        let mut p = Program::new();
        // The prelude lives in its own `__builtins__` module — *not* `ROOT` — so
        // its names do not pollute user scope (`docs/17` §17.8: every named
        // symbol requires an import). Built-in *syntax* still resolves via the
        // stored prelude `DefId`s, and the prelude's own `extend` blocks are
        // scanned program-wide by method resolution.
        let builtins = p.new_module("__builtins__".into(), ModId::ROOT, true);
        p.builtin_module = builtins;
        p.inject_builtins(builtins);
        p.collect_prelude(builtins);
        p.build_builtin_views();
        // Near-empty prelude (`docs/17` §17.8): built-in *syntax* resolves via
        // the stored prelude `DefId`s, but no built-in *name* is universally
        // visible — every named symbol (`List`, `Map`, `print`, `panic`, …) must
        // be imported. The universal-visibility maps stay empty.
        p.collect_items(ModId::ROOT, &root.items, externals, &[]);
        // Collect each resolved dependency package as a standalone subtree (not
        // reachable through the user `mod` tree); `pkg:<name>` resolves into it.
        let mut pkgs: Vec<(&String, &Vec<String>)> = ctx.packages.iter().collect();
        pkgs.sort_by(|a, b| a.0.cmp(b.0));
        for (name, key) in pkgs {
            if let Some(entry) = externals.get(key) {
                let pkg_mod = p.new_module(format!("__pkg__{name}"), ModId::ROOT, true);
                p.modules[pkg_mod.index()].path = key.clone();
                p.collect_items(pkg_mod, &entry.items, externals, key);
                p.package_roots.insert(name.clone(), pkg_mod);
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

    /// Lex, parse, and collect [`PRELUDE_SRC`] into the root module. The prelude
    /// uses a dedicated `FileId` so its spans never collide with user source.
    fn collect_prelude(&mut self, target: ModId) {
        let file = crate::span::FileId(u32::MAX);
        let (tokens, lex_errs) = crate::lexer::lex(PRELUDE_SRC, file);
        debug_assert!(lex_errs.is_empty(), "prelude lex errors: {lex_errs:?}");
        let (module, parse_errs) = crate::parser::parse(PRELUDE_SRC, &tokens);
        debug_assert!(parse_errs.is_empty(), "prelude parse errors: {parse_errs:?}");
        // The prelude has no file-backed submodules.
        self.collect_items(target, &module.items, &Externals::new(), &[]);
        let types = &self.modules[target.index()].types;
        self.item_def = types.get("Item").copied().unwrap_or(DefId(0));
        self.done_def = types.get("Done").copied().unwrap_or(DefId(0));
        self.iterator_def = types.get("Iterator").copied().unwrap_or(DefId(0));
        self.entry_def = types.get("Entry").copied().unwrap_or(DefId(0));
        self.from_residual_def = types.get("FromResidual").copied().unwrap_or(DefId(0));
        self.try_def = types.get("Try").copied().unwrap_or(DefId(0));
        self.clone_def = types.get("Clone").copied().unwrap_or(DefId(0));
        self.drop_def = types.get("Drop").copied().unwrap_or(DefId(0));
        self.join_handle_def = types.get("JoinHandle").copied().unwrap_or(DefId(0));
        self.joined_def = types.get("Joined").copied().unwrap_or(DefId(0));
        self.panicked_def = types.get("Panicked").copied().unwrap_or(DefId(0));
        self.sender_def = types.get("Sender").copied().unwrap_or(DefId(0));
        self.receiver_def = types.get("Receiver").copied().unwrap_or(DefId(0));
        self.channel_closed_def = types.get("ChannelClosed").copied().unwrap_or(DefId(0));
        self.shared_def = types.get("Shared").copied().unwrap_or(DefId(0));
        self.lock_busy_def = types.get("LockBusy").copied().unwrap_or(DefId(0));
        self.pending_def = types.get("Pending").copied().unwrap_or(DefId(0));
        self.ready_def = types.get("Ready").copied().unwrap_or(DefId(0));
        self.future_def = types.get("Future").copied().unwrap_or(DefId(0));
        self.context_def = types.get("Context").copied().unwrap_or(DefId(0));
        self.async_iterator_def = types.get("AsyncIterator").copied().unwrap_or(DefId(0));
        self.timed_out_def = types.get("TimedOut").copied().unwrap_or(DefId(0));
        self.eq_def = types.get("Eq").copied().unwrap_or(DefId(0));
        self.ord_def = types.get("Ord").copied().unwrap_or(DefId(0));
        self.to_str_def = types.get("ToStr").copied().unwrap_or(DefId(0));
        self.hash_def = types.get("Hash").copied().unwrap_or(DefId(0));
        self.map_keys_def = types.get("MapKeys").copied().unwrap_or(DefId(0));
        self.map_values_def = types.get("MapValues").copied().unwrap_or(DefId(0));
        self.map_entries_def = types.get("MapEntries").copied().unwrap_or(DefId(0));
        self.list_iter_def = types.get("ListIter").copied().unwrap_or(DefId(0));
        self.str_chars_def = types.get("StrChars").copied().unwrap_or(DefId(0));
        self.str_bytes_def = types.get("StrBytes").copied().unwrap_or(DefId(0));
        // Map the marker functions to their builtin intrinsics. A call resolving
        // to one of these defs lowers to the builtin (`docs/14`, `docs/24`).
        use crate::sema::results::Builtin;
        let values = self.modules[target.index()].values.clone();
        for (name, b) in [
            ("print", Builtin::Print),
            ("println", Builtin::Println),
            ("panic", Builtin::Panic),
            ("panic_with", Builtin::PanicWith),
            ("exit", Builtin::Exit),
            ("abort", Builtin::Abort),
        ] {
            if let Some(d) = values.get(name).copied() {
                self.builtin_fns.insert(d, b);
            }
        }
    }

    /// The builtin a marker-function `DefId` dispatches to, if any.
    pub fn builtin_of_def(&self, def: DefId) -> Option<crate::sema::results::Builtin> {
        self.builtin_fns.get(&def).copied()
    }

    /// Whether `def` is a toolchain (prelude/`core:`/`std:`) definition — it
    /// lives in `__builtins__`. Used to tell an *imported* builtin name apart
    /// from a *user* shadow when recognizing built-in intrinsics.
    pub fn is_builtin_def(&self, def: DefId) -> bool {
        self.defs[def.index()].module == self.builtin_module
    }

    /// Build the curated `core:`/`std:` module views over `__builtins__`
    /// (`docs/17` §17.8). Each view exposes exactly the names that module
    /// publishes; internal iterator adapters (`ListIter`, `MapKeys`, …) are not
    /// exposed. `print`/`println`/`panic`/etc. are added by [`Self::add_builtin_fns`].
    fn build_builtin_views(&mut self) {
        // (path, names) — each name is copied from `__builtins__` in whichever
        // namespace(s) it occupies (a unit struct is both a type and a value).
        const VIEWS: &[(&[&str], &[&str])] = &[
            (
                &["core", "prelude"],
                &[
                    "Iterator", "Item", "Done", "FromResidual", "Try", "Clone", "Drop", "Eq",
                    "Ord", "ToStr", "Hash", "Future", "Ready", "Pending", "Context",
                    "panic", "panic_with", "exit", "abort",
                ],
            ),
            (&["core", "collections"], &["List", "Map", "Set", "Entry"]),
            (&["core", "ffi"], &["Foreign", "CString", "CStr"]),
            (&["std", "io"], &["print", "println"]),
            (&["std", "thread"], &["Thread", "JoinHandle", "Joined", "Panicked"]),
            (
                &["std", "sync"],
                &[
                    "Sender", "Receiver", "ChannelClosed", "Shared", "LockBusy", "MpmcSender",
                    "MpmcReceiver", "channel", "channel_bounded", "channel_mpmc",
                    "channel_mpmc_bounded",
                ],
            ),
            (&["std", "async"], &["AsyncIterator", "TimedOut", "yield_now", "sleep", "timeout"]),
        ];
        let b = self.builtin_module.index();
        for (path, names) in VIEWS {
            let path_vec: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            let leaf = path.last().copied().unwrap_or("");
            let view = self.new_module(format!("__view_{}", path.join("_")), ModId::ROOT, true);
            for name in *names {
                if let Some(d) = self.modules[b].types.get(*name).copied() {
                    self.modules[view.index()].types.insert(name.to_string(), d);
                }
                if let Some(d) = self.modules[b].values.get(*name).copied() {
                    self.modules[view.index()].values.insert(name.to_string(), d);
                }
            }
            self.modules[view.index()].path = path_vec.clone();
            let _ = leaf;
            self.builtin_modules.insert(path_vec, view);
        }
    }

    /// Inject compiler-provided prelude types (currently `List<T>`). These have
    /// no AST item; their behavior is special-cased in the checker and code
    /// generator. Injected before user items so they get stable low ids.
    fn inject_builtins(&mut self, target: ModId) {
        let span = Span::new(crate::span::FileId(0), crate::span::BytePos(0), crate::span::BytePos(0));
        let list = self.add_def(DefKind::Struct, "List".into(), target, None, true, span);
        let t = self.add_def(DefKind::GenericParam, "T".into(), target, Some(list), false, span);
        self.defs[list.index()].generics = vec![t];
        self.modules[target.index()].types.insert("List".into(), list);
        self.list_def = list;

        let map = self.add_def(DefKind::Struct, "Map".into(), target, None, true, span);
        let k = self.add_def(DefKind::GenericParam, "K".into(), target, Some(map), false, span);
        let v = self.add_def(DefKind::GenericParam, "V".into(), target, Some(map), false, span);
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
    /// definitions, then names it imported, then the universal prelude. Parent
    /// modules are *not* searched — names cross module boundaries only via
    /// `import` (`docs/17` §3).
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

    /// Resolve a *public* value defined directly in `module` (for namespaced
    /// access `M.foo`, which only reaches the module's own exported items).
    pub fn resolve_pub_value_in(&self, module: ModId, name: &str) -> Option<DefId> {
        let def = *self.modules[module.index()].values.get(name)?;
        self.defs[def.index()].public.then_some(def)
    }

    /// The `extend` blocks visible from `module` for method resolution: the
    /// module's own plus the prelude's (`__builtins__`), since the prelude's
    /// `extend` impls — `List`/`Map`/`str` iterators, etc. — are program-wide
    /// (`docs/17` §17.8, orphan rule). The builtins are not double-counted when
    /// `module` *is* the builtins module.
    pub fn visible_extends(&self, module: ModId) -> Vec<DefId> {
        let mut out = self.modules[module.index()].extends.clone();
        if module != self.builtin_module {
            out.extend(self.modules[self.builtin_module.index()].extends.iter().copied());
        }
        out
    }

    /// The module an `import … as alias` brings into scope in `module`.
    pub fn namespace_target(&self, module: ModId, alias: &str) -> Option<ModId> {
        self.modules[module.index()].namespace_imports.get(alias).copied()
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
            for imp in &imports {
                let raw = import_path_string(&imp.path);
                let span = imp.path.span;
                let parsed = match crate::imports::classify(&raw) {
                    Ok(p) => p,
                    Err(e) => {
                        self.errors.push(SemaError::new(SemaErrorKind::Message(e.to_string()), span));
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
                            Some(target) => {
                                self.bind_import(mid, target, imp, /* toolchain = */ true)
                            }
                            None => self.errors.push(SemaError::new(
                                SemaErrorKind::Message(format!(
                                    "no built-in module `{}`",
                                    parsed.display_source()
                                )),
                                span,
                            )),
                        }
                    }
                    Scheme::SelfRoot => match self.resolve_self_root(&parsed.segments) {
                        Ok(target) => self.bind_import(mid, target, imp, false),
                        Err(msg) => self.errors.push(SemaError::new(SemaErrorKind::Message(msg), span)),
                    },
                    Scheme::SelfRel => {
                        match self.resolve_self_rel(&mod_path, &parsed, ctx, &file_to_module) {
                            Ok(target) => self.bind_import(mid, target, imp, false),
                            Err(msg) => {
                                self.errors.push(SemaError::new(SemaErrorKind::Message(msg), span))
                            }
                        }
                    }
                    Scheme::Pkg => {
                        let name = parsed.package_name().unwrap_or("").to_string();
                        if !ctx.dependencies.contains(&name) {
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
                        let Some(&pkg_root) = self.package_roots.get(&name) else {
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
                            Ok(target) => self.bind_import(mid, target, imp, false),
                            Err(msg) => self.errors.push(SemaError::new(
                                SemaErrorKind::Message(msg),
                                span,
                            )),
                        }
                    }
                    Scheme::File => {
                        // Enforce the allowlist/escape gate, then bind names from
                        // the loaded target module (`docs/17` §17.4).
                        match self.check_file_import(&mod_path, &parsed, ctx) {
                            Ok(target) => match self.file_modules.get(&target).copied() {
                                Some(m) => self.bind_import(mid, m, imp, false),
                                None => self.errors.push(SemaError::new(
                                    SemaErrorKind::Message(format!(
                                        "`{}` could not be loaded (expected file `{}`)",
                                        parsed.display_source(),
                                        target.display()
                                    )),
                                    span,
                                )),
                            },
                            Err(msg) => {
                                self.errors.push(SemaError::new(SemaErrorKind::Message(msg), span))
                            }
                        }
                    }
                }
            }
        }
    }

    /// Resolve a `self:` root path to a module in this package's tree.
    fn resolve_self_root(&self, segments: &[String]) -> Result<ModId, String> {
        let target = self
            .module_by_path(segments)
            .ok_or_else(|| format!("cannot find module `self:{}`", segments.join("/")))?;
        if self.modules[target.index()].external_unloaded {
            return Err(format!("module `self:{}` was not loaded", segments.join("/")));
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
            let child = self.modules[cur.index()].children.get(seg).copied().ok_or_else(|| {
                format!("`pkg:{name}/{}` is not a module in `{name}`", subpath.join("/"))
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
        let mut dir =
            importing_file.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
        for _ in 0..parsed.up {
            dir = dir.parent().map(|p| p.to_path_buf()).ok_or_else(|| {
                escape_message(parsed, ctx)
            })?;
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
                    format!("`{}` does not resolve to a declared module", parsed.display_source())
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
        let mut dir =
            importing_file.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
        for _ in 0..parsed.up {
            dir = dir.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| dir.clone());
        }
        for seg in &parsed.segments {
            dir.push(seg);
        }
        if dir.extension().is_none() {
            dir.set_extension("otter");
        }
        let target = normalize(&dir);

        // Direct mode (no source root): `file:` is unrestricted.
        let Some(source_root) = &ctx.source_root else { return Ok(target) };
        let source_root = normalize(source_root);
        if target.starts_with(&source_root) {
            return Ok(target); // inside the package — always allowed
        }
        // Escaping the package: must match an allowlist entry.
        let root = ctx.package_root.clone().unwrap_or_else(|| source_root.clone());
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
    /// export their documented surface; the prelude items are not user-`pub`).
    fn bind_import(&mut self, mid: usize, target: ModId, imp: &ImportItem, toolchain: bool) {
        match &imp.kind {
            ImportKind::Named(names) => {
                for n in names {
                    self.resolve_named_import(mid, target, n, toolchain);
                }
            }
            // `import "path" as M` — bind the alias to the module so `M.foo`
            // resolves against its public definitions.
            ImportKind::Namespace(alias) => {
                self.modules[mid].namespace_imports.insert(alias.name.clone(), target);
            }
            // Ambient (extension-only) imports: extensions are module-bound and
            // already active program-wide; no names are bound.
            ImportKind::Ambient => {}
        }
    }

    /// Bind one `import { name as alias }` entry from `target` into module
    /// `mid`. `toolchain` skips the `pub` gate for built-in (`core:`/`std:`)
    /// modules, whose exported surface is not marked user-`pub`.
    fn resolve_named_import(&mut self, mid: usize, target: ModId, n: &ImportName, toolchain: bool) {
        let src = n.name.name.clone();
        let bind = n.alias.as_ref().unwrap_or(&n.name).name.clone();
        let tmod = &self.modules[target.index()];
        let as_type = tmod.types.get(&src).copied();
        let as_value = tmod.values.get(&src).copied();
        if as_type.is_none() && as_value.is_none() {
            self.errors.push(SemaError::new(
                SemaErrorKind::Message(format!("no `{src}` in the imported module")),
                n.span,
            ));
            return;
        }
        // Both namespaces an item occupies (e.g. a unit struct) must be public —
        // except for toolchain modules, whose surface is not user-`pub`.
        if !toolchain {
            for d in [as_type, as_value].into_iter().flatten() {
                if !self.defs[d.index()].public {
                    self.errors.push(SemaError::new(
                        SemaErrorKind::Message(format!("`{src}` is private")),
                        n.span,
                    ));
                    return;
                }
            }
        }
        if let Some(d) = as_type {
            self.modules[mid].imported_types.insert(bind.clone(), d);
        }
        if let Some(d) = as_value {
            self.modules[mid].imported_values.insert(bind, d);
        }
    }

    fn new_module(&mut self, name: String, parent: ModId, public: bool) -> ModId {
        let id = ModId(self.modules.len() as u32);
        self.modules.push(ModuleInfo::new(id, name, Some(parent), public));
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

    fn collect_items(&mut self, module: ModId, items: &[Item], externals: &Externals, path: &[String]) {
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
                let kind = if s.is_extern { DefKind::ExternStruct } else { DefKind::Struct };
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
                self.modules[module.index()].imports.push(imp.clone());
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
                self.register_name(module, &f.name.name, def, DefKind::ExternFunction, f.name.span);
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
                self.register_name(module, &s.name.name, def, DefKind::ExternStruct, s.name.span);
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
        let p = program(
            "mod math {\n  pub function add(a: i64, b: i64): i64 { a + b }\n}\n",
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let math = p.module(ModId::ROOT).children.get("math").copied().unwrap();
        assert!(lookup_value(&p, math, "add").is_some());
        // Not visible from the root namespace.
        assert!(lookup_value(&p, ModId::ROOT, "add").is_none());
    }

    #[test]
    fn interface_and_extend_methods_collected() {
        let p = program(
            "interface Named { function name(self): str; }\n\
             struct P { x: i64 }\n\
             extend P: Named { function name(self): str { \"p\" } }\n",
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        // The user's `Named.name` interface method is collected (the prelude
        // also contributes `Iterator.next`, so filter by name).
        let iface_methods = p
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::InterfaceMethod && d.name == "name")
            .count();
        assert_eq!(iface_methods, 1);
        // The user's `extend P: Named` contributes one method; the prelude
        // also contributes `extend MapKeys/MapValues/MapEntries: Iterator<…>`
        // impls (each with one `next` method) and is collected into ROOT.
        let user_extend_methods = p
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::ExtendMethod && d.name == "name")
            .count();
        assert_eq!(user_extend_methods, 1);
        let user_extends = p
            .module(ModId::ROOT)
            .extends
            .iter()
            .filter(|&&e| {
                p.def(e).item.as_ref().is_some_and(|it| matches!(
                    it,
                    ItemKind::Extend(e) if matches!(
                        &e.target.kind,
                        TypeKind::Named { name, .. } if name.name == "P"
                    )
                ))
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
        packages.insert(dep.to_string(), vec!["__pkg__".to_string(), dep.to_string()]);
        let mut dependencies = std::collections::HashSet::new();
        dependencies.insert(dep.to_string());
        ResolveContext { project: true, dependencies, packages, ..Default::default() }
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
        assert!(p.errors.iter().any(|e| e.kind.to_string().contains("private")), "{:?}", p.errors);
    }

    #[test]
    fn pkg_import_of_undeclared_dependency_is_rejected() {
        let root = parse_module("import { x } from \"pkg:unknown\";\nfunction main() {}");
        let ctx = ResolveContext { project: true, ..Default::default() };
        let p = Program::collect_multi_ctx(&root, &Externals::new(), &ctx);
        assert!(p.errors.iter().any(|e| e.kind.to_string().contains("no dependency named")), "{:?}", p.errors);
    }
}
