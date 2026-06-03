# Implementation Roadmap

This is the durable build plan for the Otter Fusion compiler (source files use
the `.otter` extension). It is the source of truth for *what is done* and *what
is next*, so work survives across sessions. Update the status markers as phases
complete.

The language is fully specified in `docs/01`..`docs/24` (HTML). Read those for
semantics; this file is only the implementation sequencing.

## Architecture decisions (locked)

- **GC**: Build the runtime behind a `Gc` abstraction. Ship a *correct precise
  tracing collector* first (two-word object header `[type-ptr | mark/fwd]`,
  pointer at the field boundary, Cranelift `user_stack_maps` precise roots,
  async/best-effort drop via a finalizer queue, two disjoint heaps). Swap in
  real **MMTk Immix/StickyImmix** as the production plan once the pipeline runs
  end-to-end. The *semantic contract* from `docs/16-memory` is honored from the
  start; only the collector internals are staged.
- **Codegen**: **Cranelift**. `otter_fusion run` uses `cranelift-jit` (in-process)
  first; `otter_fusion build` adds `cranelift-object` + system linker. Both share one
  codegen backend.
- **Generics**: monomorphization by default; dynamic dispatch only when an
  interface name is used as a value type (fat pointer / header vtable).
- **Compilation order** (from `docs/22` §4): lex → parse → macro-expand →
  resolve modules → type-check → monomorphize → codegen.
- **Standard library architecture**: the stdlib is provider/catalog driven, not
  one blob of compiler magic. `core:*` modules are the small compiler/runtime
  substrate: desugar targets, runtime-recognized layouts, privileged handles,
  syntax protocols, and macro/compiler APIs. `std:*` modules are the official
  toolchain library, whether fully portable or target-backed. Each toolchain
  module records its path, tier, exports, and implementation kind (`Otter`,
  `RustBacked`, or `Mixed`) in `compiler::sema::stdlib`, so current built-ins
  and future target/custom sysroots share one resolution model.

## Crate layout

- `crates/compiler` (`liblangc` core): lexer, parser, AST [DONE], then `ty`,
  `sema` (resolve + check), `mir`, `monomorphize`, `codegen`.
- `crates/runtime`: GC, object layout, intrinsics, primitive `str`/`List`/`Map`,
  scheduler/executor, channels — linked into compiled programs.
- `crates/cli` (`otter_fusion`): driver, project loading, subcommands.
- `crates/lsp`: language server over `liblangc`.

## Phases

### Phase 0 — Frontend  ✅ DONE
Lexer, parser, AST, spans, diagnostics. 205 tests.

### Phase 1 — Type representation + name resolution  ✅ DONE
- [x] `ty.rs`: interned `Ty`, primitives, named/tuple/func/union/ptr/array/
      dynamic/never/Self/param/infer/error. Union normalization (flatten,
      dedup, order-independent, single collapses). 26 tests.
- [x] `ids.rs`: `DefId`, `ModId`.
- [x] `sema/symbols.rs`: `Def`/`DefKind` table, module tree built from inline
      `mod`, two namespaces, duplicate detection, generic-param/field defs,
      interface/extend method collection. 6 tests.
- [x] `sema/diag.rs`: `SemaError`.
- [x] `sema/lower.rs`: AST `Type` → `Ty` (alias expansion w/ cycle detection,
      generic args, `Self`/param resolution, primitive recognition, arity
      checks). 8 tests.
- [x] **Cross-module name resolution + imports + visibility** (`docs/17`):
      name lookup is module-scoped via `Program::resolve_{type,value}_in(module,
      name)` — own definitions, then `import`-bound names; parent modules are
      *not* searched (names cross boundaries only via `import`). The checker
      tracks `cur_module` per function.

### Modules, imports & packages  ✅ DONE (`docs/17`, `docs/23` §7–§8)
The full module/import/package system, end to end.
- **Import scheme system** (`compiler::imports`): every path carries an explicit
  scheme — `core:`/`std:`/`pkg:`/`self:` (root + `./`,`../` relative)/`file:`.
  Prefix-less paths and reserved families (`url:`/`http:`/`https:`/`+blob`/
  `pkg+https`) are pointed errors. `Program::resolve_imports` is scheme-aware:
  `self:` root walks the `mod` tree, `self:` relative resolves against the
  importing file's directory with the **package-escape rule**, `core:`/`std:`
  resolve against curated named built-in views, `pkg:` binds a dependency's
  public tree, `file:` binds names from a loaded target module after the
  allowlist/escape gate.
- **Run modes** (`docs/17` §17.13): `run`/`build`/`check` (project or direct),
  `exec` (standalone). Direct mode does the reachability walk; `pkg:`/`self:`
  are hard errors without project context. Unreferenced-source-file error.
  Module-tree loader in `crates/pkg::loader` (sibling rule for entries,
  child-dir below).
- **Near-empty prelude** (`docs/17` §17.8): the prelude lives in a hidden
  `__builtins__` module; only built-in *syntax* resolves (via stored `DefId`s).
  Every named symbol — `List`/`Map`/`print`/`println`/`panic`/`Clone`/… —
  requires an import from `core:prelude`/`core:collections`/`std:io`/`std:*`.
  `print`/`println`/`panic`/`panic_with` are importable marker functions
  dispatched by `DefId`; process-control markers `exit`/`abort` now live under
  `std:process` while using the same lowering path. The concurrency/FFI intrinsics
  (`channel`/`sleep`/`yield_now`/`timeout`/`Thread.spawn`/`Shared.new`/
  `Foreign.*`) also require their import — recognized only when
  the name resolves to a built-in (`intr_fn`/`intr_ns`). (`CString`/`CStr`/
  `Buffer` are now ordinary imported prelude types with real `extend` methods,
  not callee-shape intrinsics.) Only `str` methods +
  numeric namespaces stay free (per spec).
- **Package manager** (`crates/pkg`): `project.toml` manifest, `project.lock`
  lockfile (+ sha256 verify), semver resolution (unify compatible ranges),
  content-addressed store, sparse-HTTP registry protocol (behind a `Registry`
  trait: real `HttpRegistry` + `LocalRegistry` fixture) **and a matching
  server** (`server.rs`, dependency-free HTTP/1.1 on `TcpListener`), transitive
  resolver (path + registry deps). `pkg:` binding compiles dependency libraries
  and exposes their `pub` API. CLI: `add`/`remove`/`update`/`lock`(+`--check`)/
  `tree`/`why`/`vendor`/`login`/`logout`/`search`/`publish`/`yank`/`audit`, plus
  `serve` (host a private registry). **Live registry network round-trips are now
  exercised end-to-end**: `crates/pkg/tests/live_registry.rs` boots the server on
  an ephemeral localhost port and round-trips `HttpRegistry` connect → publish
  (auth-gated) → index → download → checksum-verify → search (yank-aware) → yank
  over real TCP (3 tests). The offline `LocalRegistry` still proves resolution.
  Deferred (advanced): git dependency *fetching* (sources recorded, not cloned),
  feature-gated optional-dep resolution (`[features]` parsed; optional deps
  skipped), multi-major coexistence (incompatible majors error), and the
  metadata-sidecar on publish (the server records empty `deps` for uploaded
  versions — sufficient to host/resolve; dependency edges are written directly).
- **Tests**: `imports` 11, `pkg` 80+ (manifest/loader/lockfile/store/semver/
  registry/resolve/commands/credentials/package), `cli` e2e (every scheme,
  run modes, escape rule, file: gating, dep commands, `pkg:` run e2e). All
  green; every example runs (JIT + native).

### Standard Library Foundation  🔧 IN PROGRESS (`docs/18`, `docs/24`, `docs/29`)

- [x] **`std:error` implemented**: explicit imports expose `Error`,
      `Annotated`, and `with_context<T, E: Error>(T | E, str): T | Annotated`.
      `Annotated` implements diagnostic debug rendering; clone/equality/hash
      are intentionally not generic because the wrapped source is only an
      `Error` object.
      This also hardened the compiler paths std relies on: super-interface
      method lookup, transitive interface impl recording for vtables, bounded
      generic inference through union templates (`T | E`), preserving
      `Unbox -> Widen/WidenDyn` HIR chains, and monomorphized generic unbox
      codegen. Covered by compiler unit tests plus e2e stdlib/interface cases.
- [x] **Stdlib catalog/provider foundation**: `compiler::sema::stdlib` records
      each built-in `core:*`/`std:*` module path, tier, exports, and
      implementation kind (`Otter`, `RustBacked`, or `Mixed`). Name resolution
      now builds curated import views from the active provider, leaving room for
      future target/custom sysroots to replace, omit, or extend `std:*` modules
      without changing source-level import syntax. Semantic collection also has
      an explicit-provider entry point, with unit coverage proving a custom
      provider can add custom `std:*` module views, omit unsupported `std:*`
      modules, and keep listed `core:*` substrate modules available; diagnostics
      for omitted known modules now name the selected provider. Provider exports
      are also validated against the collected toolchain source so a catalog
      cannot silently advertise missing symbols or duplicate names, and provider
      module paths/tier roots are checked for root-only paths, duplicates, and
      `core:`/`std:` mismatches. The e2e suite now has a project fixture
      directive for manifest-driven stdlib behavior, with `no-std` cases proving
      that `std:*` imports are rejected while `core:*` substrate imports remain
      available. The catalog unit tests also scan the stdlib require-import
      e2e diagnostics and assert every exported name is represented, so future
      catalog additions must preserve the near-empty-prelude import-gating
      coverage.
- [x] **Stdlib extension guide**: `docs/29-extending-stdlib.html` defines the
      contributor workflow for adding `core:*`/`std:*` modules, choosing
      `Otter`/`RustBacked`/`Mixed`, wiring catalog exports, adding Rust runtime
      hooks, preserving the near-empty prelude, planning custom providers, and
      covering each module with unit, integration, e2e, docs, examples, and LSP
      updates.
- [x] **`core:ffi` catalog surface tightened**: C-width aliases
      (`c_int`, `c_size_t`, etc.) plus `c_void` and `c_va_list` are real
      `core:ffi` exports now, not just documentation examples. The symbol-table
      tests verify every catalog export materializes in its builtin view so
      future `StdModuleSpec` drift is caught by CI.
- [x] **Toolchain library organization**: the old monolithic embedded
      `PRELUDE_SRC` has been replaced by `crates/compiler/src/sema/stdlib_src/`
      with module-shaped Otter Fusion files under `core/` and `std/`. The
      compiler now collects them through the explicit `TOOLCHAIN_SOURCES`
      manifest into hidden module-local owners under the private `__builtins__`
      root, then builds curated catalog views, preserving the near-empty prelude
      rule while making future sysroot/provider work a real source-layout step.
- [x] **`std:bytes` + `std:io` contracts**: added explicit `std:bytes`
      (`Bytes`, `Utf8Error`) and expanded `std:io` from print intrinsics to the
      shared `Reader`/`Writer`/`Seeker` contracts, `SeekFrom`, and `IoError` /
      `IoErrorKind` catalog exports. Covered by stdlib e2e cases and catalog
      tests. `Bytes` now also has deep `Clone`, bytewise `Eq`, deterministic
      `Hash`, compact diagnostic `ToStr` rendering, and an in-memory
      `std:io.Writer` implementation for appending bytes. Direct
      `Bytes.set(index, byte)` returns `false` for out-of-range writes,
      `Bytes.pop()` removes and returns the last byte, `Bytes.remove_at(index)`
      removes and returns one indexed byte, `Bytes.append(other)`
      snapshots the input before appending, `Bytes.truncate(len)` shrinks the
      buffer in place, `Bytes.clear()` empties the buffer, and
      `Bytes.starts_with` / `Bytes.ends_with` provide raw bytewise affix
      checks. `Bytes.from_str`
      encodes a `str` into
      UTF-8 bytes using the existing string byte iterator, and
      `Bytes.decode_utf8()` validates strict UTF-8 back to `str | Utf8Error`
      (rejecting invalid leading bytes, malformed continuations, truncation,
      overlong sequences, surrogates, and code points above U+10FFFF).
      `BytesCursor` snapshots `Bytes` and implements in-memory
      `std:io.Reader`/`Seeker` behavior with byte reads, chunk reads,
      `read_to_end`, and `SeekFrom` support.
      `Utf8Error` implements `std:error.Error`, equality, clone, hash,
      diagnostic stringification, and debug rendering. `BytesCursor` implements
      equality and hashing over both the snapshot and current cursor position,
      plus clone, diagnostic stringification, and debug rendering.
      `print`/`println` write to stdout and `eprint`/`eprintln` write to stderr
      through runtime-backed marker functions; `stdin()`, `stdout()`, and
      `stderr()` now expose byte-oriented `Reader`/`Writer` handles with raw
      reads, writes, and flushes. `BufReader` and `BufWriter` provide
      interface-object buffered adapters with chunked reads, `read_line`,
      line iteration, buffered writes, and explicit flushing.
      `std:fs.File` provides target-backed `Reader`/`Writer`/`Seeker` handles;
      generic-specialized buffered wrappers and pinned `Buffer` views remain
      future provider/library work.
      `SeekFrom` and `IoErrorKind`
      provide direct equality helpers, `Eq.eq`, overloaded `==`, clone/hash/string/debug
      methods; `IoError` implements `Error`, equality, clone, hash, and debug
      rendering.
- [x] **`std:time::Duration`**: added the portable value type as an
      Otter-authored `std:time` export with constructors, unit conversions,
      absolute subsecond component helpers, predicates, `abs`,
      equality/ordering/hash/clone/stringification, and overloaded `+`/`-`.
      Covered by explicit-import and runtime e2e cases.
- [x] **`std:time` monotonic/system clocks**: added mixed stdlib value types
      `Instant` and `SystemTime` over Rust-backed runtime clock hooks. The
      Otter-authored surface covers `now`, fixed nanosecond constructors,
      duration arithmetic, Unix-epoch helpers, equality/ordering/hash/clone,
      stringification, and debug rendering. Calendar/timezone conversions
      remain future provider/runtime work.
- [x] **`std:time` calendar/timezone value contracts**: added portable
      Otter-authored `TimeZone`, `DateTime`, and `TimeError` values plus
      constructor helpers. `DateTime.new` validates calendar ranges, leap years,
      time-of-day fields, and nanosecond precision; `TimeZone` models UTC,
      fixed offsets, and named zone identifiers as ordinary values; `TimeError`
      implements `std:error.Error`. The value layer implements equality, clone,
      hash, stringification/ISO-like rendering, immutable timezone replacement,
      and diagnostic debug rendering.
      UTC/fixed-offset system-time conversion is implemented; local timezone
      lookup, timezone databases, named-zone conversion, and leap-second policy
      remain planned provider/library work.
- [x] **`std:time` ISO-like DateTime parsing**: added pure Otter Fusion
      `parse_iso8601(s): DateTime | TimeError` plus
      `DateTime.parse_iso8601`. The parser accepts the same portable shapes
      produced by `format_iso8601`: UTC `Z`, fixed `+HH:MM` / `-HH:MM` offsets,
      optional fractional seconds up to nanosecond precision, and bracketed
      named timezone identifiers. It reuses `DateTime.new` validation and
      returns `TimeError` for malformed fields. Full timezone database
      resolution, local-time lookup, named-zone conversion, and leap-second
      policy remain planned provider/library work.
- [x] **`std:fs.Path` value surface**: added the pure Otter-authored
      `std:fs` module exporting `Path` with construction, `as_str`, `join`,
      `join_path`, `normalize`, `components`, `starts_with`, `ends_with`,
      `strip_prefix`, `parent`, `file_name`, `file_stem`, `extension`,
      `with_extension`, `is_absolute`, `is_relative`, `is_empty`, `is_root`,
      `component_count`, equality, hash, clone, and stringification.
      `normalize` is purely lexical: it collapses repeated
      slashes and `.`, resolves `..` where possible, clamps absolute paths at
      `/`, and preserves leading relative `..`; component/prefix helpers compare
      normalized slash-separated components without touching the filesystem.
      Stem/extension helpers treat a single leading dot as part of the basename,
      so `.env` has no extension while `.profile.bak` has extension `bak`.
      `DirEntry` and `FileKind` provide the portable value shape for snapshot
      directory iteration, while `Metadata` and `Permissions` provide the
      portable value shape for metadata queries; these values implement
      equality, clone, hash, stringification, diagnostic debug rendering, and
      immutable-style replacement builders that snapshot nested path/kind and
      permissions values.
      Covered by explicit-import and runtime e2e cases. `std:fs` is now mixed:
      Rust-backed runtime hooks provide `Path.exists`, `Path.is_file`, and
      `Path.is_dir`, `Path.file_kind`, `Path.byte_len`, `Path.permissions`, and
      `Path.metadata`, `Path.canonicalize`, module-level `canonicalize`,
      `native_separator`, `Path.from_native`, module-level `path_from_native`,
      `Path.to_native_str`, binary `read`/`write` over `std:bytes.Bytes`,
      UTF-8 text `read_to_string`, `write_string`, and `append_string`,
      snapshot-backed `read_dir` returning `DirEntries`, plus
      path-backed `File.open`, `File.create`,
      `File.append`, `File.open_with`, `File.path`, descriptor-backed `Reader`/`Writer`/`Seeker`
      operations, text read/write/append, close,
      explicit `OpenOptions` values for read/write/append/truncate/create/
      create_new modes with provider-independent validation before runtime
      opens, non-recursive `remove`, `rename`, `create_dir`, and
      `create_dir_all`. Async filesystem adapters remain future
      provider/runtime work.
- [x] **`std:fmt` contracts**: added the Otter-authored `std:fmt` module
      exporting `Display: ToStr`, `Debug`, `FmtSink`, and `FmtError`.
      `FmtError` implements `std:error.Error`, equality, clone, hash, and
      diagnostic debug rendering.
      Interpolation and `value as str` still lower through `ToStr`; this module
      gives libraries explicit user-facing and developer-facing rendering
      contracts without adding format strings. Pure stdlib values (`Bytes`,
      `Utf8Error`, `Duration`, `Path`, `Json`, and `std:net:types` identifier
      values, plus struct-shaped `std:http` values) now implement `Debug`;
      `Bytes` also implements `FmtSink` as the standard in-memory UTF-8
      formatting sink.
      renderable collections now implement `Debug` as
      `List<T: Debug>`, `Set<T: Eq + Debug>`, and
      `Map<K: Eq + Hash + ToStr, V: Debug>`. Primitive `Debug` remains planned
      because primitive-to-`std:fmt.Debug` interface objects need compiler/backend
      intrinsic vtable support.
- [x] **Runtime marker value semantics**: `std:async.TimedOut`,
      `std:thread.Panicked`, `std:sync.ChannelClosed`, and
      `std:sync.LockBusy` now implement equality, clone, hash, stringification,
      and diagnostic debug rendering. Runtime handles (`JoinHandle`, channel
      endpoints, `Shared`) remain handle types rather than plain value records.
- [x] **`core:async` protocol surface**: moved `AsyncIterator` out of
      `std:async` and into the core catalog alongside `Future`, `Ready`,
      `Pending`, and `Context`; those protocol definitions now live in
      `stdlib_src/core/async.otter`, with the `core:prelude` catalog view
      remaining as a compatibility/ergonomics re-export for compiler-recognized
      shapes. `for await` is syntax lowered by the compiler, so its protocol is
      core substrate; `std:async` now keeps only runtime helpers such as
      `yield_now`, `sleep`, and `timeout`. `await` and `spawn` now bake an
      explicit dynamic widening when given a concrete type implementing
      `Future<Out>`, so the backend/executor always receive the interface
      object representation they poll or schedule.
- [x] **`core:collections.Set` method surface**: replaced the exported-but-empty
      `Set<T>` stub with an Otter-authored unique-list implementation gated by
      `T: Eq`. `Set<T>()` / `Set.new<T>()`, `size`, `is_empty`,
      `contains`, `insert`,
      `remove`, `insert_all`, `is_subset`, `is_superset`, `is_disjoint`,
      `clear`, `to_list`,
      `iter`, `union`, `intersect`, and `difference` are
      covered by stdlib e2e tests. `Set<T>` now also has value semantics:
      `Eq` when `T: Eq`, `Clone` when `T: Eq + Clone`, and deterministic
      order-insensitive `Hash` when `T: Eq + Hash`, plus compact `ToStr`
      rendering when `T: Eq + ToStr`. Hash-backed storage, set
      literal syntax, and custom keyed hashers remain planned work rather than
      documented-as-implemented behavior.
- [x] **`std:collections.Deque` value collection**: added an ordinary
      Otter-authored `std:collections` module exporting `Deque<T>` and
      `deque<T>()`, later expanded with `deque_from_list<T>(List<T>)`,
      `extend_front`, `extend_back`, and `contains` when `T: Eq`. `Deque` is
      deliberately `std:`, not `core:`, because no syntax or compiler lowering
      depends on its layout. The implementation uses two internal lists for
      front/back operations, exposes push/pop/front/back, direct `get(index)`
      reads, `set(index, value)` writes, `remove_at(index)` removals,
      size/empty, `to_list`, `iter`, and `clear`, and implements
      equality, clone, ordered deterministic hashing, stringification, and an
      explicit diagnostic debug implementation with the appropriate bounds.
      Covered by explicit-import and runtime e2e cases.
- [x] **`std:hash` deterministic and keyed hasher bridge**: added the pure Otter-authored
      `std:hash` module exporting `Hasher`, `DefaultHasher`, `hash_value`, and
      `write_hash`, `combine_hash`, plus explicit `KeyedHasher` and
      `keyed_hasher(seed)`.
      This keeps the compiler-recognized `Hash` interface in `core:prelude`
      while giving user code ordinary stdlib hasher values and deterministic
      helper functions. The mixer avoids wrapping arithmetic because Otter
      Fusion integer overflow is checked. Byte-stream writes are implemented
      through `write_u8` and `write_bytes(Bytes)`, with convenience writes for
      signed integers, booleans, strings, and `Hash` values; the top-level
      `write_hash(hasher, value)` helper feeds any `T: Hash` through a
      `Hasher` interface object. `DefaultHasher` and `KeyedHasher` implement
      state equality, clone, hash,
      stringification, and debug rendering so hasher streams can be snapshotted
      and compared. Keyed map/set construction, OS-seeded HashDoS-safe hashers,
      and fast/cryptographic variants remain future work.
- [x] **`std:http` value types**: added the pure Otter-authored `std:http`
      module exporting HTTP method/version unions, `Status`,
      multi-value `Headers`, flattened `HeaderEntry`, `HttpRequest`,
      `HttpResponse`, and constructor
      helpers. This standardizes ecosystem request/response shapes without
      adding clients, servers, sockets, or wire parsing. Request/response
      constructors, accessors, and `with_*` builder helpers snapshot mutable
      sub-values such as headers and body, and builders clone unchanged
      sub-values, with append/replace/remove/clear helpers for normalized
      headers.
      `Method` and `HttpVersion` implement direct `Eq` over their rendered
      protocol strings, matching request/response equality semantics.
      `Status` exposes constructors, explicit reason replacement builders,
      and default reason phrases across common
      informational, success, redirection, client-error, and server-error codes,
      plus all five class predicates.
      Headers normalize names with ASCII case-insensitive lowercase keys on
      insertion, lookup, removal, snapshots, equality, hashing, and debug
      rendering; equality and hashing compare normalized header names by
      membership while preserving value order within each name. They expose
      flattened entry and header-name snapshots for
      ordinary iteration over repeated values. `Headers.size()` counts
      normalized names, `value_count()` counts flattened values, `is_empty()`
      checks whether any names are present, and `clear()` removes all entries
      while preserving prior snapshots. Canonical parsing/rendering and
      HTTP client/server implementations remain future/pkg work.
      `Headers`, `HttpRequest`, and `HttpResponse` now implement value
      equality, clone, structural hash, and diagnostic debug rendering.
- [x] **`std:json` value tree**: added the pure Otter-authored `std:json`
      module exporting an opaque `Json` value plus `json_null`, `json_bool`,
      `json_number`, `json_string`, `json_array`, `json_object`, and typed
      object/array extractors. The public type is intentionally not a union so
      JSON values remain stable inside `List<Json>` and `Map<str, Json>` across
      module boundaries. `Json` now implements deep `Clone`, deep `Eq`,
      structural deterministic `Hash` with order-insensitive object entries,
      compact escaped `ToStr` rendering, and diagnostic
      `Debug`. Array/object constructors and extractors deep-snapshot mutable
      `List<Json>` / `Map<str, Json>` containers and nested JSON values, so
      caller mutations cannot alias an existing JSON tree. Rendering escapes
      JSON string values and object keys for quotes,
      backslashes, common control escapes, and remaining U+0000..U+001F control
      characters. `Json.pretty(indent)` emits multi-line escaped arrays/objects
      with caller-selected spacing. `append`, `set_at`, `with_key`, and
      `without_key` provide immutable array/object replacement builders with the
      same deep snapshot semantics. Scalar/container shape predicates, array/object `len()`,
      object `contains_key`, and object `keys()`/`values()` snapshots cover
      ordinary inspection without exposing the internal union or aliasing
      nested values.
      Parsers and stricter canonicalization remain package/follow-up library
      work.
- [x] **`std:net:types` value identifiers**: added the pure Otter-authored
      `std:net:types` module exporting `IpAddr`, `SocketAddr`, `Uri`, `Url`,
      `ParseError`, and constructor functions (`ip_v4`, `ip_v6`, `ip_v6_scoped`,
      `socket_addr`, `uri`, `url`). The implemented slice covers portable
      network identifier values, rendering, equality, hashing, cloning, debug
      formatting, `Url` query-map snapshots and order-insensitive query
      membership hashing, `ParseError` error/value semantics, dotted decimal IPv4
      parsing, eight-group and `::`-compressed IPv6 parsing including final
      dotted IPv4 tails and scoped zone identifiers, lowercase hex IPv6
      rendering, IPv4 and bracketed IPv6 socket address parsing,
      structural URI parsing, structural URL parsing including bracketed IPv6
      URL hosts, URI/URL accessors, immutable URL replacement builders with
      query-map snapshots and query-entry lookup/set/remove/clear helpers,
      UTF-8 percent component encode/decode helpers, and
      explicit-import/old-core-path e2e coverage. It intentionally does not open
      sockets, perform DNS, normalize URLs, perform IDNA handling, or implement
      target-backed networking;
      those remain future `std:net`/parser work.
- [x] **`std:rand` deterministic + OS-backed RNG slice**: added the mixed
      `std:rand` module exporting `Rng`, `RandomError`, `SeededRng`,
      `OsRng`, `ThreadRng`, `random_error`, `os_rng`, `thread_rng`,
      `gen_range_i64`, `gen_range_u64`, `gen_f64`, `gen_range_f64`,
      `gen_bool`, `gen_index`, `fill_bytes_n`, `gen_bytes`,
      `choose_index`, `choose`, and `shuffle`.
      `SeededRng` is deterministic and
      reproducible, with `fill_bytes` appending generated bytes into
      `std:bytes.Bytes`; it is suitable for tests/simulations but not
      cryptographic use. `OsRng` is provider-backed through the runtime entropy
      hook and exposes fallible `try_next_u32`, `try_next_u64`, and
      `try_fill_bytes` and `try_fill_bytes_n` methods returning `RandomError`;
      its `Rng` impl panics on provider entropy failure to preserve the
      non-fallible generic `Rng` contract. `ThreadRng` is a per-value generator
      seeded from `OsRng`.
      Range helpers are half-open
      and return `low` for empty/reversed ranges, including deterministic
      `gen_f64` and `gen_range_f64` helpers built from the next 53 random bits;
      `gen_bool` samples a numerator/denominator ratio with clamped
      always-false/always-true edge cases, `fill_bytes_n` / `gen_bytes`
      generate exact-length byte buffers from any `Rng`, `gen_index` and
      `choose_index` sample uniform zero-based indexes and return `null` for
      non-positive lengths or empty lists, `choose` returns `null` for empty
      lists, and `shuffle` mutates the provided list in place.
      `SeededRng` implements state equality,
      clone, hash, stringification, and debug rendering so PRNG streams can be
      snapshotted and compared. `RandomError` implements `std:error.Error`,
      equality, clone, hash, stringification, and debug rendering.
      Cryptographic-strength API guarantees and richer statistical
      distributions remain planned work.
- [x] **`std:log` portable value/default line slice**: added the pure
      Otter-authored `std:log` module exporting `Level`, prefixed concrete level
      variants (`LogTrace`, `LogDebug`, `Info`, `Warn`, `LogError`), level
      constructor helpers, `Record`, `LoggerAlreadySet`, `log_record`, and
      default line helpers (`trace`, `debug`, `info`, `warn`, `error`) plus
      structured helpers (`trace_with`, `debug_with`, `info_with`,
      `warn_with`, `error_with`). The implemented slice gives levels, records,
      and the marker type equality/clone/hash/stringification/debug semantics
      and prints compact lines through `std:io.println`. `Level.rank()` and
      `Level.is_at_least(min)` provide portable severity ordering for filtering.
      `Record` also provides
      value-layer accessors and immutable-style `with_*` builders that clone
      field maps to avoid aliasing, including direct field lookup/presence/count,
      field addition, removal, and clearing helpers; `record(...)` and structured `*_with` helpers snapshot
      caller-provided field maps too. Record equality and hashing compare
      fields by key/value membership rather than rendered field order.
      Replaceable global logger backends remain planned provider/runtime work.
- [x] **`std:process` portable value layer + host environment/execution slice**: added the mixed
      `std:process` module exporting `Command`, `ExitStatus`, captured
      `Output`, constructor helpers, `args`, `env`, `env_all`, and `set_env`.
      `Command` builders/accessors snapshot
      argument lists, environment maps, and cwd paths, including immutable-style
      program replacement, whole-args/env replacement, env inheritance/clearing,
      cwd clearing, direct argument counts, and explicit environment
      lookup/presence/count helpers, plus command validation for empty program
      names and invalid explicit environment keys before provider execution;
      `Output` snapshots byte buffers; all three values
      implement equality, clone, structural hashing, and
      diagnostic debug rendering, with stringification for `Command` and
      `ExitStatus`. `Command` hashing keeps argument order significant while
      matching explicit environment-map equality by key/value membership rather
      than insertion order. `Output` hashing folds status/stdout/stderr in field
      order. Rust-backed hooks snapshot process argv and environment
      into ordinary `List<str>` / `Map<str, str>` values, read one variable as
      `str | null`, mutate one environment variable with validation, and run
      commands to completion through `Command.status()` / `Command.output()`,
      returning validation/provider failures as `IoError` or successful
      `ExitStatus` / captured `Output` values.
      `exit` and `abort` are imported from `std:process` and lower to the
      existing runtime process-control intrinsics. Live process spawning,
      `Child`, streamed child stdio, and richer target-specific status details
      remain future provider/runtime work.
- [x] **`core:sync:atomic.Ordering` value contract**: added the pure
      Otter-authored `core:sync:atomic` module exporting `Ordering`, its five
      memory-ordering variants, and constructor helpers. `Ordering` implements
      equality, clone, hash, stringification, and diagnostic debug rendering;
      atomic handle types and load/store/swap/compare-exchange/fetch operations
      remain target/runtime work. This moved from the earlier `std:sync:atomic`
      path because atomic operations are compiler/runtime substrate under the
      revised core/std split.
- [x] **Stdlib provider invariants**: explicit provider catalogs are validated
      before public `core:*`/`std:*` views are materialized. Duplicate modules,
      root-only paths, unaddressable path segments, tier/root mismatches,
      duplicate exports, and exports missing from bundled toolchain source are
      diagnosed and skipped instead of becoming importable partial or
      wrongly-tiered views. Custom providers can also add valid `std:*` module
      views, and `no-std` still blocks those provider-added `std:*` imports.
      The built-in module and source manifests also have unit coverage for the
      same scheme-plus-addressable-segment path shape required of custom
      providers, plus catalog-to-require-import coverage that catches exported
      names missing near-empty-prelude negative diagnostics.
- [ ] **Next stdlib slices**:
      async filesystem adapters, system-time
      local timezone database/conversion extensions for `std:time`,
      `std:net`, live process spawning and child management for
      `std:process`, cryptographic/distribution work for `std:rand`, and
      remaining collections/bytes/atomic-handle work. Each slice needs
      unit + e2e tests, docs, examples, and LSP support.

### Phase 2 — Type checking & inference  ✅ DONE
- [x] `sema/check.rs`: bidirectional checker over the imperative core —
      int/float literal defaulting + range checks, locals & scopes, binding
      patterns (binding/wildcard/tuple), assignment lvalues, unary/binary
      operators (numeric/comparison/logical/bitwise), blocks, `if`/`else`
      (branch-type union), `return`, direct function calls (arity + arg types),
      union & `dynamic` widening assignability.
- [x] Records expression types + name resolutions for codegen — now baked onto
      typed HIR nodes the checker emits (see Phase 2.5; `CheckResults` is gone).
- [x] Flow narrowing (`is`/`as`, if/else/match, `&&`/`||`) via `Adjust::Unbox`,
      incl. else-if chains and interface/union/NPO sources.
- [x] `while`/`loop` (value via `break <expr>`)/`for`, `break`/`continue`,
      `match` + compile-time exhaustiveness + reachability.
- [x] Structs (record/tuple/unit): construction (shorthand + `..spread`), field
      access/mutation, methods via `extend`; interfaces (static + dynamic
      dispatch, default methods); method resolution incl. generic `extend`;
      operator→interface desugaring; `ToStr`/string-interpolation via `to_str`.
- [x] Generics inference (functions, structs, methods, static `T.new`);
      `<T: Bound>` + monomorphized interface-method dispatch; `?`/`Try`/
      `FromResidual`; closures (by-reference cells) + captures; trailing
      closures + implicit `it`; pattern matching complete (wildcard/binding/
      literal/type-binding/unit/tuple/tuple-struct/record-struct/list/or).

### Phase 2.5 — Typed HIR (retire span side-tables)  ✅ DONE

The architecture (README §"The central decision") moves the compiler off the
implicit, span-keyed `CheckResults` side-tables and onto a **typed, resolved,
desugared HIR** that the checker produces and codegen + the LSP consume. Done
incrementally, test-gated, all existing tests green at every step. **Complete:
`CheckResults` and the `hir::lower` pass are both deleted — the checker emits
the full typed `Hir` directly and every consumer reads `analysis.hir`.**

- [x] **Stage 1 — define the complete HIR** (`compiler::hir`). A typed tree
      where every `Expr` carries its `Ty`, every name a `Res`, every call a
      `CallKind` (`Direct`/`Method`/`Builtin`/`Closure`/`Extern`), coercions are
      explicit `Adjust` nodes, builtins/foreign/numeric/concurrency ops are
      explicit `Intrinsic` variants, and `for`/`?`/operator-overload/string-
      interpolation/async carry their resolution on the node. Every node keeps
      its source `Span`. Program-level (def-keyed) facts — struct layouts,
      extern sigs, interface impls, link libs, local decls — live on the `Hir`
      container. Additive, zero behavior change. **21 structural tests** mapping
      every retired `CheckResults` table to its HIR node field. (721 total.)
- [x] **Stage 2 — lower `AST + CheckResults → HIR` losslessly**
      (`compiler::hir::lower_program`, checker untouched). Replicates the
      backend's exact call-dispatch precedence, coercion/intrinsic folding, and
      `for`-driver selection; records generic args *unresolved* (codegen
      substitutes per instance). **Verified total over the whole example
      corpus**: lowering 21/22 single-file programs yields 1673 user-source expr
      nodes with **zero `Error` nodes** (user *and* synthesized prelude bodies),
      every node carrying a real source span and a type matching the checker's
      `expr_types`. 10 focused unit tests + 1 corpus integration gate. (732
      total.) Follow-ups (forced correct in Stage 3): a multi-file (`modules`)
      lowering test, and source provenance for compiler-synthesized prelude
      bodies (currently a synthetic `FileId`).
- [x] **Stage 3 — repoint codegen to consume HIR only** (no AST tree walk);
      monomorphization worklist stays at the HIR→codegen boundary. **Done: the
      production `compile`/`compile_object` paths lower every function from the
      typed HIR, and the entire AST code-generation walk has been physically
      deleted** (`gen_stmt.rs` + `gen_match.rs` removed; the AST `gen_*` methods
      stripped from `gen_expr`/`gen_call`/`gen_struct`/`gen_collections`/
      `gen_cast`; the `use_hir`/`CgBody`/`BodyView::Ast` migration scaffolding
      collapsed). Validated by all 12 test binaries + 23 examples green, zero
      warnings, and a native object build+link smoke test.
  - [x] `&Hir` threaded through the whole backend context (`Codegen` +
        `CgShared`, reachable from every `FnGen` method) — built once via
        `lower_program` in `compile`/`compile_object`.
  - [x] Isolated def-keyed reads repointed off `CheckResults` onto the HIR:
        async-`fn` detection (`fn_sigs.async_output`), interface impls
        (`hir.iface_impls`), `@Link` libraries (`hir.link_libs`), and extern
        C-ABI signatures (`hir.extern_sigs`). 736 tests green.
  - [x] Repoint the **expression/statement/pattern walk** to `hir::Expr`
        (`backend::gen_hir`). Built as a parallel, test-gated path that reached
        full parity, then promoted to the sole walk; the value-level helpers
        (`emit_binop`, `gen_compare`, `checked_arith`, `emit_call`, locals,
        coercions) were extracted to be operand-source-agnostic and are the only
        codegen helpers that remain — the AST tree walk is gone.
    - [x] Imperative core + calls: literals, locals, `var`/assign, unary/binary
          (primitive, incl. bitwise/shift/compare/logical short-circuit), `if`,
          `block`, `while`, `loop`/`break`-value, `return`, `continue`,
          `Direct`/`Builtin`/`Extern` calls, and explicit `Adjust` coercions.
    - [x] Structs: record + tuple-struct literals, field/tuple-index read and
          write (incl. `@Transparent` newtypes, nested extern structs), tuple-
          struct constructors.
    - [x] Casts (shared `emit_cast`/`emit_is`): numeric conversions, `as`/`is`
          on unions/`dynamic` (tag check + narrow), interface up/downcast,
          `as str`; tuples; string literals + **builtin-typed** interpolation
          holes (`"n=$n"`).
    - [x] GC-correct managed results: `h_expr` marks managed-pointer results as
          stack-map roots (mirrors `result_is_managed_ref`; the node's own `ty`
          + `is_managed_ptr`'s NPO handling make one check exact).
    - [x] Collections + methods: `[…]`/`{…}` list & map literals, `xs[i]`/
          `m[k]` index load & store, and plain (non-interface) `extend` method
          calls `recv.m(..)`.
    - [x] `match` (literal / type-binding `T x` on unions / unit-variant /
          tuple-destructure arms, with guards) and `for` (List fast path,
          `Iterator` protocol incl. interface/dyn `next`, and `Map`).
    - [x] Builtin `str` and `Map` methods, via shared `emit_str_method` /
          `emit_map_method` (the AST helpers refactored to take pre-evaluated
          receiver + arg values, so both walks share the dispatch).
    - [x] Closures: env build shared via `emit_closure_value`; the lifted-body
          job (`ClosureJob`) carries an AST or HIR body (`CgBody`); closure
          expressions and closure-value calls both lower via the HIR path.
    - [x] Builtin `List` methods — `push`/`pop`/`get`/`set`/`insert`/`remove`/
          `size`/`is_empty`/`clear` **and** the higher-order `map`/`filter`/
          `fold`/`each` (closure args pass through as their lifted env value),
          via shared `emit_list_method` + `emit_list_map`/`filter`/`fold`/`each`.
    - [x] Value-producing intrinsics: empty collection constructors, builtin
          `.clone()` (shared `emit_builtin_clone`), the numeric namespace
          (`T.MIN/MAX`, `f*.NAN/is_nan/…`, `{wrapping,saturating,checked,
          overflowing}_{add,…,shl,shr}` via shared `emit_num_intrinsic` /
          `emit_int_arith`), and foreign FFI memory (`Foreign.alloc`/`free`/
          `realloc`/`alloc_flex`; the `CString`/`CStr`/`Buffer` boundary types are
          ordinary prelude types over `lang_*` runtime helpers, not intrinsics).
    - [x] Method-call dispatch (shared `emit_primitive_compare`): dynamic
          dispatch through an interface object's vtable, builtin-bound fallbacks
          (`Clone`/`Eq`/`Ord`/`ToStr`/`Hash` on primitives/`str`/collections),
          interface→concrete resolution for bounded generics, and static
          `Type.m(..)` calls.
    - [x] Concurrency intrinsics — `Thread.spawn`/`JoinHandle.join`,
          `channel<T>()`, `Shared.new`, `yield_now`/`sleep`/`cancel`. All 16
          `Intrinsic` variants lower on the HIR path.
    - [x] Channel + `Shared` builtin methods — `Sender.send`, `Receiver.recv`/
          `try_recv`, `Shared.lock`/`try_lock` (closures under the lock + the
          `R | LockBusy` union), via shared `emit_channel_*`/`emit_shared_*`.
          **48 HIR-path JIT tests** (… + thread spawn/join, channel send +
          try_recv, Shared.lock, all with AST-vs-HIR agreement). 788 total.
    - [x] `spawn EXPR` (shared `emit_spawn`) — schedule a future on a worker.
    - [x] User-`to_str` string interpolation — the HIR `StrPart::Interp` carries
          `stringify_targs` (from `call_type_args`), and `h_stringify`
          monomorphizes + calls the user `to_str`. (789 total, 49 HIR-path.)
    - [x] HIR async-analysis helpers (`h_block_has_await`, `h_scan_stmt_awaits`,
          `h_collect_block_locals` + sub-walkers) — the foundation for HIR async
          codegen; mirror the AST `support.rs` versions but read `Await`/`Bind`
          nodes directly. Unit-tested on a lowered async program. (790 total.)
    - [x] `BodyView { Ast | Hir }` abstraction (`support.rs`) + `async_state_layout`
          generalized over it (dispatches local-collection to the AST or HIR
          collector); `BodyView::has_await`/`scan_awaits` dispatch the analysis.
          The async-codegen functions can now be made body-source-agnostic.
    - [x] Async state machine on HIR: `await` (`h_await` via the shared
          `emit_await_suspend`), `async { … }` blocks (`h_async_block`), `spawn`,
          and `for await` (`h_for_async`); `BodyView` collapsed to a typed-HIR
          view that drives `define_async_fn`/`build_stateful_poll`/
          `define_async_job`; `AsyncJob.body`/`ClosureJob.body` carry
          `compiler::hir::Expr` directly.
    - [x] Operator overloads (`hir::OpOverload` carries the resolved `extend`
          method + solved type args, replacing the `operator_methods` +
          type-args span tables), `?` (`h_try`), `&`/`*` FFI pointer
          ref/deref, fixed-array `[T;N]` index load/store, `extern var`
          read/write — the last forms exercised by the test corpus + examples.
  - [ ] Repoint struct layouts + fn signatures (`compute_layout`,
        `signature_of`) onto `hir.structs` / `hir.fn_sigs` (def-keyed; can ride
        with Stage 5's `CheckResults` deletion).
- [x] **Stage 4 — repoint the LSP to HIR.** `Compiled` lowers the HIR and
      builds an `HirIndex` by walking it once: `(span, type)` for every node
      (plus each call's callee-name span → callee type) and `(span, resolution)`
      for every `Name` and folded call callee, plus per-local types/decls/param
      flags. Every position query (`resolution_at`/`expr_ty_at`/`definition_span`/
      `receiver_type_at_dot`/`semantic_tokens`) and the server's hover/
      references/rename/highlight/completion handlers now read the HIR index —
      **no `analysis.results` access remains in the LSP.** To make this work the
      HIR `Call` node gained `callee_span`/`callee_ty` provenance (the desugared
      dispatch had dropped the callee name). 5 new HIR-backed LSP tests
      (go-to-def on a method call, ctor-call resolution, references via the
      folded callee, local hover type, callee function-type hover). 795 total.
- [x] **Stage 5 — checker emits HIR directly**, deleting `CheckResults` tables
      one at a time until none remain. **DONE: the `CheckResults` struct is
      deleted entirely and the `hir::lower` pass is gone.** The type-checker
      assembles the complete typed `Hir` itself (`Checker::finish`); codegen and
      the LSP consume `analysis.hir` exclusively.
  - [x] **`link_libs` retired** (first table deleted) — `@Link(lib="…")` is a
        pure attribute scan with no type inference, so it now derives straight
        from the program via `hir::collect_link_libs` (consumed as
        `Hir::link_libs` by the JIT `dlopen` and the CLI's native `-l`). The
        checker no longer records it; the field is gone from `CheckResults`.
        3 tests (lowering + the free fn + ffi `@Link` native build).
  - [x] **8 payload-free builtin marker sets retired** — `channel_news`,
        `shared_news`, `yield_nows`, `async_sleeps`, `cstring_from_strs`,
        `cstr_to_strs`, `foreign_frees`, `foreign_reallocs`. These flagged calls
        to unshadowed prelude builtins (`channel()`, `Shared.new`, `yield_now()`,
        `sleep()`, `CString.from_str`, `CStr.to_str`, `Foreign.free`,
        `Foreign.realloc`); `lower` now recognizes them from the callee shape +
        the absence of a shadowing resolution (the same signal it already uses
        for builtin methods, and the same condition the checker used to type
        them), emitting the matching payload-free `Intrinsic`. Checker inserts +
        the 8 `CheckResults` fields deleted. New lowering tests (shape
        recognition + a user-shadowed `sleep` staying a normal `Direct` call);
        channels/shared/async/ffi examples + the realloc/free e2e tests verified.
  - [x] **`future_cancels` + `builtin_ctors` retired** — `fut.cancel()` is
        recognized in `lower` by the method name + a `Future`-typed receiver;
        `List<T>()`/`Map<K,V>()` by the callee's type name resolving to
        `List`/`Map` (the collection type already rides on the node, so the
        `Intrinsic::CollectionCtor` is payload-free). Checker inserts + both
        fields deleted; collection-ctor lowering test added; lists/maps/async
        examples verified.
  - [x] **`thread_spawns` + `thread_joins` retired** — `Thread.spawn { … }` is
        recognized by callee shape, its output `R` read from the `JoinHandle<R>`
        result type; `jh.join()` by the method name + a `JoinHandle`-typed
        receiver, its `R` read from that receiver type. Checker inserts + both
        fields deleted; thread-spawn lowering test added; threads / threads_
        hardcore examples + thread e2e tests verified.
  - [x] **`foreign_allocs` retired** — `Foreign.alloc<T>()` / `alloc_zeroed<T>()`
        recognized by callee shape; `T` recovered from the `*T | null` result
        pointer (`npo_pointee`), `zeroed` from the method name. Checker insert +
        field deleted; lowering test added; ffi example + e2e verified.
  - [x] **`num_intrinsics` retired via a shared recognizer** — the
        `(type, name) → NumIntrinsic` mapping (`i32.MAX`, `f64.NAN`,
        `i32.wrapping_add`, `f64.is_nan`, …) was extracted into pure
        `results::num_constant_of`/`num_method_of` that BOTH the checker (for
        typing + arg validation) and `lower` (to build `Intrinsic::Num`) call —
        no duplication, no side table. Checker inserts + field deleted; lowering
        test (constant + method) added; numerics example verified.
        **15 of ~43 tables retired.** This exhausts the cleanly-derivable
        markers; the remaining marker tables carry *checker-resolved* values not
        present in the result/receiver type — `foreign_flex`'s second type `E`
        (from generic args), `clone_kinds`' deep-vs-shallow decision, and
        `static_calls`/`static_recv`' static-dispatch receiver type. Those, plus
        the structural backbone, require the checker to construct HIR nodes.
  - [x] **Lock-down test net for the remaining forms** — 6 HIR-shape lowering
        tests pin the desugaring of the forms still backed by `CheckResults`
        tables (builtin `clone` → `Intrinsic::Clone`; static dispatch →
        `Method { is_static }`; operator overload → `Binary { overload }`; `?` →
        `Try`; the `Iterator` `for` driver → `ForDriver::Iter`; widen → `Adjust`).
        These are the safety net for the checker-constructs-HIR refactor: the HIR
        these forms lower to must not change when their source moves off a side
        table onto the checker's nodes. Extended with broad HIR-node coverage
        (closures, async blocks, `await` output, the `Map` `for` driver, all
        match-arm pattern kinds, user-`to_str` interpolation `stringify`, nested
        field/index span provenance) — ~820 tests total.
  - [x] **Checker emits `hir::FnSig` directly** — the FIRST genuine
        "checker constructs HIR" step (not a `lower`-side re-derivation). As
        `check_function` types each function it builds the `hir::FnSig`
        (params `(LocalId, Ty)`, return type, async output) and stores it in
        `results.fn_sigs`; `lower` copies it onto `Hir::fn_sigs` verbatim and the
        backend's `signature_of`/`define_*` read it. This retired **3 tables at
        once** — `fn_params`, `fn_return`, `async_fns`. Proves the checker can
        build HIR node types in-crate (no module cycle), the template for the
        rest of the structural backbone. Test added; all examples + 812 tests
        green. **18 of ~43 tables retired.**
  - [x] **Checker emits `hir::ExternSig` directly** — `record_extern_sig` now
        builds the HIR `ExternSig` (params + ret) as it types each extern
        function, so `lower` copies `Hir::extern_sigs` verbatim (no tuple→struct
        conversion) and the def-keyed CheckResults data is now uniformly HIR-
        typed (`fn_sigs`, `extern_sigs`, `struct_fields`/`StructFields`). ffi
        example + 812 tests green.
  - [x] **Checker builds `hir::Expr` directly for EVERY expression kind (the
        span-backbone rewrite).** `check_expr` (and `check_lvalue`, for
        assignment targets) constructs the typed HIR node for each expression as
        it is checked, into `results.node_hir` (keyed by span, embedding the
        already-built, already-coerced children). **Every `ExprKind` is now
        checker-built**: leaves (`Int`/`Float`/`Bool`/`Null`/`Char`/`Name`/
        `Discard`), operators (`Unary`/`Binary` with overloads), `Cast`/`Ref`/
        `Deref`/`TupleIndex`/`Index`/`Try`/`Await`/`Spawn`, aggregates (`Str`
        interpolation / `Map` / `Struct` incl. field-init shorthand / `Field`
        incl. numeric-namespace intrinsics), the full call-dispatch classifier
        (`build_call_kind` — direct / extern / builtin / method / tuple-ctor /
        closure calls and every marker intrinsic, mirroring the old `lower_call`),
        control flow (`If`/`Match`/`While`/`For`/`Loop`/`Block` with their nested
        blocks, statements, and patterns via `build_block`/`build_stmt`/
        `build_pattern`/`build_for_driver`), `Return`/`Break`/`Continue`, and
        `Closure`/`AsyncBlock`. Verified by node_hir shape tests for each family
        plus the full e2e suite.
  - [x] **`lower` repointed onto `node_hir` as the sole construction path; the
        duplicate table-driven builder is deleted.** `lower_expr` consumes the
        checker-built node and applies its top-level coercion; the parallel
        `lower_expr_kind` / `lower_call` / `lower_struct_lit` / `lower_str_parts`
        / `lower_map_items` / `lower_overload` / `lower_exprs` / `finish_call` /
        `intrinsic` / `is_builtin_recv` / `for_driver` / `list_elem` / `field_ref`
        / `lower_else` / `lower_arm` / `res_at` / `error_ty` / `type_args` /
        `npo_pointee` (~600 lines) are **removed**. A `record_node_locals` walker
        re-establishes the `Body.locals` map that `res_at`'s side effect used to
        build. Verified: instrumenting the (now error-recovery-only) fallback
        showed **zero** misses across the entire test corpus + every example
        (JIT and native); the only remaining fallback is a graceful `Error` node
        for expressions the checker could not resolve in already-error programs
        (e.g. the LSP analysing half-typed code), matching the prior recovery.
        `lower` now keeps only the top-level body scaffolding (`lower_block` /
        `lower_stmt` / `lower_pattern` + the `adjustment`/`expr_type`/`local_ty`/
        `pattern_types`/`struct_fields` accessors).
  - [~] **Delete the span tables from `CheckResults`** (table-by-table). The
        retirement mechanism: a check method stashes the datum it just computed
        in a *transient* per-fact slot on the `Checker` (a `Cell<Option<…>>`),
        which `build_hir_node` consumes the instant `check_expr_inner` returns
        for the same node. Construction is synchronous + depth-first — every
        child node is built (and its slot consumed) before its parent's check
        method writes these — so one slot per fact suffices and the persistent
        span-keyed `HashMap` is gone; the datum lives only on the resulting HIR
        node field.
      - [x] **`operator_methods`** → `Checker::pending_overload`, consumed into
            the HIR `Unary`/`Binary` `overload: OpOverload` field. (Its operator
            type-args, formerly a second write into `call_type_args`, fold into
            `OpOverload.type_args`.) Deleted from `CheckResults`.
      - [x] **`cast_targets`** → `pending_cast_target` → HIR `Cast.target`
            (set *after* the operand check so a nested cast can't clobber it).
      - [x] **`awaits`** → `pending_await` → HIR `Await.output`.
      - [x] **`async_spawns`** → `pending_spawn` → HIR `Spawn.output`.
      - [x] **`try_branches`** + **`residual_conversions`** →
            `pending_try_branch` / `pending_residuals` → HIR `Try.{branch,
            residual_conversions}` (`check_try` refactored to use locals for its
            own internal `is_try` / "nothing to propagate" checks instead of
            re-reading the tables). Both deleted.
      - [x] **`clone_kinds`** → `pending_clone_kind` → HIR `Intrinsic::Clone`
            (6 write sites in `check_builtin_clone`).
      - [x] **`static_calls`** + **`static_recv`** → `pending_static_recv`
            (`Some(recv)` marks a static call) → HIR `Call`
            `{is_static, recv_static}` (both static-call paths; set after the
            arg checks).
      - [x] **`foreign_flex`** → `pending_foreign_flex` → HIR
            `Intrinsic::ForeignFlex`.
      - [x] **`for_iters`** + **`for_maps`** + **`for_async_iters`** →
            `pending_for_driver` (a fully-built `ForDriver`) → HIR `For.driver`
            (`check_for` rewritten to compute the driver, check the body, *then*
            stash it — so a nested `for` in the body can't clobber the slot;
            `build_call_kind` takes all its call facts up front so no branch
            leaks). The `List` fast path is the build-time fallback.
      - [x] **`stringify_methods`** + the stringify-targs use of
            **`call_type_args`** → `pending_stringify` (a per-hole `VecDeque` of
            `(to_str, targs)` in source order, popped per `Interp` part) → HIR
            `StrPart::Interp.{stringify, stringify_targs}`.
      - [x] **`call_type_args`** (each generic call's type args) →
            `pending_type_args` → HIR `Call` `Direct`/`Method` `type_args` (set
            after the arg checks in all three generic-call paths; the dead
            `q_span` write — its targs already ride on `TryBranch.targs` — was
            removed). Both `stringify_methods` and `call_type_args` deleted.
      - [x] **`closures`** + **`async_blocks`** → `pending_closure` /
            `pending_async` → HIR `Closure` / `AsyncBlock` node fields. The
            **backend** no longer reads these tables: its `captured_locals` set
            (the locals some closure captures, cell-backed by codegen) is now
            built by `Hir::captured_locals()`, a production HIR walk over the
            bodies' `Closure`/`AsyncBlock` nodes. `check_thread_spawn`'s capture
            check reads the closure's HIR node from `node_hir` instead of the
            table. Both deleted.
      - 831 tests green + every example (JIT and native) after each deletion.
        **17 span tables retired so far.**
  - [x] **The checker emits each function's *whole* body `Block`**
        (`results.fn_bodies`, keyed by `DefId`): `check_function` calls
        `build_block` on the body after checking it, and `lower_program`
        assembles the `Body` from that block directly (recording its locals via
        `record_block_locals`). Verified: instrumenting `lower`'s block-lowering
        fallback showed **zero** hits across the whole test corpus + every
        example (JIT and native) — so for any well-formed program the checker is
        the sole HIR-body producer; `lower_block`/`lower_stmt`/`lower_pattern`
        remain only as the graceful error-recovery path for half-typed code.
        826 tests green (+ the runtime suite).
  - [x] **`adjustments`** deleted — coercions are now baked directly onto the
        HIR node instead of a side table. A widening (`Widen`/`WidenDyn`) is baked
        in place by `expect` via `bake_coercion` (the child node already exists);
        a flow-narrowing `Unbox` is **recovered structurally** by `build_name_node`
        from the local's declared (boxed union/interface) type vs the narrowed use
        type — the same `was_boxed && now_single` test `check_ident` used, with no
        write. `hir_child`/`lower_expr` no longer apply any adjust (nodes carry it
        baked). Verified incl. flow-narrowing, widening, and dynamic-dispatch
        (`WidenDyn`) examples JIT + native.
  - [x] **`expr_types`** deleted — the `expr_ty(span)` accessor now reads the
        checked type off the expression's built HIR node in `node_hir` (unwrapping
        a baked `Adjust` to recover the pre-coercion type), since every checked
        expression has a node. `check_expr` no longer writes the table; the direct
        `expr_types.get` reuse sites (inference, dispatch) go through `expr_ty`;
        `Paren` is mirrored at its span (transparent) so either span resolves; the
        `Str`/struct-shorthand idents (checked via `check_ident`, not `check_expr`)
        build their `Name` into `node_hir` so their type resolves. `build_hir_node`
        is now an exhaustive match (the `_ => None` catch-all is gone).
  - [x] **`pattern_types`** deleted — a `TypeBind`/`UnitPath` pattern's matched
        variant type is recomputed where needed (`build_pattern` and the
        exhaustiveness checker, via a shared `pattern_test_ty` helper that
        `lower_ty`s the annotation / `mk_named`s the unit path — the function's
        generic env is still active at body-build time). This made the
        `build_block`/`build_stmt`/`build_pattern` chain `&mut self`. The HIR
        `Pattern.ty` for other kinds (informational, unread by codegen) is the
        child/local type or `error`. Verified: match exhaustiveness, type-binding
        patterns, destructuring — JIT + native.
  - [x] **`resolutions`** deleted — **the last span-keyed table.** Every
        resolution now lives on a `Name` HIR node stored in `node_hir`: value-name
        uses and `self` (built by `check_ident`/the `SelfExpr` arm via
        `record_res`), call-dispatch markers (each `resolutions.insert(callee.span,
        …)` became `record_res` — a `Name(res)` node at the callee span; node-hir
        keys are unique per span so there is no clobber, unlike a transient),
        assignment lvalues, and **pattern/var bindings** (`bind` stores a
        `Name(Local)` marker). `results.resolution(span)` is a shim that reads the
        `Name` off `node_hir` (unwrapping a narrowing `Adjust`); `hir_local_at`
        and `build_call_kind` go through it. Verified: name resolution, method/
        static/closure dispatch, pattern binding, captures — the whole test corpus
        + every example, JIT and native.
      - **ALL ~21 span-keyed `CheckResults` side tables are now retired.** The
        only `HashMap<Span, _>` left in `CheckResults` is `node_hir` — the typed
        HIR the checker emits (the bridge itself, not a side table).
  - [x] **HIR construction is now total; the `lower` construction/fallback path
        is deleted.** `build_block`/`build_stmt`/`build_pattern`/`build_else`/
        `build_field_pattern` no longer return `Option` — an expression the
        checker couldn't build (only in an already-erroring program) degrades to
        an `Error` node via `hir_child_or_error`. So `check_function` emits a body
        `Block` for *every* function, and `lower_program` no longer needs its
        parallel table-driven builder: `lower_block`/`lower_stmt`/`lower_pattern`/
        `lower_expr`/`lower_field_pattern`/`pat_ty`/`path_def`/`local_at`/
        `build_span_index` and the `span2def`/`module` plumbing are all removed.
        `hir::lower` is now just `collect_link_libs` + a thin `lower_program`
        assembler (copies the def-keyed tables, records `Body.locals` by walking
        the checker's block) + the literal parsers the checker's build uses. 853
        tests green, examples JIT + native.
  - [x] **The checker emits the def-keyed HIR directly; 5 of 6 def-keyed tables
        retired from `CheckResults`.** The `Checker` owns a `hir: Hir` and writes
        `hir.structs` / `hir.fn_sigs` / `hir.extern_sigs` / `hir.iface_impls` /
        `hir.local_decls` as it resolves each definition (and reads them back —
        `hir_field_index`→`hir.structs`, `implements_dyn`→`hir.iface_impls`).
        `Analysis` now carries `hir`; `analyze()` finishes it via `lower_program`
        (which adds link-libs + assembles `Body` blocks from the checker's blocks).
        The backend reads `analysis.hir.{fn_sigs,structs}`. Those 5 fields are
        **deleted** from `CheckResults`, which now holds only its transient
        working state: `local_types`, `node_hir`, `fn_bodies`. 853 tests green,
        examples JIT + native, 0 warnings.
  - [x] **Final: `CheckResults` deleted, `hir::lower` deleted, `lower_program`
        gone.** The 6th def-keyed table `local_types` moved to `Hir::local_types`
        (a program-wide map; `Hir::local_ty` reads it, the backend's global
        `local_ty` repointed there). The `node_hir`/`fn_bodies` hand-off became
        checker-private working fields (`Checker.node_hir` keyed by span,
        `Checker.fn_bodies` keyed by `DefId`); `Checker::finish` drains them into
        the emitted `Hir` — adding link-libs (`hir::collect_link_libs`) and
        assembling each `Body` (params + locals walked out of the block by the
        relocated `record_block_locals`/`record_node_locals`/`record_pattern_locals`
        walkers). `analyze_multi` now does `ck.check_program(); ck.finish()` with
        no separate lowering step. `Analysis` dropped its `results` field; its
        span-keyed `expr_ty(span)`/`resolution(span)` tooling accessors now scan
        the `Hir` (`Hir::find_expr`). Every external `lower_program` caller
        (backend ×4, cli, lsp, pretty/test harnesses) reads `analysis.hir`
        directly. The `CheckResults` struct and the entire `hir::lower` module
        (`lower.rs`) are **physically deleted**; `results.rs` keeps only the
        shared leaf vocabulary (`ValueRes`, `Adjust`, `Builtin`, `TryBranch`,
        `CloneKind`, `ForIter`, `NumIntrinsic`, …) the HIR nodes carry. All
        ~854 tests green (0 warnings), every example JIT + native, all four
        `--emit` modes + DWARF intact.
- [ ] **Debuggability:** `--emit=tokens|ast|hir|clif` with stable pretty-
      printers; DWARF line tables for built programs.
  - [x] `hir::pretty::print_program` — a stable, deterministic HIR printer
        (definitions in `DefId` order; every expr annotated with its type, every
        name with its resolution, every call with its dispatch kind). 4 tests.
  - [x] CLI `otter_fusion emit tokens|ast|hir <file>` — deterministic IR dumps
        to stdout. `tokens` (kind + span per line) and `hir` (the printer above)
        have purpose-built printers; `ast` uses a deterministic pretty-`Debug`.
        4 e2e tests.
  - [x] **AST source-printer** (`compiler::ast_print`) — renders any
        `ast::Module`/`Expr`/`Item` back to *correct* Otter Fusion source:
        canonical four-space indentation, one statement per line, and
        conservative parentheses around operator sub-expressions so precedence
        never changes meaning. Covers every node (all `ExprKind`/`PatternKind`/
        `TypeKind`/`ItemKind`/`StmtKind`, doc comments, attributes, trailing
        closures with the implicit `it` binding, generics with bounds/defaults).
        Guaranteed idempotent on its own output and verified by 13 round-trip
        unit tests (`parse → print → parse` re-parses with zero errors and
        prints identically). Wired into `otter_fusion expand <file>`, which
        parses the entry file and prints it back; output re-parses to the same
        AST *and* type-checks identically (certified across all 22 examples).
        3 e2e tests.
  - [x] `emit clif` — `otter_fusion emit clif <file>` dumps every function's
        generated Cranelift IR (post-codegen, pre-machine-code), with a source-
        symbol header per function, in code-generation order. Backed by
        `backend::compile_clif` (a `Codegen.clif` capture buffer filled by
        `record_clif` before each `define_function`; the module is never
        finalized). Deterministic. 2 e2e tests.
  - [x] DWARF source-level line tables for built programs.
        - [x] **Source-location threading** — HIR codegen tags every node's
              instructions with its source byte offset via Cranelift
              `set_srcloc` (`h_expr`), so the machine code carries source
              provenance (Cranelift records per-instruction `MachSrcLoc` ranges).
              Additive + safe: examples/native build/`--emit=clif` all green.
        - [x] **Debug-line data capture** — each compiled function's
              `MachSrcLoc` ranges are captured as `(func, code byte offset,
              source byte offset)` (`Codegen.line_info`, filled by
              `capture_safepoints`), exposed on `Jit::source_line_entries()` and
              tested. This is the source-line provenance the line program
              serializes. `gimli` dependency added.
        - [x] **`.debug_line`/`.debug_info` emission** (`backend::dwarf`) — a
              `gimli` line program over the captured per-function source-line
              ranges (one sequence per function; rows map a function-relative
              code offset to a source line via `byte_to_line` over the threaded
              `SourceMap`), plus a compile-unit DIE. Function start addresses are
              emitted as 8-byte `Address::Symbol` relocations against each
              function's object symbol (a `gimli::write::Writer` records them);
              DWARF cross-section offsets are written as literal values (self-
              contained in one object). Sections added to the `ObjectProduct`
              before `emit()`. Wired into `compile_object` for **ELF** targets
              (standard `.debug_*` + absolute relocations, links cleanly);
              `byte_to_line` unit-tested; native macOS/`--emit=clif`/examples all
              green.
        - [x] **Mach-O DWARF** — `__debug_*` sections in the `__DWARF` segment
              (format-detected in `emit_dwarf`); links cleanly via `ld64` and the
              executable carries a debug map to the object's DWARF. Verified by
              a test that the emitted native object contains a `debug_line`
              section, and by every example building natively + running. DWARF is
              now emitted on both ELF and Mach-O.

### Phase 3 — MIR + monomorphization  ✅ DONE (MIR deferred by design)
Per the locked architecture (README + goals), **monomorphization lives at the
HIR→codegen worklist**, and a separate typed MIR is a deliberately-deferred later
project (not a blocker — the language is fully featured without it).
- [x] **Monomorphization collector**: instance-based `(DefId, Vec<Ty>) → FuncId`
      with an on-demand worklist; the checker infers/substitutes type args
      (carried on HIR `Call.type_args`), codegen substitutes layout/clty/type-id
      per instance. Covers generic functions, structs, `extend` methods, and
      static `T.new` inference.
- [x] **Closure → environment struct lowering** (by-reference cells) and **dyn
      dispatch vtables** (interface-object `{vtable,data,type_id}` box,
      `emit_vtable`) — implemented directly on the HIR→codegen path.
- [ ] **Typed MIR** (CFG, explicit drops/safepoints/barriers as metadata) —
      *deferred by design*. Drops/safepoints/barriers are emitted directly during
      codegen today; a MIR would be an optimization/clarity layer, not new
      behavior.

### Phase 4 — Cranelift codegen + runtime  ✅ FEATURE-COMPLETE (optimization ongoing)
All designed language features lower end-to-end on the Cranelift backend (JIT via
`cranelift-jit` + native object/link via `cranelift-object`), JIT≡native byte-
identical and GC-stress clean. What remains is **performance optimization** of the
generated code + compiler (see goals.txt), not missing features.
- [x] `crates/backend`: Cranelift 0.132 JIT. Lowers primitive-typed functions,
      locals (Cranelift vars), int/float/bool/char literals, unary/binary ops
      (incl. signed/unsigned division & shifts, float ops), comparisons,
      short-circuit `&&`/`||`, blocks, `if`/`else` with value merge, `return`,
      and direct calls + recursion. JIT-executes and returns results. 11
      end-to-end tests (factorial=120, arithmetic, shadowing, …).
- [x] Integer `/`,`%` by zero now **panics with a message** via `lang_panic`
      (exit 101), not a raw trap. CLI test asserts it.
- [x] **Integer overflow + shift + division panics** (`docs/14` §2): `+`/`-`/`*`
      use Cranelift's `{s,u}{add,sub,mul}_overflow` and panic via `lang_panic` on
      overflow (debug semantics — the only build profile so far; release
      wrapping awaits build profiles). Shifts (`<<`/`>>`) panic when the amount
      is `>=` the operand bit width (always, per spec). Signed `INT_MIN / -1`
      (and `% -1`) is guarded into a panic instead of a hardware trap. `as`
      int→int casts still truncate silently (spec-correct). 5 CLI tests.
- [x] **Diverging builtins** (`docs/14`, `docs/24`): `panic(str): never`,
      `exit(i32): never`, `abort(): never` are compiler builtins typed to return
      `never` (a subtype of every type, so usable in value position). Backend
      calls the runtime (`lang_panic`/`lang_exit`/`lang_abort`) then emits a trap
      and marks the block terminated so following code is correctly dead. 3 CLI
      tests (panic message + exit 101, value-position `never`, exit code).
- [x] **Cast panics (`docs/14` §2/§6)**: `as` float→int now panics on NaN or
      out-of-range (`gen_float_to_int`: `lo <= v < hi` bound check in the source
      float type, NaN fails both compares), and `as` int→char panics on a code
      point `> 0x10FFFF` or in the surrogate range `0xD800..=0xDFFF`
      (`guard_valid_char`). Both emit `lang_panic` messages (exit 101). 5 CLI
      tests. (`List[i]`/`Map[k]`/`substring` OOB already panic with a message +
      exit 101 directly in the runtime intrinsic.)
- [x] **`panic_with(value): never`** (`docs/14` §1): value widened to `dynamic`
      (the language never inspects it), evaluated for effects, then `lang_panic`.
      1 CLI test. (`assert`/`debug_assert` are NOT in the spec — not added.)
- [x] **Build profiles (`docs/14` §5)**: `otter_fusion run --release` / `lang build
      --release` select the release profile (`backend::set_release_profile`, a
      module-level flag like `set_gc_enabled`). In release, overflowing `+`/`-`/
      `*` wrap via plain `iadd`/`isub`/`imul` (no overflow guard), and signed
      `INT_MIN / -1` wraps to `INT_MIN` (and `% -1` → `0`) via a `select` that
      sanitizes the divisor so the hardware op never traps. Debug (default) keeps
      the checked-panic semantics. Shifts past the bit width and divide-by-zero
      panic in *both* profiles. Release also configures Cranelift's speed
      optimization pipeline, while debug keeps `--emit=clif` close to source.
      Normal executable codegen is root-seeded (`main` for run/build, exact body
      for isolated test/bench children) and discovers callees, vtables, closures,
      async jobs, generic instances, and `Drop` finalizers lazily, so unused source
      bodies and untouched stdlib functions are omitted from JIT/object artifacts.
      3 CLI tests + backend object-symbol DCE regressions.
- [x] **Numeric helper namespaces** (`docs/18` §10, `docs/14` §5): primitive
      type names act as namespaces. Constants `T.MIN`/`T.MAX` (all int types) and
      `f*.INFINITY`/`NEG_INFINITY`/`NAN`; float predicates `f*.is_nan`/
      `is_infinite`/`is_finite`; and the integer overflow-arithmetic families
      `{wrapping,saturating,checked,overflowing}_{add,sub,mul}` (`checked` →
      `T | null`, `overflowing` → `(T, bool)`). Recognised in `check_field`
      (constants) / `check_call` (methods) on a primitive type name, recorded as
      `results.num_intrinsics`, lowered directly by codegen (overflow via
      Cranelift `{s,u}{add,sub,mul}_overflow`, saturating clamps by result sign).
      JIT + native parity; `examples/numerics.otter`; 1 CLI test.
- [x] **Numeric `div`/`rem`/`neg`/`shl`/`shr` families** (`docs/14` §5,
      `docs/18` §10): the matrix is now complete. `check_num_method`
      recognises five new bases (with the correct arity per op: `neg` is
      unary, `shl`/`shr` take a `u32` shift count, the rest stay `(T, T)`)
      and records the same `NumIntrinsic::IntArith` shape. `gen_int_arith`
      split into four per-op-family helpers — `addsubmul` (the original
      logic, factored), `divrem`, `neg`, `shift` — sharing a
      `package_int_arith` step that handles the four return shapes
      (`T` / `T | null` / `(T, bool)` / saturating-via-`select`). Each op
      respects spec semantics: div/rem panic on `/0` for every family
      EXCEPT `checked_*` (which folds it into the null branch); signed
      `INT_MIN / -1` is the only true `div`/`rem` overflow and wraps to
      `INT_MIN` / `0`; `neg` overflows on signed `INT_MIN` (or unsigned
      non-zero); shifts overflow when the count `>= BITS`. Saturating
      shifts saturate the *count* to `BITS - 1`. `examples/numerics.otter`
      extended; 5 new CLI tests (div/rem matrix, panic on `/0`, neg, shift,
      native parity). JIT + native + GC-stress parity.
- [x] `str` + `print`/`println` + `as` casts → real programs with output.
      `crates/runtime` (provisional, pre-GC, leaks): `LangStr` repr, str
      literals (escapes), concat (`str+str`), `int/uint/float/bool/char→str`,
      `print`/`println`. Backend lowers string literals (read-only data),
      casts (full primitive matrix: int↔int, int↔float, float↔float, int↔char,
      →str), and builtin calls. Checker has `as`/`is` + `print`/`println`
      builtins (temporary prelude; real `std:io` import later). `examples/
      hello.otter` runs and prints. 31 new tests (backend str/cast + CLI e2e
      "hello world" / computed fib via the actual binary).
- [x] Loops: `while`, `loop` (value-producing via `break <expr>`), `break`,
      `continue` — checker (enclosing-loop stack, value-break only in `loop`)
      and codegen (header/body/exit blocks, nested loops). 10 tests.
- [x] **Structs** (record + tuple + unit): construction (incl. field-init
      shorthand + `..spread`), field access/mutation, tuple-index, nested
      structs, structs as params/returns, heap reference (aliasing) semantics.
      Alignment-respecting field layout (`docs/02` §9) computed from checker-
      recorded lowered field types (`results.struct_fields`). Runtime
      `lang_alloc` (provisional, leaks — header/collector staged next).
      19 tests (10 backend incl. mixed-width layout + aliasing, 5 checker, +e2e).
### GC (precise tracing, spec-exact via Cranelift stack maps)  ✅ DONE (single + multi-threaded)
The precise tracing collector is complete and correct **including concurrent
reclamation while multiple mutators run** (custom slab allocator `gc_alloc` +
`WORLD`-mutex stop-the-world barrier — see the concurrent-reclamation entry
below). **Per-thread allocation buffers (TLABs) are now implemented** (see the
"Per-thread TLABs" entry below): allocation no longer touches a global lock on
the hot path, ~2× multi-thread allocation throughput, behavior unchanged.
Remaining GC throughput work (the full MMTk Immix move) does not change behavior.
User chose the spec-exact path (Cranelift `user_stack_maps`). Staged:
- [x] **Step 1**: two-word object header `[desc | mark]` + per-type descriptor
      blobs (`size`, `kind`, pointer-field offsets = trace map) + a tracked
      managed heap (`runtime::gc`). Struct/tuple/union-box allocations migrated
      to descriptor-based `lang_alloc`. No collection yet; 440 tests, no
      regression. (Collection stays off until str/List are managed too.)
- [x] **Step 2**: `str` (inline bytes), `List` (handle + managed growable
      buffer), and `Map` (handle + slot buffer) are all under the heap with
      descriptors + variable-size alloc; managed elements are traced.
- [x] **Step 3**: mark-sweep collector (`gc::collect(roots)`) — marks from a
      precise root set following descriptor trace maps (plain ptr-offsets;
      `List` handle scans buffer elements when `elem_is_ptr`), sweeps the
      registry freeing unmarked. 2 unit tests (reachable kept / unreachable
      freed). Takes explicit roots; Step 4 supplies them.
- [x] **Step 4 — precise roots (DONE).** `preserve_frame_pointers` enabled;
      managed-ref **locals and temporaries** marked (`declare_var_needs_stack_map`
      / `declare_value_needs_stack_map`). Backend captures each function's
      `frame_layout().frame_to_fp_offset` + `buffer.user_stack_maps()` and, after
      linking, registers `(return-addr pc, frame_to_fp, SP-relative ref offsets)`
      with the runtime. The collector walks the frame-pointer chain (arm64 `x29`),
      matches each return address to its safepoint **directly** (Cranelift keys
      maps at the return point), reconstructs `SP = caller_fp − frame_to_fp`,
      reads the precise root slots, and runs mark-sweep. Triggered from
      `lang_alloc` on a 1 MiB budget (or every alloc with `OTTER_FUSION_GC=stress`).
      Enabled for `otter_fusion run`; left off for the in-process parallel test harness
      (one shared heap, single-thread scan). Validated by a forced-collect CLI
      test (live `str`/struct survive 300 garbage-allocating iterations).

The tracing GC is functionally complete for single-threaded programs.
- [x] **Step 5 — stop-the-world foundation for threads (`docs/20`).** The
      collector now coordinates across mutator threads via **cooperative
      safepoints**: a per-thread `Mutator` registry (`MUTATORS`, registered on
      first GC touch, dropped at thread exit), a global `STOP` flag, and
      `Running`/`Parked`/`Native` states with each non-running thread's frame
      pointer recorded. Generated code polls `lang_gc_safepoint` at every loop
      header; blocking runtime calls will bracket with `enter_native`/
      `leave_native`. `maybe_collect` takes a one-collector turn (`GC_TURN`),
      stops the world (sets `STOP`, waits for all other mutators to park/native),
      scans **every** thread's stack (`scan_stack_roots_from(fp)`), mark-sweeps,
      then resumes (condvar). Single-threaded programs never park (the only
      mutator is the collector), so behavior is unchanged — all 552 single-
      threaded tests pass and every example runs under `OTTER_FUSION_GC=stress`. The
      multi-thread paths (parking, multi-stack scan, resume) are validated by a
      runtime unit test that runs stop-the-world cycles against 3 real
      safepoint-polling OS threads. `Thread.spawn`/`join` (the language surface)
      builds on this next, then channels + `Shared<T>`.
- [x] **Step 6 — `Drop` finalizers (`docs/10`, `docs/16` §8).** Type descriptors
      gained a `type_id` word (`[size][kind][type_id][n_ptrs][offsets…]`); a type
      with an `extend T: Drop` impl emits its id into its objects' descriptors and
      registers its compiled `drop(self)` in a runtime `type_id → fn` table
      (`lang_gc_register_drop`, done after JIT finalize / in the native C entry).
      Collection is now **two-phase**: after the normal mark, unreachable objects
      that carry a finalizer are *resurrected* (their graph re-marked so `drop`
      can safely read fields), moved to a finalize queue, and — after the world
      resumes but still under the GC turn — their `drop` runs, then they are
      freed. Best-effort / unreachability-triggered / unordered, per the spec (no
      scope-exit hook for *general* managed objects). JIT + native parity;
      `examples/drop.otter`; 1 CLI test; 564 tests.
      **Deterministic endpoint release (`docs/16` §8 carve-out) — DONE for channel
      endpoints:** general `Drop` stays best-effort, but `Sender`/`Receiver` are
      reference-counted runtime handles released *deterministically* at worker
      scope exit, so a channel closes on the last-sender drop without a collection
      — see the Channels entry below (`recv` → `ChannelClosed`, `Receiver:
      Iterator` terminates).
      **Generic `Drop` types — DONE:** `extend<T> S<T>: Drop` registers a finalizer
      *per monomorphization*. The object-header drop slot is keyed by the compiled
      `drop` method's `FuncId` (offset past `GENERIC_DROP_TID_BASE`) rather than the
      shared `def`, so `S<int>` and `S<str>` run distinct `drop` glue; the allocation
      site declares the instance (pinning its `FuncId` and enqueueing the body) and
      `collect_drops` registers every compiled generic-`drop` instance. Works for both
      GC-managed (best-effort) and `@RefCounted` (deterministic) generic types.
      TODO: `Shared` lock release on a panicking body.
      **`@RefCounted` — opt-in deterministic reference counting (`docs/16` §8.1) — DONE:**
      the channel-endpoint carve-out is now generalized into a real, user-facing
      object kind. A `@RefCounted struct` carries a hidden **atomic strong-count**
      word at field-block offset 0 (new descriptor `KIND_REFCOUNTED` + an `n_rc`
      trailer listing owned refcounted-field offsets — the trailer is now written on
      *every* descriptor so the collector reads it uniformly). Runtime intrinsics
      `lang_rc_retain` / `lang_rc_release` (in `runtime::gc`): release at count 0 runs
      the type's `Drop` synchronously, releases owned refcounted fields (cascade),
      then frees — no collection needed. The backend inserts ARC across codegen
      (`FnGen::rc_owned` scope-exit release in `emit_return`; retain/move at
      bind/copy/param/return/capture per a conservative owned-vs-borrowed classifier;
      heap-store retain at struct/tuple field stores and at the `elem_to_i64` /
      `box_value` choke points). The tracing GC is retained as the **cycle-collector
      backstop** (Python-style): a reference cycle keeps its counts > 0 and is
      reclaimed by the collector (drops on the finalizer path). The GC sweep /
      finalizer release dying objects' refcounted children to surviving referents so
      counts stay exact. Checker rejects `@RefCounted` off a plain `struct`
      (`extern struct`, `@Transparent`, non-struct). Determinism boundary: values
      held only by a GC collection / `Shared` / channel / `union`-`dynamic` box / a
      closure capture are GC-timed (never prematurely freed — always memory-safe).
      `docs/16` §8.1; `examples/refcounted.otter`; 5 runtime unit tests + 15 e2e
      cases (`tests/cases/refcounted/`). Deferred: `Weak<T>`; deterministic (vs
      GC-timed) drop for collection/`union`-held refcounted values.
- [~] **Concurrent reclamation — attempted, gated (memory-safe).** Collection is
      still skipped while >1 mutator is live (`maybe_collect`): garbage is retained
      during concurrency and reclaimed after threads join — never freeing a live
      object. Enabling collection under concurrency was attempted end-to-end (the
      stop-the-world machinery — cooperative safepoints, park/native states,
      per-thread published roots unioned by the collector — is correct for the
      `stop_the_world_coordinates_mutator_threads` unit test and **every existing
      concurrency e2e case under `OTTER_FUSION_GC=stress`** — but those barely
      exercise *true* concurrent collection: their workers are short-lived and
      (mostly) non-allocating (`Thread.spawn(() => ident(n))`), so they finish
      almost immediately and the program is effectively single-mutator whenever a
      collection actually runs. The **first genuine test — several long-lived
      workers each allocating heavily in a loop — reveals a use-after-free**: a
      managed object reachable only through a cross-thread root not in the union
      of published roots gets swept, crashing later (control-flow corruption / a
      `memmove` from freed bytes).
      The single-threaded path is unaffected (one stack, scanned at the exact
      alloc point — verified clean under the same heavy load). Initiating
      collection only from clean generated safepoints did **not** close it.
      **Root cause (confirmed by a leak-instead-of-free diagnostic): the
      collector over-collects** — a live object reachable only through a
      cross-thread root absent from the union of published roots is swept. The
      miss is intermittent and allocation-volume-amplified (a timing race in the
      root-publication/scan handshake, not a systematic gap — each individual
      construct misses < ~1/8 of runs, the combined heavy workload ≈ always).
      *Diagnostic method for the fix* (re-add behind env flags): make the sweep
      `survivors.push` instead of `dealloc` ⇒ heavy workload runs clean, proving
      over-collection; poison swept payloads with `0xEF` + leak ⇒ the wrongly-
      freed object is read as data (a corrupt `str`/length), so it's a managed
      data object on a mutator whose published roots missed it. **Conservative
      cross-thread stack scanning** (read *every* word of each stopped thread's
      frame-chain region, a strict superset of the precise safepoint slots) was
      also tried: it *reduced* hard crashes and turned most failures into
      *wrong-but-non-crashing* results — but did **not** eliminate the corruption.
      Since a conservative scan only *adds* roots (it cannot itself corrupt), this
      proves the missed live pointer is **not on any scanned stack** — it is
      register-resident at the stop point, so the fix needs **register capture**.
      **Signal-based stop-the-world was then implemented and tried** (`SIGUSR1` +
      `sigaction`, the handler captures the interrupted thread's `ucontext`
      registers + SP and parks it, the collector conservatively scans each
      stopped thread's `[sp, stack_base)` + saved registers; `pthread_sigmask`
      blocks the signal while a mutator holds a GC lock). It **did not work and
      introduced hangs** — exposing the *real* architectural blocker: **the GC
      heap and the runtime's transient `Vec`/`String` allocations both use the
      system allocator**, so a thread signalled *inside `malloc`/`free` holds the
      allocator's internal lock*; the collector's `dealloc` during sweep then
      deadlocks on it. Masking the signal around our own heap lock does not help —
      the runtime calls `malloc` in many un-maskable places (string concat,
      `to_string`, …). **Definitive conclusion: concurrent reclamation requires a
      *custom GC allocator* (so the collector never contends with mutators on the
      system-`malloc` lock) — i.e. the MMTk/Immix move — not merely a
      stop-the-world protocol.** Reverted to the safe gate (correct, retains
      garbage during concurrency); the precise repro, diagnostics, and this
      blocker are pinned here. The signal-based STW code + `libc` register-capture
      is the right starting point once the heap is on a custom allocator.
- [x] **Custom GC allocator (`gc_alloc`) — the deadlock prerequisite is now
      closed.** Managed objects no longer come from the system allocator: a
      slab-backed, size-segregated free-list allocator (`crates/runtime/src/
      gc_alloc.rs`) serves every GC object, and the sweep/finalizer/teardown
      paths reclaim into its free lists. The collector therefore **never calls
      `free`** — a stop-the-world sweep can no longer deadlock against a mutator
      parked inside `malloc`. Slabs (1 MiB, 16-byte aligned) are carved by
      bump-pointer; freed blocks recycle by size class (16 B granularity ≤ 512 B,
      then power-of-two ≤ 1 MiB; larger served exact). Blocks are zeroed on reuse.
      5 unit tests (size classes, distinct/zeroed/aligned blocks, reuse + re-zero,
      large objects, multi-slab). Single-threaded GC + all 175 e2e cases green,
      including the GC-stress cases — which were **silently not stressing**: they
      set `LANG_GC=stress` but the runtime reads `OTTER_FUSION_GC` (stale from the
      binary rename); fixed across the 7 cases + the framework docs, so the churn
      suites now genuinely collect on every alloc (~26× slower, as expected).
- [x] **Concurrent reclamation — DONE (world-barrier stop-the-world).** The
      collector now runs even while multiple mutators are live; the gate is
      removed. Two prerequisites made it sound: the custom slab allocator
      (`gc_alloc`, no system-`malloc` contention during sweep) and a **world
      barrier** (`WORLD: Mutex<()>`). The collector holds `WORLD` across the
      entire stop→mark→sweep→resume, and **every transition into `M_RUNNING`** —
      a thread resuming from a park, returning from a native call (`leave_native`),
      or a freshly-spawned worker starting (`gc::thread_start`, called at the top
      of both spawn paths) — must acquire `WORLD` and re-check `STOP` first, while
      presenting as non-running so the collector's quiescence wait never deadlocks
      on it. This gives true mutual exclusion: **no mutator executes program code
      (and so never mutates the heap) while the collector marks/sweeps**, which is
      exactly the invariant a snapshot-and-proceed scheme could not hold. Verified
      against the deterministic repro that previously SIGSEGV'd every run: the
      heavy 8-worker churn passes **130/130** under `OTTER_FUSION_GC=stress`
      (0 crashes, 0 hangs, exact result), the light repro **30/30**, and a new
      regression case `concurrency/concurrent_gc_reclamation.otter` (6 workers,
      heavy alloc, exact schedule-independent total) is in the suite. Full
      workspace green (1038) + 176 e2e cases. The earlier diagnostic journey
      (kept for the record):
        1. **It is over-collection.** Sweep-leaks-instead-of-frees ⇒ 0 failures;
           real free ⇒ corruption. A *live* object is being reclaimed.
        2. **The missed root is not on any stack.** Added conservative scanning of
           each stopped thread's whole `[sp, top]` region (a superset of the
           precise safepoint slots, filtered by `is_obj`) ⇒ hard crashes dropped
           but *wrong-but-non-crashing* results remained. A conservative scan only
           *adds* roots, so this proves the live pointer is not stack-resident.
        3. **It is not in callee-saved registers either.** Added explicit
           callee-saved register capture (`x19–x28` / `rbx,r12–r15`) at every
           park/native/collect point, unioned into the roots ⇒ corruption *still*
           persisted (~1/3 of stress runs).
      Stack + register capture together are exhaustive for a *statically* live
      root at a stopped point, so the survivor is a **timing race in the
      stop-the-world root-publication handshake**, **not a static missed root**.
      A runtime **invariant assertion** (`collect()` checks no other mutator is
      `M_RUNNING`) **confirmed this directly** — it fires several times per stress
      run: the collector marks/sweeps using a snapshot while a thread is running,
      so it unions that thread's *stale* published roots (from its last park) and
      sweeps objects the thread has allocated since. Three real fixes were then
      implemented and verified to *reduce* but not eliminate it:
        - **`leave_native` `NATIVE→RUNNING` re-check** (Dekker-style: tentatively
          set `RUNNING`, re-read `STOP` under SeqCst, revert to `NATIVE` if a
          collection began — having run zero mutator instructions). Closes the
          native-return race.
        - **Root gather under the `MUTATORS` lock** (the final quiescence check
          and the snapshot are now atomic, so a registering thread can't slip in
          between them).
        - **Eager `thread_start()`** — a spawned worker registers and parks
          *before* running program code, so it is never an unaccounted runner.
      With all three, the assertion *still* fired (snapshot-and-proceed cannot, on
      its own, stop a thread from re-entering `RUNNING` after the quiescence
      check). **That is what the world barrier above resolves** — by making the
      `→ RUNNING` transitions themselves block on the lock the collector holds,
      rather than trying to detect them after the fact.
- [x] **Per-thread TLABs (allocation throughput) — DONE (behavior-neutral).**
      Allocation used to take **two global mutexes per object** — `gc_alloc`'s
      slab allocator (free-list `HashMap` lookup) and `gc`'s live-object registry
      (`HashMap` insert) — so under multiple mutators the allocator serialized all
      threads (4-thread churn was *slower* than single-thread for the same work).
      Two complementary per-thread buffers fix this, behind the unchanged `Gc`
      interface (`alloc`/`alloc_var`/`collect`/`free`), with **zero behavior
      change**:
        1. **Memory TLAB (`gc_alloc`).** Each thread owns a `LocalCache`: a
           private **bump region** carved in `TLAB_CHUNK` (256 KiB) chunks from
           the global slabs, plus **per-class local free lists** (array-indexed by
           [`class_index`], no hashing) refilled in `REFILL_BATCH` (64) batches
           from the global free lists. `alloc` = local-free pop → local bump →
           refill; the global lock is touched ~once per 64 allocations (or per
           chunk), not per allocation. Mutators **never** `free` (reclamation is
           the collector's job), so `free` always returns blocks to the *global*
           free lists (reusable by every thread, never stranded on the freeing
           thread); a thread's local frees are flushed back to the global pool on
           exit via `LocalCache`'s `Drop` (touches only the `'static` global — safe
           during TLS teardown). Large objects (> 1 MiB) bypass the cache.
        2. **Object-registry TLAB (`gc`).** The per-object `heap.objects`
           `HashMap` insert moved off the hot path into a **per-thread alloc log**
           (`Mutator::alloc_log`, the same proven per-thread-state-unioned-at-STW
           pattern as `Mutator::roots`). At the start of every collection — world
           stopped, so no thread is pushing — `drain_alloc_logs_into` merges every
           thread's log into the global registry, after which the precise-root
           scan and sweep see a complete object set exactly as before.
           `@RefCounted` objects still register **globally** at alloc (they are
           freed deterministically by `lang_rc_release` from any thread, so they
           need global O(1) findability). On thread exit the log is drained into
           the global registry **before** deregistering (`MutatorHandle::drop`), so
           a worker's pinned result graph stays findable. `bytes_since_gc` became a
           lock-free global atomic (`BYTES_SINCE_GC`).
      **Measured** (release, struct/list/string churn): single-thread ≈ **20%**
      faster (810 ms → 650 ms for 4 M allocs); **4-thread ≈ 2×** faster (1.45 s →
      730 ms) — multi-thread allocation now scales instead of contending. Tests:
      7 `gc_alloc` unit tests (size classes, distinct/zeroed/aligned, recycle +
      re-zero, large objects, multi-slab, **concurrent distinct blocks**), all
      `gc` unit tests green, the full e2e + GC-stress + concurrency suites green,
      a new `concurrency/worker_returns_heap_graph.otter` (long-lived workers
      allocating heavily and **returning a heap graph** consumed on the main
      thread — exercises the thread-exit drain + cross-thread result reachability,
      historically the breaking scenario; verified 150× under stress + 20× debug),
      a new `gc/alloc_throughput.otter` deterministic companion, and benches in
      `examples/gc_bench.otter`. The full **MMTk Immix** move stays the future
      throughput plan (the `gc` interface is unchanged, so it remains a drop-in).
- [x] **Methods via `extend`** (inherent): `self` (by-pointer for structs, so
      mutation is visible to the caller), method args, methods calling methods,
      methods returning `Self`-typed values, same method name on different types
      (symbols mangled by def id). Checker resolves `recv.m(..)` against visible
      `extend` blocks (exact target match; generic-extend matching deferred);
      methods reuse the function checker/codegen with `self` + extend generics
      layered in. 10 tests (6 backend + 4 checker).
- [x] **Anonymous tuples**: construction `(a, b)`, `.0` indexing, destructuring
      in `var (a, b) = …` (incl. nested + wildcard), tuples as field types /
      returns / mixed element types. Heap-boxed, reusing the struct layout
      machinery (`layout_of_fields` / `tuple_layout`). 6 tests.
- [x] **Discriminated unions** (the core feature): a `{type_id, data}` heap box.
      Implicit widening via checker-recorded *adjustments* (`results.adjustments`)
      that codegen boxes; widening union→wider-union is a no-op (box carries the
      real type id). `is T` = runtime tag check (or static for concrete
      operands); `as T` narrows with a panic on mismatch (`lang_panic`, exit
      101) and unboxes, or returns the box when narrowing to a sub-union. Covers
      `T | null` optionals, structs-in-unions, str variants. 8 tests + CLI demo.
- [ ] **Bug fixed**: `LocalId`s were resetting per function and colliding in the
      program-wide `local_types` map (could mis-type any multi-function program);
      ids are now globally unique.
- [x] **`match` expression**: scrutinee dispatch over wildcard/binding/literal/
      type-binding (`i64 n`)/unit-struct/tuple patterns + **struct destructuring**
      patterns (record `Circle { radius }` / `{ x: a, .. }` and tuple-struct
      `Rect(w, h)`), with guards (`if`), payload extraction (unbox union variants,
      destructure tuples/structs), and compile-time **exhaustiveness** (irrefutable
      arm, or all union variants + `null`-literal covered). Codegen is a
      top-to-bottom test/bind/body chain on the union tag; a struct pattern tests
      the box tag against the variant's type id, then binds fields from the
      unboxed struct via its layout. Unit-struct values lower as null-pointer
      placeholders (only their type id matters). `examples/match.otter` (Shape
      areas + destructuring perimeters + `i64|str|null` describe).
- [x] **Or-patterns + list patterns** (`docs/07`): `1 | 2 | 3 => …` /
      `Red | Green => …` (alternatives must not bind — the checker rejects a
      binding alternative; codegen ORs each alternative's match test). List
      patterns `[]` / `[x]` / `[a, b]` / `[head, ..tail]` over a `List<T>`: a
      runtime length test (`==` fixed, or `>=` when a `..rest` is present) then
      element binds (leading at `0..rp`, trailing at `n-k+j`) plus the `..tail`
      bound as a fresh sub-list (`lang_list_slice`, GC-paused). Exhaustiveness:
      or-patterns flatten into the covered-variant set; a lone `[..]`/`[..rest]`
      is irrefutable (any other list pattern is a length test needing a
      catch-all). 4 CLI tests (or / or-binding-rejection / list incl. GC-stress);
      JIT + native + GC-stress parity. Pattern matching is now complete:
      wildcard, binding, literal, type-binding, unit, tuple, tuple-struct,
      record-struct, list, and or.
- [x] **Struct destructuring patterns** (`docs/07`): `var Point { x, y } = p;` /
      `var Pair(a, b) = pr;` in irrefutable `var` bindings and as `match` arms —
      record (with field rename `{ x: a }` and `..` rest) and tuple-struct forms,
      nesting (`Wrap { p: Point { x, y }, tag }`). Checker `bind_pattern` /
      `check_pattern` resolve field types via `tuple_fields`/`record_fields` and
      the matched variant via `struct_pattern_ty`; codegen `h_bind_pattern` loads
      each field from the struct layout (`h_load_field`) and the match path
      unboxes the payload then delegates to it. 3 CLI tests (var / match /
      non-exhaustive rejection); JIT + native + GC-stress parity.
- [x] **Flow narrowing** (`docs/12` §4): `if x is T { … }` narrows `x` to `T`
      in the then-branch and to the complement in the else-branch — no explicit
      `as` needed (`x + 1`, `x.v`, `"$x"` all work in-branch). Implemented via a
      checker narrowing overlay + an `Adjust::Unbox` coercion the code generator
      applies (unbox the union box to the known variant). 4 tests.
- [ ] Generic struct construction inference (currently needs explicit `<...>`).
- [x] **String interpolation**: `"$ident"` and `"${expr}"` desugar to a concat
      chain of `to_str` over each part (`docs/01` §8); stringifies the primitive
      set (`as str`) + `str` identity + `null`→"null". **User types are
      interpolatable via their `to_str(self): str` method** (hand-written or
      `@Derive(ToStr)`-synthesised) — the checker records the method
      (`results.stringify_methods[span]`) and codegen calls it. Holes with no
      `to_str` are rejected. 7 tests; `examples/hello.otter` uses it.
- [x] **`?` operator** (`docs/13` §2): partitions the operand union against the
      enclosing return type — variants also in `R` are failures (early-returned,
      boxed through `R`), the rest are the success value (unboxed). Errors on
      empty success ("always returns") / empty failure ("nothing to propagate").
      3 backend + 2 checker tests; CLI demo (find_user/greet).
- [x] **`FromResidual` conversion for `?`** (`docs/13` §4): a failure variant
      `E` *not* in the return type `R` propagates via `Target.from_residual(e)`
      when an `extend Target: FromResidual<E>` exists with `Target ∈ R`. Prelude
      gained `interface FromResidual<R>` (`Program.from_residual_def`); checker
      `find_residual_conversion` scans extends matching target ∈ R and interface
      arg = E, recording `results.residual_conversions[q_span]`; codegen
      (`gen_try`) tests each residual tag, unboxes the payload, calls
      `from_residual`, re-boxes through `R`, and returns. 1 CLI test
      (`IoError`/`ParseError` → `AppError`); GC-stress clean. (`Try` for
      non-union wrapper types — §3 — is the remaining piece.)
- [x] **Generics (monomorphization)**: generic free functions + generic structs.
      Checker does call-site **inference** (unify param types vs args) or takes
      explicit `<...>` args, substitutes into param/return types, and records
      per-call type args (`results.call_type_args`). Codegen is **instance-based**
      (`(DefId, Vec<Ty>) → FuncId`): a worklist seeds non-generic functions and
      declares generic instances on demand at call sites (transitive), each
      compiled under a `Param→concrete` substitution applied to clty/type-id/
      layout via `resolve_shallow`. 7 backend + 3 checker tests; `examples/
      generics.otter`. (Generic methods on `extend`, and nested-generic type-arg
      keys, are follow-ups.)
- [x] **`List<T>`** (builtin generic): injected prelude type (`Program.list_def`,
      no AST item, special-cased). Runtime is a growable `Vec<i64>` of 8-byte
      slots; codegen widens each element to `i64` (uextend) and narrows on read.
      Supports `[a, b, c]` literals (incl. empty with annotation), `xs[i]` /
      `xs[i] = v` (panic OOB), and methods `push`/`size`/`is_empty`/`set`/
      `clear`/`pop`/`insert`/`remove` (`pop`/`remove` → `T|null`, boxed-union
      pattern; `insert` shifts + panics if `i > size`) + `get`/`map`/`filter`/
      `each`/`fold` + `truncate`/`contains`/`index_of`. `contains(v): bool` /
      `index_of(v): i64|null` require `T: Eq` (checker `is_equatable`) and
      dispatch element equality through `gen_elem_eq` (intrinsic `icmp`/`fcmp`/
      `lang_str_eq` for scalars/`str`; the type's `eq` impl via `extend_method_
      fref` for user types). `truncate(n)` shortens to ≤ `n` (`lang_list_
      truncate`). `iter(): Iterator<E>` returns a prelude `ListIter<E>` cursor
      view (drives `for x in xs.iter()` via the protocol). 7 backend + (checker)
      + 10 CLI tests; `examples/lists.otter`. docs/18 §5 `List` API now COMPLETE.
- [x] **`for x in xs`** over a `List<T>`: lowered to an index loop (size/get),
      pattern binding per element, with `break`/`continue` (via the loop stack).
      `for await` rejected; general `Iterator` protocol is a follow-up. 4 tests.
- [x] **Operator overloading** for user nominal types: `a + b`/`==`/`!=`/`<`/…
      dispatch to the operand's visible `extend` method (`add`, `eq`, `lt`,
      …) resolved by name; importable `Eq`/`Ord` remain real `core:prelude`
      contracts, while arithmetic/bitwise protocol labels such as `Add` are not
      catalog exports today. `!=` negates `eq`. Checker records the method in
      `results.operator_methods[op_span]`; codegen emits a method call. 4 tests.
- [x] **`str` methods + content comparison**: `size`/`byte_size`/`is_empty`/
      `contains`/`starts_with`/`ends_with`/`substring`/`to_upper`/`to_lower`/
      `trim`/`repeat`/`replace`/`index_of` (→ `i64|null`) + `split(): List<str>`
      and `get(i): char|null` (runtime intrinsics; checker `check_str_method`).
      `split` builds the result `List<str>` under a GC pause (`lang_str_split`);
      `get` returns the `i`-th USV or the `null` variant (`lang_str_char_at`).
      Fixed a real bug: `==`/`!=`/`<`/… on `str` now compare *content*
      (lang_str_eq/cmp), not pointer identity. 8 + 2 + 2 tests.
      `chars(): Iterator<char>` / `bytes(): Iterator<u8>` are snapshot-backed
      prelude iterators (`StrChars`/`StrBytes` holding a `List<char>`/`List<u8>`
      built by `lang_str_to_chars`/`lang_str_to_bytes`), driven by the standard
      `Iterator` protocol; direct `for ch in s` desugars to `s.chars()` via the
      new `ForDriver::StrChars` (codegen index-loops the snapshot). docs/18 §4
      `str` API now COMPLETE.
- [x] **`List.get(i): E | null`** — bounds-checked, returns a boxed union
      (establishes the union-returning-method codegen pattern). 3 tests.
- [x] **`Map<K, V>`** (`docs/18` §6): open-addressing hash table (a `KIND_MAP`
      handle + slot buffer, both managed), `str` or integer keys. Map literals
      `{ k: v, ..spread }` (parser tries map-first, commits on `key:`; `{}` stays
      a block), `Map<K,V>()`/`Map.new<K,V>()` constructors, and `get`(`V|null`)/
      `set`/`size`/`is_empty`/`contains`/`remove`(`V|null`)/`clear`/`keys`/
      `values`. GC traces pointer keys/values. 17 tests + `examples/maps.otter`.
      - **Fixed a real pre-existing GC bug** surfaced by stress-testing Map:
        `lang_list_push`/`lang_map_set` take a managed value *by value* (not a
        stack-map root); the internal grow allocation could collect it before it
        was stored. Now bracketed with a new re-entrant `gc::pause/resume`. Also
        rooted interpolation/concat temporaries (`gen_str_literal` now marks each
        part + accumulator `needs_stack_map`) — string interpolation previously
        corrupted under stress GC.
- [x] **Real toolchain source** (`stdlib_src/core/*.otter`,
      `stdlib_src/std/*.otter`): lexed/parsed/collected through the
      `TOOLCHAIN_SOURCES` manifest into hidden module-local owners under the
      private `__builtins__` root before user items, with separate synthetic
      `FileId`s per bundled source file. Holds
      `struct Item<T>`, `struct Done`, `interface Iterator<T>`, and the rest of
      the catalog-backed std/core surface — not magic and not in user scope
      without imports.
- [x] **Record-struct generic inference**: `Item { value: v }` infers `Item<U>`
      from field values (seeded by the expected type), mirroring generic-call
      inference (`infer_struct_args` + `unify`).
- [x] **General `Iterator` protocol** (`docs/18` §8): any type with
      `next(self): Item<T> | Done` drives `for x in it` (checker `iterator_elem`
      records `results.for_iters`; codegen `gen_for_iterator` loops calling
      `next`, breaks on the `Done` tag, unwraps `Item<U>.value`; break/continue
      work). `List` keeps its index-loop fast path. 7 tests +
      `examples/iterators.otter`.
      - **Fixed another GC root bug** (general): `box_value` boxed a managed
        payload (e.g. an `Item` struct, a `str`) without rooting it across the
        box allocation — a collection there freed it. Now `mark_root`s a managed
        payload before `lang_alloc`. (The earlier Map stress tests passed only
        because their values were also reachable via the map.)
- [x] **Generic bounds `<T: I>` + monomorphized interface-method dispatch**
      (`docs/11` §1, §4): `param_bounds` stored on generic-param defs; checker
      `bound_ifaces`/`type_implements`/`check_bounds` enforce that each type
      argument implements its parameter's interface bounds (clear error if not);
      a method call on a type parameter resolves through its bounds to the
      interface method (`resolve_bound_method` + `iface_method_sig`), and codegen
      monomorphizes it to the concrete `extend` impl via a precomputed
      `iface_impls` table (`resolve_iface_method`). Interface methods now store
      their signature like `extend` methods. Works with method args and generic
      interfaces (`T: Iterator<i64>` calling `next()`). 4 tests +
      `examples/generic_bounds.otter`.
- [x] **Interface objects / dynamic dispatch** (`docs/11` §5): using an
      interface name as a value type (`var s: Show`, a param, a `List<Show>`
      element) builds an interface object — a managed `{vtable, data}` fat-pointer
      box. Checker: `implements_dyn` makes a concrete type assignable to an
      interface it implements (records `Adjust::WidenDyn`); `check_dyn_method_call`
      type-checks method calls on interface values. Codegen: `emit_vtable` builds
      a per-`(type, interface)` vtable data object (function pointers via
      `write_function_addr`), `gen_widen_dyn` boxes `{vtable, data}`, and
      `gen_dyn_method_call` loads the slot and `call_indirect`s. GC traces the
      data pointer through the box. 5 tests + `examples/dynamic_dispatch.otter`.
- [x] **Interface default methods** (`docs/10`): an interface method may carry a
      default body (`function greet(self): str { "Hi " + self.name() }`); an
      implementer that does not override it uses the default. Implemented by a
      pre-collection pass `sema/defaults.rs::expand_default_methods` that copies
      each un-overridden default body into the implementing `extend` block as an
      ordinary method (so `Self` resolves to the `extend`'s target for free — no
      new monomorphisation path), re-spanning every copied node to keep the
      span-keyed HIR collision-free across implementers (the `derive` rule).
      Works with overrides, defaults calling other (possibly overridden) methods
      through `self`, multiple implementers, and dynamic dispatch (the synthesised
      method is in the vtable). JIT + native + GC-stress parity.
      **Generic & cross-module defaults — DONE:** a generic interface
      (`interface Bounded<T> { … default … }`) has its type parameters substituted
      with the `extend`'s interface arguments (`T` → `i32`) throughout the copied
      signature and body (method-local generics shadow them); a `pub` interface from
      another module supplies its defaults via a program-wide index of every `pub`
      interface (built in `analyze_multi_ctx`), with locals shadowing foreigns and
      ambiguous cross-module names left unresolved (never a wrong body). 8 unit tests
      (`sema/defaults`), 5 e2e cases, 3 CLI cross-module tests, 2 LSP tests.
- [x] **Generic `extend` method resolution** (`docs/11`): `resolve_method` now
      unifies a generic `extend<…> Target<…>`'s target against the receiver and
      returns the solved substitution; method param/return types are substituted,
      and the extend's type arguments are recorded at the call site so codegen
      monomorphizes the method. `build_subst` maps the enclosing extend's
      generics (then the method's). Works for methods returning a type parameter
      or another generic struct, mutating methods, and generic iterators in
      `for` loops (`ForIter.next_targs`). 5 tests + `examples/generic_methods.otter`.
- [x] **Static methods** (`docs/09` §10, `docs/10` §10): a method declared with
      **no `self`** is static (there is *no* `static` keyword — the keyword was
      removed; absence of `self` is the sole signal, set as `Def.is_static`).
      Called `Type.method(args)` and, through a bound, `T.static_method()` for a
      generic param `T: I`. Checker `try_static_call` (in `check_call`, before the
      instance-method path) detects a receiver that names a concrete type
      (`check_type_static_call` → `resolve_method` on the nominal type) or an
      in-scope generic param (`check_bound_static_call` → `resolve_bound_method`
      through its interface bounds); records `results.static_calls`/`static_recv`.
      Codegen `gen_static_call` emits the call with **no receiver**, resolving an
      interface static method to the concrete impl via `resolve_iface_method`.
      `local_env` now carries the function's generics + `Self` (`cur_generics`/
      `cur_self_ty`) so `T`/`Self` resolve in bodies. Calling an instance method
      as static (or vice-versa) is a clear error. JIT + native parity; method-
      level generics (`Type.wrap<i64>(..)`) supported; `examples/static_methods.otter`;
      3 CLI tests. TODO: static calls directly on primitive type names
      (`i32.default()`).
- [x] **Static method inference on generic structs** (`docs/11` §3): a static
      call on a generic type like `Box.new(99)` now infers the type arguments
      from the call's argument types — no explicit `<i64>` needed, mirroring
      generic-free-function inference. `check_type_static_call` keeps a
      parametric `Param(g_struct)` receiver, runs `resolve_method` (so an
      `extend<T> Box<T>` still solves its own generics into the same map),
      then `unify`s each parameter type against its argument, fixed-points the
      substitution (extend → struct → method generics may chain), records the
      now-solved receiver in `static_recv`, and re-checks each argument against
      the final substituted type. The monomorphization args (`call_type_args`)
      are also chain-resolved so codegen sees only concrete types, and bounds
      are enforced on the inferred arguments. Unsolved struct generics produce
      a struct-anchored "cannot infer generic argument" error pointing at the
      receiver. JIT + native + GC-stress parity; `examples/static_methods.otter`
      extended with a `Box.new` showcase; 5 new CLI tests
      (basic / Self-return / multi-arg / native / uninferable-error).
- [x] **Method-level generics on `extend` methods** (`docs/11` §3): two
      bugs uncovered and fixed. (1) `collect_extend_members` collected the
      method's generic-param defs but discarded the result vector, leaving
      `prog.def(method).generics` empty — so any reference to `<U>` in the
      signature errored "cannot find type `U` in scope". `Def.generics` is
      now set from the collected vec. (2) Instance method calls (the
      `check_method_call` path) only substituted the extend's generics —
      they had no inference for the method's own generics. The path now
      unifies parameter types against arg types to solve method generics
      (or accepts explicit `<...>` from the call site, plumbed through
      `check_method_call_with_generics`), chain-resolves the full
      substitution, records the extend-then-method args in
      `call_type_args`, and enforces bounds. `type_implements` was also
      extended to recognise that a `Param(T)` with bound `I` satisfies
      `I` (needed for the new bounds enforcement when the inferred arg is
      itself a type parameter — e.g. in derived `extend<A: Eq> S<A>: Eq`'s
      synthesised body). JIT + native + GC-stress parity; 3 new CLI tests
      (infer / explicit annotation / chained map with native parity).
- [x] **`is`/`as` on interface objects** (`docs/12`): the `{vtable, data}` box
      now also stores the concrete type id (24 B). `is` compares it; `as`
      down-casts (panic on mismatch, returns the data pointer) and up-casts
      (concrete `as Iface` builds the box). Flow narrowing (`if s is Dog`) now
      unboxes interface objects too (the payload sits at offset 8, like unions).
      3 tests; `examples/dynamic_dispatch.otter` extended.
- [x] **`for` over a bounded type parameter and over an interface object**:
      `iterator_elem` resolves `next` through a `Param`'s bounds or an
      interface's methods; `gen_for_iterator` dispatches an interface-method
      `next` to the concrete impl (bounded param, monomorphized) or through the
      vtable (interface object). An interface type satisfies its own `T: I`
      bound. 3 tests; `examples/iterators.otter` extended.
- [x] **`Map` indexing + `for entry in map`** (`docs/18` §6): `map[k]` reads
      (runtime `lang_map_index`, panics on a missing key) and `map[k] = v`
      writes; `for entry in map` yields a prelude `Entry<K, V>` struct (`key`,
      `value`) — codegen snapshots the keys, then builds an `Entry` per key.
      6 tests + `examples/maps.otter` extended.
- [x] **Closures** (`docs/09`): `(params) => body` anonymous functions that
      capture enclosing locals *by value*. Checker does capture analysis
      (`closure_stack`; a local with an id predating the closure is captured),
      infers parameter types from annotations or the expected `Func` type, and
      records a `ClosureInfo`. Codegen lifts each closure to a separate function
      `(env, params…) -> ret` (a `ClosureJob` worklist drained by the driver),
      allocates a managed environment `[fn_ptr, captures…]` (captures GC-traced),
      and calls closures via `call_indirect`. First-class: stored in vars/lists,
      passed to and returned from functions (higher-order). 7 tests +
      `examples/closures.otter`.
- [x] **Named functions as first-class values** (`docs/09` §4): a bare function
      name in value position (`var f = inc;`, passed to a higher-order fn, stored
      in a `List<(i64)=>i64>`, used as a `map` argument) lowers to a closure-style
      env `[thunk_ptr]` whose thunk adapts the function's `(params…)->ret` ABI to
      the uniform closure-call ABI `(env, params…)->ret` — a synthetic
      `Direct`-call body reusing the closure path with zero new codegen plumbing
      (`emit_fn_value`). Generic functions as values are rejected with a clear
      error (call them directly). JIT + native + GC-stress parity;
      `functions/first_class_value` case.
- [x] **Trailing closures + implicit `it` + `List` higher-order methods**
      (`docs/09`, `docs/18`): a trailing closure (`xs.map { it*2 }`) is the
      call's final argument (merged into args at both checker and codegen
      dispatch); a parameterless closure with a one-parameter expected type binds
      `it`. `List` gained `map`(`(E)=>U`→`List<U>`), `filter`, `each`, `fold` —
      codegen iterates the list and `call_indirect`s the closure per element.
      5 tests + `examples/closures.otter` extended. (Captures are by value, so
      `each` is for side effects; `fold` is the accumulator.)
- [x] **Multi-file modules** (`docs/17`): `Program::collect_multi(root,
      externals)` collects the root plus the parsed bodies of file-backed
      submodules (keyed by module path); the driver (`load_submodules`)
      discovers `mod foo;` declarations and loads `<dir>/<stem>/foo.otter`
      recursively (nested submodules supported), feeding `analyze_multi`. Named
      imports, **`import "path" as M` namespace imports** (`M.foo(..)` resolves a
      public function in the aliased module), `pub` visibility, and strict
      scoping work in both JIT and native builds. 6 CLI tests. (Ambient
      extension-only imports and `pkg:` cross-package paths deferred.)
- [x] **`Map.keys()`/`values()`/`entries()` return real `Iterator` objects**
      (`docs/18` §6). Prelude `struct MapKeys<K>`/`MapValues<V>`/
      `MapEntries<K, V>` each carry a snapshot list (built by the existing
      `lang_map_entries` runtime) plus an `index`; the prelude's
      `extend MapKeys<K>: Iterator<K>` etc. implement `next(self): Item<T> |
      Done` against that snapshot. `entries()` snapshots keys and looks up
      values lazily per `next()` (so the keys frozen at call time, value
      mutations during iteration are visible — same as `for entry in map`).
      Codegen: `gen_map_method` allocates the iterator struct via
      `struct_layout` + `alloc_struct`, marks the snapshot pointer as a
      stack-map root across the alloc (the recurring "managed temp held
      across alloc must be a stack-map root" rule), zeroes `index`. 5 new
      CLI tests; JIT + native + GC-stress parity.
- [x] **Capture-by-reference closures** (`docs/09` §7): every captured local is
      cell-backed. A captured local's Cranelift variable holds a pointer to a
      managed 8-byte cell whose content is the local's value; outer-scope and
      closure-body reads/writes both go through that cell, so primitive
      mutations and reference re-assignments propagate to the outer scope.
      Backend gained `bind_local` / `read_local` / `write_local` /
      `bind_local_cell` helpers, an `FnGen::cell_content` map, and a
      `Codegen::captured_locals` set computed once from `results.closures` and
      `results.async_blocks`. Closure env layout: every capture slot stores
      the cell pointer (no longer the value), so the env descriptor traces
      every cap slot. The async state machine's slot layout is similarly
      promoted to `PTR` for cell-backed locals, so a captured local survives
      `await` suspensions intact. Checker fix: `check_lvalue` for an `Ident`
      target now calls `record_capture` so a closure that *writes* a captured
      local makes it into the closure's `captures` list. Closure
      `gen_local_use` (also used for `SelfExpr`) now routes through
      `read_local`. 6 new CLI tests (primitive/str/multi-closure/self-field/
      GC-stress/native); JIT + native + GC-stress parity, all 21 examples
      clean.
- [x] **0-arg closure calls fixed** (regression closed): `var f = () => 42;
      f()` previously errored "call target not lowerable". The parser was
      computing the Call expression's `span` via `expr.span.join(close_span)`
      with `close_span = expr.span` (the Ident's span) when both the args list
      and the trailing closure were absent — so the Call and its callee Ident
      had identical spans, and `expr_types[callee.span]` got overwritten with
      the call's return type, hiding the callee's `Func` type from codegen.
      `parse_call_args_and_optional_trailing` now also returns the `)` token's
      span; both call sites use it as the close-span fallback so the Call's
      span properly extends past the callee. 4 new CLI tests
      (i64 return / capture+mutate / `str` return / native).
- [x] **Anonymous function expressions** (`docs/09` §4): `function(params): Ret
      [async] { body }` in expression position is the same kind of value as an
      arrow closure (uniform by-reference capture). `sema/anf.rs` desugars a
      (non-generic) `AnonFn` into the equivalent `Closure` with a block body, so
      it reuses closure codegen verbatim — and, when `async`, composes with the
      async-closure → async-block desugar. Works bound, capturing, as a `map`
      argument, and `async`. 2 CLI tests; JIT + native + GC-stress parity.
- [x] **`Try` for non-union wrapper types** (`docs/13` §3): prelude
      `interface Try<Output, Residual> { function branch(self): Output |
      Residual }`. A wrapper struct now participates in `?` by writing
      `extend<T> Wrapper<T>: Try<O, R> { function branch(self): O | R { … } }`.
      Checker (`find_try_impl` in sema/check/control.rs): scans every
      `extend Target: Try<O, R>` whose target unifies with the operand
      type, solves the extend's generics, returns the `branch` method, its
      monomorphization args, and the resulting `O | R` union. `check_try`
      now classifies each variant uniformly: union operand → all variants
      are failure candidates; Try operand → `Output` variants are always
      successes, `Residual` variants are failure candidates. Each failure
      candidate is then a *direct* failure (in R), a *converted* failure
      (via `FromResidual<E>`, `docs/13` §4), or, for the union case,
      stays a success (historical lenience). For Try, a residual with no
      conversion path produces a clear error. Codegen (`gen_try`): if a
      `TryBranch` is recorded at the `?` span, calls `branch` first with
      its solved type-args to get the union box, then runs the existing
      tag-dispatch on it; the failure list is the residual minus any
      conversion variants to avoid double-handling. New `TryBranch`
      struct in `sema::results` holds the method, targs, union, output,
      and residual. JIT + native + GC-stress parity. 4 new CLI tests
      (basic / FromResidual chained / native / plain-type rejected).
- [x] User procedural macros (`docs/22` — done, see the dedicated entry).
- [ ] Ambient/`pkg:` imports; concurrent GC.
- [x] **Native object output + linking for `otter_fusion build`** (`docs/23`): the
      codegen backend is now generic over `cranelift_module::Module`, so the
      same lowering drives the JIT (`compile`) and a `cranelift-object`
      `ObjectModule` (`compile_object`). `otter_fusion build foo.otter [-o exe]` emits a
      relocatable object and links it against the runtime **static library**
      (`libruntime.a`, `crate-type = ["rlib", "staticlib"]`) via the system `cc`
      driver into a standalone executable. Because function load addresses are
      unknown at compile time, the emitted C entry point (`main`) registers each
      GC safepoint at startup — `func_addr(f) + code_offset` forms the precise
      pc, with the SP-offset arrays emitted as (4-byte-aligned) data objects —
      then enables the collector and calls the program's `main`. Symbol mangling
      (the leading `_` on Mach-O) is applied by the `object` crate, so bare
      `lang_*` names match `libruntime.a`. macOS needs the host triple promoted
      from `darwin` to `macosx` (a real `LC_BUILD_VERSION`). 4 CLI tests
      (hello, JIT-vs-native output parity, GC-stress live-root survival,
      panic→exit 101); all 11 `examples/*.otter` build and run natively with
      byte-identical output to `otter_fusion run`, including under `OTTER_FUSION_GC=stress`.

### Phase 5 — System features  ✅ DONE (advanced deferrals tracked in "What's next")
- [~] **Threads (`docs/20` §1): `Thread.spawn`/`join`/`detach` work, sync *and*
      async workers.** `Thread.spawn(() => R)` (positional or trailing closure)
      runs the closure on a real OS thread (`runtime::threads::lang_thread_spawn`
      reads the fn pointer from the closure env and runs it) and returns a
      `JoinHandle<R>` (prelude struct holding a registry id).
      **Async worker overload:** when the closure is async (`() => Future<R>`,
      including the trailing form `Thread.spawn { async => … }`), the worker drives
      that future to completion on its own OS thread
      (`lang_thread_spawn_async` = closure-call + `block_on`-drive) and the handle
      still joins on the *awaited* `R` (not `Future<R>`). Such a worker therefore
      MAY `await` and lock a `Shared<T>` — only a *synchronous* `Thread.spawn`
      closure cannot lock (the narrowed compile error). The checker detects the
      async closure (return type `Future<R'>`) and yields `JoinHandle<R'>`; the
      backend passes the `Pending` tid and no `float_kind` (the awaited value rides
      as raw bits through `block_on`). A captured channel endpoint is owned by the
      *future* (released on its completion, not when the building closure returns)
      so a worker can `await` then `send`/`recv` across a suspension. **`detach()`**
      relinquishes a worker fire-and-forget (`lang_thread_detach` drops the
      registry claim + detaches the OS thread); works for sync and async workers.
      **One OS thread per worker is intended** — `Thread.spawn` is the real-OS-thread
      primitive (Rust `std::thread::spawn` analogue), so a worker may block freely;
      massive lightweight concurrency is the `spawn` keyword's job (and a future
      `Task.spawn` on an M:N executor — see "What's next"). **Worker-panic
      isolation is now done** (see its own item below). JIT + native parity;
      `examples/async_thread_spawn.otter` + `concurrency/async_thread_spawn_*`
      cases (lock, parallel, detach, cross-thread channel, GC-stress).
      **`JoinHandle<R>.join()` is async + non-blocking**: it
      yields a `Future<Joined<R> | Panicked>` so the joining task *suspends*
      (`lang_thread_join_future` registers a waker; the worker wakes it on
      publish) instead of parking the OS thread. From sync code the implicit-async
      driver runs it to completion; user surface is just `var r = h.join()` (see
      the Async note below — async is implicit, with the `spawn` keyword).
      The checker recognises `Thread.spawn`, records the result type, and rejects
      mutable-managed captures (deep-clone of mutable managed captures via
      `Clone` is the follow-up); codegen builds the handle/union.
      Handles are GC-pinned (`lang_gc_pin`) for their lifetime. JIT + native
      parity; `examples/threads.otter`; 3 CLI tests incl. multi-worker GC stress.
      **Captures are by-value snapshots** (`docs/20` §6 cross-thread isolation):
      a spawn closure stores each captured local's *value* (a primitive copy, or
      an immutable managed pointer) in its env — not a shared cell — so mutating
      the captured variable after the spawn is never observed by the worker
      (`emit_closure_value_kind(by_value=true)` + the by-value bind in
      `define_closure`). **Float results work** (`f64`/`f32`): the worker calls
      the lifted closure with the matching result ABI and carries the value's
      raw bits across the boundary (`lang_thread_spawn`'s `float_kind`), and the
      `Joined<R>.value` slot is byte-identical to the float. `concurrency/
      spawn_capture_is_snapshot` + `thread_float_result` cases.
      **GC reclamation now runs concurrently with live mutators** — the
      single-mutator gate is removed (world-barrier STW + `gc_alloc`; see the GC
      §). (The intermittent threaded *crash* once attributed to contention was a
      separate async-state-machine bug — a sync `for`+`await` loop losing its
      iteration state across a suspend — now fixed; see the async section.)
- [x] **Worker-panic isolation (`docs/14`, `docs/20` §1, `docs/21` §11): a
      panicking worker fails only itself.** A language `panic` raised in generated
      code cannot be unwound by the host unwinder (Cranelift frames have no unwind
      tables), so each worker runs its body under a `setjmp`/`longjmp` **panic
      boundary** installed at the worker's entry (`crates/runtime/src/panic_boundary.c`
      built by `build.rs` via the `cc` crate; Rust glue in `panic_boundary.rs`).
      `lang_panic` checks `otter_pb_active()`: on a worker it captures the message
      and `longjmp`s to the boundary (restoring the saved context across the
      generated frames soundly, no frame-walk); on the **main** thread no boundary
      is installed, so a panic stays fatal (exit 101). The boundary
      (`run_under_boundary`) restores the invariants the `longjmp` skipped: it
      drains held `Shared` locks (`lang_shared_release_all` — no poisoning,
      `docs/20` §4) and the thread's transient cross-`poll` GC pins
      (`gc::release_unwind_pins`, used by `block_on`), then materialises the
      message as a pinned `str`. `finish_worker` publishes `Panicked { message }`
      so a `JoinHandle.join()` surfaces it recoverably, while a `spawn EXPR`
      awaiter has the panic *re-propagated* at its own `await` (`spawn_poll`) —
      the promise-rejection model (`docs/21` §11). Sibling workers are unaffected.
      Covers `Thread.spawn` (sync + async closures), the `spawn` keyword, and
      executor-multiplexed `Task.spawn`/`spawn` futures. Dedicated OS-thread
      workers install the boundary at worker entry; executor tasks install it at
      the poll call site so a panicking task unwinds only its own state machine
      and the shared worker returns to its run queue. JIT + native parity; the C
      shim is bundled into both the `rlib` (JIT) and `libruntime.a` (native).
      Tests: 5 runtime unit/integration (panic_boundary + a real worker-panic), 4
      CLI integration (isolated join, lock-release, `spawn` propagation,
      GC-stress parity), and e2e `concurrency/{worker_panic_sibling_survives,
      worker_panic_gc_stress,spawn_panic_propagates,lock_released_on_panic,
      task_spawn_panic_many_siblings_single_worker_gc_stress,
      task_spawn_panic_steal_contention_gc_stress,
      spawn_future_panic_sibling_survives_single_worker}`.
- [x] **Channels (`docs/20` §2): `channel<T>()`, `send`, `recv`, `try_recv`, and
      deterministic close-on-last-sender-drop + `Receiver: Iterator`.**
      `channel<T>()` (a recognised builtin, like `Thread.spawn`) allocates a
      runtime channel (`runtime::channels`: a single `Mutex<{queue, waiters,
      senders, receivers}>` + a `Condvar` for the blocking iterator — one lock so
      "empty → register waker", "enqueue → wake", and the endpoint-count
      transitions are all atomic) and returns a `(Sender<T>, Receiver<T>)` tuple
      (prelude structs carrying the channel id). `Sender<T>.send(v)` enqueues +
      wakes any waiter (non-blocking) and returns **`null | ChannelClosed`**
      (`ChannelClosed` once every receiver is dropped). **`Receiver<T>.recv()` is
      async + non-blocking**: it yields a `Future<T | ChannelClosed>`
      (`lang_chan_recv_future`, carrying the `T`- and `ChannelClosed`-variant tids
      so the runtime can box the resolved union) so the task *suspends* on an
      empty channel instead of parking the OS thread; `try_recv(): T | null` polls
      without blocking.
      **Deterministic close (`docs/16` §8 carve-out):** the channel tracks live
      *sender* and *receiver* reference counts. `channel<T>()` starts each at 1;
      `Sender.clone()` emits `lang_chan_sender_acquire` (another producer); a
      `Thread.spawn` worker that captured an endpoint emits a *scope-bound*
      `lang_chan_sender_release` / `lang_chan_receiver_release` when it returns
      (wired through `FnGen.endpoint_releases`, drained on every `emit_return`
      path; populated for `by_value` spawn-closure captures via
      `channel_endpoint_kind`). When the last sender is released the channel
      closes — *immediately, no GC needed* — waking the recv-future waiters and
      the blocking condvar; a drained `recv()` then resolves to `ChannelClosed`.
      **`Receiver: Iterator`**: `for x in rx` lowers to `ForDriver::Channel`
      (checker `receiver_elem`), codegen `h_for_channel` blocking-recvs via
      `lang_chan_recv_blocking(id, *got)` and terminates (`Done`) on close+drain.
      Queued values are GC-pinned (`add_extra_root`) while in the queue and
      unpinned on receipt. Element types are restricted to immutable values for
      now (no deep clone-on-send yet). JIT + native parity; `examples/channels.otter`
      (`for sq in rx`); **7 CLI tests** (iterator-close, recv→ChannelClosed,
      multi-sender clone, send-after-receiver-drop, managed-element GC-stress,
      native parity, try_recv) + **4 runtime unit tests** (last-sender-release,
      drain-then-close, receiver-drop-closes-for-sending, blocking-recv-wakes-on-
      close cross-thread) + **6 e2e cases** (`channel_send_recv`,
      `channel_iterator_close`, `channel_multi_sender_close`,
      `channel_drain_then_close` — closed-before-consume buffered drain,
      `channel_close_gc_stress` — managed `str` elements across the queue under
      `OTTER_FUSION_GC=stress`, `channel_send_after_receiver_drop` —
      `send`→`ChannelClosed`).
      The deterministic-release facility is the seed for a future opt-in
      `@RefCounted` object kind (see goals.txt). TODO: `channel_mpmc`; bounded
      channels; move/deep-clone-on-send for non-immutable `T`; heap-escaping
      endpoints (stored in a long-lived struct) still rely on the best-effort GC
      backstop rather than scope release.
- [x] **`Shared<T>` (`docs/20` §4): an explicit mutex.** `Shared.new(value)`
      (recognised builtin) creates a runtime mutex cell (`runtime::shared`: a
      logical `locked` flag + value guarded by a short-held `Mutex`/`Condvar`,
      keyed by id) and returns a `Shared<T>` handle. `lock(body)` acquires (in GC
      native state), runs the closure with the protected value (a managed `T` is
      mutated in place), releases, returns the body's result; `try_lock(body)`
      returns `R | LockBusy` without blocking. `Shared.clone()` clones the
      *handle* (same cell), and a `Shared` is thread-shareable so workers capture
      it. The inner value is GC-pinned for the cell's lifetime. Mutex serializes
      concurrent increments (2×5000 → 10000, deterministic under stress). JIT +
      native parity; `examples/shared.otter`; 2 CLI tests. TODO: lock release on a
      panicking body (needs unwinding); reentrancy is undefined (per spec).
- [x] **Async (`docs/21`) — COMPLETE, with an implicit-async surface.**
      > **SURFACE NOTE (current design, `docs/21` rewritten):** async is now
      > **implicit**. There is **no** user-visible `async` modifier, `await`
      > expression, `block_on` builtin, `async { … }` block, or `for await` —
      > those were removed from the language surface. A single keyword **`spawn
      > call_expr`** evaluates to a `JoinHandle<T>` (like Go's `go`). User code
      > never spells `Future<T>`: `sleep` returns `null`, `Receiver.recv()`
      > returns `T`, `JoinHandle.join()` returns `Joined<T> | Panicked`,
      > `Shared.lock(fn)` returns the closure's result. The compiler decides which
      > functions need state-machine codegen by a fix-point pass
      > (`propagate_async_calls`): a function is "async" iff its body reaches a
      > suspending op (`sleep`/`yield_now`/`recv`/`join`) or calls another async
      > function; non-suspending functions keep straight-line codegen with zero
      > executor overhead. `main` is driven by `lang_block_on` automatically if it
      > is async. **The `Future`/`Pending`/`Ready`/state-machine/`block_on`
      > machinery described below all still exists internally** — it is exactly
      > what the implicit surface lowers to; the user just never names it. The
      > `[x]` sub-bullets that mention `await`/`async {}`/`for await` describe the
      > internal lowering, reachable from the implicit surface.

      Prelude:
      `interface Future<Output> { poll(self, ctx: *Context): Ready<Output> |
      Pending }`, `Ready<T>`, `Pending`, `extern struct Context`,
      `interface AsyncIterator<T>`, `TimedOut`. The checker type-checks `async`
      functions (return type must be `Future<Output>`; the body yields
      `Output`), `await` (operand must be a `Future`; result is its `Output`;
      only inside an async body), bare `async { … }` blocks and `async`
      closures, and the **"forgot to await"** lint (a discarded `Future`
      statement is an error). Codegen lowers an async function / `async { … }`
      block to a **`Future` state machine**: a `poll(self, ctx)` function runs
      the body (returning `Ready<Output>`), and the public symbol becomes a
      *constructor* that allocates the state struct (holding the captured
      arguments / locals) and wraps it in a `Future<Output>` interface-object
      box whose single vtable slot is `poll`. `block_on(fut): Out` (a recognised
      builtin) drives the future to completion via the runtime executor
      (`runtime::async_rt::lang_block_on`: a poll loop that parks on a condvar
      waker between `Pending` polls, in GC-native state, pinning the future as a
      GC root across polls). **`await` is fully lowered**: an async function
      whose body contains `await` becomes a real suspendable state machine — the
      state struct holds every body local, `poll` dispatches on a saved state
      word to resume at the right `await`, each `await` saves the live locals +
      the inner future and returns `Pending`, and the executor re-polls on wake.
      `yield_now()` (a `Future<null>` that suspends once, self-waking) exercises
      genuine park/resume cycles. `await`s in `if`/`while`/`match` bodies work.
      JIT + native parity; GC-stress clean. **`await` inside `async { … }`
      blocks** works (`block_on(async { await … })`, the doc's main launcher).
      **`spawn(fut): JoinHandle<T>`** drives a future on a worker thread (reusing
      the `JoinHandle`/`join` machinery → `Joined<T> | Panicked`). **`for await x
      in stream`** drives an `AsyncIterator<T>` (`next_async()` is awaited each
      step). `yield_now()` provides genuine suspension; **`sleep(ms)`** is a
      timer-thread-backed future; **`.cancel()`** is a (no-op) abort for the
      compute-only futures we build. `examples/async.otter` exercises the whole
      surface; 14 CLI tests + 2 runtime tests; JIT + native + GC-stress parity.
- [x] **`await` ANF hoisting** (`docs/21`): `await` may appear nested in a
      larger expression — function arguments, operands of `+`/`-`/comparisons,
      index `xs[await i]`, field receivers, `?`, casts, tuple/list/struct/map
      literals, string interpolation `"${await e}"`, and `if`/`match`
      conditions. A source-level pass `sema/anf.rs::hoist_awaits` (run before
      collection, like `derive`) rewrites each nested `await` into a preceding
      `var __await_N = …;` binding, preserving left-to-right evaluation order
      (every *effectful* prior operand is also hoisted so side effects don't
      reorder). `contains_await` sees through control-flow bodies, so an `await`
      hidden in an `if`/`match`/loop branch used as an operand is hoisted whole —
      no sibling temporary is left live across the suspend. The pass is a strict
      no-op for await-free code. JIT + native + GC-stress parity.
- [x] **`await` in genuinely-conditional positions** (`docs/21` §4): the right
      operand of `&&`/`||` (runs only when the left does not short-circuit) and a
      `while` condition (runs once per iteration). These must NOT be lifted out —
      that would change *whether*/*how often* the future is awaited — so ANF
      rewrites each as its own *scope*: a nested operand `await` is hoisted into a
      block that executes only when (and as often as) the position is reached
      (`a && { var t = await b; t > 0 }`), keeping a surviving `await` at a
      statement-level suspend site with no live sibling temporary. The backend
      suspend-site scan (`h_scan_value_await`/`h_scan_for_state`) recurses into the
      `&&`/`||` right operand and the `while` condition so every such `await` gets
      a state slot. Short-circuit suppresses the awaited side effect; a `while`
      condition suspends exactly once per iteration. `await` in a sync function
      (even in an operand) is still rejected by the checker. Covered for the bare
      operand/condition forms, nested-`await` operands (scoped in place), chained
      `&&`, `await` in both operands, evaluation order vs. an effectful left, and
      `||` in a `while` condition (+ a managed-value GC-stress case). 7 ANF unit
      tests + 13 CLI integration tests + 9 e2e cases (`tests/cases/async/`); JIT +
      native + GC-stress parity.
- [x] **Async closures** (`docs/21` §7): both the arrow form `(p) async => E`
      and the anonymous-function form `function(p): Future<T> async { … }` are
      desugared by `sema/anf.rs` into a plain closure returning an async block —
      `(p) => async { E }` — reusing the closure-environment + async-block
      state-machine codegen with **no special case** (the `is_async` branch of
      `h_closure_kind` and the non-empty-`params` branch of `h_async_block` are
      therefore unreachable, kept only as internal-invariant guards). Calling it
      builds the future (capturing `p` + the outer environment by reference,
      `docs/09` §7) without running `E`; `await` (or `spawn`) drives it. The
      closure's value type is `(p) => Future<T>` — a callable, not a bare
      `Future` — so it stores in a struct/list and passes as a higher-order
      argument. Capture-by-reference mutations made inside the body (across an
      `await`) are visible to the next drive and to the enclosing scope. An
      `async` body is a `Future` state machine and so **cannot be `extern`**:
      `record_extern_sig` rejects `extern function … async;` (the same rule that
      forbids extern async closures). Coverage: 12 CLI tests (arrow + `function`
      forms, capture mutation visibility, value typed as `(…) => Future<T>`,
      struct/list storage, `for await` inside the body, `spawn`-ed call, extern
      rejection, JIT + native + GC-stress parity), 2 backend unit tests (desugar
      shape; state-struct layout holds captures as traced cell-pointer slots in
      the save/restore set), 1 LSP hover test (`(i64) => Future<i64>`), 8 e2e
      cases under `tests/cases/async/` + `spawn_async_closure` under
      `tests/cases/concurrency/`; `examples/async.otter`. Async *anonymous
      function* expressions (`function(..) async { }` bound to a `var`) work too
      — `sema/anf.rs` desugars a non-generic `AnonFn` into the equivalent
      closure, which then composes with the async-closure desugar.
- [x] **`for await` over a non-variable stream**: `for await x in make()` now
      works — `sema/anf.rs` hoists a non-`Ident`/`self` stream expression into a
      preceding `var` (gaining a state-machine slot so it survives the
      per-iteration suspends). 1 CLI test; JIT + native + GC-stress parity.
- [x] **`for await` over an interface `AsyncIterator` object** (or a bounded
      `T: AsyncIterator<U>` param): the checker's `async_iterator_elem` now
      resolves `next_async` through the interface (mirroring the sync
      `iterator_elem`), and `h_for_async` dispatches it via the vtable for an
      interface receiver (else through the concrete impl). 1 CLI test; JIT +
      native + GC-stress parity.
- [x] **`timeout(fut, ms): Future<T | TimedOut>`** (`docs/21` §9): a racing
      future. The runtime `lang_async_timeout` builds a future whose `poll`
      polls the inner future first (reboxing its `Ready<T>` value into the `T`
      variant of `T | TimedOut`) and, on the inner being pending, arms a deadline
      timer that resolves the race to `TimedOut`. The code generator supplies
      `T`'s type id + pointer-ness (so the runtime builds the variant box and the
      collector traces its payload) plus the `TimedOut`/`Ready`/`Pending` ids —
      the "type-id plumbing" that lets the runtime rebox the value variant
      without `select`. Recognized as the `timeout` builtin
      (`Intrinsic::AsyncTimeout { output }`). 2 CLI tests (value vs `TimedOut`;
      managed `str` value under GC stress); JIT + native + GC-stress parity;
      `examples/async.otter`.
- [x] **Sync `for` loop with an `await` in its body** (`docs/21`): a `for` loop
      that is not itself `for await` but whose body suspends now preserves its
      iteration state across the suspend. The loop's codegen-internal iterable
      pointer(s) + index counter live in Cranelift SSA, which does **not** survive
      a `poll` return — so `async_state_layout` reserves per-loop state-struct
      slots (`(primary, secondary, index)`, keyed by `iter.span` via
      `h_scan_for_state`; the iterable slots are GC-traced) and all four sync
      `for` drivers (`ListFast`/`Iterator`/`Map`/`StrChars`) read/write their
      iteration state through those slots when inside an async body. **This was
      the actual root cause of the long-suspected "threaded runtime instability"**
      — a threaded `gather` (`for h in handles { await h.join() }`) hit it
      whenever a join future was `Pending`. Threaded async-join programs that
      crashed 2–90% of the time are now deterministic (`many_threads`/
      `thread_storm`/1600-spawn stress: 0 crashes across many runs). 3 new
      deterministic regression tests (`iterators/for_await_in_body_{list,map}`,
      `for_await_nested`); JIT + native + GC-stress parity.
- [~] FFI (`docs/19`): **extern functions + extern structs + raw pointers work.**
      *Extern functions* over the C ABI: primitives and raw pointers (`*T`),
      called by their real symbol name (JIT resolves via `dlsym`, native via the
      linker); checker records each lowered signature (`results.extern_sigs`),
      backend `gen_extern_call` declares a C-ABI import; `clty_of` maps `*T` →
      machine pointer. **Extern structs (`docs/19` §3)**: header-less, C-ABI
      laid out, **stack-allocated** (no GC involvement — fields are scalars /
      raw pointers, validated `ReprC` by the checker). Construction, field
      access/mutation, and the **layout decorators `@Packed(N)` / `@Align(N)` /
      `@Union`** all work (over-aligned `@Align(N>16)` is honored by
      over-allocating + rounding the base pointer). **`&place`** (address-of) on
      an extern-struct place yields its `*T` address (no pin needed for stack
      values); **`*p`** dereferences a raw pointer — identity for a
      pointer-to-extern-struct, a scalar load otherwise — and **`*p = v`** stores
      a scalar; both **panic on null** (exit 101). **`*A as *B`** (and
      `*T ↔ usize/isize`) pointer reinterpret casts are no-ops. The canonical
      C out-pointer pattern works against real libc (`memcpy(&dst, &src, n)`),
      JIT + native byte-identical, GC-stress clean. `examples/ffi.otter`; 14 CLI
      tests. **`extern var` (`docs/19` §4)**: a C global, read and written
      through an imported writable data symbol (`extern_var_addr`); the checker
      resolves it as an assignable place; tested against libc `optind`
      (read = 1, then write-back); JIT + native parity; 2 CLI tests.
      **Fixed-size arrays `[T; N]` (`docs/19` §4)**: valid as extern struct
      fields; laid out inline (`N * size(T)`, element alignment); `arr[i]`
      reads/writes elements (no bounds check, raw FFI); `&arr[i]` is an element
      address (fillable by a C function); extern struct literals may omit fields
      (the C block is zero-filled on construct, so an array field with no literal
      form starts at zero). JIT + native + GC-stress parity; 3 CLI tests +
      `examples/ffi.otter` (`Sockaddr { family, data: [u8;14] }`). **Opaque
      `extern type` handles (`docs/19` §4)**: an incomplete C handle used only
      behind a pointer (`*File`, `**Sqlite3`). The `*Named(ExternType)` → machine
      pointer path round-trips through C (verified with libc `tmpfile`/`fileno`/
      `fclose`, JIT + native); dereferencing to an opaque value is rejected (no
      value representation). 2 CLI tests. **Null-pointer optimization (NPO,
      `docs/19` §2)**: a `*T | null` union is laid out as a single *raw* nullable
      pointer (`null` == `0x0`), NOT a `{type_id, data}` box — and crucially is
      NOT GC-traced (it points into foreign/unmanaged memory). Widening is the
      identity (`null` → `0`), `is null`/`is *T` are null/non-null tests, `as`
      reinterprets, and `if p is null { … } else { … }` flow-narrows. This makes
      C functions returning nullable pointers (libc `malloc(): *T | null`) work
      with real null-checks + heap writes. JIT + native + GC-stress parity; 3 CLI
      tests + `examples/ffi.otter`. `match`/`?` on an NPO union are rejected
      (use the `is null` check). **Foreign allocation (`docs/19` §5)** — the
      full family: `Foreign.alloc<T>()` / `alloc_zeroed<T>()` (`sizeof(T)`
      bytes), `alloc_flex<T, E>(n)` (`sizeof(T) + n*sizeof(E)`, flexible array
      member), `realloc<T>(p, new_size)` (resize preserving bytes), and
      `free(p)`. All return a raw `*T | null` (NPO). The runtime
      (`runtime/foreign.rs`) wraps the system allocator with a size header (so
      `free`/`realloc` recover the layout); fields are written through `*p`.
      Recognized as builtins (like `Shared.new`); JIT + native + GC-stress
      parity (foreign blocks are never traced); 4 CLI tests + `examples/ffi.otter`.
      **Boundary string/byte types (`docs/19` §6, `docs/18` §9)**: `CString` is
      an owning, `@RefCounted` managed handle — `from_str(s)` copies a `str` into
      a fresh NUL-terminated foreign-heap C string; `as_ptr` hands the raw `*u8`
      to C, `as_cstr` borrows a `CStr`, `byte_len` is C `strlen`, `to_str` copies
      back to a managed `str` — and it **frees its buffer deterministically on
      scope exit** via `Drop` (no manual `free`). `CStr` is a borrowed
      `extern struct { ptr: *u8 }` view (`from_ptr`, `byte_len`, `to_str`).
      `Buffer` is an `extern struct { data: *u8, size: u64 }` foreign-heap byte
      region with *manual* lifetime (`alloc(size): Buffer | null`, bounds-checked
      `get(i): u8 | null` / `set(i, v)`, `free`). All three are ordinary prelude
      types (`extend` blocks over small runtime helpers `lang_cstr_len` /
      `lang_buffer_read` / `lang_buffer_write`; `lang_foreign_outstanding` exposes
      the live-foreign-block count for leak assertions). Verified against libc
      `strlen`/`strcmp`/`memcmp`/`strncmp`; JIT + native + GC-stress parity (drop
      frees the buffer — the foreign-block counter returns to baseline); CLI tests
      + `tests/cases/ffi/` e2e + `examples/ffi.otter`. **Extern-struct-by-value
      escape (`docs/19` §3)**: returning an extern struct by value, or boxing one
      into a non-NPO union (`Buffer | null`, `Pt | null`), copies its bytes into a
      managed heap block (`GC_KIND_PLAIN`, traced reference *to* it, never inside)
      so the value does not dangle past its frame — fixing a latent miscompile
      that truncated a 16-byte struct to an 8-byte stack pointer. **Loudly
      rejected**: `&` on non-extern-struct scalars, deref of an opaque type,
      `match`/`?` on `*T | null`. **Nested extern structs
      (`docs/19` §3)**: an `extern struct` field of another extern struct is laid
      out *inline* (its bytes, not a pointer): `field_size_align` returns the
      inner C-layout size/align, construction byte-copies the inner value in
      (`copy_bytes`, 8/4/2/1-byte chunks), field access yields the address of the
      inline bytes, and scalar / whole-struct mutation both work. Correct offsets
      (`stime` at +16, `maxrss` at +32 in a `Rusage`-shaped struct) prove inline
      layout. JIT + native + GC-stress parity; 1 CLI test + `examples/ffi.otter`.
      **`@Link(lib = "…")` (`docs/19` §13)**: directs symbol resolution — the
      JIT `dlopen`s the library so `dlsym` finds the symbol, and the native build
      passes `-l<lib>` to `cc`. The checker collects the libs
      (`results.link_libs`, de-duped) from extern-function `@Link` attrs.
      Verified against zlib `crc32` ("hello" → 907060870), JIT + native +
      GC-stress; 1 CLI test. **`@Transparent` ABI newtype (`docs/19` §3)**: a
      single-field struct (`struct Num(i32)`) whose runtime representation and C
      ABI are exactly its field's — no heap box. `transparent_inner` makes
      `clty_of`/`is_managed_ptr` see through it; construction returns the inner
      value, `.0` is the identity. Verified via libc `abs(Num(-5)) == 5` (proving
      the i32 ABI); JIT + native + GC-stress; 2 CLI tests. **`@CallConv("c"|
      "system"|"stdcall"|"fastcall")` (`docs/19` §7)**: the checker validates
      placement (extern functions only) and value (default `"c"`); the backend
      wires the selected convention into the extern-call signature
      (`extern_call_conv`). On the 64-bit targets the compiler emits, the four
      spellings coincide with the platform C ABI (`stdcall`/`fastcall` are 32-bit
      x86 conventions that 64-bit ABIs fold into the default, as C compilers do).
      Verified calling libc under each convention; JIT + native; CLI tests + 2
      e2e error cases. **`@Variadic` (`docs/19` §13) done — via `libffi`**: Cranelift
      has no fixed/variadic ABI boundary (`ir::Signature` is just
      `{params, returns, call_conv}`), so a variadic call cannot be lowered as an
      ordinary call (aarch64-apple-darwin passes variadics on the stack; x86-64
      SysV needs `%al`; Windows x64 duplicates floats). The backend marshals the
      arguments — C-default-promoted in variadic position (`f32`→`f64`, sub-int→
      `int`), `@Transparent` seen through to its inner scalar — into a flat
      8-byte-slot value buffer + tag array and routes the call through the runtime
      `lang_variadic_call` shim, which drives `libffi`'s `ffi_prep_cif_var`/
      `ffi_call` (`crates/runtime/src/variadic.rs`, a minimal hand binding to the
      system `libffi` since neither `pkg-config` nor autotools is assumed; JIT links
      it via the runtime build script, native links `-lffi`). The checker rejects
      `@Variadic` off an extern import, with no fixed prefix, with decorator args,
      or with a non-scalar/`str` variadic argument; a call below the fixed arity is
      an arity error. Used as a value (not called by name) the import is an ordinary
      non-variadic fn pointer over its fixed prefix. Verified against real `printf`/
      `snprintf` (int/str/double/char/hex, i64/u64, f32 promotion, negatives,
      `@Transparent`); JIT + native parity + GC-stress; runtime + checker unit
      tests, 4 e2e `run` cases, 7 e2e error cases, 1 native-parity test.
- [x] `@Derive` + procedural macros (`docs/22`): **`@Derive(Eq)`, `@Derive(Ord)`,
      `@Derive(ToStr)`, `@Derive(Clone)`, and `@Derive(Hash)` work** — a source-level desugaring
      (`sema/derive::expand_derives`, run in `analyze`/`analyze_multi` before
      collection) synthesises one deduped `extend` block: field-by-field `eq`
      (`==`/`!=`), lexicographic `lt`/`le`/`gt`/`ge` (`<`/`<=`/`>`/`>=`; `Ord`
      implies `Eq`), `to_str(self): str` rendering each field via `as str`
      (record/tuple/unit forms), and/or `clone(self): Self` constructing a fresh
      value with each field `.clone()`d. The `Clone` impl declares the `Clone`
      interface (so `(type, Clone)` lands in the impl table for monomorphized
      bound dispatch); the others resolve by operator/name. **All four derives
      also work on generic structs** (record, tuple, unit): the impl is a generic
      `extend<T: Eq + Ord + ToStr + Clone> S<T>: …`. On a generic struct,
      per-field operations are synthesised as method calls —
      `self.fi.eq(other.fi)` / `.lt(…)` / `.to_str()` / `.clone()` — (the
      `==`/`<`/`as str` forms don't apply to a bare type parameter) that dispatch
      through each field's bound. `Eq`/`Ord`/`ToStr` are now real prelude
      interfaces (`Program.eq_def`/`ord_def`/`to_str_def`), primitives/`str`
      satisfy them via `type_implements`, and codegen (`gen_method_call`) emits
      the intrinsic comparison / `as str` for a primitive receiver — mirroring how
      `Clone` dispatches. Every derived interface is declared on the synthesised
      `extend` so concrete derived types satisfy `T: Eq`/`Ord`/`ToStr`/`Clone`
      bounds (e.g. as another generic struct's element); generic `to_str` also
      works through string interpolation (`tostr_method` records the extend's
      type args at the interpolation site).
      Two underlying fixes enabled this: the seed phase no longer compiles a
      method of a *generic* `extend` with an empty substitution (monomorphized
      per call site — also fixes hand-written generic `extend` methods), and
      generic **tuple-struct construction** now infers its type arguments from
      the positional argument types (`check_tuple_ctor`), with codegen laying the
      value out for the inferred instance. Operator overloads on a generic type
      now record/pass the extend's type args (`call_type_args[op_span]`).
      Synthesised nodes get unique spans in a virtual file (`FileId(u32::MAX-1)`)
      so the span-keyed checker tables don't collide; the CLI renders diagnostics
      on virtual files without an excerpt. 13 CLI tests (incl. native parity).
      **User procedural macros now work end-to-end** (`docs/22`, see the
      dedicated entry below).
- [x] **User procedural macros** (`docs/22`): a `@ProcMacro pub function
      Name(MacroContext, ASTNode): ASTNode` written in the language is
      JIT-compiled and run at compile time (phase 2, before type checking) by a
      new `crates/macros` driver, for all three invocation forms — decorator on
      items, expression `@Name(args)`, and block `@Name { … }` (new
      `ExprKind::MacroCall`). The `core:compiler` surface
      (`ASTNode`/`MacroContext`/`Span` + `__ast_*`/`__mctx_*` externs + method
      wrappers) lives in the prelude; `ASTNode` is an **opaque handle** into a
      per-thread AST arena in `backend::macro_host` (so its host symbols register
      into every JIT — the seeded prelude surface methods must link, but are
      compiled lazily via `is_macro_surface_method`, never seeded). The engine
      gathers macro defs, builds a self-contained macro sub-program (dependency
      closure + `i64`-ABI entry shims), compiles it via
      `backend::compile_with_symbols`, expands invocations recursively to a fixed
      point, then strips the compile-time-only macro defs from the runtime
      program. **Hygiene** is a gensym (`ctx.fresh_ident`/`unhygienic`, §5);
      **sandbox** rejects any macro that uses a `std:` name (§6); diagnostics via
      `ctx.error` (fatal) / `warn` / `note` (informational) + `ASTNode.error_marker()`
      (§7); **recursion** is depth-limited (default 128, `[macros]
      recursion_limit` in the manifest) with an invocation-chain error (§10). CLI
      runs `expand_user_macros` in `prepare` before analysis; the LSP expands a
      clone before analysis (cross-boundary diagnostics/hover/goto) and offers
      `@`-prefix macro-name completion. ~21 CLI unit tests + 6 backend arena unit
      tests + 5 `tests/cases/macros/` e2e cases + 2 LSP tests + manifest test;
      `examples/macros.otter` (JIT + native parity).
- [x] **`Hash` + user-typed `Map` keys** (`docs/15` §7, `docs/18` §6): prelude
      `interface Hash { function hash(self): u64 }` (`Program.hash_def`).
      Primitives + `str` implement `Hash` intrinsically (`type_implements`);
      `.hash()` on those receivers routes — through `check_builtin_hash`, then
      backend `gen_primitive_hash` — to the runtime entry points
      `lang_hash_i64` / `lang_hash_str` / `lang_hash_f64` (splitmix64 / FNV-1a;
      shared with the runtime `Map`'s built-in strategy). `@Derive(Hash)`
      synthesises an `extend T: Hash` whose body XORs each field's `.hash()`
      (record/tuple/unit; concrete and generic via the `T: Hash` bound).
      `Map<K, V>` keys may now be any `K: Eq + Hash`: the map handle carries
      two nullable function-pointer slots (`hash_fn`, `eq_fn`); `gen_map_new`
      takes the addresses of the key type's `extend` `hash`/`eq` methods (via
      `iface_impls` + `declare_instance` + `func_addr`) and passes them to
      `lang_map_new`. Built-in integer/`str` keys keep the runtime's
      original (now centralised) hashing — the function-pointer slots stay
      null and the runtime falls back. The map's eq fn-ptr uses a `u8` C ABI
      return (`docs/15` §7) so the user `eq`'s compiled `bool` (Cranelift
      `I8`) matches exactly, no trampoline. 5 new CLI tests + updated
      `examples/maps.otter` (`Coord` keyed map); JIT + native + GC-stress
      parity.
- [x] **`Clone`** (`docs/10`/`docs/15` §8): prelude `interface Clone`
      (`Program.clone_def`). `.clone()` is intrinsic for immutable values
      (primitives/`char`/`bool`/`str`/`null` — `CloneKind::Identity`, since
      sharing an immutable value is observationally a deep copy) and for `List`/
      `Map` of immutable elements (`lang_list_clone`/`lang_map_clone` copy the
      backing storage into a fresh managed object, GC-paused so the new object
      survives). User types clone through a `clone(self): Self` impl (derived or
      hand-written), resolved by the normal method path. A `T: Clone` bound
      type-checks via `type_implements` (immutable / immutable-collection /
      declared `extend … : Clone`) and dispatches in codegen to the intrinsic
      (builtin receiver) or the concrete impl (user receiver). Cloning a `List`/
      `Map` of mutable elements was previously rejected; **now** deep-cloned
      element-by-element when the element implements `Clone` (`CloneKind::
      ListDeep`/`MapDeep`). A recursive `gen_clone_value(v, ty)` codegen
      helper dispatches over immutable values, handle types, collections (of
      immutable elements via the runtime helper, of mutable via the deep
      paths), and user types (resolved through `iface_impls[(T, clone_def)]`).
      `gen_list_clone_deep` / `gen_map_clone_deep` allocate a fresh
      collection, root the source + destination across the per-element
      allocation, and push each cloned element/value back; `Map` deep clone
      reuses the source map's hash/eq fn-ptrs and snapshots the keys via
      `lang_map_entries` to avoid concurrent-mutation hazards. 3 new CLI
      tests (List/Map deep clone with mutation observability + native
      parity). `examples/clone.otter`; JIT + native parity;
      GC-stress clean. 4 CLI tests.

### Phase 6 — Toolchain  ✅ DONE (advanced pkg/LSP deferrals tracked in "What's next")
- [x] `otter_fusion` CLI (`crates/cli`): Clap-based `check` / `build` / `run` (auto
      `--help`/`--version`) over the full pipeline, with caret diagnostics +
      error recovery. `run` JIT-executes `main`; `build [-o exe]` emits a native
      object and links a standalone executable (see Phase 4). Verified on
      `examples/`.
- [x] **LSP server + VS Code extension** (`crates/lsp` → `otter_fusion_lsp`, plus
      `editors/vscode`). The server is built on `tower-lsp`/`tokio` and reuses
      the front-end (`Compiled` recompiles each open buffer; queries are driven by
      a `HirIndex` built over the typed **HIR** — the old span-keyed `CheckResults`
      tables are gone, retired into HIR node fields per Phase 2.5). Features: live
      diagnostics (lex+parse+sema), hover (types +
      symbol/builtin signatures), go-to-definition (name-precise, for
      functions/methods/globals/struct ctors/locals), find-references, rename,
      document symbols (items + struct fields + interface/`extend` methods),
      completion (keywords/builtins/top-level defs/locals), and full semantic
      tokens (resolution-driven classes refining a bundled TextMate grammar).
      Editor positions are converted UTF-16↔UTF-8 (`LineIndex` on the hot path).
      11 unit tests in `crates/lsp`; the extension compiles (`npm run compile`).
- [x] **`project.toml` manifest** (`docs/17` §17.1): `otter_fusion run|build|emit`
      accept a project directory or a `project.toml` path and resolve the entry
      source file from the manifest — explicit `entry = "..."`, else the
      `kind`-derived default (`{src}/main.otter` for `binary`, `{src}/lib.otter`
      for `library`/`library+bins`; `src` defaults to `src`). A hand-rolled
      `key = "value"` reader (no TOML dependency) parses the fields the toolchain
      uses; a missing entry is a clear error. Submodules load from the entry via
      the existing convention. 3 CLI tests (dir + manifest-path + default entry +
      submodule + missing-entry). A `.otter` path still works directly.
- [x] **Multi-file LSP analysis**: the server now loads a document's file-backed
      submodules (`docs/17`) before analysis, so cross-module `import`s resolve
      and the open file type-checks against them (previously every import errored
      "cannot find module"). `Compiled::new_multi` parses each `mod` file into the
      same `SourceMap`/`Externals` and runs `analyze_multi`; the server's reader
      prefers an open, unsaved editor buffer over the on-disk copy (the
      unsaved-buffer overlay). A missing submodule degrades gracefully (the
      import surfaces as a diagnostic; no crash). Diagnostics/queries stay scoped
      to the open document. 2 LSP tests.
- [x] **Cross-file LSP goto-definition**: `definition_span` now accepts a
      definition in any loaded file (not just the open document), excluding
      virtual files (prelude / synthesised code) via the real `file_count`. The
      `goto_definition` handler maps the def span's `FileId` back to that file's
      URI (`Url::from_file_path` on the `SourceMap` entry) and computes the range
      against that file's text, so jumping to an imported symbol opens the
      correct submodule file. 1 LSP test.
- [x] **`otter_fusion doc`** (`docs/23`): generate Markdown API documentation for a
      file or project's `pub` items — doc comments (`///`) rendered as prose, each
      signature sliced from source (a function shows its header up to the body;
      a struct/interface/alias/var shows the whole declaration), private items
      omitted, attributes (`@Derive`) kept. Printed to stdout. 1 CLI test.
- [x] **`otter_fusion run --time` / `exec --time`** — print the program's pure
      execution time (the body of `main` and anything it drives) to stderr,
      *excluding* lexing/parsing/type-checking/JIT compilation. The driver times
      only `Jit::run_main()`; the line is stable and machine-parseable:
      `execution time: 135.541µs (135541 ns)` (adaptive human unit + exact ns).
- [x] **End-to-end test suite + framework** (`tests/cases/`, runner in
      `crates/cli/tests/{framework,suite}.rs`). Every case is a self-contained
      `.otter` program with its own `import`s, carrying its expectations inline as
      `//@` directives (`kind: run|compile-error|panic`, `exit`, `stderr`,
      `release`, `serial`, `env: K=V`, `known-bug`) and `//~` exact-stdout lines.
      The runner discovers the corpus, runs each via the real `otter_fusion`
      binary (with `--time` for run/panic cases), checks stdout/exit/stderr, and
      prints a status + per-category timing report. `OTTER_TEST_BLESS=1`
      regenerates `//~` blocks for passing `run` cases. **176 cases** across 29
      categories (run / panic / compile-error, plus GC-stress
      `OTTER_FUSION_GC=stress` and concurrency cases incl. a 100-thread storm and
      a heavy concurrent-GC-reclamation churn). We test failure modes, not just
      happy paths. (Run via `cargo test -p cli --test suite`.)
  - **Known-bug / XFAIL catalog** (LLVM-style): a `known-bug` case states the
    *spec-correct* behaviour the implementation does not yet meet; it is expected
    to fail today (reported XFAIL, does not fail the suite) and is flagged XPASS
    (suite failure) if it ever starts passing, so the marker gets removed. The
    suite thus **catalogs the unfinished surface instead of hiding it**. The
    catalog is **currently empty** — all six previously-surfaced XFAILs were
    fixed and promoted into their categories: tuple-pattern arity mismatch (now
    a clean compile error, no backend crash), duplicate struct-literal field
    (rejected), record pattern on a tuple struct (clear diagnostic), named
    function as a first-class value (works, via a closure-ABI thunk),
    `Thread.spawn` of a float-returning fn (works for `f64`/`f32`), and spawn
    capture-by-value snapshots (`docs/20` §6 — the prior "mutable loop variable"
    XFAIL's spec-correct behavior). The intermittent threaded crash once blamed
    on contention turned out to be the sync-`for`+`await` state bug (now fixed,
    see the async section); thread-spawning cases still run `serial` to keep
    cross-process CPU contention low. See `tests/README.md`. Runs under
    `cargo test -p cli --test suite`.
- [x] **Cross-file LSP references + rename**: the HIR index already spans every
      analyzed module (the open document plus its loaded submodules), so
      `references` and `rename` now collect use sites across **all** of them and
      map each span to its own file's URI via a shared `span_to_location` helper
      (also used by go-to-definition). `rename` groups edits per file into one
      `WorkspaceEdit`. Hover already reports cross-module symbol types (the type
      rides on the HIR node). 1 LSP test (a symbol used in the open doc *and*
      within a submodule surfaces references in both files). Project-wide
      indexing of files that *import* the open document (reverse references) is
      the remaining follow-up.
- [x] **Type-position go-to-definition**: goto on a type name written in a type
      annotation (param / return / field / alias / `extend` target / generic
      bound) jumps to that type's definition. Type-position names aren't value
      resolutions in the HIR, so the LSP resolves them from the AST: a recursive
      `walk_type` over every item-level type position builds `(name-span, name)`
      refs (`collect_type_refs`), `type_def_span_at(off)` resolves the innermost
      one to its def's name span (`def_name_span`), and `goto_definition` tries
      it as a fallback after value resolution. 1 LSP test (param + return + field
      type names). Follow-up: body `var x: T` / `e as T` / pattern type names.
- [x] **`otter_fusion test` + the `test` keyword** (`docs/23`): named tests
      `test "name" { … }` — a *contextual* keyword (special only as `test "…" {`
      at item position, so `test` stays usable as an identifier). Each is a
      zero-arg unit body (`DefKind::Test`) checked like a function and compiled by
      the same seed/codegen path; a test passes if its body completes and fails if
      it panics (assertions via `panic`). `otter_fusion test [path]` enumerates the
      tests and runs **each in its own child process** (`test <path> --exact
      <symbol>`, a hidden flag) so a panicking test fails only itself; it prints
      each outcome + a summary and exits non-zero if any failed (surfacing the
      panic message on failure). `examples/tests.otter`; 3 CLI tests
      (pass/fail/all-pass + identifier-still-works) + 2 parser tests.
- [x] **`otter_fusion bench` + the `bench` keyword** (`docs/23`): `bench "name" {
      … }` — the same contextual-keyword + `DefKind::Test` machinery as `test`
      (an `is_bench` flag on `TestItem` distinguishes them), so it shares all of
      the checking/codegen. `otter_fusion bench [path]` runs each `bench` body in
      its own child (`bench <path> --exact <symbol>`) that warms up then times an
      **adaptive** iteration count (grows ×4 until the window is ≥ ~50ms) and
      prints `ns/iter (<n> iters)`. `test` runs only `test`s, `bench` only
      `bench`es. 1 CLI test (timing + separation). `examples/tests.otter`.
- [x] **`otter_fusion lint`** (`docs/23`): lightweight static lints over the typed
      HIR (no extra inference) — **unused local variables** (a `var` binding never
      read; parameters and `_`-prefixed names exempt), **unused private functions**
      (a non-`pub` free function never called or used as a value;
      `main`/tests/benches/methods exempt), and **unreachable code** (a statement
      after a diverging one — `return`/`break`/`continue` or any `never`-typed
      expression like `panic`/`exit`, recursing into nested blocks). A single HIR
      walk (`crates/cli/lint.rs`) collects local reads + def references; diagnostics
      render to stderr, a count to stdout. Purely informational (a clean compile
      with warnings still exits zero; a compile *error* fails). 3 CLI tests
      (unused / clean / unreachable); 0 false positives across all examples. The
      analysis lives in `compiler::lint` (shared), and the **LSP publishes these
      as editor `WARNING` diagnostics** for the open document (only when it is
      otherwise error-free, so the HIR is complete) — squiggles for unused
      vars/fns and dead code, alongside the existing error diagnostics. The LSP
      also offers a **code-action quick-fix** ("Prefix `_` to silence unused
      `name`") for each unused-variable binding overlapping the request range —
      the in-editor counterpart of `otter_fusion fix`.
- [x] **`otter_fusion fix`** (`docs/23`): safe automatic fixes — currently renames
      each unused local variable to `_name` (inserts `_` at the binding, silencing
      the unused-variable lint without removing code; the var is unused so there
      are no read sites to update). Reuses `lint::analyze`, groups edits per file,
      applies them right-to-left so offsets stay valid, and rewrites in place;
      `--check` reports without writing. 1 CLI test (check → no change; apply →
      `_`-prefixed, used var untouched, lint then clean).
- [x] **`otter_fusion fmt`** (`docs/23`): a conservative source formatter
      (`crates/cli/fmt.rs`). Normalizes **indentation** (two spaces per bracket
      level, closer-leading lines dedented), strips trailing whitespace, collapses
      blank-line runs, and ensures a single trailing newline. A string/comment-
      aware single scan computes each line's bracket depth (brackets inside
      strings / `//` / nested `/* */` are ignored; block-comment interiors are left
      verbatim). It deliberately does **not** rewrap or re-space within a line, so
      no token ever crosses a line boundary — and every reformat is verified by
      **re-lexing the output and requiring an identical token stream** (same
      kinds + text), so `fmt` can only change whitespace, never code (it refuses
      to write otherwise). `fmt <file|dir>` formats in place (recurses dirs,
      skipping hidden/`target`); `--check` lists unformatted files and exits
      non-zero (CI gate). Idempotent; verified across all 22 examples (0 token-
      stream violations; formatted output runs identically). 5 unit + 1 CLI test.
      Follow-up: token-level intra-line spacing + line wrapping. The formatter
      lives in `compiler::fmt` (shared), and the **LSP exposes it as a
      `document_formatting` provider** (format-on-save in the editor): the handler
      formats the open buffer, verifies the token-preservation invariant, and
      returns a whole-document edit (declining if it would change tokens). 2 LSP
      tests; the VS Code extension picks the capability up automatically.
- [x] **`otter_fusion repl`** (`docs/23`): a line-oriented read-eval-print loop
      (`crates/cli/repl.rs`). Each line is classified — a **declaration**
      (`function`/`struct`/…/`test`/`bench`) accumulates as a top-level item; a
      **`var` binding** accumulates as a persistent local (replayed each eval, so
      later lines see it); a trailing-`;` **statement** runs once; a bare
      **expression** is printed via string interpolation. Each evaluation builds a
      fresh single-file program (auto-imported prelude + accumulated items + a
      `main` of the bindings + the current line), analyzes and JIT-runs it; a line
      that fails to compile is reported and **not** accumulated, so the session
      stays valid. `:help`/`:reset`/`:quit`; missing `;` is added for terse input.
      State persistence + function definitions + error recovery verified by a
      stdin-piped CLI test.
- [x] **`otter_fusion explain` + diagnostic codes** (`docs/23`): the structured
      semantic-error kinds carry stable codes `E0001`–`E0019`
      (`SemaErrorKind::code`), surfaced in diagnostics as `error[E0006]: …`.
      `otter_fusion explain <code>` (case-insensitive) prints a long-form
      explanation; an unknown code lists the available ones. The recurring
      free-form `Message` categories were **promoted to coded kinds**
      (`E0013`–`E0019`): no-such-method, no-such-field, unknown/missing/duplicate
      struct-literal field, non-exhaustive match, and `break`/`continue` outside
      a loop — each a structured `SemaErrorKind` variant reproducing the exact
      prior message text, with an `explain` entry. Truly one-off diagnostics
      stay free-form `Message` (idiomatic — not every diagnostic needs a stable
      index, cf. Rust). 2 CLI tests (E0006 + the 7 promoted codes round-tripped
      through `check` and `explain`) + 4 checker unit tests assert the kinds.
- [x] **`otter_fusion expand`** (`docs/23`): parse the entry file and print it
      back through the AST source-printer (`compiler::ast_print`, above).
      Best-effort on parse errors; output re-parses to the same AST and
      type-checks identically (certified across all 22 examples). 3 e2e tests.
- [x] **Promote recurring `Message` errors to coded kinds** — done (see the
      `explain` entry above; `E0013`–`E0019`).
- [x] **`pkg:` live registry network round-trips** — done: a dependency-free
      sparse-HTTP registry server (`pkg::server`, CLI `otter_fusion serve`) hosts
      the full protocol, and `crates/pkg/tests/live_registry.rs` round-trips the
      `HttpRegistry` client against it over real TCP (publish/index/download/
      verify/search/yank).
- [x] **Custom GC allocator** (`gc_alloc`) — done; no system-`malloc` contention
      during sweep, and the GC-stress suite actually stresses (see GC §).
- [x] **Concurrent-GC reclamation** — DONE via the world-barrier stop-the-world
      (see GC §): the collector runs while multiple mutators are live, the gate is
      removed, and the deterministic heavy-churn repro that previously SIGSEGV'd
      every run is clean 130/130 under stress, with a regression case in the
      suite.
- [ ] Remaining: only the advanced deferrals noted inline — git dependency
      *fetching*, feature-gated optional-dep resolution, multi-major coexistence,
      and the publish metadata-sidecar. Per-thread TLABs are now **done** (see the
      GC section); the full MMTk Immix move is the remaining behavior-neutral
      throughput follow-up.

### Phase 7 — Embedding engine (`std:engine`, `docs/26`)  🔧 DESIGN — NOT STARTED
Run Otter Fusion *from* Otter Fusion: compile + execute guest source inside a
sandboxed **isolate** (own heap/GC/registries, capability whitelist, host-bound
modules, hard resource limits) — the substrate for edge-function-style workloads
and plugin systems. **Spec: `docs/26-engine.html`.** The substrate is proven: the
backend is already a Cranelift JIT and the macro system (Phase 5 / `docs/22`)
already compiles guest source from a string, binds host functions into a virtual
module, JITs it, and calls it by pointer. This phase turns that one-shot,
compile-time machinery into a persistent, re-entrant, sandboxed runtime surface.
**Locked design decisions (user-approved):** bridged data crosses by
**copy-by-value over a `@Bridge` repr-C layout** (never by shared heap pointer, so
the heaps stay independent); resource isolation uses **per-isolate runtime state**
(each isolate owns its heap/GC/registries, selected by a thread-local
current-isolate pointer). Staged so each step is shippable:
- [ ] **Stage 1 — single-isolate `load`/`invoke` (primitives).** `std:engine`
      prelude surface (`Isolate`/`Unit`/`Policy`/`Limits`/`Stats`/`LoadError`/
      `EngineError`) + host externs; compile guest source under a capability policy;
      invoke a `pub` entry by name returning a primitive. Per-entry trampolines
      follow the macro-shim pattern.
- [ ] **Stage 2 — capability policy enforcement (`docs/26` §3).** A policy field on
      the resolve context + per-isolate built-in-view whitelist enforced at import
      resolution (deny-by-default; compile-time rejection of forbidden `std:`/`core:`
      modules; `pkg:`/`file:` denied unless granted with vetted source).
- [ ] **Stage 3 — `@Bridge` ABI (`docs/26` §5).** The `@Bridge` decorator: frozen
      repr-C layout (reuse extern-struct machinery), bridge-compatibility checking
      (plain-data only; no managed refs / `Drop` / dyn / unions), and field-by-field
      copy-by-value marshalling in trampolines + bindings; `str` copied as bytes.
- [ ] **Stage 4 — host bindings + bridge channels (`docs/26` §4, §6).** Bind
      host/native functions into virtual `host:` modules the guest imports
      (generalize the macro-host symbol registration to arbitrary signatures +
      modules); engine-managed bridge channels / `Shared` cells that span the
      boundary; entry/signature introspection on `Unit`.
- [ ] **Stage 5 — per-isolate runtime state (`docs/26` §8).** *The deep one.* Thread
      a runtime-context handle (selected by a thread-local current-isolate pointer)
      through every `lang_*` entry point so heap, GC, mutator set, channel/`Shared`/
      finalizer registries, and allocation accounting are per isolate; stop-the-world
      GC operates per isolate. Foundation for hard limits + true isolation.
- [ ] **Stage 6 — hard limits + stats (`docs/26` §8).** `max_heap_bytes`/
      `max_alloc_bytes` enforced in the per-isolate allocator; `timeout_ms` +
      cancellation via a deadline/cancel flag polled at the safepoints codegen
      already emits; `max_stack_depth` guard; `Stats` from the per-isolate accounting.
      Earlier stages run on the shared global heap with best-effort limits and say so.
- [ ] **Stage 7 — async entries (`docs/26` §7).** Detect an `async` guest entry
      (Future-returning) and drive it on the isolate's executor, completing the
      host's `await` when the guest future resolves (preserves the no-user-visible-
      `block_on` rule). Interacts with the M:N executor work (Phase 5 deferral).
- [ ] **Stage 8 — cold-start caching (`docs/26` §9).** A content-addressed
      compiled-artifact store over the existing native-object path (`compile_object`):
      key = hash of guest source + capability policy + participating `@Bridge` layouts
      + target triple + compiler/runtime version; on hit `mmap`/`dlopen` the cached
      object and resolve entries by symbol (runtime `lang_*` resolve against the host
      process, as the JIT does) instead of recompiling; on miss compile + store. Plus
      a **warm isolate pool** over a cached `Unit` (a payoff of Stage 5's per-isolate
      state — a fresh isolate is a fresh heap/registry set over already-mapped code).
      Cache key correctness is the critical property: a mismatch is always a miss,
      never a silent reuse under a different policy/ABI. **Heap snapshots**
      (V8-startup-snapshot analogue — snapshot a unit's post-init heap, restore on
      spawn) are noted as a *future* tier, harder under the tracing GC, NOT in scope.
- Tests (when picked up): unit + integration + e2e — capability denial (compile
  error), `@Bridge` round-trips (host↔guest, all bridge types), host bindings +
  bridge channels, arbitrary-entry invocation, per-isolate heap independence,
  hard memory cap → `OutOfMemory`, timeout → `Timeout` (incl. tight-loop latency),
  guest panic containment, async entry driven from a host `await`; JIT + native
  parity + GC-stress. Keep `docs/26`, examples, LSP and ROADMAP consistent.

## Current state (verified 2026-05-30)

**The language is feature-complete end-to-end at production quality.** The full
pipeline (lex → parse → derive/default/ANF desugar → collect → check → typed HIR →
monomorphized Cranelift codegen → JIT *and* native link) runs every designed
feature, JIT≡native byte-identical and GC-stress clean.

- **Tests green:** `cargo test --workspace` → **1038 unit/integration tests, 0
  failures**; the e2e suite (`cargo test -p cli --test suite`) → **176 cases**
  across 29 categories. All **22 examples** run identically under `otter_fusion run`
  and `otter_fusion build` (native). *(Minor: 2 clippy-style warnings in `runtime`
  test code — "direct cast of function item into an integer" — cosmetic.)*
- **Phases 0–2.5: DONE.** Frontend, types + name resolution, full type checking &
  inference, and the typed-HIR refactor (all 21 span-keyed `CheckResults` tables
  retired; the checker assembles the `Hir` directly; `hir::lower` deleted).
- **Phase 3:** monomorphization + closure/dyn lowering DONE at the HIR→codegen
  worklist; a separate typed **MIR is deferred by design** (not a missing feature).
- **Phase 4 (codegen+runtime): feature-complete** — all language features lower;
  remaining work is *performance optimization* (goals.txt), not features.
- **GC: DONE**, including **concurrent reclamation while multiple mutators run**
  (`gc_alloc` slab allocator + `WORLD`-mutex stop-the-world barrier) and
  **per-thread TLABs** (per-thread memory bump cache + per-thread object-registry
  log, ~2× multi-thread allocation throughput, behavior-neutral). Remaining GC
  work is throughput-only (the full MMTk Immix move).
- **Phase 5 (system features):** structs, unions, generics, interfaces (static +
  dynamic + default methods), error handling (`?`/`Try`/`FromResidual`), pattern
  matching (complete), closures (by-ref cells), threads/channels/`Shared<T>`,
  **async (implicit, `spawn` keyword)** incl. `sleep`/`timeout`/for-await/async
  closures, FFI (extern fns/structs/vars/arrays/opaque types/NPO/`Foreign.*`/
  CString/CStr/`@Packed`/`@Align`/`@Union`/`@Transparent`/`@Link`), all `@Derive`s
  (Eq/Ord/ToStr/Clone/Hash), `Drop`, full stdlib collection/string/numeric APIs.
- **Phase 6 (toolchain): DONE** — `check`/`build`/`run`/`exec` (`--release`,
  `--time`), `test`/`bench`, `lint`/`fix`/`fmt`, `repl`, `doc`, `expand`,
  `explain` (`E0001`–`E0019`), full module/import/package system + live registry
  (`serve`), LSP + VS Code extension (multi-file, cross-file goto/refs/rename).

## What's next (drives goals.txt)

**Immediate (active goals):**
1. **Deeper backend / compiler optimizations** (split from the completed first
   backend optimization pass; see `possible-optimizations.txt`): escape-analysis
   stack allocation for non-escaping final structs, call-graph-guided inlining,
   devirtualization of statically-known interface-object paths, and thinner
   type-tag/vtable paths after devirtualization. Keep all observability
   (`--emit=tokens|ast|hir|clif`, DWARF, `--time`) and JIT≡native parity.
   *No behavior change; benchmark with `bench` + `run --time`.*
2. **Finish the long tail** (goals.txt "finish end to end"): work the remaining
   items below one by one, test-gated, docs/LSP/examples kept consistent.

**Recently completed:**
- **Interface direct/branch fast paths: DONE.** HIR codegen now recognizes
  interface values constructed directly from concrete structs, and
  interface-valued `if`/`match` expressions, when they are immediately consumed
  by `is Concrete`, by a concrete `as` downcast, or as an interface-method
  receiver. When the source concrete type is statically known and non-
  `@RefCounted`, codegen evaluates the concrete value and produces the final
  boolean/downcast result or direct concrete method call directly, skipping the
  temporary `{vtable,data,type_id}` wrapper, vtable pointer store/load, type-id
  stamp/load, and `call_indirect`. Interface values that escape into
  locals/results still keep the ordinary wrapper representation. Added backend
  CLIF/runtime regressions for direct immediate tests/downcasts, immediate `if`
  and `match` tests/downcasts/method receivers, plus the boxed-local fallbacks,
  JIT/native CLI parity regressions, and `examples/interface_devirt_bench.otter`
  coverage for `otter_fusion bench`.
- **Union branch tag fast paths: DONE.** HIR codegen now recognizes
  union/`dynamic`-typed `if` and `match` expressions that are immediately
  consumed by `is` or by a concrete `as` narrowing. When each branch/arm has a
  statically-known unmanaged scalar/null variant, codegen evaluates the selected
  path and produces the final boolean/narrowed value directly, avoiding a
  transient union box and runtime tag load. The ordinary escaping path was
  tightened at the same time: union-valued `if`/`match` joins that flow into
  locals/results now box concrete branch values before the merge block, so later
  tag checks see the correct `{type_id,data}` representation. Added backend
  CLIF/runtime regressions for the immediate fast paths and boxed-local
  fallbacks, JIT/native CLI parity regressions, and
  `examples/union_tag_bench.otter` for `otter_fusion bench`.
- **Empty-struct payload/tag thinning: DONE.** Ordinary final structs with no
  runtime fields, no `Drop`, no `@RefCounted`, no transparent representation,
  and no extern layout now use a null field-block sentinel instead of allocating
  an empty managed payload. Union/dynamic boxes still carry the semantic type id,
  and interface objects still carry their vtable/type id, but their data slot is
  a non-traced null sentinel for these empty structs. This implements the first
  safe part of the "empty structs don't need to be allocated" optimization
  without changing `is`/`as`, pattern matching, dynamic dispatch, or GC
  observability. Added CLIF-shape/runtime backend regressions for direct empty
  construction, union tagging, and interface wrapping, a JIT/native CLI parity
  regression, and `examples/empty_struct_tag_bench.otter` for
  `otter_fusion bench`.
- **Escape-analysis stack allocation for scalar final struct locals: DONE.**
  HIR codegen now pre-scans each function body for record- and tuple-struct
  locals whose binding-site literal/constructor is proven not to escape:
  whole-value uses such as calls, returns, aliases, address-of, casts, boxing,
  closure capture, and async capture disqualify the value, while field loads and
  field stores remain eligible. The first production-safe slice only applies to
  ordinary final structs with scalar fields and no traced pointers,
  `@RefCounted` ownership, channel endpoints, `Drop`, spread, transparent
  representation, arrays, or nested aggregate fields. Eligible locals use a
  zeroed Cranelift stack slot with the same in-frame field-block pointer
  representation; escaping structs keep the managed heap object path. Added
  CLIF-shape/runtime backend regressions for the stack and heap sides, JIT/native
  CLI parity regressions, and `examples/stack_struct_bench.otter` for
  `otter_fusion bench`.
- **Conservative scalar helper inlining: DONE.** Backend codegen now collects
  whole-HIR direct-call counts and uses them as a call-graph signal for a tiny
  direct-call inliner. Non-generic free functions over unmanaged scalar
  parameter/return types can be emitted inline when called once (or when
  trivially tiny), including simple scalar `let` temporaries before the trailing
  expression. Larger helpers called multiple times stay as real calls to avoid
  code growth. The slice deliberately excludes managed values,
  ownership-sensitive types, async, closures, assignments, control flow, nested
  calls, and allocations. Added CLIF-shape/runtime backend regressions for the
  inlined and non-inlined paths, JIT/native parity CLI regressions, and
  `examples/inlining_bench.otter` for `otter_fusion bench`.
- **Staged interface receiver devirtualization: DONE.** HIR codegen now
  recognizes interface method calls whose receiver is an immediately-created
  interface object, such as `(Concrete { ... } as Interface).method()` or a baked
  `WidenDyn` wrapper. It evaluates the concrete receiver first, roots it across
  argument evaluation for GC safety, and emits a direct monomorphized call to the
  concrete impl instead of allocating the `{vtable,data,type_id}` wrapper and
  issuing a `call_indirect`. It also tracks conservative straight-line facts for
  interface-typed locals initialized or assigned from known concrete values, so
  `var s: Shape = Rect { ... } as Shape; s.area()` can call the concrete impl
  directly while preserving the interface box as the local representation.
  The fact inference also survives `if` and `match` expression joins when every
  branch/arm produces the same concrete implementor. Facts are dropped for
  captured locals, locals assigned inside branch/loop/match bodies, and
  mixed-concrete joins, so interface-typed parameters and genuinely dynamic
  receivers still use vtable dispatch. Added CLIF-shape and runtime backend
  regressions for immediate receivers, straight-line locals, same-concrete and
  mixed-concrete `if`/`match` initializers, branch joins, captured locals, and
  interface parameters, plus `examples/interface_devirt_bench.otter` to benchmark
  the devirtualized-local, same-concrete-if, same-concrete-match, and
  dynamic-parameter paths with `otter_fusion bench`.
- **Backend optimization pass: DONE.** Release builds now ask Cranelift for its
  speed-oriented backend optimization pipeline while debug keeps CLIF readable.
  CLI/native codegen is root-seeded (`main` or exact test/bench body) and lazily
  pulls reachable callees, closures, async jobs, vtables, generic instances, and
  finalizers; unused helpers and untouched stdlib functions are not emitted.
  Allocation descriptors now declare non-generic `Drop` finalizers on demand, so
  dead-code trimming cannot drop finalizers that are reachable only from object
  headers. Added backend JIT and native object-symbol regressions plus CLI smoke
  coverage for debug and release runs.
- **Async `Shared<T>` lock (`docs/20` §4): DONE.** `lock`/`try_lock` are now
  `lock<R>(self,body):Future<R> async` / `try_lock<R>(self,body):Future<R|LockBusy> async`,
  awaited by callers. The runtime cell (`crates/runtime/src/shared.rs`) is a task-aware
  async mutex — a `locked` flag + a **FIFO** waiter queue of `(waker_data, wake_fn)`; a
  contended acquire registers the executor's waker and returns `Pending` (no OS-thread
  parking), so the lock is fair and starvation-free. The lock is built as a runtime
  `Future` (`lang_shared_lock_future`) the caller's `await` drives: acquire → run the body
  closure under the lock (driving an `async` body's future to completion, so the lock is
  HELD across the body's `await`s — fixing the release-before-await footgun) → clone the
  result out *while held* (via a codegen-emitted clone thunk) → release. Cancel/panic
  release via a per-thread held-lock set (`lang_shared_release_all`) — drained by the
  **worker-panic boundary** (now done; see its entry above) so a panicking lock body
  releases the lock with no poisoning. Only *synchronous* `Thread.spawn`
  workers cannot lock (the narrowed compile error → use an *async* `Thread.spawn` worker or
  the `spawn` keyword); an async worker drives its future with a real executor and may lock.
  A new sema escape/detachment taint pass
  rejects references that outlive the body (`.clone()` detaches; a returned reference is
  cloned at the boundary). Tests: e2e `tests/cases/concurrency/*` (mutual exclusion under
  contention, held-across-`await`, `try_lock` busy/free, non-reentrancy, escape rejection
  ×4, clone hatch, return clone-out, GC-stress) + runtime/sema/backend unit tests.
  *Limitation:* a float-typed protected value/result is rejected with a clear error
  (uniform integer/pointer body ABI) — wrap in a struct. *Deferred:* `RwLock<T>` (separate
  primitive).
- **`@RefCounted` — opt-in deterministic reference counting (`docs/16` §8.1): DONE.**
  Generalizes the channel-endpoint carve-out into a user-facing object kind: atomic
  strong count, synchronous `Drop` + free at count 0, ARC retain/release across
  codegen, tracing GC as the cycle backstop. See the Phase-5 GC/Drop entry. Deferred:
  `Weak<T>`; deterministic (vs GC-timed) drop for collection/`union`-held values.

**Remaining features / deferrals (each test-gated when picked up):**
- **`await` in a short-circuit operand / loop condition** — genuinely conditional
  suspension — **done.** `sema/anf.rs` rewrites the `&&`/`||` right operand and the
  `while` condition as their own *scope* (rather than hoisting the `await` out),
  preserving evaluation order and conditional suspension frequency; the backend
  suspend-site scan recurses into those positions. See the Phase-5 async entry.
- **User procedural macros** (`docs/22`) — **done** (decorator/expression/block
  forms, JIT-run at compile time, hygiene, sandbox, recursion limit). Remaining
  refinements: cross-*package* (`pkg:`) compiled macro plugins; precise spans
  rendered into macro-generated source; macro-using-macro at the definition site;
  privileged build-script macros (§future).
- **Channel close on last-`Sender` drop** + `Receiver: Iterator` termination —
  needs *deterministic* `Drop` (tension with GC-timed best-effort `Drop`).
- **Worker-panic isolation** — **done** (`Thread.spawn`, `spawn` keyword, and
  executor-multiplexed `Task.spawn`): a panicking worker/task fails only itself
  (surfaces as `Panicked` on `join`, or re-propagates at a `spawn` awaiter).
  Dedicated OS-thread workers install a `setjmp`/`longjmp` panic boundary at
  worker entry because host unwinding can't cross Cranelift frames; executor
  tasks install the same boundary at the poll call site so a panicking task
  unwinds only its own state machine and leaves sibling tasks on the same worker
  thread alive. Locks released, roots dropped. See the Phase-5 concurrency entry.
- **M:N work-stealing executor + `Task.spawn` (`std:task`)** — **done.**
  Landed the first executor-backed slice: `spawn EXPR` now schedules onto a
  lazily-started M:N worker pool with per-worker queues, stealing, a global
  injector, task-local held-`Shared` lock tracking, poll-site panic boundaries,
  and duplicate-poll suppression for wake-during-poll futures; `std:task` is
  importable and `Task.spawn` now accepts both sync `() => R` and async
  `() => Future<R>` closures, returning a task-specific `std:task::JoinHandle<R>`
  with `join`/`detach`/`cancel`/`abort` and scheduling them on the executor.
  The executor sizes from hardware parallelism with a higher bounded cap and an
  `OTTER_FUSION_TASK_WORKERS=N` runtime override for stress/performance tuning.
  `future.cancel()` now has
  real executor teeth for `spawn EXPR` futures: it marks the underlying task
  cancelled, stops future polls, wakes waiters, and releases any task-held
  `Shared` locks while suspended; generated async-block futures also carry a
  cleanup hook so cancellation can run prompt state cleanup for owned captures
  instead of waiting for GC. Runtime-built `sleep` and `timeout` futures also
  unregister their one-shot reactor/timer interests when cancelled, and
  cancelling a pending `timeout` future cascades cancellation into its inner
  loser future. `Task.spawn` handle cancellation uses the same path and `join()`
  now resolves to `Joined<R> | Panicked | Cancelled`; the OS-thread
  `std:thread::JoinHandle<R>` remains distinct and intentionally has no
  `cancel()` or `abort()` method. `timeout(fut, ms)` now calls the same
  cancellation hook when the timer wins, so cancellable loser futures such as
  `spawn EXPR` release their suspended task state. `Thread.spawn` remains the
  dedicated OS-thread primitive.
  Tests added for sync and async `Task.spawn`, by-value capture snapshots for
  both sync and async `Task.spawn` closures (JIT/native), compiler/e2e rejection
  of non-shareable mutable captures through `Task.spawn`, async `Task.spawn` with
  `Shared.lock`, spawn-future cancellation releasing a held lock, mass
  `spawn EXPR` future cancellation releasing captured sender endpoints so
  receivers close promptly, including a 512-task stress-GC storm (JIT + native),
  task-handle
  `cancel()`/`abort()` joining as `Cancelled`, `abort()` sharing the same
  suspended-state cleanup path as `cancel()` and releasing task-held `Shared`
  locks (JIT + native), sync and async `Task.spawn`
  panics joining as `Panicked` while a sibling task completes (JIT + native),
  one-worker `Task.spawn` panic-after-await while holding `Shared` releasing the
  lock and allowing a sibling executor task to complete (JIT + native),
  one-worker stress-GC `Task.spawn` panic storms proving many sibling tasks
  still join and no task reports false cancellation, four-worker stress-GC
  steal-contention panic storms proving poll-site boundaries do not tear down
  shared executor workers, and a `spawn EXPR` panic-sibling case proving a
  panicking spawned future does not stop unrelated spawned futures on the same
  one-worker executor,
  `timeout(h.join(), ms)` cancelling only the join waiter while the
  `Task.spawn` worker continues and can be joined later (JIT + native),
  multiple concurrent `JoinHandle.join()` waiters on the same executor task
  all waking and observing the completed result (JIT + native), same-frame
  multiple join futures and 128 spawned waiter tasks concurrently awaiting one
  `Task.spawn` join handle all waking as `Cancelled` after handle cancellation
  (JIT + native),
  sync-closure `Task.spawn`
  cancellation remaining cooperative (no forced `Cancelled` without an `await`
  suspension point) with a runtime guard against treating closure envs as future
  cleanup boxes, active-poll cancellation taking effect only once the poll
  returns `Pending`, cancellation publishing waking every registered join/spawn
  waiter exactly once, timeout loser cancellation releasing a held lock plus
  512 losing `spawn EXPR` futures releasing captured channel endpoints and
  512 losing channel `recv()` futures releasing pinned handoff state plus parked
  channel waiters under stress GC (JIT + native), shared
  reactor-backed timer wakeups for many sleeping executor tasks, direct runtime
  cancellation cleanup for pending `sleep` and `timeout` registrations, explicit
  JIT/native cancellation of a pending `timeout(sleep(...), ...)` future cleaning
  both timer registrations, runtime mass-cancellation coverage proving suspended
  executor tasks unregister their persistent waker entries and release handoff
  roots, staggered
  `Task.spawn` timer deadlines joining deterministically in JIT/native, direct
  scheduler unit coverage for round-robin worker queues, global injector
  priority, stealing, wake-during-poll coalescing,
  wake-while-already-queued preservation, duplicate runnable-copy suppression,
  duplicate pending join/spawn waiter coalescing, post-unlock reschedule
  draining for the lost-wake race, parking `Pending` tasks that have not
  actually been woken (no executor busy-requeue backstop),
  `Shared` head-only wake handoff with FIFO acquisition, runtime-only GC
  safepoints for executor loops that are registered mutators but have no
  generated language frame, GC-rooted `Task.spawn` handle pinning across the
  pin-call safepoint, generated-side closure-env pinning across
  `Thread.spawn`/`Task.spawn` runtime handoff safepoints (with detached
  stress-GC coverage on both surfaces), `List`/`Map` locals classified as
  managed stack-map roots,
  async poll loop safepoints flushing live locals into the pinned future state,
  and rooted list receivers across allocating method arguments,
  a shared one-shot readiness reactor core for future I/O backends (registration,
  cancellation, stale-readiness suppression, reentrant wake safety, and direct
  C ABI wake-hook coverage, including mass registration/cancel/readiness cleanup),
  cross-task channel
  send/recv through `Task.spawn`, uneven-yield channel fan-in fairness across
  many `Task.spawn` workers,
  `Task.spawn.detach()` completion plus 1024 detached async workers sending over
  cloned channels and releasing endpoints until the receiver observes close,
  detached `Task.spawn` panic isolation that still lets sibling executor work complete
  (JIT + native), 256 detached async workers with managed `List` locals live
  across awaits under stress GC while channel close proves captured endpoints
  release (JIT + native), JIT executor/GC-stress coverage for
  task-held `Shared` locks, native/JIT parity for the non-stress task cases,
  explicit native/JIT parity for `Task.spawn` sync workers, async workers,
  `Shared.lock`, `JoinHandle.join()`, and cancellation joining as `Cancelled`,
  negative compiler/e2e coverage proving OS-thread `JoinHandle` has neither
  `cancel()` nor `abort()`, LSP hover/type-table coverage for `Task.spawn`
  returning `JoinHandle<R>` and task-only `join`/`cancel`/`abort` method types
  including `Cancelled`, `examples/task_spawn.otter` covering sync workers,
  async workers, `Shared.lock`, and cancelled joins, native/JIT parity for small executor fanout, 1000-task,
  4096-task, 8192-task, 16384-task, and 32768-task JIT/native executor fanout through the `spawn`
  keyword, 2048- and 4096-task unevenly-yielding `spawn` keyword tasks sending through cloned
  channel endpoints on a two-worker executor (JIT + native),
  1024 concurrent sleeping `spawn` tasks woken by the shared
  timer/reactor driver (JIT + native), 1024 concurrent sleeping `Task.spawn`
  workers joined through executor handles on the same timer/reactor path
  (JIT + native), 1000-task JIT/native `Task.spawn`
  JoinHandle fanout, 4096-task, 8192-task, 16384-task, and 32768-task `Task.spawn` fanout on a
  four-worker executor (JIT + native), four-worker mixed high-contention
  `spawn` keyword and `Task.spawn` workloads
  combining yields, `Shared.lock`, channel sends, and joins/awaits (JIT + native),
  high-fanout `Task.spawn` channel traffic with cloned sender endpoints
  including 2048-, 4096-, and 8192-task unevenly-yielding workers on a two-worker executor
  (JIT + native),
  native/JIT executor GC-stress parity, 256-task `spawn` keyword and
  `Task.spawn` GC-stress fanout with managed `List` locals live across awaits
  (JIT + native), `spawn` keyword and `Task.spawn` workers with managed `Map`
  locals live across repeated await suspensions under stress GC (JIT + native),
  128-task `spawn` keyword and `Task.spawn` fanout returning managed `List`
  result graphs under stress GC (JIT + native), ordinary HIR
  channel-endpoint local release on function return, and
  cancellation of suspended `Task.spawn` async workers releasing captured
  sender endpoints so receivers observe `ChannelClosed` (single-worker and
  512-worker fanout plus a 512-worker stress-GC cancellation storm, and repeated
  4-wave/128-total `spawn` keyword and `Task.spawn` cancellation storms under
  stress GC proving stale wakers, join waiters, roots, and channel endpoints are
  cleaned up across executor reuse, JIT + native);
  the GC STW unit tests now model production
  workers by registering/gating spawned mutators with `thread_start()` before
  safepoint polling, removing a parallel-test race that could poison the shared
  runtime test heap; `timeout()` now
  preserves already-flattened ready union payloads (for example
  `timeout(rx.recv(), ms)` matches `ChannelClosed`, not a nested union tag) with
  JIT/native coverage; stress coverage for `Map` locals held by async
  main before its first await and by executor workers across repeated
  await suspensions; poison-tolerant executor/reactor/channel/`Shared` locks so
  task queues, task registry/waker-shard locks, task poll/cancellation locks,
  join/cancel/detach state, reactor registrations,
  timer-driver waits, channel registries, channel queues, shared registries,
  shared lock-state queues, and filesystem handle registries recover from
  poisoned runtime mutexes instead of stranding high-concurrency work; the GC
  allocator's global free-list lock also recovers after poison so stress-GC
  executor workers can keep allocating/recycling managed objects, and GC
  collector-turn/world-barrier/resume-generation locks recover instead of
  suppressing future collections or stranding stopped workers; null managed
  task results no longer occupy extra-root slots during join/detach handoff,
  avoiding bogus root churn in high-volume null-returning task workloads; async
  channel receive waiters now prune dead
  executor-task wakers and coalesce duplicate registrations before send/close
  wakeups so repeated polls and cancelled receiver tasks do not amplify
  high-fanout channel traffic; and
  concrete `Clone` captures for `Thread.spawn`/`Task.spawn` snapshotted into the
  worker environment at the spawn boundary (JIT + native), while `Shared` handles
  remain shared and channel endpoints keep their ownership-transfer semantics;
  generic `T: Clone` captures and `List<T>` captures now snapshot through the same
  monomorphized clone path before `Thread.spawn` handoff / `Task.spawn` executor
  scheduling (JIT + native), while unbounded `T` remains rejected; and a repeatable CLI integration soak gate now
  reruns the 2048-, 4096-, and 8192-task `spawn` keyword channel fan-in cases, the
  2048-, 4096-, and 8192-task `Task.spawn` channel fan-in cases, repeated 32-wave
  256-task `spawn` keyword and `Task.spawn` fanout/channel/join reuse cases, and
  the one-worker panic/lock-release case multiple times to catch scheduler
  wake/fairness flakes and stale registry cleanup issues; a dedicated one-worker
  512-waiter `Task.spawn` cancellation fanout now proves many spawned join waiters
  can all register, be woken as `Cancelled`, and complete without depending on a
  second worker to rescue progress, with a 256-waiter stress-GC sibling proving
  the same path while every allocation can trigger STW collection (e2e +
  native/JIT parity); aggregate construction and managed `List` index loads now
  mark their managed temporaries as stack-map roots immediately, tightening GC
  visibility around returned tuples and endpoint handles loaded from collections;
  tuple/struct stores of channel endpoints now clone the tiny managed endpoint
  handle object without acquiring an additional endpoint count, preserving
  deterministic channel close while keeping returned-tuple `Receiver` handles
  valid under stress GC;
  `List<Sender<T>>` / `List<Receiver<T>>` now store a fresh container-owned
  endpoint handle plus a matching runtime endpoint reference, release old
  endpoint references on `set`/`clear`/`truncate`, move references out through
  `pop`/`remove`, and acquire borrowed `get` plus `list[i]` index-load results,
  closing the stale-endpoint races exposed by one-worker stress-GC timeout
  cancellation and by rebinding an indexed `List<Sender<T>>` snapshot while the
  list still owns the sender (e2e + native/JIT);
  the CLI native/JIT
  parity harness now recovers a poisoned native-build lock, caps concurrent
  helper subprocesses to avoid oversubscribing high-concurrency executor stress
  runs, and watchdogs helper subprocesses plus native build/run steps with
  process-group cleanup so hung concurrency programs fail with captured
  diagnostics instead of stranding the suite or leaving background helper
  processes alive; native linking now chooses the freshest `libruntime*.a`
  archive rather than a stale un-hashed `libruntime.a`, preventing native builds
  from silently running older runtime/GC scheduler code after runtime-only
  rebuilds; concrete `std:io` stdin/stdout/stderr async methods now lower to
  runtime futures that park executor tasks, run the target stdio operation on a
  helper, and wake through the shared cancellable reactor, with cancellation
  draining reactor waiters and managed result roots plus JIT/native stdout/stderr
  parity and stdin e2e coverage.
  Target shape:
  `Task.spawn` has the same
  surface as `Thread.spawn` (sync or async closure → `JoinHandle<R>` with
  `join`/`detach`) and the same safety model (by-value capture snapshots for
  cross-task isolation + `Shared<T>`/channels for shared state), but scheduled on
  the executor instead of a dedicated OS thread. **Complements, not replaces,**
  `Thread.spawn` (kept for dedicated/blocking work). **Folds in cancellation
  teeth** (`docs/21` §8): on the executor, `future.cancel()` stops polling the task
  and drops its state machine (running drops, releasing held `Shared` locks /
  endpoints via the per-task release path); `Task.spawn`'s
  `JoinHandle.cancel()`/`abort()` + a `Cancelled` join result
  (`Joined<R> | Panicked | Cancelled`); `timeout()`/`select!` losers ride the same
  path. Cancellation is **cooperative** (effective at the next `await`) — no
  forceful kill; a `Thread.spawn` OS-thread worker has no hard kill (cooperative
  `Shared<bool>`/channel signal only). Explicitly *not* stackful
  goroutine-style green threads (transparent blocking, no `async`/`await` coloring):
  that is a second concurrency model in tension with the explicit-async design and
  needs Go-runtime-level syscall handoff + preemption.
- **FFI tail — done:** `@CallConv` decorator; a managed `CString`/`Buffer` handle
  type with `Drop`; `@Variadic` via `libffi` (Cranelift has no portable varargs
  ABI, so variadic calls are marshalled through `ffi_prep_cif_var`/`ffi_call` —
  see the completed-work entry above).
- **Generic `Drop` types; generic-interface default methods; cross-module
  interface default methods** — currently scoped out with clear errors, not
  miscompiles.
- **Package manager advanced:** git-dependency *fetching* (sources recorded, not
  cloned), feature-gated optional-dep resolution, multi-major coexistence, publish
  metadata-sidecar.
- **LSP follow-ups:** reverse references (files that *import* the open doc),
  body-position type goto (`var x: T`, `e as T`, pattern type names).
- **GC throughput:** per-thread TLABs **done** (~2× multi-thread alloc); the full
  MMTk Immix move remains (behavior-neutral).
- **`fmt` follow-up:** token-level intra-line spacing + line wrapping (currently
  whitespace/indentation only, comment-preserving infra permitting).
- **Embedding engine (`std:engine`, `docs/26`)** — run guest Otter Fusion inside a
  sandboxed isolate (capability whitelist, host bindings, `@Bridge` copy-by-value
  ABI, per-isolate heap/GC + hard limits). Design done (`docs/26`, Phase 7);
  substrate proven by the macro JIT. Largest new piece is per-isolate runtime state.

## Historical: initial vertical-slice target (achieved long ago)
Smallest end-to-end program that exercised the full pipeline at the start:
`function main() { print_int(40 + 2) }` → JIT → prints `42`. The slice has since
been expanded to the entire language.
