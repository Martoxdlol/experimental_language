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
  non-waiting best-effort `Drop` finalizers via a finalizer queue, two disjoint heaps). Swap in
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
  `serve` (host a private registry). `remove --dry-run` validates and reports the
  dependency removal without mutating `project.toml`, while normal `remove`
  applies the same parsed manifest edit. `vendor --dry-run` resolves the graph
  and reports how many packages would be copied without deleting or copying
  `vendor/` contents. `update --dry-run --verbose` re-resolves dependencies,
  reports whether `project.lock` would change, and prints a deterministic
  normalized lockfile line diff without writing. **Live registry network round-trips are now
  exercised end-to-end**: `crates/pkg/tests/live_registry.rs` boots the server on
  an ephemeral localhost port and round-trips `HttpRegistry` connect → publish
  with metadata sidecar (auth-gated) → index → download → checksum-verify →
  search (yank-aware) → yank over real TCP (3 tests). The offline
  `LocalRegistry` still proves resolution. `otter_fusion publish` now sends
  package dependency edges and the package feature map in the sidecar, and the
  built-in registry persists them into sparse-index JSON-lines; path/git
  dependencies are rejected at publish time because registry consumers cannot
  resolve those sources. Feature-gated optional dependencies are resolved:
  active manifest/index features expand `dep:name` and `name/feature` entries,
  and optional deps stay out of the graph until enabled. Git dependencies are
  fetched through a bare mirror cache, resolved to exact commits, materialized
  under `~/.otter_fusion/git/<url-hash>/<rev>/`, and recorded in the lockfile
  with deterministic source-tree checksums. Existing locks keep branch/tag/
  default refs pinned until update-style resolution ignores the lock. Multi-major
  coexistence is implemented: resolver graph nodes are package instances keyed
  by source + semver-compatible range, so incompatible major requirements for
  the same package name can coexist, tree/why/vendor display them deterministically,
  and package-internal `pkg:name` imports resolve through the importing package's
  own dependency context instead of a flat global name.
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
      `Bytes.insert(index, byte)` inserts at any valid boundary and returns
      `false` for negative or out-of-range positions, `Bytes.pop()` removes and
      returns the last byte, `Bytes.remove_at(index)`
      removes and returns one indexed byte, `Bytes.append(other)`
      snapshots the input before appending, `Bytes.truncate(len)` shrinks the
      buffer in place, `Bytes.resize(len, fill)` shrinks or grows with a fill
      byte, `Bytes.fill(byte)` overwrites existing bytes in place,
      `Bytes.clear()` empties the buffer, and
      `Bytes.starts_with` / `Bytes.ends_with` provide raw bytewise affix
      checks while `Bytes.index_of(byte)` / `Bytes.last_index_of(byte)` /
      `Bytes.contains(byte)` and `Bytes.index_of_bytes(needle)` /
      `Bytes.last_index_of_bytes(needle)` / `Bytes.contains_bytes(needle)`
      provide raw byte search helpers. `Bytes.from_str`
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
      as async helpers over private standard-stream futures; `stdin()`,
      `stdout()`, and `stderr()` now expose byte-oriented handle methods with
      raw reads, writes, and flushes. Generic `Reader`/`Writer`/`Seeker`
      contracts are ordinary value APIs for non-waiting in-memory sources and
      adapters; concrete stdio methods and their `*_async` aliases return
      futures, with direct e2e coverage for stdin, stdout, and stderr concrete
      async paths.
      `BufReader` and `BufWriter` provide
      interface-object buffered adapters with chunked reads, `read_line`,
      line iteration, buffered writes, and explicit flushing.
      `std:fs.File` provides target-backed future-returning byte IO and seek methods;
      generic-specialized buffered wrappers and pinned `Buffer` views remain
      future provider/library work.
      `SeekFrom` and `IoErrorKind`
      provide direct equality helpers, `Eq.eq`, overloaded `==`, clone/hash/string/debug
      methods; `IoError` implements `Error`, equality, clone, hash, and debug
      rendering.
- [x] **Corrected public `std:fs.File` descriptor setup/close contract**:
      converted `File.open`, `File.create`, `File.append`, `File.open_with`,
      and `File.close` from old ordinary-result descriptor boundaries into
      helper-backed `Future<...>` methods under the corrected
      no-public-blocking rule. The Otter-authored stdlib now awaits private
      `__otter_fs_file_open_async` / `__otter_fs_file_close_async` intrinsics;
      backend/JIT lowering registers only the async runtime constructors; the
      runtime keeps crate-private encoded open/close helpers behind async
      futures; and cancellation cleanup closes a descriptor produced by a late
      cancelled open result. Positive fs/examples now await setup/close, old
      constructor/close not-awaitable guards were replaced with requires-await
      guards, LSP type/member completions advertise the new future returns, and
      docs/18/docs/20/docs/21/docs/24 were refreshed. Later corrected-contract
      work converted module-level fs helpers, target-backed `Path` queries, and
      `File` text helpers to the same helper-backed future contract.
- [x] **Corrected public `std:fs` helper/path/text contract**: converted
      module-level `read_to_string`/`write_string`/`append_string`,
      binary `read`/`write`, `read_dir`, `remove`, `rename`, `create_dir`,
      `create_dir_all`, `canonicalize`, target-backed `Path.exists`/type/
      metadata/permission/canonicalization queries, and `File` text helpers to
      helper-backed `Future<...>` methods/functions. The Otter-authored stdlib
      awaits private `__otter_fs_*_async` intrinsics; backend/JIT lowering
      registers only async runtime constructors; runtime keeps crate-private
      encoded helpers behind the async future machinery; positives/examples now
      await the operations; not-awaitable guards were replaced with
      requires-await fixtures; LSP namespace/member completions advertise exact
      future returns; and docs/18/docs/20/docs/21/docs/24 were refreshed.
- [x] **`std:time.Duration`**: added the portable value type as an
      Otter-authored `std:time` export with constructors, unit conversions,
      absolute subsecond component helpers, predicates, `abs`,
      equality/ordering/hash/clone/stringification, and overloaded `+`/`-`.
      Covered by explicit-import and runtime e2e cases.
- [x] **`std:time` monotonic/system clocks**: added mixed stdlib value types
      `Instant` and `SystemTime`, then aligned their target-backed reads with
      the corrected async contract. The Otter-authored surface covers awaitable
      `Instant.now()`, awaitable `Instant.elapsed()`, awaitable
      `SystemTime.now()`, fixed nanosecond constructors, duration arithmetic,
      Unix-epoch helpers, equality/ordering/hash/clone, stringification, and
      debug rendering. `sleep(Duration)` returns `Future<null>` and reuses the
      runtime timer/reactor path instead of exposing an ordinary-result sleep
      boundary; compile, e2e, and LSP regressions prove it is awaitable while
      still distinct from `std:async.sleep(ms)` by accepting a `Duration`.
      Calendar/timezone conversions remain future provider/runtime work.
- [x] **`std:time` calendar/timezone value contracts**: added portable
      Otter-authored `TimeZone`, `DateTime`, and `TimeError` values plus
      constructor helpers. `DateTime.new` validates calendar ranges, leap years,
      time-of-day fields, and nanosecond precision; `TimeZone` models UTC,
      fixed offsets, and named zone identifiers as ordinary values; `TimeError`
      implements `std:error.Error`. The value layer implements equality, clone,
      hash, stringification/ISO-like rendering, immutable timezone replacement,
      and diagnostic debug rendering.
      UTC/fixed-offset system-time conversion and awaitable provider-backed
      local offset conversion are implemented; timezone databases, named-zone
      conversion, and leap-second policy remain planned provider/library work.
- [x] **`std:time` ISO-like DateTime parsing**: added pure Otter Fusion
      `parse_iso8601(s): DateTime | TimeError` plus
      `DateTime.parse_iso8601`. The parser accepts the same portable shapes
      produced by `format_iso8601`: UTC `Z`, fixed `+HH:MM` / `-HH:MM` offsets,
      optional fractional seconds up to nanosecond precision, and bracketed
      named timezone identifiers. It reuses `DateTime.new` validation and
      returns `TimeError` for malformed fields. Full timezone database
      resolution, named-zone conversion, and leap-second policy remain planned
      provider/library work.
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
      Rust-backed helper-backed futures provide `Path.exists`, `Path.is_file`,
      `Path.is_dir`, `Path.file_kind`, `Path.byte_len`, `Path.permissions`, and
      `Path.metadata`, `Path.canonicalize`, module-level `canonicalize`,
      `native_separator`, `Path.from_native`, module-level `path_from_native`,
      `Path.to_native_str`, binary `read`/`write` over `std:bytes.Bytes`,
      UTF-8 text `read_to_string`, `write_string`, and `append_string`,
      snapshot-backed `read_dir` returning `DirEntries`, plus
      path-backed future-returning `File.open`, `File.create`,
      `File.append`, `File.open_with`, `File.path`, descriptor-backed
      future-returning close, byte IO, and seek operations, text read/write/append,
      concrete `File` async aliases (`read_async`, `read_to_end_async`,
      `write_async`, `write_all_async`, `flush_async`, and `seek_async`) backed
      by helper-backed, reactor-woken runtime futures,
      explicit `OpenOptions` values for read/write/append/truncate/create/
      create_new modes with provider-independent validation before runtime
      opens, non-recursive `remove`, `rename`, `create_dir`, and
      `create_dir_all`.
- [x] **`std:io`/`std:fs` ordinary/future contract clarification**:
      documented that `std:io` generic in-memory `Reader`/`Writer`/`Seeker` and
      buffered IO adapters are ordinary value-returning surfaces, while
      `File` open/create/append/open_with constructors, `File.close()`,
      module-level `std:fs` helpers, path filesystem queries, `File` text
      helpers,
      executor-integrated stdio print/stderr helpers and concrete
      stdin/stdout/stderr handle methods return futures over helper-backed
      async paths, and file descriptor byte IO/seek methods return futures
      through both ordinary concrete method names and explicit `*_async` aliases.
      Added compile-error e2e guards that
      prove `print(...)`, `println(...)`, `eprint(...)`, and `eprintln(...)`
      produce `Future<null>` values that must be awaited, and that
      `stdin().read(...)`, `stdin().read_to_end(...)`,
      `stdout().write(...)`, `stdout().write_all(...)`,
      `stdout().flush(...)`, `stderr().write(...)`,
      `stderr().write_all(...)`, and `stderr().flush(...)`
      produce futures that must be awaited, and
      `File.open(...)`, `File.create(...)`, `File.append(...)`,
      `File.open_with(...)`, `File.close(...)`, `File.read(...)`,
      `File.read_to_end(...)`, `File.write(...)`,
      `File.write_all(...)`, `File.flush(...)`, and `File.seek(...)`
      also produce futures that must be awaited, and
      `File.read_to_string(...)`, `File.write_string(...)`,
      `File.append_string(...)`, `read_to_string(...)`, `write_string(...)`,
      `append_string(...)`, `read(...)`, `write(...)`, `read_dir(...)`,
      `rename(...)`, `create_dir(...)`, `create_dir_all(...)`,
      `Path.exists(...)`, `Path.metadata(...)`, `Path.canonicalize(...)`,
      `canonicalize(...)`, and `remove(...)` also produce futures that must be
      awaited. The negative guards still prove
      that
      `await buf_reader(...).read(...)`,
      `await buf_reader(...).read_to_end(...)`,
      `await buf_reader(...).read_line()`, `await buf_reader(...).lines()`,
      `await buf_writer(...).write(...)`,
      `await buf_writer(...).write_all(...)`,
      and `await buf_writer(...).flush(...)` are rejected because those buffered
      adapter APIs are ordinary in-memory values rather than explicit async
      surfaces. LSP diagnostics cover the broader
      async-contract mistakes, and
      LSP member-completion coverage now also proves concrete stdin/stdout/
      stderr and `File` byte IO/seek methods advertise exact `Future<...>`
      returns, while buffered-adapter methods advertise
      ordinary `i64 | IoError` / `null | IoError` returns, with buffered reader
      line/iterator helpers locked as ordinary non-`Future` values, and concrete
      `*_async` aliases advertise matching `Future<...>` returns.
      LSP namespace completion for `std:io` also proves `stdin`/`stdout`/
      `stderr` and buffered-adapter constructors advertise ordinary
      non-`Future` returns, while print helpers advertise `Future<null>`.
      LSP namespace completion for `std:fs` now
      also proves module-level file/directory helpers advertise
      `Future<...>` returns while `open_options` stays an ordinary value, and
      type-namespace completion
      proves `File.open`/`create`/`append`/`open_with` advertise
      `Future<File | IoError>` returns.
      The async/concurrency worker guidance now also names future-returning
      concrete `std:io` stream handles explicitly rather than hiding them under
      generic "file I/O" wording, and the task guidance names remaining
      ordinary in-memory stdio buffered/generic surfaces directly.
- [x] **`std:fmt` contracts**: added the Otter-authored `std:fmt` module
      exporting `Display: ToStr`, `Debug`, `FmtSink`, and `FmtError`.
      `FmtError` implements `std:error.Error`, equality, clone, hash, and
      diagnostic debug rendering.
      Interpolation and `value as str` still lower through `ToStr`; this module
      gives libraries explicit user-facing and developer-facing rendering
      contracts without adding format strings. Pure stdlib values (`Bytes`,
      `Utf8Error`, `Duration`, `Path`, `Json`, and `std:net/types` identifier
      values, plus struct-shaped `std:http` values) now implement `Debug`;
      `Bytes` also implements `FmtSink` as the standard in-memory UTF-8
      formatting sink.
      renderable collections now implement `Debug` as
      `List<T: Debug>`, `Set<T: Eq + Debug>`, and
      `Map<K: Eq + Hash + ToStr, V: Debug>`. Primitive scalars and `str`
      now implement `Debug` through compiler/runtime intrinsics, including
      primitive-to-`std:fmt.Debug` interface objects and monomorphized
      `T: Debug` calls; strings and chars render quoted with diagnostic escapes.
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
      covered by stdlib e2e tests, with LSP member-completion coverage for the
      helper surface. `Set<T>` now also has value semantics:
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
      size/empty, `to_list`, `iter`, `clear`, `reversed`, and in-place
      `reverse`, and implements
      equality, clone, ordered deterministic hashing, stringification, and an
      explicit diagnostic debug implementation with the appropriate bounds.
      Covered by explicit-import and runtime e2e cases.
- [x] **`std:hash` deterministic and keyed hasher bridge**: added the mixed
      Otter/runtime-backed `std:hash` module exporting `Hasher`,
      `DefaultHasher`, `hash_value`, and `write_hash`, `combine_hash`, plus
      explicit `KeyedHasher`, `keyed_hasher(seed)`, provider-seeded
      `os_keyed_hasher()`, and `HashSeedError`.
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
      and compared. `os_keyed_hasher()` now returns a future that builds a
      `KeyedHasher` from the selected provider's OS entropy hook or resolves to
      `HashSeedError` when entropy is unavailable, keeping OS entropy reads out
      of immediate pure hasher calls. Keyed map/set construction and
      fast/cryptographic variants remain future work.
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
      while preserving prior snapshots. LSP member-completion coverage now locks
      the implemented `Headers`, `HttpRequest`, and `HttpResponse` accessor and
      builder surfaces for editor use. Canonical parsing/rendering and
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
      nested values. LSP member-completion coverage now locks the implemented
      `Json` helper surface, including shape predicates, snapshot accessors,
      builders, deep value helpers, compact rendering, and `pretty(indent)`.
      Parsers and stricter canonicalization remain package/follow-up library
      work.
- [x] **`std:net/types` value identifiers**: added the pure Otter-authored
      `std:net/types` module exporting `IpAddr`, `SocketAddr`, `Uri`, `Url`,
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
      sockets, normalize URLs, perform IDNA handling, or implement
      target-backed networking;
      those remain future `std:net`/parser work.
- [x] **`std:net` provider-backed name resolution slice**: added the mixed
      `std:net` module exporting `resolve(host): Future<List<IpAddr> | IoError>`.
      The Rust runtime hook uses the selected provider's address resolver and
      returns a compact length-framed list of textual IP addresses; the
      async Otter-authored stdlib wrapper decodes provider payloads into existing
      `std:net/types.IpAddr` values via `parse_ip_v4` / `parse_ip_v6`, or
      resolves the future to `IoError` variants for provider and
      malformed-payload errors. Added runtime unit tests, e2e JIT/native coverage for deterministic
      IPv4-literal resolution plus provider-error handling, near-empty-prelude
      import enforcement for `resolve`, and LSP analysis coverage for files that
      import the new stdlib function. TCP streams/listeners are now covered by
      the follow-up slices below; async network adapters, IDNA, and additional
      platform-specific socket options remain planned target-backed work.
- [x] **`std:net` historical TCP stream/listener slice**: added mixed
      `TcpStream` and `TcpListener` handles backed by provider TCP sockets.
      Both are deterministic `@RefCounted` runtime handles with shared clone
      semantics and explicit `close()`; the original direct provider-backed
      public shape was later superseded by corrected async-contract slices, so
      current wait-capable TCP operations return futures. The runtime handle
      registries store per-handle `Arc<Mutex<...>>` entries so private provider waits do not
      hold the whole registry lock. Added runtime loopback coverage, e2e
      JIT/native loopback coverage over port `0`, near-empty-prelude import
      enforcement for the new TCP types, and LSP analysis coverage for TCP
      imports. UDP sockets are now covered by the follow-up slice below; async
      network adapters, IDNA, and additional platform-specific socket options
      remain planned target-backed work. Corrected async-contract follow-ups
      have since superseded the original ordinary-result TCP connect and accept
      constructors: `TcpStream.connect`/`connect_timeout` and
      `TcpListener.accept`, TCP stream byte I/O, TCP stream metadata/control,
      TCP listener bind/setup/control, and UDP bind/connect/datagrams/
      metadata/control/options/multicast/close now return futures.
- [x] **`std:net` historical UDP socket slice**: added a mixed `UdpSocket`
      handle backed by provider UDP sockets. It is a deterministic
      `@RefCounted` runtime handle with shared clone semantics and explicit
      `close`; it supports `bind`, `local_addr`, `send_to(Bytes, SocketAddr)`,
      and `recv_from(Bytes): (i64, SocketAddr) | IoError`, appending each
      received datagram into the caller-provided `Bytes` buffer and returning
      the received byte count plus source address. Added runtime length-framed
      ABI coverage, e2e JIT/native loopback datagram coverage over port `0`,
      near-empty-prelude import enforcement for `UdpSocket`, and LSP analysis
      coverage for UDP imports. Corrected async-contract follow-ups have since
      superseded the public datagram operations: `send`, `recv`, `peek`,
      `send_to`, `recv_from`, and `peek_from` now return futures; the UDP
      setup/control follow-up also made bind, connect, metadata, options,
      multicast controls, and close awaitable. Async network adapters, IDNA, and
      additional platform-specific socket options remain planned target-backed
      work.
- [x] **`std:net` first historical socket-option slice**: added provider-backed option
      methods for the original socket handles. The early direct-result stream
      and datagram nodelay/TTL public shapes were superseded by corrected
      async-contract follow-ups, so every wait-capable option/control surface
      named here is now future-returning. Private runtime helpers validate TTL
      payloads and feed provider failures into the async public futures. Added runtime ABI coverage,
      TCP/UDP loopback e2e option round trips, LSP analysis coverage, and docs.
      Async network adapters, IDNA, and additional platform-specific socket
      options remain planned target-backed work.
- [x] **`std:net` historical TCP listener TTL socket-option slice**: extended
      the provider-backed `TcpListener` handle with old direct-result TTL get/set
      methods. Corrected async-contract follow-ups superseded those public
      shapes with `Future<u32 | IoError>` and `Future<null | IoError>` methods.
      Private runtime helpers validate the `u32` payload, wrap provider listener
      TTL get/set operations, and feed failures into the async public futures.
      Added runtime ABI coverage, TCP loopback e2e option coverage, LSP analysis
      and member completion coverage, and docs while keeping async network
      adapters, IDNA, and additional platform-specific socket options planned
      target-backed work.
- [x] **`std:net` historical UDP broadcast socket-option slice**: extended the
      provider-backed `UdpSocket` handle with old direct-result broadcast get/set
      methods. Corrected async-contract follow-ups superseded those public
      shapes with future-returning methods. Private runtime helpers wrap the
      provider UDP broadcast option and feed failures into the async public
      futures. Added runtime ABI coverage, UDP loopback e2e option coverage, LSP
      analysis coverage, and docs while keeping async network adapters, IDNA,
      and other platform-specific socket options planned target-backed work.
- [x] **`std:net` historical IPv4 multicast-loop socket-option slice**:
      extended `UdpSocket` with old direct-result IPv4 multicast-loop get/set
      methods. Corrected async-contract follow-ups superseded those public
      shapes with future-returning methods. Private runtime helpers wrap the
      provider IPv4 multicast loopback option and feed failures into the async
      public futures. Added runtime ABI coverage, UDP loopback e2e option
      coverage, LSP analysis coverage, and docs while keeping async network
      adapters, IDNA, multicast membership, and other platform-specific socket
      options planned.
- [x] **`std:net` historical IPv6 multicast-loop socket-option slice**:
      extended `UdpSocket` with old direct-result IPv6 multicast-loop get/set
      methods. Corrected async-contract follow-ups superseded those public
      shapes with future-returning methods. Private runtime helpers wrap the
      provider IPv6 multicast loopback option and feed failures into the async
      public futures. Added runtime ABI coverage over an IPv6 loopback UDP
      socket, source-level UDP e2e option coverage, LSP analysis coverage, and
      docs while keeping async network adapters, IDNA, multicast membership, and
      other platform-specific socket options planned.
- [x] **`std:net` historical IPv4 multicast membership slice**: extended
      `UdpSocket` with old direct-result IPv4 join/leave membership methods.
      Corrected async-contract follow-ups superseded those public shapes with
      future-returning methods. The Otter stdlib layer passes rendered portable
      `IpAddr` values into private provider helpers; the runtime validates IPv4
      group/interface payloads, performs provider membership calls, and feeds
      failures into the async public futures. Added runtime ABI coverage,
      source-level UDP e2e membership coverage, LSP analysis coverage, and docs
      while keeping async network adapters, IDNA, and other platform-specific
      socket options planned.
- [x] **`std:net` historical IPv6 multicast membership slice**: extended
      `UdpSocket` with old direct-result IPv6 join/leave membership methods.
      Corrected async-contract follow-ups superseded those public shapes with
      future-returning methods. The Otter stdlib layer passes rendered portable
      `IpAddr` group values and provider interface indexes into private provider
      helpers; the runtime validates IPv6 group payloads and `u32` interface
      indexes, performs provider membership calls, and feeds failures into the
      async public futures. Added runtime ABI coverage, source-level UDP e2e
      membership coverage, LSP analysis coverage, and docs while keeping async
      network adapters, IDNA, and other platform-specific socket options
      planned.
- [x] **`std:net` historical IPv4 multicast-TTL socket-option slice**:
      extended `UdpSocket` with old direct-result IPv4 multicast-TTL get/set
      methods. Corrected async-contract follow-ups superseded those public
      shapes with future-returning methods. Private runtime helpers wrap the
      provider IPv4 multicast TTL option, validate the `u32` payload, and feed
      failures into the async public futures. Added runtime ABI coverage, UDP
      loopback e2e option coverage, LSP analysis coverage, and docs while
      keeping async network adapters, IDNA, multicast membership, and other
      platform-specific socket options planned.
- [x] **`std:net` historical UDP socket-error readback slice**: extended
      `UdpSocket` with old direct-result socket-error readback. Corrected
      async-contract follow-ups superseded that public shape with
      `Future<IoError | null>`. Private runtime helpers wrap provider
      `SO_ERROR` readback, preserve provider-error payloads inside the future
      result, and return `null` when a healthy socket has no pending error.
      Added runtime ABI coverage, UDP loopback e2e coverage, LSP analysis
      coverage, and docs while keeping async network adapters, IDNA, and other
      platform-specific socket options planned.
- [x] **`std:net` historical TCP socket-error readback slice**: extended
      `TcpStream` and `TcpListener` with old direct-result socket-error
      readback. Corrected async-contract follow-ups superseded those public
      shapes with `Future<IoError | null>` methods. Private runtime helpers wrap
      provider `SO_ERROR` readback for both handle registries, preserve provider
      failures inside the future result, and return `null` when freshly
      connected/bound loopback handles have no pending error. Added runtime ABI
      coverage, TCP loopback e2e coverage for listener/client/server stream
      handles, LSP analysis coverage, and docs while keeping async network
      adapters, IDNA, and other platform-specific socket options planned.
- [x] **`std:net` historical nonblocking socket-option slice**: extended the
      original provider-backed handles with `set_nonblocking(bool):
      null | IoError` on `TcpStream`, `TcpListener`, and `UdpSocket`. Corrected
      async-contract follow-ups superseded those public shapes with
      `Future<null | IoError>` methods while preserving `set_nonblocking` as
      provider socket-mode configuration, not executor integration. Runtime
      helpers toggle provider nonblocking mode on the shared socket handles and
      feed failures into the async public futures. Added runtime ABI coverage, TCP and UDP
      loopback e2e option coverage, LSP analysis/member-completion coverage,
      and docs while keeping async network adapters, IDNA, and additional
      platform-specific socket options planned.
- [x] **`std:net` historical socket timeout option slice**: extended
      `TcpStream` and `UdpSocket` with old direct-result provider-backed
      read/write timeout get/set operations using `std:time.Duration | null`.
      Corrected async-contract follow-ups superseded those public shapes with
      future-returning methods. Passing `null` clears the selected provider
      timeout, finite non-negative durations are encoded as nanoseconds across
      the private runtime ABI, and invalid/unsupported values resolve through
      the async public futures. Added runtime ABI coverage including clear and
      invalid negative-duration cases, TCP/UDP loopback e2e coverage, LSP
      analysis/member-completion coverage, JIT/native parity, and docs while
      keeping async network adapters, IDNA, and additional platform-specific
      socket options planned.
- [x] **`std:net` connected UDP datagram slice**: extended the provider-backed
      `UdpSocket` handle with connected-peer setup, connected send/recv, and
      peer-address helpers alongside the existing address-explicit datagram
      operations. Corrected async-contract follow-ups superseded the early
      direct-result public shapes: connected setup, peer address, connected
      datagrams, address-explicit datagrams, setup/control, options, multicast,
      and close all now return futures where they touch provider state. The
      runtime hooks use the provider's
      connected UDP filtering/default-peer behavior, preserve raw byte payloads
      through the existing hex ABI, append received bytes into caller-owned
      `Bytes`, and feed provider failures into the async public futures.
      Added runtime ABI coverage, loopback e2e coverage, LSP analysis and
      member-completion coverage, JIT/native parity, and docs while keeping
      async network adapters, IDNA, and additional platform-specific socket
      options planned.
- [x] **`std:net` TCP timed-connect slice**: extended `TcpStream` with
      `connect_timeout(addr, timeout: Duration)`, backed by
      the provider's finite TCP connect-with-timeout operation. This historical
      slice originally returned `TcpStream | IoError`; the corrected async-contract
      slice superseded the public shape to `Future<TcpStream | IoError>`. The runtime hook
      validates the public duration as non-negative nanoseconds before crossing
      into the provider, resolves invalid or failed connects through the async
      public future, and registers successful streams in the same deterministic
      handle registry as `connect`. Added runtime ABI coverage including a
      negative-duration error, loopback e2e coverage, LSP analysis coverage,
      JIT/native parity, and docs while keeping async network adapters, IDNA,
      and additional platform-specific socket options planned.
- [x] **`std:net` TCP stream peek slice**: extended `TcpStream` with stream
      peek, backed by the provider's non-consuming stream peek operation.
      Corrected async-contract follow-up superseded the early direct-result
      public shape: live `TcpStream.peek(buf)` returns `Future<i64 | IoError>`.
      The stdlib method follows the same caller-owned `Bytes` buffer convention
      as stream reads: it appends the peeked bytes to the supplied buffer and
      returns the byte count through the future result, while the runtime hook
      preserves arbitrary byte payloads through the existing hex ABI and leaves
      the stream readable afterward. Added runtime coverage for
      peek-before-read and invalid negative lengths, TCP loopback e2e coverage,
      LSP analysis/member-completion coverage, JIT/native parity, and docs while
      keeping async network adapters, IDNA, and additional platform-specific
      socket options planned.
- [x] **`std:net` UDP datagram peek slice**: extended `UdpSocket` with
      `peek(buf: Bytes)` for connected sockets and `peek_from(buf: Bytes)` for
      address-explicit sockets, both backed by provider non-consuming datagram
      peek operations. The corrected async-contract UDP datagram slice later
      superseded the public return shapes to `Future<i64 | IoError>` and
      `Future<(i64, SocketAddr) | IoError>`.
      The stdlib methods use the same caller-owned `Bytes` convention as
      `recv`/`recv_from`, appending peeked payload bytes while leaving the
      datagram available for a later receive. Runtime hooks preserve arbitrary
      byte payloads through the existing hex/length-framed ABI and resolve
      invalid negative lengths through the async public futures. Added runtime
      peek-before-receive coverage, UDP loopback e2e coverage, LSP
      analysis/member-completion coverage, and docs while keeping async network
      adapters, IDNA, and additional platform-specific socket options planned.
- [x] **`std:net` historical value/future contract clarification**: documented the
      then-current provider-backed shape for `resolve`, `TcpStream`,
      `TcpListener`, and `UdpSocket`, then superseded it under the corrected
      async contract. Current wait-capable DNS, TCP, and UDP public operations
      return explicit futures over private provider helpers. `set_nonblocking`
      is now documented as provider socket-mode configuration, not executor
      integration. Added a compile-error e2e guard proving
      `TcpStream.connect_timeout(...)` returned `TcpStream | IoError` and could not
      be `await`ed at the time; the later corrected async-contract slices
      superseded DNS, TCP connect constructors, listener accept, TCP stream
      byte I/O/control, TCP listener setup/control, and all wait-capable UDP
      operations with requires-await guards.
      The same not-awaitable guard coverage was expanded across
      TCP listener `bind`, TCP listener
      `accept`, TCP stream `close`, TCP stream
      `read`/`read_to_end`/`write`/`write_all`/`flush`/`peek`, and UDP
      `bind`/`connect`/`send`/`recv`/`peek` plus address-aware
      `send_to`/`recv_from`/`peek_from`. The concurrency and async chapters now
      no longer call out TCP stream byte I/O or UDP setup/control as public
      wait boundaries; those methods are now future-returning helper-backed
      operations.
      TCP stream address accessors, error readback, nodelay/nonblocking,
      read/write timeout, TTL option methods, and close were later superseded
      with future-returning surfaces and requires-await guards. TCP listener address/error/nonblocking/TTL/close
      controls and UDP address/error/nonblocking/timeout/TTL/broadcast/
      multicast/close controls now have the same direct guard coverage.
      Added an LSP open-document diagnostic regression
      source for the same not-awaitable boundary classes (`std:net`,
      `std:process`, `std:fs`, `std:io`, `std:sync`, `std:thread`, and
      `std:task`); verified with
      `cargo test -p lsp diagnostics_report_ordinary_boundaries_are_not_awaitable -- --nocapture`
      under a PTY. Async network adapters and the remaining TCP/UDP
      wait-capable handle corrections were completed in later slices under the
      corrected no-public-blocking rule.
      This is a contract-honesty slice, not the async-network implementation.
- [x] **`std:net` first async TCP stream adapter slice**: added explicit
      `AsyncTcpStream` before the corrected async-contract work changed the
      plain `TcpStream` public operation shapes.
      The adapter exposes `connect(addr): Future<AsyncTcpStream | IoError>`,
      `from_stream`, `into_stream`, `read_async`, `write_async`, `peek_async`,
      address accessors, close, clone, stringification, and debug rendering.
      Runtime private futures hand TCP connect/read/write/peek to helper
      threads and wake through the existing reactor registration path, so an
      executor poll does not park on provider socket waits; cancellation removes
      the reactor registration and drops late helper results. Added stdlib e2e
      loopback coverage, import-gating coverage for the new type, LSP analysis
      and member-completion source coverage, docs/18/docs/20/docs/21/docs/24,
      and compile-only Rust checks. Async listener/UDP adapters, shared
      async byte-stream protocols, and deeper readiness-native provider
      integration remained planned at that point.
      Follow-up not-awaitable coverage now also proves `from_stream(...)` and
      `into_stream()` are ordinary conversion helpers, not async TCP
      operations. Later corrected-contract work superseded the original
      adapter metadata/control shape too: `AsyncTcpStream.peer_addr()`,
      `local_addr()`, and `close()` now return futures and must be awaited.
- [x] **`std:net` async TCP listener accept adapter slice**: added explicit
      `AsyncTcpListener` before the corrected no-public-blocking rule was
      applied to the plain listener method. The adapter exposes `bind`,
      `from_listener`, `into_listener`, `accept_async`, listener
      metadata/options, close, clone, stringification, and debug rendering.
      `accept_async()` returns
      `Future<(AsyncTcpStream, SocketAddr) | IoError>` and uses the same
      cancellable helper-thread plus reactor-wake path as the TCP stream async
      adapter, so executor polls do not park on provider `accept` waits. Added
      the `examples/async_tcp_listener.otter` direct example, loopback
      JIT/native e2e coverage, e2e example coverage that awaits every
      `TcpStream` client byte-I/O future while isolating the loopback client
      body in an async `Thread.spawn` worker,
      import-gating coverage for the new type, LSP analysis/member-completion
      source coverage, docs/18/docs/20/docs/21/docs/24, and targeted
      compile/run verification. At that point, remaining async network
      work stayed active: shared async byte-stream protocols (completed in the
      shared protocol slice below),
      timeout-specific adapter conveniences, deeper readiness-native provider
      integration, and stress/performance coverage.
      Follow-up not-awaitable coverage proved the original adapter
      setup/control shape at the time. Corrected no-public-blocking work later
      superseded that shape: `AsyncTcpListener.bind(...)`, listener metadata,
      error readback, TTL, and close now return futures; `from_listener(...)`
      and `into_listener()` remain ordinary conversion helpers.
      Follow-up corrected-contract work superseded the original "do not change
      `TcpListener.accept()`" stance: plain `TcpListener.accept()` now returns
      `Future<(TcpStream, SocketAddr) | IoError>`, and `accept_async()` remains
      a convenience wrapper that adapts the accepted stream to
      `AsyncTcpStream`.
- [x] **Corrected public TCP listener accept contract**: changed
      `TcpListener.accept()` from the old ordinary-result wait shape into
      `Future<(TcpStream, SocketAddr) | IoError>` under the no-public-blocking
      rule. The stdlib awaits the existing private async accept intrinsic,
      `AsyncTcpListener.accept_async()` now wraps that public future and adapts
      the stream to `AsyncTcpStream`, the backend no longer registers the
      retired direct accept symbol, and the runtime encoded accept helper is
      crate-private async-runtime machinery. Replaced the old accept
      not-awaitable guard with a requires-await fixture, updated loopback TCP
      server helpers to await accept inside async `Thread.spawn` workers, and
      refreshed LSP/docs/goals. Remaining TCP stream instance I/O/control and
      UDP socket operations were completed by later corrected-contract slices.
- [x] **Corrected public TCP stream byte-I/O contract**: changed
      `TcpStream.read`, `read_to_end`, `write`, `write_all`, `flush`, and
      `peek` from old ordinary-result wait shapes into `Future`-returning methods
      under the no-public-blocking rule. The stdlib awaits private async
      stream intrinsics over helper-backed runtime operations; the backend no
      longer registers the retired direct byte-I/O symbols; and runtime encoded
      helpers are crate-private async-runtime machinery. Replaced the old
      byte-I/O not-awaitable guards with requires-await fixtures, updated
      loopback TCP examples and stress/timeout regressions to await stream I/O,
      and refreshed LSP/docs/goals. Remaining TCP metadata/control and UDP
      socket operations were completed by later corrected-contract slices.
- [x] **Corrected public UDP datagram contract**: changed plain
      `UdpSocket.send`, `recv`, `peek`, `send_to`, `recv_from`, and
      `peek_from` from old ordinary-result wait shapes into `Future`-returning
      methods under the no-public-blocking rule. The stdlib now awaits the
      existing private async UDP intrinsics over helper-backed runtime
      operations; the backend no longer registers the retired direct datagram
      symbols; and runtime encoded UDP datagram helpers are crate-private
      async-runtime machinery. Replaced the old UDP datagram not-awaitable
      guards with requires-await fixtures, updated the loopback UDP e2e case to
      await plain datagram methods, and refreshed LSP/docs/goals. Remaining UDP
      bind, connect, metadata/control, timeout/socket-option, multicast, and
      close methods were completed by the follow-up slice below.
- [x] **Corrected public TCP listener setup/control contract**: changed
      `TcpListener.bind`, `local_addr`, `take_error`, `set_nonblocking`, `ttl`,
      `set_ttl`, and `close` from old ordinary-result wait shapes into
      `Future`-returning methods under the no-public-blocking rule, and made
      the matching `AsyncTcpListener.bind`, metadata, TTL, and close helpers
      awaitable too. The stdlib awaits private async listener intrinsics over
      helper-backed runtime operations; the backend no longer registers the
      retired direct listener bind/control symbols; and runtime encoded listener
      helpers are crate-private async-runtime machinery. Replaced old listener
      not-awaitable guards with requires-await fixtures, updated TCP listener
      examples and stress/timeout fixtures to await listener setup/control,
      added cancellation cleanup coverage for late listener-bind results, and
      refreshed LSP/docs/goals. TCP stream metadata/control was completed by
      the follow-up slice below; the later UDP setup/control slice completed
      the UDP methods too.
- [x] **Corrected public TCP stream metadata/control contract**: changed
      `TcpStream.peer_addr`, `local_addr`, `take_error`, `nodelay`,
      `set_nodelay`, `set_nonblocking`, read/write timeout get/set, `ttl`,
      `set_ttl`, and `close` from old ordinary-result wait shapes into
      `Future`-returning methods under the no-public-blocking rule, and made
      the matching `AsyncTcpStream.peer_addr`, `local_addr`, and `close`
      wrappers awaitable too. The stdlib awaits private async stream-control
      intrinsics over helper-backed runtime operations; the backend no longer
      registers the retired direct-result stream metadata/control symbols; and runtime
      encoded stream-control helpers are crate-private async-runtime machinery.
      Replaced old TCP stream and async-stream metadata/control not-awaitable
      guards with requires-await fixtures, updated TCP examples and
      stress/timeout fixtures to await stream control, refreshed LSP completion
      and analysis expectations, and updated docs/goals. Remaining UDP
      setup/control methods were completed by the follow-up slice below.
- [x] **Corrected public UDP setup/control contract**: changed
      `UdpSocket.bind`, `local_addr`, `connect`, `peer_addr`, `take_error`,
      `set_nonblocking`, read/write timeout get/set, TTL get/set,
      broadcast/multicast options and membership, and `close` from old
      ordinary-result wait shapes into `Future`-returning methods under the
      no-public-blocking rule, and made matching `AsyncUdpSocket` bind,
      metadata/control/options/multicast/close wrappers awaitable while keeping
      `from_socket`, `into_socket`, clone/stringification/debug as ordinary
      value helpers. The stdlib awaits private async UDP setup/control
      intrinsics over helper-backed runtime operations; the backend no longer
      registers the retired UDP setup/control direct-result symbols; and runtime encoded
      UDP helpers are crate-private async-runtime machinery. Added
      cancellation cleanup coverage for late UDP bind results, replaced old
      UDP setup/control not-awaitable guards with requires-await fixtures,
      updated UDP examples/stress/timeout fixtures to await setup/control,
      refreshed LSP completion/type-namespace/analysis expectations, and
      updated docs/goals.
- [x] **Async UDP example wording correction**: refreshed
      `examples/async_udp.otter`, its mirrored e2e fixture, and goals memory so
      the datagram-shaped `AsyncUdpSocket` example no longer describes
      bind/address lookup/close as ordinary setup/control boundaries. The live
      wording now matches the corrected contract: bind, address lookup,
      datagram send/receive, and close are awaited futures; `from_socket` and
      `into_socket` remain ordinary conversion helpers.
- [x] **`std:net` async UDP datagram adapter slice**: added explicit
      `AsyncUdpSocket` as a datagram-shaped wrapper over the same UDP handle
      model. A later corrected-contract slice also made base `UdpSocket`
      datagram operations future-returning. The adapter exposes `bind`,
      `from_socket`, `into_socket`,
      connected `send_async`/`recv_async`/`peek_async`, address-aware
      `send_to_async`/`recv_from_async`/`peek_from_async`, socket metadata and
      options, close, clone, stringification, and debug rendering. Runtime
      private futures reuse the crate-private provider encoders through the same
      cancellable helper-thread plus reactor-wake path used by the TCP async
      adapters, so executor polls do not park on provider UDP waits and late
      results are discarded after cancellation. Added
      `examples/async_udp.otter` plus a test-gated copy under
      `tests/cases/examples/`, loopback JIT/native e2e coverage for connected
      and address-aware datagrams including peek-before-recv,
      import-gating coverage for the new type, LSP analysis/member-completion
      source coverage, docs/18/docs/20/docs/21/docs/24, and targeted
      compile/run verification. At that point, remaining async network work
      stayed active: shared async byte-stream protocols (completed in the
      shared protocol slice below), timeout-specific adapter
      conveniences, deeper readiness-native provider integration, and stress/
      performance coverage.
      Follow-up not-awaitable coverage proved the original adapter
      setup/control shape at the time. Corrected no-public-blocking work later
      superseded that shape: `AsyncUdpSocket.bind(...)`, adapter metadata,
      peer setup, socket options, multicast membership controls, and close now
      return futures; `from_socket(...)` and `into_socket()` remain ordinary
      conversion helpers.
- [x] **Async network timeout/cancellation cleanup regression slice**: hardened
      helper-backed async network cancellation so late cancelled TCP connect,
      connect_timeout, and accept results release any provider stream handle
      they registered before the runtime discards the result. Cancellation
      still removes the reactor registration and result root for every
      helper-backed I/O future;
      UDP late receives may consume the target datagram at the provider layer,
      so cleanup is defined as unregistering the waiter and discarding the late
      result while preserving subsequent socket usability; docs/24 now mirrors
      that explicit UDP datagram-consumption caveat. Added runtime units proving
      late cancelled TCP connect/connect_timeout/accept release registered
      stream handles, plus JIT/native e2e regressions for timed-out `AsyncUdpSocket.recv_from_async`
      and timed-out `AsyncTcpListener.accept_async` followed by a successful
      fresh operation. Updated docs/21/docs/24, ROADMAP, and goals. At that
      point, remaining async network work stayed active: shared async
      byte-stream protocols (completed in the shared protocol slice below),
      timeout-specific convenience APIs, deeper readiness-native provider
      integration, and broader stress/performance coverage.
- [x] **Async TCP stream read-timeout cleanup slice**: added
      `std_net_async_tcp_read_timeout_cleanup.otter`, proving that timing out
      helper-backed `AsyncTcpStream.read_async` removes the reactor waiter,
      discards the late read result, and leaves the stream usable for a later
      read. The regression uses a separate TCP control connection so the
      cancelled data-stream read can hold its provider lock without blocking the
      test's coordination path; it documents that the stale provider bytes may
      be consumed by the cancelled helper. Verified JIT and native parity.
      Broader async network stress/performance coverage and deeper
      readiness-native provider integration remain active follow-up work.
- [x] **Async network GC-stress rooting slice**: added bounded GC-stress
      coverage for helper-backed async networking with
      `std_net_async_gc_stress.otter`. The case keeps managed `List<str>` and
      `Bytes` state live across an `AsyncUdpSocket.recv_from_async` await while
      `OTTER_FUSION_GC=stress` collects aggressively, then verifies JIT and
      native parity. This intentionally stays small and serial to avoid
      oversubscribing network/helper threads under stress mode. Broader async
      network stress/performance coverage remains active follow-up work.
- [x] **Async TCP stream GC-stress rooting slice**: added
      `std_net_async_tcp_gc_stress.otter`, a serial stress case that keeps
      managed `List<str>` and caller-owned `Bytes` buffers live across
      helper-backed `AsyncTcpStream.write_async` and `read_async` awaits while
      an isolated loopback server body awaits the plain `TcpStream` byte-I/O
      futures.
      Verified JIT and native parity under `OTTER_FUSION_GC=stress`; this
      complements the UDP datagram stress case without broad helper-thread
      oversubscription. Broader async network stress/performance coverage
      remains active follow-up work.
- [x] **`std:net` async TCP connect timeout convenience slice**: added
      `AsyncTcpStream.connect_timeout(addr, timeout: Duration):
      Future<AsyncTcpStream | IoError>` as an explicit async timed-connect
      adapter. At the time this deliberately preserved
      `TcpStream.connect_timeout()`'s ordinary-result public shape; the corrected
      async-contract slice later superseded that public shape with
      `Future<TcpStream | IoError>`. Runtime private futures reuse the provider finite TCP
      connect-with-timeout operation through the same helper-thread plus
      reactor-wake path as `AsyncTcpStream.connect`; invalid negative durations
      resolve through the future result, and cancellation uses the same late
      TCP stream-handle cleanup path as plain async connect, with focused
      runtime coverage for both connect forms. Added loopback JIT/native e2e
      coverage including an invalid-duration negative case, LSP analysis/member
      completion coverage, `examples/async_tcp_timeout.otter` plus a
      test-gated copy under `tests/cases/examples/`, docs/18/docs/21/docs/24
      updates, and goals bookkeeping. At that point, remaining async network
      work included shared async byte-stream protocols (completed in the shared
      protocol slice below), additional timeout convenience decisions,
      deeper readiness-native provider integration, and broader stress/
      performance coverage.
- [x] **Shared async byte-stream protocol slice**: added
      `std:io.AsyncReader` and `std:io.AsyncWriter` as Otter Fusion's generic
      async byte-stream contracts instead of copying Rust's `AsyncRead` /
      `AsyncWrite` naming. `Stdin`, `Stdout`, `Stderr`, descriptor-backed
      `std:fs.File`, and `std:net.AsyncTcpStream` implement the matching
      protocols while keeping their existing concrete async methods. UDP remains
      deliberately datagram-shaped: `AsyncUdpSocket` keeps `send_async`,
      `recv_async`, `peek_async`, and address-aware variants rather than being
      forced into a stream protocol that would hide datagram boundaries. Added
      `examples/async_io_contracts.otter` plus a test-gated copy under
      `tests/cases/examples/`, generic async file/TCP e2e coverage, ordinary-reader
      and ordinary-writer rejection coverage, widened near-empty-prelude import
      gating, docs/18/docs/20/docs/21/docs/24 updates, and goals bookkeeping.
      Remaining
      async-network work: deeper readiness-native provider integration,
      additional timeout convenience decisions, and broader stress/performance
      coverage.
- [x] **Helper-backed async I/O native-state ownership fix**: tightened the
      runtime future helper path so helper threads no longer perform a
      thread-wide native-state enter/leave around completion. The private
      provider helpers own `gc::native_wait(...)` around the actual host wait;
      helper completion now runs after the wait, encodes the Otter result,
      roots it, and wakes the reactor without leaving a native state it did not
      enter. The shared timer driver now registers as a runtime mutator before
      invoking reactor wakers and parks its idle/deadline condvar waits in
      runtime-native/no-root state. Added focused runtime regression coverage and
      verified the serial async-runtime unit slice under a PTY (`cargo test -p runtime
      async_rt::tests -- --nocapture --test-threads=1`), plus GC-stress
      JIT/native async I/O and async network smoke runs. Refreshed docs/21 and
      docs/24. Remaining async-network work stays active: deeper readiness-native
      provider integration, additional timeout convenience decisions, and
      broader stress/performance coverage.
- [x] **LSP async/blocking signature detail slice**: completion details for
      embedded std/core definitions now render syntactic type structure
      recursively instead of collapsing non-document types to labels such as
      `Future`, `tuple`, or `union`. Focused LSP member-completion coverage now
      proves `std:net.AsyncTcpStream`, `AsyncTcpListener`, and `AsyncUdpSocket`
      advertise their exact `Future<... | IoError>` return shapes, including
      all six async UDP datagram methods (`send_async`, `recv_async`,
      `peek_async`, `send_to_async`, `recv_from_async`, and `peek_from_async`),
      plus `AsyncTcpStream` adapter/metadata/control completions. Later
      corrected-contract work superseded the original `AsyncTcpStream` and
      `AsyncTcpListener` metadata/control completion expectations: live
      completions now advertise futures for TCP stream/listener setup/control
      and close, while `AsyncTcpStream.from_stream`/`into_stream` and
      listener conversion helpers remain ordinary values. `TcpStream`
      completion coverage now proves metadata/error helpers,
      nodelay/nonblocking/timeout/TTL controls, close, and byte-I/O methods all
      advertise `Future<...>` returns. `TcpListener`
      completion coverage originally proved accept advertised a `Future<...>`
      return while bind, metadata/error, socket-option, and close were ordinary;
      later corrected-contract work superseded that shape so live `TcpListener`
      bind/setup/control/close completions advertise futures too. Later UDP
      corrected-contract work likewise superseded the original
      `AsyncUdpSocket` and base `UdpSocket` setup/control completion
      expectations: live UDP bind/connect/metadata/error/timeout/socket-option/
      multicast/close completions now advertise `Future<...>` returns, while
      `AsyncUdpSocket.from_socket` and `into_socket` remain ordinary
      conversion helpers. Namespace
      completion coverage now proves top-level `std:net.resolve(host)` advertises
      its `Future<List<IpAddr> | IoError>` return and must be awaited before DNS
      results are used.
      Type-namespace completion coverage now proves async
      network conversion/setup helpers (`AsyncTcpStream.from_stream`,
      `AsyncTcpListener.from_listener`, and `AsyncUdpSocket.from_socket`)
      advertise immediate non-`Future` returns, while corrected-contract work now
      makes `AsyncTcpListener.bind` and `AsyncUdpSocket.bind` awaitable, while
      `TcpStream.read`/`read_to_end`/`write`/`write_all`/`flush`/`peek`
      completions are visibly awaitable. Static type-namespace completion now proves
      `TcpStream.connect`/`connect_timeout` return `Future<TcpStream | IoError>`
      while `AsyncTcpStream.connect`/`connect_timeout` return
      `Future<AsyncTcpStream | IoError>`. LSP TCP import-analysis coverage now
      also awaits wait-capable TCP futures in its accepted sample program
      (`TcpListener.accept`, `AsyncTcpListener.accept_async`,
      `TcpStream.peek`, `AsyncTcpStream` read/write/peek/connect helpers, and
      generic `AsyncReader`/`AsyncWriter` calls), leaving only pure conversion
      helpers unawaited. Verified with
      `cargo test -p lsp
      type_namespace_completion_marks_tcp_connectors_awaitable -- --nocapture`,
      `cargo test -p lsp member_completion_lists_std_net -- --nocapture` and
      `cargo check -p lsp --tests` under a PTY.
- [x] **LSP stdio print-helper async correction slice**: `std:io`
      print helpers are no longer compiler builtins. Hover/signature-help now
      sees them as ordinary stdlib functions returning `Future<null>`, and
      focused LSP unit coverage proves `print`/`println`/`eprint`/`eprintln`
      are absent from the builtin signature table.
- [x] **LSP runtime-handle intrinsic completion slice**: member completion now
      mirrors checker-recognized intrinsic methods on runtime handles that are
      not declared as ordinary stdlib `extend` methods. `std:thread.JoinHandle`
      completions show `join(): Future<Joined<R> | Panicked>` and
      non-awaitable `detach(): null`; `std:task.JoinHandle` completions show
      `join(): Future<Joined<R> | Panicked | Cancelled>` plus non-awaitable
      `detach`/`cancel`/`abort`; channel endpoint completions show
      `Sender.send(value): null | ChannelClosed`, async
      `Receiver.recv(): Future<T | ChannelClosed>`, and immediate non-blocking
      `Receiver.try_recv(): T | null`. Compile-error guards also prove
      `await Sender.send(...)` is rejected because send is an immediate
      non-blocking enqueue, not an explicit async receive surface. Verified with
      `cargo test -p lsp member_completion -- --nocapture` and
      `cargo check -p lsp --tests` under a PTY.
- [x] **`std:net` async-adapter documentation drift audit**: refreshed the
      authoritative module/concurrency/stdlib docs so they no longer describe
      async network adapters as planned or partially implemented. `docs/17` now
      lists `AsyncTcpStream`, `AsyncTcpListener`, and `AsyncUdpSocket` in the
      `std:net` module row, names wait-capable DNS/TCP/UDP operations as
      future-returning/reactor-backed surfaces, and scopes ordinary socket
      helpers to adapter conversions or pure address/value accessors; `docs/20`
      and `docs/24` now point to the explicit async adapter set instead of vague
      "where implemented" wording. Verified with a stale-text sweep for
      `planned async adapters`, `async network adapters remain planned`,
      `future socket operations`, and `where implemented` across the active
      async/blocking stdlib docs.
- [x] **`std:rand` deterministic + OS-backed RNG slice**: added the mixed
      `std:rand` module exporting `Rng`, `RandomError`, `SeededRng`,
      `OsRng`, `ThreadRng`, `random_error`, `os_rng`, `os_bytes`,
      `thread_rng`,
      `gen_range_i64`, `gen_range_u64`, `gen_f64`, `gen_range_f64`,
      `gen_triangular_f64`, `gen_bates_f64`, `gen_irwin_hall_f64`,
      `gen_min_f64`, `gen_max_f64`, `gen_midrange_f64`, `gen_median_f64`,
      `gen_bool`, `gen_index`, `fill_bytes_n`, `gen_bytes`,
      `choose_index`, `choose`, `weighted_index`, `choose_weighted`,
      `sample_indices`, `sample`, `shuffle`, and `shuffled`.
      `SeededRng` is deterministic and
      reproducible, with `fill_bytes` appending generated bytes into
      `std:bytes.Bytes`; it is suitable for tests/simulations but not
      cryptographic use. `OsRng` is provider-backed through the runtime entropy
      hook and exposes fallible `try_next_u32`, `try_next_u64`,
      `try_fill_bytes`, and `try_fill_bytes_n` methods as explicit futures
      resolving to values or `RandomError`; `os_bytes(count)` returns a future
      resolving to an owned provider-entropy `Bytes` buffer or `RandomError`
      (empty for non-positive counts). `OsRng` intentionally does not implement
      the ordinary `Rng` interface because target entropy can wait on host
      state. `ThreadRng` is a per-value generator seeded by awaiting `thread_rng()`.
      Range helpers are half-open
      and return `low` for empty/reversed ranges, including deterministic
      `gen_f64` and `gen_range_f64` helpers built from the next 53 random bits;
      `gen_triangular_f64` samples a symmetric triangular distribution by
      averaging two uniform f64 draws and scaling over the same low/high range;
      `gen_bates_f64` samples a Bates distribution by averaging a caller-chosen
      count of uniform f64 draws before scaling over the same low/high range,
      returning `low` for non-positive draw counts or empty/reversed ranges;
      `gen_irwin_hall_f64` samples the Irwin-Hall family by summing a
      caller-chosen count of uniform f64 draws over the requested half-open
      low/high range, returning `0.0` for non-positive draw counts or
      empty/reversed ranges;
      `gen_min_f64` and `gen_max_f64` sample the minimum and maximum order
      statistic from a caller-chosen count of uniform f64 draws over the
      requested half-open low/high range, returning `low` for non-positive draw
      counts or empty/reversed ranges;
      `gen_midrange_f64` samples the midpoint between the observed minimum and
      maximum from the same caller-chosen draw set and range, and
      `gen_median_f64` samples the median order statistic from that draw set,
      averaging the two middle observed values for even draw counts; both
      return `low` for non-positive draw counts or empty/reversed ranges;
      `gen_bool` samples a numerator/denominator ratio with clamped
      always-false/always-true edge cases, `gen_binomial` counts successes
      across repeated ratio trials with the same edge-case clamping,
      `fill_bytes_n` / `gen_bytes`
      generate deterministic exact-length byte buffers from any `Rng`,
      `gen_index` and
      `choose_index` sample uniform zero-based indexes and return `null` for
      non-positive lengths or empty lists, `choose` returns `null` for empty
      lists, `weighted_index` / `choose_weighted` sample unsigned integer
      weights and return `null` for empty, all-zero, overflowed, or length-
      mismatched distributions, `sample_indices` returns distinct zero-based
      indexes without replacement, `sample` returns a new list of draws without
      replacement while leaving the source list unchanged, and
      `shuffle` mutates the provided list in place while `shuffled` returns a
      shuffled copy.
      `SeededRng` implements state equality,
      clone, hash, stringification, and debug rendering so PRNG streams can be
      snapshotted and compared. `RandomError` implements `std:error.Error`,
      equality, clone, hash, stringification, and debug rendering.
      Cryptographic-strength API guarantees and additional statistical
      continuous distributions remain planned work.
- [x] **`std:rand` symmetric triangular f64 distribution helper**: added pure
      Otter Fusion `gen_triangular_f64(rng, low, high): f64`, implemented by
      averaging two `gen_f64` uniform draws and scaling that average across the
      half-open low/high range, returning `low` for empty/reversed ranges.
      Exported it through the stdlib catalog, added deterministic e2e coverage
      for midpoint, two-draw, and edge cases, expanded near-empty-prelude
      import-gating coverage, extended LSP rand analysis coverage, and updated
      docs/18, docs/24, ROADMAP, and goals while keeping cryptographic-strength
      guarantees and additional continuous distributions planned.
- [x] **`std:rand` Bates f64 distribution helper**: added pure Otter Fusion
      `gen_bates_f64(rng, draws, low, high): f64`, implemented by averaging
      `draws` independent `gen_f64` uniform draws and scaling that average
      across the half-open low/high range, returning `low` for non-positive
      draw counts or empty/reversed ranges. Exported it through the stdlib
      catalog, added deterministic e2e coverage for fixed, sequence, and edge
      cases, expanded near-empty-prelude import-gating coverage, extended LSP
      rand analysis coverage, and updated docs/18, docs/24, ROADMAP, and goals
      while keeping cryptographic-strength guarantees and additional continuous
      distributions planned.
- [x] **`std:rand` Irwin-Hall f64 distribution helper**: added pure Otter Fusion
      `gen_irwin_hall_f64(rng, draws, low, high): f64`, implemented by summing
      `draws` independent `gen_range_f64` samples over the requested half-open
      low/high range, returning `0.0` for non-positive draw counts or
      empty/reversed ranges. Exported it through the stdlib catalog, added
      deterministic e2e coverage for fixed, sequence, and edge cases, expanded
      near-empty-prelude import-gating coverage, extended LSP rand analysis
      coverage, and updated docs/18, docs/24, ROADMAP, and goals while keeping
      cryptographic-strength guarantees and additional continuous distributions
      planned.
- [x] **`std:rand` min/max order-statistic f64 helpers**: added pure Otter
      Fusion `gen_min_f64(rng, draws, low, high): f64` and
      `gen_max_f64(rng, draws, low, high): f64`, implemented by taking the
      minimum or maximum of `draws` independent `gen_range_f64` samples over the
      requested half-open low/high range. Both helpers return `low` for
      non-positive draw counts or empty/reversed ranges. Exported them through
      the stdlib catalog, added deterministic e2e coverage for fixed,
      sequence, and edge cases, expanded near-empty-prelude import-gating
      coverage, extended LSP rand analysis coverage, and updated docs/18,
      docs/24, ROADMAP, and goals while keeping cryptographic-strength
      guarantees and additional continuous distributions planned.
- [x] **`std:rand` midrange f64 helper**: added pure Otter Fusion
      `gen_midrange_f64(rng, draws, low, high): f64`, implemented by taking the
      midpoint between the observed minimum and maximum of `draws` independent
      `gen_range_f64` samples over the requested half-open low/high range. It
      returns `low` for non-positive draw counts or empty/reversed ranges.
      Exported it through the stdlib catalog, added deterministic e2e coverage
      for fixed, sequence, and edge cases, expanded near-empty-prelude
      import-gating coverage, extended LSP rand analysis coverage, and updated
      docs/18, docs/24, ROADMAP, and goals while keeping cryptographic-strength
      guarantees and additional continuous distributions planned.
- [x] **`std:rand` median f64 helper**: added pure Otter Fusion
      `gen_median_f64(rng, draws, low, high): f64`, implemented by sampling a
      caller-chosen count of `gen_range_f64` values over the requested half-open
      low/high range, insertion-sorting the observed values, and returning the
      middle value (or the average of the two middle values for even draw
      counts). It returns `low` for non-positive draw counts or empty/reversed
      ranges. Exported it through the stdlib catalog, added deterministic e2e
      coverage for fixed, odd/even sequence, and edge cases, expanded
      near-empty-prelude import-gating coverage, extended LSP rand analysis
      coverage, and updated docs/18, docs/24, ROADMAP, and goals while keeping
      cryptographic-strength guarantees and additional continuous distributions
      planned.
- [x] **`std:log` portable value/default line slice**: added the pure
      Otter-authored `std:log` module exporting `Level`, prefixed concrete level
      variants (`LogTrace`, `LogDebug`, `Info`, `Warn`, `LogError`), level
      constructor helpers, `Record`, the portable `Logger` interface,
      `LoggerAlreadySet`, `log_record`, and default line helpers (`trace`,
      `debug`, `info`, `warn`, `error`) plus structured helpers (`trace_with`,
      `debug_with`, `info_with`,
      `warn_with`, `error_with`). The implemented slice gives levels, records,
      logger interface dispatch, and the marker type
      equality/clone/hash/stringification/debug semantics
      and returns `Future<null>` from default line helpers that print compact
      lines through async `std:io.println`. `Level.rank()` and
      `Level.is_at_least(min)` provide portable severity ordering for filtering.
      `Record` also provides
      value-layer accessors and immutable-style `with_*` builders that clone
      field maps to avoid aliasing, including direct field lookup/presence/count,
      field addition, removal, and clearing helpers; `record(...)` and structured `*_with` helpers snapshot
      caller-provided field maps too. Record equality and hashing compare
      fields by key/value membership rather than rendered field order. LSP
      member-completion coverage now locks the implemented `Level` filtering
      helpers and `Record` accessor/builder surface for editor use.
      Process-global logger installation (`set_logger` / `logger`) remains
      planned non-waiting registry work; default stderr emission must still go
      through `Logger.log(...): Future<null>`.
- [x] **`std:process` portable value layer + host environment/execution/child slice**: added the mixed
      `std:process` module exporting `Command`, `ExitStatus`, captured
      `Output`, live `Child`, constructor helpers, `args`, `env`, `env_all`,
      and `set_env`.
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
      order. Rust-backed futures snapshot process argv and environment
      into `List<str>` / `Map<str, str>` values, read one variable as
      `str | null`, mutate one environment variable with validation, and resolve
      command futures through awaited `Command.status()` / `Command.output()`
      with exact captured stdout/stderr byte decoding covered by e2e tests,
      and spawn live provider child processes through awaited `Command.spawn()`,
      returning validation/provider failures as `IoError` or successful
      `ExitStatus` / captured `Output` / `Child` values. `Child` is a
      deterministic `@RefCounted` runtime handle: cloned handles share the same
      child table entry, `id()` exposes the provider process id, awaited
      `wait()` resolves the provider child once and caches the observed
      `ExitStatus`, awaited `kill()` requests provider termination, and final
      handle drop releases the runtime registry entry
      without implicitly killing the OS process. The current `Command` surface
      has no pipe configuration yet, so child stdin/stdout/stderr accessors
      return `null`.
      `exit` and `abort` are imported from `std:process` and lower to the
      existing runtime process-control intrinsics. `ExitStatus` now carries
      provider-populated `core_dumped: bool | null` and
      `stopped_signal: i32 | null` and `continued: bool | null` details in
      addition to code and signal, with value semantics and exact command/child
      decoding. Streamed child stdio remains future provider/runtime work.
- [x] **`std:process` execution/control async contract**: converted
      `Command.spawn()`, `Command.status()`, `Command.output()`, `Child.wait()`,
      and `Child.kill()` to awaitable process futures. Runtime execution,
      output capture, child spawn, child wait, and child kill now go through the
      helper-backed async future substrate with reactor wakeups; cancellation of
      a completed-but-discarded async spawn releases the child handle table entry
      instead of leaking it. Updated docs/18, docs/20, docs/21, docs/24,
      examples, e2e tests, and LSP member-completion expectations so process
      execution/control advertise `Future<...>` and cannot be used as immediate
      old-style values.
      Top-level host process helpers `args`, `env`, `env_all`, and `set_env`
      are now awaitable futures too; `exit` and `abort` remain non-returning
      process-control markers rather than wait-capable helpers. LSP namespace
      completion for `std:process` advertises the host-state helpers as
      `Future<...>` and keeps constructors/value helpers ordinary.
      `Child.id()` and the current child-stdio accessors
      `stdin()` / `stdout()` / `stderr()` still have direct not-awaitable guards
      so editor-visible child metadata does not drift into async-looking
      surface area.
      Pure command value/validation helpers (`program`, `args`, `env`,
      `arg_count`, `env_var_value`, `has_env_var`, `env_count`, `cwd`,
      `validation_error`, `validate`, and `is_valid`) are guarded as ordinary
      non-awaitable values too. Command value builders (`with_program`,
      `with_args`, `arg`, `with_env`, `env_var`, `inherit_env`, `clear_env`,
      `with_cwd`, and `clear_cwd`) are also guarded as fresh `Command` values,
      so only a deliberately designed async process surface can acquire an async-looking shape.
      LSP member-completion coverage mirrors this split: execution methods and
      child wait/control advertise `Future<...>` returns, while child
      metadata/current-stdio accessors and the pure command value/builder
      surface advertise immediate non-`Future` return types.
      `examples/async_process.otter` shows the intended awaited process
      execution shape, and `tests/cases/examples/async_process.otter` keeps
      that example contract e2e-gated. LSP member-completion coverage now also
      proves `Command.spawn`,
      `Command.status`, `Command.output`, `Child.wait`, `Child.kill`,
      `Child.id`, and the current child stdio accessors
      advertise the correct async execution/control signatures and immediate
      metadata returns.
      Streamed child stdio remains explicit backlog work.
- [x] **`std:process` host-helper async contract**: converted top-level
      `args()`, `env(name)`, `env_all()`, and `set_env(name, value)` to
      awaitable futures over the same helper-backed async runtime substrate as
      process execution/control. The runtime now exposes shared encoded helper
      implementations for argv snapshots, environment lookup/snapshot, and
      environment mutation, plus async future constructors with reactor wakeups.
      Updated `std_process_basic` and `std_process_exec_basic` to await host
      helpers, replaced the old helper not-awaitable guards with
      `*_requires_await` compile-error regressions, and updated LSP namespace
      completion plus docs/18/docs/20/docs/21/docs/24 so those helpers no
      longer appear as ordinary-result wait surfaces.
- [x] **`std:process` retired direct-result runtime ABI cleanup**: removed JIT
      registration and runtime exports for the old direct-result process
      argument/environment, command execution, child wait, and child kill
      symbols. The async future constructors remain the only generated-code
      entry points for wait-capable process state, execution, and child-control
      operations; runtime tests exercise the shared encoded helpers directly,
      keeping provider-wait coverage private implementation machinery rather
      than a public or JIT-resolvable ordinary-result wait surface.
- [x] **`std:process.ExitStatus` stopped-signal detail**: extended the
      provider-backed process status payload with a nullable
      `stopped_signal: i32 | null` field, populated from Unix
      `ExitStatusExt::stopped_signal()` where available and `null` on providers
      that cannot report it. `ExitStatus` now stores and exposes
      `stopped_signal(): i32 | null`, includes it in
      equality/hash/clone/debug semantics, and adds `was_stopped()` plus the
      value constructor `ExitStatus.stopped(signal)`. Updated runtime ABI tests,
      process value e2e coverage, LSP member completion, docs/18, docs/24,
      ROADMAP, and goals while keeping streamed child stdio and the follow-up
      continued-state status slice planned.
- [x] **`std:process.ExitStatus` continued-state detail**: extended the
      provider-backed process status payload with a nullable
      `continued: bool | null` field, populated from Unix
      `ExitStatusExt::continued()` where available and `null` on providers that
      cannot report it. `ExitStatus` now stores and exposes
      `continued(): bool | null`, includes it in equality/hash/clone/debug
      semantics, and adds `was_continued()` plus the value constructor
      `ExitStatus.continued_status()`. Updated runtime ABI tests, process value
      and awaitable command-completion e2e coverage, LSP member completion, docs/18,
      docs/24, ROADMAP, and goals while keeping streamed child stdio planned.
- [x] **`core:sync/atomic.Ordering` value contract**: added the pure
      Otter-authored `core:sync/atomic` module exporting `Ordering`, its five
      memory-ordering variants, and constructor helpers. `Ordering` implements
      equality, clone, hash, stringification, and diagnostic debug rendering;
      `AtomicI32`, `AtomicI64`, `AtomicU32`, `AtomicU64`, `AtomicBool`, and
      `AtomicPtr<T>` are now runtime-backed atomic handles: owned
      `@RefCounted` native atomic cells with load/store/swap/compare-exchange
      operations, type-specific fetch operations (`fetch_add`/`fetch_sub` for
      integer handles; `fetch_and`/`fetch_or`/`fetch_xor` for bools; no pointer
      arithmetic), operation/order validation, deterministic native cleanup,
      shared handle cloning/capture semantics, runtime unit tests, e2e coverage
      including cross-thread increments/toggles/pointer stores,
      invalid-ordering panic coverage, LSP completion coverage, and JIT/native
      parity. The concurrency and stdlib-extension docs now describe the
      completed handle family instead of the old FFI fallback plan. This moved
      from the earlier `std:sync/atomic` path because atomic
      operations are compiler/runtime substrate under the revised core/std
      split.
- [x] **Stdlib provider invariants**: explicit provider catalogs are validated
      before public `core:*`/`std:*` views are materialized. Provider names must
      be stable non-empty ASCII identifiers (`[A-Za-z0-9._-]`), and invalid
      identities stop view construction before diagnostics or future lockfile
      metadata can be polluted. Duplicate modules, root-only paths,
      unaddressable path segments, tier/root mismatches, duplicate exports, and
      exports missing from bundled toolchain source are diagnosed and skipped
      instead of becoming importable partial or wrongly-tiered views. Custom
      providers can also add valid `std:*` module views, and `no-std` still
      blocks those provider-added `std:*` imports. The built-in module and
      source manifests also have unit coverage for unique paths and the same
      scheme-plus-addressable-segment path shape required of custom providers,
      plus catalog-to-require-import coverage that catches exported
      names missing near-empty-prelude negative diagnostics. Catalog/provider
      diagnostics render module paths with the public import spelling (for
      example `core:sync/atomic`, not colon-separated submodules).
- [ ] **Next stdlib slices**:
      named-zone timezone database/conversion extensions and leap-second policy
      for `std:time`,
      deeper readiness-native provider integration, richer async network
      timeout ergonomics after an Otter-specific API decision, and additional
      platform-specific socket options for `std:net`,
      streamed child stdio for `std:process`, cryptographic guarantees and
      remaining continuous distribution work for `std:rand`, pinned `Bytes`/`Buffer` views for
      `std:bytes`, and collection follow-ups such as set literal syntax,
      hash-backed `Set`, and keyed collection construction. Each slice needs
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
  - [x] Repoint struct layouts + fn signatures (`compute_layout`,
        `signature_of`) onto `hir.structs` / `hir.fn_sigs`: backend
        `support::compute_layout` reads def-keyed `analysis.hir.structs` and
        substitutes generic args from the instantiation, while
        `support::signature_of` reads def-keyed `analysis.hir.fn_sigs` for
        parameter locals/types and return types. Backend definition and
        declaration paths call those helpers, so the old `CheckResults`
        struct/signature side-table dependency is gone.
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
        for the same node. Construction is eager + depth-first — every
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
- [x] **Debuggability:** `--emit=tokens|ast|hir|clif` with stable pretty-
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
      header; runtime calls that may wait will bracket with `enter_native`/
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
      `Shared` lock release on a panicking body is now handled by the
      worker/task panic boundary and task-held lock cleanup; see the Shared and
      worker-panic entries below.
      **`@RefCounted` — opt-in deterministic reference counting (`docs/16` §8.1) — DONE:**
      the channel-endpoint carve-out is now generalized into a real, user-facing
      object kind. A `@RefCounted struct` carries a hidden **atomic strong-count**
      word at field-block offset 0 (new descriptor `KIND_REFCOUNTED` + an `n_rc`
      trailer listing owned refcounted-field offsets — the trailer is now written on
      *every* descriptor so the collector reads it uniformly). Runtime intrinsics
      `lang_rc_retain` / `lang_rc_release` (in `runtime::gc`): release at count 0 runs
      the type's `Drop` immediately as non-waiting cleanup, releases owned
      refcounted fields (cascade), then frees — no collection needed. The backend
      inserts ARC across codegen
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
- [x] **Bug fixed**: `LocalId`s were resetting per function and colliding in the
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
- [x] **Generic struct construction inference**: record struct construction
      (`Box { value: v }`) infers generic arguments from field values and
      expected type, and tuple-struct construction infers them from positional
      arguments. Covered by generic clone / tuple-construction CLI regressions
      and examples; unresolved cases still emit the clear "cannot infer generic
      argument" diagnostic.
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
      non-union wrapper types is now complete too; see the Phase 5 `Try`
      entry and `tests/cases/error_handling/try_interface.otter`.)
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
      slots; codegen widens each element to an `i64` slot (integer extension or
      bit-preserving `f32`/`f64` packing) and narrows on read.
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
      level generics (`Type.wrap<i64>(..)`) supported; primitive type names are
      concrete static-call receivers as well (`i32.default()` and other
      primitive `extend` static methods); `examples/static_methods.otter`;
      CLI tests cover concrete, primitive, generic-bound, and native parity.
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
      scoping work in both JIT and native builds. Public named imports now
      re-export selected type/value names (including aliases) through facade
      modules without exposing the original module path, and public namespace
      imports re-export qualified namespace calls such as
      `Facade.Util.answer()`. Ambient extension-only imports now activate the
      imported module's `extend` blocks for method/interface resolution without
      binding names, and `pub import "..."` re-exports that extension activation
      transitively for umbrella modules/packages, matching docs/17. `pkg:`
      cross-package paths are covered by the package-manager entries below. 10+
      CLI tests.
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
- [x] **Ambient imports, `pkg:` imports, and concurrent GC.** Ambient
      extension-only imports now record the imported module for extension-method
      visibility; named and namespace imports activate extensions from the
      imported module as well, public named imports re-export selected
      type/value names through facade modules, public namespace imports
      re-export qualified namespace calls, and public ambient imports re-export
      extension activation through transitive umbrella chains. `pkg:` imports,
      `pkg:<name>/<pub mod>` subpaths, contextual package dependency maps,
      multi-major coexistence, and live registry/git resolution are implemented
      in the package-manager layer. Concurrent GC reclamation is done through the
      world-barrier STW design described in the GC section.
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
- [~] **Threads (`docs/20` §1): `Thread.spawn`/`join`/`detach` work, ordinary
      non-async *and*
      async workers.** `Thread.spawn(() => R)` (positional or trailing closure)
      runs the closure on a real OS thread (`runtime::threads::lang_thread_spawn`
      reads the fn pointer from the closure env and runs it) and returns a
      `JoinHandle<R>` (prelude struct holding a registry id).
      **Async worker overload:** when the closure is async (`() => Future<R>`,
      including the trailing form `Thread.spawn { async => … }`), the worker polls
      that future until it resolves on its own OS thread
      (`lang_thread_spawn_async` = closure-call + private root-driver) and the
      handle still joins on the *awaited* `R` (not `Future<R>`). Such a worker
      therefore MAY `await` and lock a `Shared<T>` — only an *ordinary non-async* `Thread.spawn`
      closure cannot lock (the narrowed compile error). The checker detects the
      async closure (return type `Future<R'>`) and yields `JoinHandle<R'>`; the
      backend passes the `Pending` tid and no `float_kind` (the awaited value rides
      as raw bits through the private root-driver entry). A captured channel
      endpoint is owned by the *future* (released when the future resolves, not when the building closure returns)
      so a worker can `await` then `send`/`recv` across a suspension. **`detach()`**
      relinquishes a worker fire-and-forget (`lang_thread_detach` drops the
      registry claim + detaches the OS thread); works for ordinary non-async and async workers.
      **One OS thread per worker is intended** — `Thread.spawn` is Otter Fusion's
      primitive for CPU-heavy or OS-thread-affine work that must run outside the
      shared executor, not a public substitute for wait-capable target APIs;
      massive lightweight concurrency is the `spawn` keyword's job and
      `Task.spawn` on the M:N executor. **Worker-panic
      isolation is now done** (see its own item below). JIT + native parity;
      `examples/async_thread_spawn.otter` + `concurrency/async_thread_spawn_*`
      cases (lock, parallel, detach, cross-thread channel, GC-stress).
      **`JoinHandle<R>.join()` is async + non-blocking**: it
      yields a `Future<Joined<R> | Panicked>` so the joining task *suspends*
      (`lang_thread_join_future` registers a waker; the worker wakes it on
      publish) instead of parking the OS thread. User code awaits the returned
      future from an async context; async `main` is polled by the runtime root
      executor until it resolves (see `docs/21`).
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
      separate async-state-machine bug — an ordinary `for`+`await` loop losing its
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
      (`gc::release_unwind_pins`, used by the private root driver), then materialises the
      message as a pinned `str`. `finish_worker` publishes `Panicked { message }`
      so a `JoinHandle.join()` surfaces it recoverably, while a `spawn EXPR`
      awaiter has the panic *re-propagated* at its own `await` (`spawn_poll`) —
      the promise-rejection model (`docs/21` §11). Sibling workers are unaffected.
      Covers `Thread.spawn` (ordinary non-async + async closures), the `spawn` keyword, and
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
      deterministic close-on-last-sender-drop + async receiver iteration.**
      `channel<T>()` (a recognised builtin, like `Thread.spawn`) allocates a
      runtime channel (`runtime::channels`: a single `Mutex<{queue, waiters,
      senders, receivers}>` plus private host-side condvar machinery — one lock so
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
      closes — *immediately, no GC needed* — waking the recv-future waiters; a
      drained `recv()` then resolves to `ChannelClosed`. **Async receiver
      iteration**: `for await x in rx` lowers to `ForDriver::ChannelAsync`, awaits
      the same channel-recv future each step, and terminates when that future
      resolves to `ChannelClosed`. Plain `for x in rx` is rejected by
      the checker; generated code no longer registers or calls any blocking
      channel-receive ABI, and the old exported runtime symbol is gone.
      Queued values are GC-pinned (`add_extra_root`) while in the queue and
      unpinned on receipt. Element types are restricted to immutable values for
      now (no deep clone-on-send yet). JIT + native parity; `examples/channels.otter`
      (`for await sq in rx`) and `examples/async_channel_iteration.otter`;
      **CLI/e2e tests** (async iterator close, recv→ChannelClosed,
      multi-sender clone, send-after-receiver-drop, managed-element GC-stress,
      native parity, try_recv, rejected plain receiver loop) + **runtime unit
      tests** (last-sender-release, drain-then-close,
      receiver-drop-closes-for-sending, private host-side close wake coverage) +
      **e2e cases** (`channel_send_recv`,
      `channel_iterator_close`, `channel_multi_sender_close`,
      `channel_drain_then_close` — closed-before-consume buffered drain,
      `channel_close_gc_stress` — managed `str` elements across the queue under
      `OTTER_FUSION_GC=stress`, `channel_send_after_receiver_drop` —
      `send`→`ChannelClosed`).
      The deterministic-release facility was generalized into the opt-in
      `@RefCounted` object kind. The reserved `std:sync` constructor names
      `channel_bounded`, `channel_mpmc`, and `channel_mpmc_bounded` now emit
      explicit planned-feature diagnostics if called or used as function values,
      instead of silently type-checking through placeholder Otter bodies. TODO:
      implement real MPMC fan-out, bounded back-pressure with async `send`, and
      move/deep-clone-on-send for non-immutable `T`.
- [x] **Channel async-receive boundary clarification**: documented the split between
      async non-blocking `Receiver.recv()`, immediate non-blocking
      `Receiver.try_recv()`, and async receiver drains via `for await`.
      Superseded by the corrected async/blocking contract slice: the old
      `Receiver: Iterator` / `for x in rx` / `ForDriver::Channel` /
      ordinary-result channel-receive ABI story has been removed from checker,
      HIR, codegen, examples, docs/20, docs/21, docs/24, and e2e expectations.
      `channel_plain_iterator_rejected.otter` proves plain receiver
      iteration is rejected, while `async_channel_iteration.otter` covers the
      direct awaited drain example.
- [x] **Spawn handle async-contract clarification**: documented that
      `Thread.spawn` is Otter Fusion's one-OS-thread-per-worker primitive for
      CPU-heavy or OS-thread-affine work and recoverable worker panic
      reporting, not as a substitute for wait-capable target APIs. `Task.spawn` runs
      on the shared executor and must not be used to hide wait-capable
      operations. Both APIs return
      non-awaitable `JoinHandle<R>` handles; user code awaits the `join()` future
      to observe completion, while `spawn EXPR` is the direct `Future<T>`
      fan-out form. Added compile-error e2e guards for `await Thread.spawn(...)`
      and `await Task.spawn(...)`, now covering both ordinary-closure and
      async-closure overloads; the LSP diagnostic regression now covers both
      overload families too, so editor analysis reports the same non-awaitable
      `JoinHandle<R>` boundary as the CLI. Refreshed docs/examples, and removed
      unnecessary Rust-copy framing from the contract text.
- [x] **GC native-state brackets for private provider waits**: added a
      tight runtime `gc::native_wait(...)` helper and used it around private
      provider/runtime operations behind explicit async surfaces that may wait on the host: filesystem
      file/path IO, standard stream reads/writes/flushes, process
      status/output/spawn/child wait/child kill, duplicate child-wait table
      waiters, host argument/environment snapshots and environment mutation,
      private runtime machinery behind async `std:time` clock reads, timer
      sleep, and local-time offset lookup, DNS resolution, TCP listener
      bind, TCP connect/read/read_to_end/write/write_all/flush/peek/accept,
      TCP stream address/error/nodelay/nonblocking/timeout/TTL controls, TCP
      listener address/error/nonblocking/TTL controls, UDP bind/connect/send/
      recv/peek variants, UDP
      address/peer/error/nonblocking/timeout/TTL/broadcast/multicast controls,
      `Thread.spawn` OS worker creation,
      async runtime timer/helper thread creation, `Task.spawn`
      executor worker-pool thread creation, and private OS entropy reads behind
      awaitable `std:rand.OsRng` / entropy-seeded helper futures. Channel receiver waits are
      public async futures/`for await` loops; the remaining private receive helper is
      `cfg(test)` runtime-test machinery, not exported and not registered for generated code. The
      bracket marks the mutator as native only while the host operation runs,
      then leaves native state before encoding Otter Fusion result values, so
      stop-the-world GC can scan roots and proceed without waiting for those
      provider waits to return. The shared async timer driver registers before
      waking tasks and publishes runtime-native/no-root state while parked on
      timer condvars, and idle M:N task executor workers publish the same
      runtime-native/no-root state while parked on the work-queue condvar.
      `std:fs.File` descriptor operations now look up
      an `Arc<Mutex<File>>` under the short-held file-registry lock, release the
      registry before the provider read/write/flush/seek, serialize only the
      individual descriptor while inside `gc::native_wait(...)`, and drop explicit
      async `File.close()` handles inside the same marker after registry removal. Updated
      docs/16/docs/24 and refreshed the channel example fixture wording.
- [x] **GC native-state brackets for network handle close/release**: routed TCP
      stream, TCP listener, and UDP socket explicit `close()` plus deterministic
      release hooks through the same remove-first, drop-inside-`gc::native_wait(...)`
      shape used by the private `std:fs.File.close()` helper. This keeps
      private handle cleanup aligned with the provider-wait audit while
      preserving the existing public contracts and registry error behavior.
- [x] **GC native-state bracket for `std:process.Child` table release**: changed
      `lang_process_child_release` so the child-table mutex is held only for
      removal, then the removed `ChildEntry` is dropped inside `gc::native_wait(...)`
      before returning. This preserves the documented contract that releasing a
      `Child` handle does not kill or wait for the OS child, while keeping host
      handle cleanup aligned with the process wait/kill/spawn native-state audit.
- [x] **GC native-state bracket for removed-in-flight `std:process.Child` drops**:
      routed the local provider `Child` drops in the rare paths where a table
      entry is removed while `Child.wait()` or `Child.kill()` temporarily owns
      the OS child through a shared `drop_os_child_native_wait(...)` helper. This
      keeps all process child handle cleanup on the same native-state boundary
      as table release, wait, and kill, even when concurrent handle release wins
      the table race.
- [x] **GC native-state bracket for idle task-executor workers**: added focused
      source regression coverage for `wait_for_task`, locking in that M:N
      executor workers enter runtime-native/no-root state before parking on the
      work-queue condvar and leave native state after wakeup. This complements
      the existing worker-pool thread-creation bracket and keeps idle
      `Task.spawn` workers from holding stop-the-world GC as running mutators.
- [x] **GC native-state bracket for the internal root-future driver**: added
      focused source regression coverage for `lang_drive_root_future`, locking
      in that async `main` and async `Thread.spawn` root-driver parks enter GC
      native state before parking on the condvar between `Pending` polls and
      leave native state after wakeup. This remains an internal runtime
      mechanism; Otter Fusion still has no user-visible `block_on` API.
- [x] **GC native-state bracket for internal `Shared<T>` runtime mutex waits**:
      added focused source regression coverage for the shared runtime
      `runtime_lock(...)` helper, locking in that registry/cell/held-lock mutex
      contention enters runtime-native/no-root state before the host mutex wait
      and leaves native state after wakeup. This preserves the public
      `Shared.lock` contract: contended source-level locks suspend the task
      through the FIFO waiter queue instead of parking an executor worker.
- [x] **Concurrency docs runtime-boundary alignment**: updated docs/20 so the
      user-facing concurrency chapter now mirrors the runtime audit for idle
      M:N executor workers and internal `Shared<T>` registry/cell mutex waits:
      both publish runtime-native/no-root state while parked, without changing
      the public `Task.spawn` or `Shared.lock` contracts.
- [x] **Async docs runtime-boundary alignment**: updated docs/21 so the
      worker-thread warning distinguishes ordinary non-waiting stdlib
      calls from runtime-internal waits. Follow-up wording now says those
      runtime-private waits do not permit wait-capable stdlib operations to hide
      inside ordinary value-returning calls in task polls. The async chapter now
      names executor worker startup, idle work-queue parks, root-future driver
      waits, timer driver parks, and internal `Shared<T>` registry/cell
      bookkeeping as GC native/runtime-native boundaries, while preserving the
      public contracts: no user-visible `block_on`, and `Shared.lock`
      contention suspends the task instead of parking an executor worker.
- [x] **GC native-state regression for concrete `std:io` stream hooks**:
      added focused runtime source coverage locking in that print/eprint,
      stdout/stderr write and flush, and stdin read/read_to_end host stream
      operations stay inside `gc::native_wait(...)`. Updated docs/24 to connect
      private stdio helper waits to the same native-state audit while
      preserving public concrete stdin/stdout/stderr methods as futures.
- [x] **Concrete stdio native-state source-regression wording cleanup**:
      renamed the runtime source regression and assertions so private stdio host
      waits are described as using the `gc::native_wait(...)` native-state marker,
      not a source-level API shape.
- [x] **Time local-offset native-state source-regression wording cleanup**:
      renamed the runtime source regression so private local UTC offset provider
      lookup is described as using the native-state marker path, not a public
      wait-capable API shape.
- [x] **Runtime native-state source-regression wording sweep**: renamed the
      remaining runtime source regression tests for OS/runtime thread creation,
      process environment/child lifecycle, filesystem close cleanup, channel
      receive waits, and network bind/control/close cleanup so they describe
      private native-state marker/helper paths rather than public-looking
      wait boundaries.
- [x] **Async print documentation snippet cleanup**: refreshed the test-suite
      README and docs/20/docs/21/docs/26 snippets so `std:io.println` is always
      awaited from async context, matching the future-returning public print
      helper contract.
- [x] **Early handbook async print snippet cleanup**: refreshed docs/01/02/04/
      06/07/08/09/10/11/12/13/14/18 snippets so `std:io.print` examples are
      awaited from async contexts instead of implying an immediate public
      output helper.
- [x] **Module overview async wait-surface cleanup**: refreshed docs/08, docs/17,
      and docs/18 so timer polling uses `await sleep(...)`, `std:async.sleep`
      and channel receive are named as future-returning surfaces, and
      filesystem/process target-backed operations are summarized as async
      provider-backed futures rather than plain target-backed helpers.
- [x] **RefCounted Drop non-waiting cleanup wording**: refreshed docs/16 and the
      refcounted example so deterministic `Drop` is illustrated with non-waiting
      resource-token bookkeeping rather than an immediate file-descriptor close
      story, ordinary explicit cleanup examples await async close/release/commit
      operations, private GC-native provider waits are described without a
      public wait-capable cleanup story, and it explicitly states that wait-capable
      cleanup belongs behind async stdlib futures.
- [x] **Thread.spawn ordinary-worker wording cleanup**: refreshed docs/20,
      docs/24, the async thread-spawn example, and the focused async-worker
      lock fixture so the non-async
      `Thread.spawn` overload is called an ordinary non-async worker rather than
      using the old label, while preserving the explicit guidance that
      `Thread.spawn` is CPU/OS-thread-isolation machinery and not a public
      substitute for wait-capable target APIs.
- [x] **Task.spawn ordinary-worker wording cleanup**: refreshed the task-spawn
      example, mirrored e2e fixture, focused task-spawn capture/ordinary-closure
      regressions, and stale ROADMAP/goals memory so executor `Task.spawn`
      non-async closures are called ordinary non-async workers rather than using
      the old label, while preserving the explicit async-surface and
      non-awaitable `JoinHandle` contracts.
- [x] **Task.spawn ordinary-worker wording follow-up**: renamed the remaining
      sync-named task fixtures, refreshed task cancellation/panic/native-parity
      expected output, and tightened compiler diagnostics/comments so ordinary
      non-async `Task.spawn`/`Thread.spawn` workers are not described with
      old shorthand.
- [x] **Task.spawn async-lock cancellation parity fixture**: promoted the
      native-parity `Task.spawn`/`Shared.lock`/cancellation program into a
      direct e2e fixture with awaited stdio output, then reused that fixture in
      the CLI native/JIT parity test so async-contract regressions cannot hide
      in duplicated embedded source.
- [x] **Process child stdio placeholder wording follow-up**: tightened
      docs/20, docs/21, docs/24, goals memory, and the focused child
      stdin/stdout/stderr not-awaitable guards so current child stdio is named
      as non-Future placeholder accessors returning async-stream-or-null shapes,
      not as ordinary stdio or process wait surfaces.
- [x] **Ordinary-control-flow wording follow-up**: refreshed stale test and
      roadmap wording so non-awaiting short-circuit operands, ordinary closure
      desugaring, ordinary `for` loops inside async bodies, and ordinary
      non-async `Thread.spawn` lock rejection are not described with old
      shorthand.
- [x] **Engine invocation non-async wording cleanup**: refreshed docs/26's
      isolate invocation section so host-callable entries are described as
      non-async or async rather than the old paired wording, preserving the no
      user-visible `block_on` contract while leaving the engine design
      unchanged.
- [x] **Compiler/backend ordinary/internal wording cleanup**: refreshed backend
      ARC comments, Shared-lock runtime comments, checker comments, and HIR
      roadmap memory so immediate refcounted cleanup, ordinary non-async
      `Thread.spawn` workers, and eager depth-first HIR construction are not
      described with old shorthand.
- [x] **Channel plain-iterator fixture wording cleanup**: renamed the remaining
      channel plain-receiver rejection fixture away from the old iterator
      shorthand and refreshed roadmap/goals memory so receiver drains are
      described as `recv()` futures or `for await`, while plain receiver
      iteration remains rejected.
- [x] **Backend ordinary-for await-state wording cleanup**: refreshed backend
      async-state-layout comments and HIR driver docs so ordinary `for` loops
      whose bodies suspend, plus the ordinary `Iterator` protocol, are not
      described with old shorthand.
- [x] **Macro/package host wording cleanup**: refreshed macro-host and
      package-registry server comments so same-thread macro expansion and
      registry request serving are not described with old public-contract
      shorthand; no Otter language surface changed.
- [x] **Async closure/backend non-async wording cleanup**: refreshed backend and
      compiler comments/tests so async-closure desugaring, ordinary `for`
      await-state scanning, direct `main` invocation, and print-helper intrinsic
      exclusions use non-async/ordinary terminology instead of old shorthand.
- [x] **Residual non-async regression naming cleanup**: renamed the remaining
      CLI/runtime regression identifiers and private task-cancellation enum
      variant that used old `sync` shorthand for ordinary non-async closures.
      The runtime cancellation behavior is unchanged: ordinary non-async task
      closures are value workers, while future-state cancellation remains the
      only path that drops generated future state.
- [x] **FFI Drop/native-state wording cleanup**: refreshed docs/19 and the docs
      index so pin/handle cleanup is described as explicit release plus
      non-waiting best-effort finalizer fallback, single-call foreign memory
      access is not described with old immediate-call wording, and extern
      safepoint prose uses private native-state terminology rather than a
      public-looking state name.
- [x] **Shared/process contract wording cleanup**: tightened docs/20 and
      docs/24 so internal `Shared<T>` host-mutex contention is described as a
      runtime-native/no-root implementation boundary, while public
      `Shared.lock` remains task suspension; process child `wait()` prose now
      says awaiting the future resolves and caches the provider child status
      instead of using old direct-operation phrasing.
- [x] **LSP adapter fixture naming cleanup**: renamed remaining editor fixture
      locals for plain TCP/UDP adapter conversion results from old shorthand to
      `plain_*`, and refreshed the older process roadmap entry so awaited
      `Child.wait()` is described as resolving provider child status.
- [x] **Live target-operation routing wording cleanup**: tightened docs/20 and docs/24 so
      wait-capable target operations are described as explicit async futures,
      not routed through an ordinary `Thread.spawn` path or exposed as
      non-`Future` provider-backed listener methods. Refreshed older tracker
      wording away from stale immediate-helper cleanup phrasing.
- [x] **docs/24 async declaration spelling cleanup**: normalized the stdio and
      logging API reference blocks from copied prefix-style async declarations
      to Otter-shaped `pub function ...: Future<...>` declarations, preserving
      the explicit awaitable public contracts without implying alternate syntax.
- [x] **docs/24 process reactor wording cleanup**: tightened the
      `std:process` execution/control callout so provider-backed process start,
      process-exit status/output capture, child control, and host process-state helpers are
      described as awaitable reactor-woken futures rather than as a non-async
      public process operation.
- [x] **Process status/output wording cleanup**: tightened docs/18,
      docs/24, ROADMAP, goals, and the process execution fixture metadata so
      `Command.status()` / `Command.output()` are described as awaitable
      process-exit status/output-capture futures rather than with copied
      wording that can imply a blocking process API.
- [x] **Captured process output accessor boundary cleanup**: tightened docs/18,
      docs/24, ROADMAP, and goals so `Output.status()` / `stdout()` /
      `stderr()` are described as non-waiting snapshot accessors over data
      already produced by awaiting `Command.output()`, distinct from the
      awaitable `Command.status()` process completion surface. Added a
      compile-error guard proving captured `Output.status()` cannot be awaited.
- [x] **docs/21 root-driver park wording cleanup**: refreshed the async chapter
      so private root-driver and timer-driver runtime boundaries are described
      as GC native/no-root parks between future polls/deadlines, not as a
      public wait or `block_on`-like source operation.
- [x] **Drop non-waiting finalizer wording cleanup**: refreshed docs/15,
      docs/16, and the drop example/mirrored e2e fixture so `Drop` is described
      as best-effort non-waiting finalization, not an async cleanup hook. The
      example now tells users to call explicit cleanup and await it when the
      release may wait.
- [x] **docs/24 stdio handle-constructor wording cleanup**: tightened the
      `std:io` catalog prose so `stdin()`, `stdout()`, and `stderr()` are
      ordinary handle constructors whose target-backed methods are awaitable,
      not "async handles" themselves. Added compile-error guards proving the
      constructors cannot be awaited directly.
- [x] **docs/29 Rust-backed hook async-contract wording cleanup**: tightened
      the stdlib contributor guide so hooks that wait on host/runtime state are
      required to keep that wait private behind async, future-returning public
      contracts, with private native-wait behavior called out explicitly in the
      review checklist.
- [x] **docs/26 engine wait-wording cleanup**: refreshed the engine chapter's
      isolate isolation and warm-pool performance prose so it describes
      per-isolate GC as not stopping other isolates and warm pools as reducing
      setup latency, without implying a public wait or blocking engine API.
- [x] **docs/26 engine timeout escape-hatch wording cleanup**: tightened the
      planned engine timeout section so pathological optimized loops must stay
      bounded by allocation limits or preserve cooperative safepoints, rather
      than suggesting a public thread-abandon or `block_on`-style timeout path.
- [x] **TCP example Thread.spawn boundary wording cleanup**: tightened the
      async TCP listener/timeout examples and mirrored e2e fixtures so
      `Thread.spawn` is described only as two-sided loopback example
      isolation, while connect/accept/read/write/timeout network work remains
      explicit awaited futures at the Otter surface.
- [x] **Async TCP GC-stress Thread.spawn boundary wording cleanup**: tightened
      the async TCP GC-stress roadmap/goals summary so the loopback server is
      described as isolated stress-test machinery, while plain `TcpStream`
      byte-I/O remains explicit awaited futures rather than a public
      `Thread.spawn` target-wait route.
- [x] **docs/16 memory-summary Drop wording cleanup**: tightened the memory
      chapter lead so `Drop` is described as best-effort non-waiting
      finalization, with wait-capable release routed through explicit awaited
      cleanup rather than a wait-capable finalizer story.
- [x] **docs/17 std:thread index boundary cleanup**: tightened the module-index
      row so `Thread.spawn` is described as the one-OS-thread-per-worker API for
      CPU-heavy or OS-thread-affine work, with non-awaitable handles and async
      `join()`, rather than a vague target-worker primitive that could be read
      as a substitute for wait-capable target APIs.
- [x] **Runtime time-hook boundary comment cleanup**: tightened
      `crates/runtime/src/time.rs` comments so the module is described as
      private clock/local-offset helper machinery behind async public futures,
      while public `std:time.sleep(Duration)` is explicitly routed through the
      reactor timer future in `async_rt` rather than any public sleep ABI.
- [x] **docs/18 process arg/env summary cleanup**: tightened the stdlib process
      summary so ordinary `Command` arg/env helpers are described as
      command-local snapshot inspection, while host argv/environment
      access/mutation remains explicitly future-returning.
- [x] **UDP network guidance wording cleanup**: tightened the async chapter's
      executor-worker warning and stdlib UDP docs so datagram/setup/control
      operations are described through future-returning `UdpSocket` and
      datagram-shaped `AsyncUdpSocket` surfaces, not as a vague handle shortcut
      for wait-capable network work.
- [x] **`std:log` awaitable helper wording/editor cleanup**: tightened
      docs/18/docs/24 so `record(...)` is explicitly a timestamping
      `Future<Record>` and default/structured line logging helpers are
      `Future<null>` writes through async stdio, then extended LSP namespace
      completion coverage so those log helpers advertise their future-returning
      public contract.
- [x] **`std:log` planned global logger boundary cleanup**: tightened the
      planned global logger prose so `set_logger` / `logger` are described as
      non-waiting registry helpers, while default stderr emission remains behind
      `Logger.log(...): Future<null>`, and added a compile-error guard proving
      those planned registry helpers are not live public APIs yet.
- [x] **Planned sync primitive async-contract cleanup**: tightened docs/20,
      docs/24, ROADMAP, and goals so planned `RwLock`, `Once`, and `Lazy`
      wait-capable acquisition/initialization paths are future-returning public
      surfaces, while immediate variants are reserved for explicit non-waiting
      `try_*` probes or value constructors. Added a compile-error guard proving
      those planned names are not live ordinary wait-capable APIs today.
- [x] **Async resolution wording cleanup**: tightened docs/21, the async example
      and mirrored fixture, focused thread/concurrency fixture metadata, and
      older roadmap async-worker/root-driver/Shared-lock notes so futures are
      described as resolving through `await` or private root-driver machinery,
      without old root-helper wording. Reused the
      source-level `block_on` rejection guard plus focused async/thread example
      runs as verification.
- [x] **Await/process resolution wording follow-up**: tightened the remaining
      docs/21 await rule and docs/24/ROADMAP process-future prose so they speak
      about futures resolving through `await`, without old command/root-helper
      phrasing. Renamed the ordinary-task cancellation regression away from an
      old filename while preserving its join behavior.
- [x] **Process reference resolution wording follow-up**: tightened docs/18,
      docs/21, docs/24, ROADMAP, and goals so process status/output references
      speak about awaited process-exit status, output capture, and I/O result
      callbacks rather than command-completion wording that can read like a
      blocking process boundary. Focused requires-await and process example
      runs verify the live future-returning contract.
- [x] **Private root-driver source wording follow-up**: tightened runtime and
      backend source comments so `lang_drive_root_future` and async `main`
      lowering are described as private root-driver polling machinery that
      resolves the future, without old public root-helper wording. Refreshed
      the async runtime process-future comment to say it resolves command
      status. Verification recorded in the current slice.
- [x] **Thread/task source resolution wording follow-up**: tightened runtime,
      backend, and checker comments plus focused runtime/test metadata so async
      `Thread.spawn`, `Task.spawn`, `Shared.lock`, detached workers, and process
      futures are described as resolving, joining, or continuing independently,
      without old public root-helper wording. Focused
      runtime builds and detached-task e2e verification cover the renamed tests
      and fixture wording.
- [x] **Shared-lock source resolution wording follow-up**: tightened the
      `Shared.lock` runtime poller comments and async-worker checker comment so
      async lock bodies and async worker closures are described as being polled
      until their futures resolve, with task suspension and lock retention
      explicit, without old public root-helper wording.
      Focused runtime/compiler test builds plus direct shared-lock e2e coverage
      verify the touched paths.
- [x] **HIR async-worker resolution wording follow-up**: tightened HIR builtin
      docs, async `Thread.spawn` examples, and concurrency fixture metadata so
      async workers are described as polling futures until they resolve and
      detached workers signal results over channels, without old root-helper
      wording. Focused compiler build plus direct async-thread example and
      lock-worker e2e coverage verify the touched paths.
- [x] **Runtime async-worker polling wording follow-up**: tightened the
      `lang_thread_spawn_async` runtime docs, async-spawn runtime test name, and
      remaining async-thread fixtures so worker futures are described as being
      polled until they resolve, with result-channel signalling and GC root
      resolution paths explicit. Focused runtime/compiler builds plus async
      thread detach/parallel/GC-stress e2e coverage verify the touched paths.
- [x] **Async example root-future wording follow-up**: tightened the async and
      threads examples plus their mirrored e2e fixtures so async `main`, direct
      `await`, and gathered worker results are described as root/task polling
      until futures resolve, without the old public root-helper vocabulary.
- [x] **Concurrency docs/root-poll wording follow-up**: tightened docs/20 and
      backend/runtime source comments so async `Thread.spawn`, async `main`, and
      `Shared.lock` describe futures as being polled or awaited until they
      resolve, with the private root-driver window named as implementation
      machinery rather than source-visible blocking behavior.
- [x] **Roadmap await-poll wording follow-up**: tightened roadmap async-closure
      and async-`Shared.lock` entries so `await`/`spawn` are described as polling
      futures until they resolve, not as a public drive operation.
- [x] **Async-closure await-poll wording follow-up**: tightened docs/21,
      async-closure ANF comments, and CLI async iterator/closure regression
      comments so `await` and `for await` are described as polling futures or
      async iterators until they resolve rather than as public drive operations.
- [x] **docs/21 root-future polling wording follow-up**: tightened the async
      chapter lead, diagram, async-`main` execution paragraph, summary, and CLI
      native async-`Thread.spawn` parity comment so root futures and async
      worker futures are described as being polled until they resolve, with
      timer deadlines managed by the shared timer driver and the private root
      driver kept internal.
- [x] **Engine async-entry polling wording follow-up**: tightened docs/26,
      ROADMAP Stage 7, and the engine goal text so planned async guest entries
      are described as being polled on the isolate executor from the host await
      path, preserving the no-user-visible-`block_on` rule.
- [x] **Async-main and closure metadata polling wording follow-up**: tightened
      docs/21, async-closure fixtures, shared examples, CLI async regression
      names/comments, runtime GC pin comments, and older goals memory so async
      futures are described as being polled or awaited until they resolve rather
      than as public drive operations.
- [x] **Runtime GC native-wait helper rename**: renamed the retired private Rust
      runtime marker and helper suffixes to `gc::native_wait(...)` /
      `_native_wait`, so GC native-state bracketing is described as private
      runtime machinery behind async/reactor-backed public contracts rather than
      as a source-level wait operation.
- [x] **Private root-driver ABI rename**: renamed the internal runtime/backend
      root future driver from the old private symbol to
      `lang_drive_root_future`, and refreshed source regressions so async
      `main` / async `Thread.spawn` polling now uses an explicit root-driver
      ABI name while the unresolved source-level `block_on` guard remains
      intact.
- [x] **Thread.spawn CPU/affinity guidance cleanup**: tightened docs/20,
      docs/21, docs/24, and the task/thread examples so `Thread.spawn` is
      recommended only for CPU-heavy or OS-thread-affine work that needs one OS
      thread per worker, with wait-capable target APIs remaining explicit
      public futures.
- [x] **Stdlib source future-contract regression**: added a compiler-side
      stdlib manifest regression that directly checks the bundled `std:io`,
      `std:fs`, `std:net`, `std:process`, `std:time`, and `std:async`
      source-declared signatures for the no-public-wait contract, then extended
      the same guard to `std:rand`, `std:hash`, and `std:log` entropy/timestamp/
      stdio-derived surfaces: suspect target-backed waits must remain
      `Future<...>` surfaces. The guard now covers the broader concrete
      fs/file/path and DNS/TCP/UDP/async-adapter method set, plus representative
      forbidden old direct-result wait signatures, so future source drift has
      to break a compiler unit test instead of silently reaching users. The
      intrinsic channel receive contract remains covered by CLI positive/negative
      guards. docs/29 now requires this style of source-level future-contract
      coverage for Rust-backed stdlib hooks.
- [x] **Backend async-hook registry/lowering regression**: added a backend
      regression over both the JIT runtime-symbol registry and native intrinsic
      lowering so wait-capable stdlib integration hooks remain wired through
      their `*_async` symbols, while the retired ordinary-result fs/io/net/
      process/time/rand hook names cannot reappear as JIT-resolvable or native
      importable source-visible operations.
- [x] **Runtime wait-capable export catalog regression**: added a runtime
      source regression over exported `lang_*` ABI symbols, including macro-made
      fs path hooks, so stdio/fs/net/process/rand/time wait-capable runtime
      constructors remain `*_async`, channel receive/thread join/shared lock stay
      future-constructor ABIs, and retired ordinary-result wait exports cannot
      reappear alongside the async constructors. Non-waiting release/bookkeeping
      hooks remain explicit carve-outs.
- [x] **LSP wait-capable completion catalog regression**: added a compact LSP
      completion regression that checks the editor-visible signatures for the
      major public wait-capable stdlib surfaces still advertise `Future<...>`:
      stdio, fs/file/path, net/DNS/TCP/UDP, process/environment/child control,
      time/async timers, rand/hash/log provider hooks, channel receive, and
      thread join.
- [x] **Historical goals async-contract memory cleanup**: refreshed stale
      `goals.txt` DONE entries from pre-correction fs/io/process/net slices so
      they no longer describe live target-backed file, stdio, process, or socket
      waits as ordinary `Reader`/`Writer`/`Seeker` or ordinary-result provider
      surfaces. The entries now preserve historical context while naming the
      current future-returning public contracts and private async runtime
      machinery.
- [x] **Async/time marker source-contract cleanup**: refreshed the Otter-authored
      `std:async` and `std:time` marker stubs so `yield_now`, both `sleep`
      surfaces, and `timeout` spell their public `Future`-returning async
      contracts in source. Intrinsic lowering still
      routes calls to the runtime timer/reactor paths, and LSP alias/namespace
      signatures continue to advertise the future contracts.
- [x] **Non-async/ordinary async-wait wording cleanup**: refreshed docs/21,
      tests/README, iterator await-in-body fixture descriptions, the
      non-async await-in-condition negative fixture name/description, ROADMAP
      async-state-machine notes, and stale goals memory so public wording says
      non-async functions/code or ordinary `for` loops instead of sync
      functions/code/loops.
- [x] **LSP stdin async completion detail closure**: tightened the concrete
      stdio member-completion regression so
      `Stdin.read_to_end_async` must advertise `Future<i64 | IoError>` just like
      `Stdin.read_async`; corrected-contract stdio work now also makes
      `read` / `read_to_end` visibly future-returning public methods.
- [x] **Stdio historical contract memory cleanup**: audited the live stdio
      compiler/runtime path and confirmed source-level print helpers plus
      concrete stdin/stdout/stderr handle waits route through future-returning
      public methods and private encoded runtime helpers. Updated historical
      goals that still described concrete stdio handle methods as ordinary
      not-awaitable values; buffered in-memory adapters remain ordinary.
- [x] **docs/18 stdio target-backed boundary cleanup**: tightened the stdio
      architecture prose so `Reader`/`Writer`/`Seeker` are scoped to
      non-waiting in-memory sources and adapters, while target-backed stdin,
      stdout, stderr, files, and sockets use async concrete methods and/or
      `AsyncReader`/`AsyncWriter`. The print helpers are documented as async
      Otter-authored functions over private standard-stream futures, not public
      runtime intrinsics.
- [x] **docs/18 target-backed IO future wording cleanup**: removed the last
      stale pre-correction result phrase from the stdlib architecture prose.
      The handbook now says target-backed files, sockets, stdin, stdout, and
      stderr expose wait-capable operations only as explicit `Future`-returning
      APIs over helper-backed async paths, while non-waiting in-memory
      `Reader`/`Writer`/`Seeker` adapters remain ordinary value contracts.
- [x] **docs/24 stdio cancellation wording cleanup**: refreshed the concrete
      stdio future cancellation paragraph so late cancelled helpers leave later
      operations on the same underlying target stream state, without calling
      that target-backed state a public value surface.
- [x] **docs/24 private native-state marker wording cleanup**: refreshed the
      stdio, process environment, and entropy paragraphs so private
      `gc::native_wait(...)` usage is described as an internal native-state marker
      around provider waits behind reactor-woken futures, not a user-facing wait
      shape.
- [x] **Filesystem docs/24 Path signature cleanup**: removed stale duplicate
      old ordinary `Path.exists`/metadata-query signatures from the extended
      stdlib reference block. The surrounding `std:fs` text already said the
      module has no public ordinary-result filesystem wait surface; the signature table now
      matches the future-returning path-query contract and requires-await guards.
- [x] **Process docs/24 execution signature cleanup**: corrected the extended
      stdlib reference block so `Command.status`, `Command.output`,
      `Command.spawn`, `Child.wait`, and `Child.kill` advertise
      `Future<...>` returns, matching the async process callout, source stdlib,
      LSP completions, and requires-await fixtures.
- [x] **Async spawn wording precision**: corrected the docs/21 parallel fan-out
      example so `spawn EXPR` results are described as futures to await, not
      join handles. This keeps `spawn` distinct from `Thread.spawn`/`Task.spawn`
      handle APIs under the no-public-blocking contract.
- [x] **Process docs async-contract wording cleanup**: tightened docs/24 so
      `std:process` execution/wait/control guidance points at explicit async
      process futures instead of a `Thread.spawn` routing story.
      It no longer implies a generic wrapper exists or should be copied in by
      default.
- [x] **Async roadmap `block_on` wording cleanup**: removed stale language that
      described `block_on(fut)` as a recognized source builtin. The roadmap now
      matches docs/21 and the compile-error guard: `lang_drive_root_future` is
      an internal runtime entry for async `main` / async `Thread.spawn` root
      futures, while user code awaits futures and cannot call `block_on`.
      Tightened the negative fixture so unrelated near-empty-prelude diagnostics
      cannot mask the actual unresolved-`block_on` check.
- [x] **Live runtime/LSP `block_on` wording cleanup**: tightened source comments
      in the LSP CodeLens path and runtime root-driver wake/poll path so they
      describe async `main` and async `Thread.spawn` execution as a private
      root-future driver, not a user-visible root-helper. Added CodeLens
      regression coverage for async `main` and re-ran the unresolved
      `block_on` compile-error guard.
- [x] **Backend/thread root-driver wording cleanup**: tightened the remaining
      async `Thread.spawn` backend/runtime comments so the generated
      `lang_thread_spawn_async` path names the private root driver and its
      `Pending` type id instead of describing a source-level root-helper.
      The public guard remains the unresolved-`block_on` compile-error case,
      while runtime unit coverage keeps the private root-driver parking path
      inside the GC native-state boundary.
- [x] **Async docs wait-boundary wording cleanup**: tightened docs/21's
      no-user-visible-`block_on` wording so user code awaits futures and cannot
      hide wait-capable public APIs behind `Thread.spawn`; internal root-driver
      waits remain private runtime machinery behind async futures.
- [x] **Stdio ordinary-contract boundary guard under no-public-blocking**:
      tightened docs/18 and docs/24 so `std:io.Reader`/`Writer`/`Seeker` are
      described only as non-waiting in-memory contracts, not target-stream
      protocols. Renamed the async-contract negative fixtures away from stale
      sync wording and added compile-error guards proving `Stdin`, `Stdout`,
      and `Stderr` do not implement the ordinary in-memory contracts; target
      stdio remains available only through `Future`-returning concrete methods
      and `AsyncReader`/`AsyncWriter`.
- [x] **Process child stdio async-stream placeholder guard**: changed
      `std:process.Child` current-stdio placeholder fields/accessors from
      ordinary `Writer | null` / `Reader | null` shapes to
      `AsyncWriter | null` / `AsyncReader | null`, preserving the current
      `null` behavior while preventing planned streamed child stdio from being
      documented or surfaced as ordinary in-memory contracts. Added
      compile-error guards that assigning child stdin/stdout/stderr accessors
      to ordinary `Writer`/`Reader` unions is rejected, and updated docs plus
      LSP field-completion details/expectations so editor completions show the
      async-stream-or-null field types.
- [x] **Network docs async-surface wording cleanup**: tightened docs/20 and
      docs/24 so network/task guidance points at explicit async socket futures
      instead of `Thread.spawn` indirection. The docs continue to point
      executor-friendly network code at `TcpStream`/`TcpListener`/`UdpSocket`
      futures and their async adapter types.
- [x] **Async TCP connect completed-result cancellation cleanup**: added a
      focused runtime regression for the race where an async TCP connect helper
      has already produced and rooted its encoded success result, but the future
      is cancelled before the next poll consumes it. The cancellation path now
      has direct coverage proving it removes the future cell, removes the extra
      result root, and releases the registered TCP stream handle instead of
      leaking a cancelled connection.
- [x] **Async TCP accept completed-result cancellation cleanup**: added the
      matching runtime regression for a completed-but-unpolled async TCP accept
      result. Cancelling that future now has direct coverage proving the encoded
      result root is removed, the future cell is drained, and the accepted TCP
      stream handle is released instead of leaking a connection.
- [x] **Async UDP completed-result cancellation root cleanup**: added focused
      runtime coverage for a completed-but-unpolled async UDP datagram receive
      result. UDP cancellation has no stream handle to release, but the
      datagram-shaped encoded result still must be removed from the future cell
      and extra-root set when the future is cancelled before the next poll.
- [x] **Async filesystem completed-result cancellation root cleanup**: added
      focused runtime coverage for a completed-but-unpolled
      `std:fs.File.read_to_end_async` helper result. Filesystem cancellation has
      no descriptor handle to release from the encoded read payload, but the
      byte-buffer-shaped result must still be drained from the future cell and
      removed from the GC extra-root set while leaving the file descriptor
      explicitly owned by the caller.
- [x] **Async filesystem late-result cancellation cleanup**: added focused
      runtime coverage for a `std:fs.File.read_to_end_async` helper completing
      after its future has already been cancelled. The regression locks in that
      no result is rooted, no cancelled waiter is woken, and the descriptor is
      not implicitly closed by discarding the late byte-buffer payload. Updated
      docs/21 and docs/24 so the late-result discard contract covers
      helper-backed stdio/filesystem/network futures instead of only naming
      network helpers.
- [x] **Async stdio late-result cancellation cleanup**: added focused runtime
      coverage for a concrete stdio helper result completing after its future
      has already been cancelled. The regression locks in that the scalar flush
      result is not rooted or stored, the cancelled waiter is not woken, and the
      cancellation owner can still drain the reactor registration.
- [x] **Concrete stdio async cancellation docs alignment**: tightened docs/24's
      `std:io` concrete-handle section so it names the same cancellation
      contract as docs/21: cancelled stdin/stdout/stderr async futures remove
      their reactor registration, discard late helper results, do not wake
      cancelled waiters, and may consume provider stdin bytes that satisfied a
      timed-out read without making the ordinary `Reader`/`Writer` contracts
      awaitable.
- [x] **Async cancellation contract wording cleanup**: tightened docs/21's
      cancellation contract so `.cancel()` immediately unregisters I/O waiters
      without waiting on target I/O, rather than describing cancellation cleanup
      as a user-facing cancellation cleanup shape. This keeps the cancellation docs aligned
      with the corrected no-public-blocking contract and the existing
      timeout/cancellation cleanup regressions.
- [x] **Task guidance explicit async-I/O alignment**: tightened docs/20 so
      the `Task.spawn` executor-worker warning no longer points only at async
      network adapters. It now names the existing executor-friendly concrete
      stdio future-returning handle methods, concrete `std:fs.File` byte
      IO/seek futures plus their `*_async` aliases, and async network adapters
      as the current async I/O routes, while refusing to present
      `Thread.spawn` as a substitute for wait-capable target APIs.
- [x] **Task.spawn example async-I/O wording alignment**: refreshed
      `examples/task_spawn.otter` and its mirrored e2e fixture so the example
      no longer describes `Task.spawn` only as a fit for generic async or
      CPU-small tasks; it now also names explicit async-I/O tasks while keeping
      CPU-heavy or OS-thread-affine work on `Thread.spawn`.
- [x] **CPU-heavy Thread.spawn example boundary wording**: refreshed
      `examples/threads_hardcore.otter` and its mirrored e2e fixture so the
      CPU-heavy worker example explicitly says it belongs on dedicated
      `Thread.spawn` OS threads rather than executor `Task.spawn` workers,
      preserving executor capacity for lightweight async tasks and explicit
      async I/O.
- [x] **docs/20 rejected green-thread wording cleanup**: refreshed the
      concurrency task-spawn summary so the deliberately rejected stackful
      green-thread model is described as implicit suspension without an
      explicit async surface, not as an implicit public suspension story.
- [x] **Channel async-iteration example replacement**: removed the old
      plain receiver-iterator Thread.spawn example / mirrored fixture and
      replaced them with `examples/async_channel_iteration.otter` plus
      `tests/cases/examples/async_channel_iteration.otter`, proving receiver
      drains are awaited with `for await`.
- [x] **Channel iterator docs correction**: tightened docs/20 so the public drain
      form is `for await x in rx`; plain `for x in rx` is rejected instead of
      documented as an ordinary-result drain boundary.
- [x] **Engine bridge-channel docs correction**: tightened docs/26 so planned
      bridge channels reuse the corrected docs/20 channel contract: FIFO
      delivery, immediate non-blocking `send`/`try_recv`, async `recv()`
      futures, `for await` receiver drains, and close-on-last-sender, with no
      receive wait hidden behind the host/isolate boundary.
- [x] **Async root wait docs Thread.spawn wording**: tightened docs/21's
      no-user-visible-`block_on` explanation so it names an explicit
      `Thread.spawn` dedicated-OS-thread boundary instead of generic
      "dedicated-thread boundaries" wording.
- [x] **Async worker wait docs I/O async-surface alignment**: tightened
      docs/21's executor-worker warning so it names concrete stdio
      future-returning methods and `std:fs.File` byte IO/seek futures beside
      their async aliases and the async network adapters, matching docs/20 and
      docs/24.
- [x] **Async executor-worker docs current-surface cleanup**: removed the
      stale `sync DB` placeholder from docs/21's executor-worker warning so
      the list names current documented wait-capable surfaces instead of implying
      a database API exists today.
- [x] **Async executor-worker docs timer async-surface alignment**: tightened
      docs/21's executor-worker warning for the old timer split. Superseded by
      the corrected async contract: `std:time.sleep(Duration)` is itself an
      awaitable timer surface.
- [x] **Stdlib summary Task.spawn async-surface wording**: tightened
      docs/24's `std:task` summary so wait-capable operations point at
      an explicit async surface,
      matching `std:async.sleep`, concrete stdio async methods,
      `std:fs.File` byte IO/seek futures and aliases, and network adapters.
- [x] **Async chapter summary async-surface wording**: tightened docs/21's
      key-takeaways summary so ordinary non-waiting stdlib APIs stay
      ordinary until an explicit async surface exists.
- [x] **Buffered stdio negative-test async-surface wording**: refreshed the
      buffered reader/writer not-awaitable fixture descriptions so they say
      `BufReader`/`BufWriter` ordinary in-memory methods are not async surfaces, not
      "not explicit async surfaces", avoiding adapter-taxonomy wording for Otter
      Fusion's value-vs-explicit-async contract.
- [x] **Stdio helper/line-iterator fixture async-surface wording**: refreshed
      the remaining stale stdio not-awaitable fixture descriptions so
      historical `print`/`eprint` not-awaitable wording is superseded by the
      corrected `Future<null>` print-helper contract, while
      `BufReader.lines()` still returns an ordinary iterator, not an explicit
      async stream surface.
- [x] **Stdio print-helper async correction**: replaced the old
      aggregate `println`/`eprintln` not-awaitable fixture with
      `Future<null>` requires-await guards and a direct async stdout/stderr
      print-helper run case. Concrete stdio async methods remain
      helper-backed, reactor-woken futures rather than vaguely reactor-backed
      futures.
- [x] **Stdio LSP print-helper async-surface wording**: refreshed the LSP
      signature-help contract entry so top-level print helpers are stdlib async
      functions rather than old ordinary compiler builtins, matching the
      requires-await guards and editor signature checks.
- [x] **Process contract explicit-async-surface wording**: refreshed the
      current `std:process` value/future clarification so process execution,
      child wait/control, and host process helpers are described as explicit
      awaitable process futures, while command value builders remain ordinary
      value surfaces rather than copied adapter-shaped APIs.
- [x] **Process summary async-surface wording**: refreshed docs/18 and
      docs/24 process summary rows plus the detailed process callout so planned
      process async work is described as deliberately designed async process
      surfaces rather than old adapter terminology.
- [x] **TCP nonblocking fixture async-surface wording**: refreshed the
      `TcpStream.set_nonblocking` not-awaitable fixture description so it says
      socket mode configuration is not an executor-integrated async surface,
      keeping the provider-mode knob distinct from async network adapters.
- [x] **Listener/UDP nonblocking fixture async-surface wording**: refreshed
      the `TcpListener.set_nonblocking` and `UdpSocket.set_nonblocking`
      not-awaitable fixture descriptions so every provider nonblocking toggle
      is consistently documented as socket-mode configuration, not an
      executor-integrated async surface.
- [x] **Socket nonblocking docs async-surface wording**: tightened docs/24's
      TCP and UDP option paragraphs so provider-backed `set_nonblocking`
      toggles are described as deliberate provider-readiness/custom-mode knobs
      outside the Otter Fusion executor, not async surfaces.
- [x] **UDP pre-correction executor-warning docs**: tightened docs/20 and docs/21
      so executor/task guidance explicitly named the then-ordinary-result
      `UdpSocket` `send`/`recv`/`peek`/`send_to`/`recv_from`/`peek_from` beside
      TCP stream waits, matching the existing UDP not-awaitable guards and
      `AsyncUdpSocket` adapter split at the time. The corrected UDP datagram
      contract later superseded that public shape with future-returning plain
      `UdpSocket` datagram methods and requires-await guards.
- [x] **Process child accessor async-surface docs**: tightened docs/20 and
      docs/21 so the executor guidance distinguishes wait-capable process
      execution/wait/control/helper operations from immediate non-`Future`
      `Child.id` and current child-stdio accessors, matching the direct
      not-awaitable guards without pretending those value accessors are
      executor-integrated async process surfaces.
- [x] **Process child-stdio fixture wording cleanup**: refreshed the child
      stdin/stdout/stderr not-awaitable fixture metadata so those current
      accessors are described as non-Future current-stdio placeholders, while
      streamed child stdio remains planned async-stream work.
- [x] **Filesystem executor-warning concrete surface docs**: tightened docs/20
      and docs/21 at the time to name the pre-correction ordinary-return
      `std:fs` module helpers, directory snapshots/mutations, target-backed
      `Path` queries, and `File` text helpers beside future-returning
      descriptor methods. Corrected-contract work now supersedes that warning:
      those wait-capable filesystem surfaces are explicit futures too.
- [x] **Stdio executor-warning concrete surface docs**: tightened docs/20 and
      docs/21 so executor/task guidance names ordinary non-waiting
      in-memory `std:io` `Reader`/`Writer`/`Seeker` methods and buffered
      adapter reads/writes beside the async print helpers and concrete
      stdin/stdout/stderr async methods, matching
      docs/24 and the stdio/buffered not-awaitable guard coverage.
- [x] **Timer summary async-contract correction**: updated docs/24's module
      summary table so `std:time.sleep(Duration)` is visible as an async
      non-blocking `Future<null>` timer, while `std:async.sleep(ms)` remains the
      millisecond helper over the same timer/reactor substrate.
- [x] **std:async helper wording cleanup**: refreshed docs/17, docs/18, and
      docs/24 module summaries plus matching bookkeeping so `yield_now`,
      `sleep`, and `timeout` are described as executor/runtime helpers rather
      than an adapter family. This keeps the async library surface distinct from
      actual wrapper/adapter types such as `AsyncTcpStream`.
- [x] **Thread summary async-contract docs**: tightened docs/24's
      `std:thread` summary so `Thread.spawn` is visible as the
      one-OS-thread-per-worker primitive for CPU-heavy or OS-thread-affine work,
      not an ordinary-result target-operation route, while `Task.spawn`/`spawn` remain the high-scale executor
      routes; the summary also states that thread `JoinHandle`s are
      non-awaitable, `detach()` is immediate, and OS-thread handles have no
      `cancel()`/`abort()` with explicit no-method guards for both names.
- [x] **Concurrency source-comment module spelling cleanup**: refreshed
      compiler/backend/runtime/LSP comments that describe Otter Fusion
      concurrency/core surfaces so they use the language's module/type wording
      (`std:task` `JoinHandle`, `std:sync` `Sender`, `core:async` `Future`)
      rather than Rust-style double-colon paths. Real Rust implementation paths
      such as `std::thread::spawn` remain unchanged.
- [x] **Async I/O helper-backed reactor wording**: tightened docs/20, docs/21,
      and docs/24 so current async I/O is described as helper-backed futures
      that register cancellable one-shot reactor wakeups/completion callbacks,
      not as already-readiness-native provider integration; the latter remains
      planned work.
- [x] **Runtime async helper comment wording cleanup**: tightened
      `crates/runtime/src/async_rt.rs` comments so private stdlib helper-backed
      futures are described as wait-capable provider operations handed to helper
      threads behind cancellable reactor registrations, not as a user-facing
      stdlib future category. Reactor cancellation wording now says waiters are
      immediately unregistered without waiting on target I/O.
- [x] **Memory-model async helper native-state docs**: tightened docs/16's
      runtime native-state callout so helper-backed async stdio, filesystem, and
      network futures explicitly split helper-thread creation from provider
      waits: creation is bracketed by the caller, private provider waits use the
      helper's `gc::native_wait(...)` marker, and completion encodes and roots the
      result plus wakes the reactor without a thread-wide native-state
      enter/leave.
- [x] **Retired target-backed value wording cleanup**: refreshed the stdlib
      architecture docs and runtime entropy comment so target-backed stdio,
      file/socket streams, and OS entropy are described as exposing waits only
      through explicit `Future`-returning public helpers over private runtime
      futures.
- [x] **Runtime entropy Future-surface wording cleanup**: tightened the
      `std:rand` runtime entropy module comment so OS entropy reaches Otter
      Fusion user code only through explicit `Future`-returning public helpers
      over private runtime futures.
- [x] **Timer marker source-contract guard**: documented the Otter-authored
      `std:async.sleep(ms)` / `std:time.sleep(Duration)` marker stubs as
      compiler-recognized future-returning timer helpers, fixed marker
      recognition to follow the resolved builtin definition through named
      import aliases, and added checker/e2e regressions proving aliased
      `yield_now`, `std:async.sleep`, `timeout`, and `std:time.sleep` keep their
      `Future<...>` contracts rather than falling back to the payload-free
      source bodies.
- [x] **LSP timer marker alias signature guard**: centralized editor-facing
      signatures for the compiler-recognized `std:async` / `std:time` marker
      stubs, then routed hover, signature help, namespace completion, and
      function completion details through that contract. Focused LSP coverage
      now proves named import aliases for `yield_now`, `std:async.sleep`,
      `timeout`, and `std:time.sleep` advertise `Future<...>` returns and keep
      alias-aware signature-help labels instead of leaking the payload-free
      marker bodies.
- [x] **Async TCP stream constructor correction**: changed public
      `TcpStream.connect(addr)` and `TcpStream.connect_timeout(addr, timeout)` to
      return `Future<TcpStream | IoError>` under the corrected no-public-blocking
      contract. The stdlib wrappers now await the existing private
      `__otter_net_tcp_connect_async` and
      `__otter_net_tcp_connect_timeout_async` futures, the backend no longer
      registers exported direct-result TCP connect symbols, and the runtime keeps
      the encoded connect helpers private to async future machinery and unit
      tests. Replaced the old connect not-awaitable guards with
      `std_net_tcp_connect_requires_await.otter` and
      `std_net_tcp_connect_timeout_requires_await.otter`, updated loopback TCP
      run coverage to await the constructors, and refreshed LSP completion so
      `TcpStream` constructors advertise `Future<TcpStream | IoError>`.
      Later corrected-contract slices completed TCP stream instance I/O/control,
      listener setup/accept, and UDP datagram/setup/control cleanup.
- [x] **Async network timeout convenience contract docs**: clarified that
      the TCP connect-timeout constructors are the dedicated async timeout
      operations currently accepted, while ordinary async TCP read/write/peek,
      listener accept, and UDP datagram operation deadlines use
      `std:async.timeout(...)` around the explicit future today. Added negative
      e2e guards for unapproved copied convenience names such as
      `read_timeout_async`, `accept_timeout_async`, and `recv_timeout_async`;
      richer timeout ergonomics remain planned target-backed work only after an
      Otter-specific API decision.
- [x] **Async network timeout example**: added `examples/async_network_timeout.otter`
      plus a test-gated mirrored fixture showing the current Otter Fusion
      pattern for async network operation deadlines: wrap the explicit async
      datagram future in `std:async.timeout(...)`, discard the timed-out stale
      result, and keep using the socket for a later datagram instead of adding
      copied `recv_timeout_async`/`send_timeout_async` methods.
- [x] **Concurrency timeout guidance alignment**: tightened docs/20's task
      worker guidance so it names the same async network deadline contract as
      docs/21/docs/24: use `std:async.timeout(...)` around explicit async
      futures today, except for the dedicated TCP connect-timeout constructors,
      and do not infer copied
      `read_timeout_async`/`accept_timeout_async`/`recv_timeout_async` method
      families.
- [x] **Stdlib architecture network timeout summary alignment**: tightened
      docs/18's `std:net` catalog row so the architecture summary matches
      docs/20/docs/21/docs/24: async network adapters are helper-backed futures
      with one-shot reactor wakeups, operation deadlines use
      `std:async.timeout(...)` except for TCP connect-timeout constructors, and
      copied timeout method families are not current API.
- [x] **Stdlib extended summary network timeout alignment**: tightened
      docs/24's module summary row so the quick-reference `std:net` entry
      carries the same async deadline contract as the detailed section:
      `std:async.timeout(...)` wraps explicit async futures today except for
      TCP connect-timeout constructors, copied timeout method families are not
      current API, and richer timeout ergonomics remain planned only after an
      Otter-specific API decision.
- [x] **LSP async network timeout completion guard**: added focused editor
      completion coverage proving `AsyncTcpStream`, `AsyncTcpListener`, and
      `AsyncUdpSocket` do not advertise copied timeout convenience method
      families such as `read_timeout_async`, `accept_timeout_async`,
      `recv_timeout_async`, or address-aware UDP timeout variants. This keeps
      the editor surface aligned with the documented `std:async.timeout(...)`
      wrapper contract.
- [x] **Broader async network timeout-name negative coverage**: added e2e
      compile-error guards for additional copied convenience names
      `write_timeout_async` and `send_to_timeout_async`, complementing the
      earlier `read_timeout_async`, `accept_timeout_async`, and
      `recv_timeout_async` guards so both stream writes and address-aware UDP
      sends stay on the explicit `std:async.timeout(...)` wrapper contract.
- [x] **Complete copied async network timeout-name e2e guard set**: added
      compile-error coverage for the remaining obvious copied timeout method
      names across implemented async network operations:
      `peek_timeout_async`, connected UDP `send_timeout_async` /
      `peek_timeout_async`, and address-aware UDP `recv_from_timeout_async` /
      `peek_from_timeout_async`. Together with the earlier guards, every current
      async TCP/listener/UDP operation now has negative coverage against
      method-family drift.
- [x] **Async network timeout-name documentation closure**: aligned
      docs/20, docs/21, docs/24, and the docs/18/docs/24 summary rows with the
      completed copied-timeout guard set, so the documentation now describes
      the full rejected timeout-shaped family across TCP read/write/peek,
      listener accept, and connected/address-aware UDP operations instead of
      only the earlier sample names.
- [x] **Process async example metadata wording**: replaced the former
      Thread.spawn-based process fixture with `async_process`, so the example
      suite now demonstrates awaited `std:process` execution instead of a
      `Thread.spawn` target-operation recommendation.
- [x] **Thread.spawn async-surface wording cleanup**: tightened
      docs/20, docs/21, docs/24, ROADMAP, and the async TCP timeout example so
      `Thread.spawn` is described only as a one-OS-thread-per-worker primitive
      for CPU-heavy or OS-thread-affine work. It is no longer
      presented as an ordinary route for wait-capable target APIs; those must
      expose explicit async futures at the Otter Fusion surface. The existing
      ordinary non-async and async-closure `Thread.spawn` not-awaitable guards continue to
      prove the API returns `JoinHandle<R>`, not a bare awaitable future.
- [x] **Async TCP timeout fixture metadata cleanup**: refreshed the mirrored
      `tests/cases/examples/async_tcp_timeout.otter` description so it names an
      awaited TCP server whose network operations stay explicit futures instead
      of calling the server blocking. The source example and mirrored e2e both
      await listener bind/address lookup, accept, stream read/write/close, and
      timed connect.
- [x] **Stdlib implementation-kind async wording cleanup**: refreshed docs/18,
      docs/24, and docs/29 implementation-kind examples so target-backed
      `std:fs`, `std:process`, `std:net`, time, hash, and rand hooks are
      explicitly described as awaitable where they can wait, and removed stale
      `std:io.print`/`eprintln` Rust-backed examples now that print helpers are
      ordinary async stdlib functions. Docs/24's `std:io` intro now says
      `Reader`/`Writer`/`Seeker` are ordinary value-returning contracts for
      non-waiting in-memory sources/adapters, while target-backed byte streams
      use async methods and/or `AsyncReader`/`AsyncWriter`.
- [x] **Stdlib architecture process async summary**: tightened docs/18's
      `std:process` catalog row so it explicitly classifies
      `Command.status()`/`output()`, `Command.spawn()`, and child
      `wait()`/`kill()` as async process futures, while keeping streamed child
      stdio as planned provider/runtime work.
- [x] **Stdlib architecture stdio/fs explicit-async summary**: corrected
      docs/18's `std:io` text so it no longer claims there is no generic async
      I/O protocol while `AsyncReader`/`AsyncWriter` are implemented. The
      summary now states the actual contract: `Reader`/`Writer`/`Seeker` and
      buffered adapters remain ordinary value-returning surfaces, while
      async-capable stdio/file handles expose future-returning concrete methods
      plus aliases and `AsyncReader`/`AsyncWriter` implementations.
- [x] **Stdlib stdio/fs helper-backed async wording closure**: tightened
      docs/18 and docs/24 so concrete stdio and descriptor-backed file async
      methods are described as helper-backed futures that wake through the
      reactor, not as vague reactor-backed methods that could be mistaken for
      completed readiness-native provider integration.
- [x] **Module-index stdio helper-backed async wording**: tightened the
      docs/17 `std:io` module index row so stdin/stdout/stderr async concrete
      methods are described as helper-backed, reactor-woken methods rather than
      reactor-backed methods, keeping the import/module overview aligned with
      the detailed stdlib and async/runtime docs.
- [x] **Process not-awaitable fixture async-surface wording**: refreshed the
      `std_process_*_not_awaitable` fixture descriptions so the pre-correction
      ordinary-return process execution/control/helper surfaces and pure value
      helpers are described as not explicit async process surfaces, rather than
      using adapter taxonomy that could imply a default copied async process
      surface family. Corrected-contract work later replaced wait-capable
      process guards with requires-await fixtures.
- [x] **Process pure-value fixture wording closure**: refreshed the remaining
      `std:process` not-awaitable fixture descriptions so command
      builders/accessors/predicates/snapshots and `exit`/`abort` markers are
      described as immediate pure value/control surfaces, while process
      execution, child wait, child kill, argv/env inspection, and other
      wait-capable process operations remain future-returning public APIs.
- [x] **Roadmap process async-surface wording closure**: refreshed the remaining
      roadmap process backlog and completed-slice wording so planned async
      process work is described as deliberately designed async process surfaces,
      not copied adapter terminology.
- [x] **Filesystem not-awaitable fixture async-surface wording**: refreshed the
      `std_fs_*_not_awaitable` fixture descriptions so module helpers, path
      queries, and `File` text helpers were recorded as pre-correction
      ordinary-return filesystem surfaces. Corrected-contract work has since
      replaced those guards with requires-await fixtures for all wait-capable
      filesystem helpers, path queries, and `File` methods.
- [x] **Channel not-awaitable fixture async-surface wording**: refreshed the
      channel `send`, `try_recv`, and retired iterator `next` fixture
      descriptions so immediate enqueue/poll operations and the old
      ordinary-result receive-drain history is not described as a public async
      receive surfaces, keeping the guard metadata aligned with `Receiver.recv()`
      and `for await` drains as the awaited receive operations.
- [x] **Network not-awaitable fixture async-surface wording**: refreshed the
      historical `std_net_*_not_awaitable` fixture descriptions so DNS,
      then-ordinary-result TCP stream/listener operations, socket controls, and UDP
      datagram/control operations are described as not explicit async
      resolver/network/stream/listener/UDP surfaces, keeping guard metadata
      aligned with the explicit `AsyncTcpStream`/`AsyncTcpListener`/
      `AsyncUdpSocket` adapter split. Corrected-contract follow-ups have since
      replaced DNS, TCP connect, TCP accept, TCP stream byte-I/O, and UDP
      datagram guards with requires-await fixtures.
- [x] **Async network setup wrapper wording**: refreshed the async-network
      setup/conversion not-awaitable fixture descriptions and matching roadmap
      notes so `AsyncTcpStream.from_stream`, then-current
      `AsyncTcpListener.bind`/`from_listener`, and `AsyncUdpSocket.bind`/
      `from_socket` were described as ordinary wrapper setup/conversion
      boundaries rather than async operations. Later corrected-contract work
      superseded `AsyncTcpListener.bind`, which is now awaitable.
- [x] **Async network wrapper value-helper not-awaitable guards**: added
      compile-error e2e coverage proving async-network wrapper value helpers
      remain ordinary values: `AsyncTcpStream.clone()` returns an
      `AsyncTcpStream`, `AsyncTcpListener.to_str()` returns `str`, and
      `AsyncUdpSocket.debug()` returns `str`, none of them `Future`s.
- [x] **Shared handle setup/clone not-awaitable guards**: added compile-error
      e2e coverage proving `Shared.new(value)` and `Shared.clone()` are ordinary
      handle construction/clone surfaces, while `Shared.lock` and
      `Shared.try_lock` remain the awaited lock surfaces.
- [x] **Shared lock result flattening not-awaitable guards**: added
      compile-error e2e coverage proving `Shared.lock` and `Shared.try_lock`
      await the body until it resolves and produce flattened body values (`R` or
      `R | LockBusy`); attempting to await those resolved values is rejected.
- [x] **Shared lock result wording cleanup**: refreshed the flattening
      regression metadata so `Shared.lock` and `Shared.try_lock` are described
      as awaited lock surfaces resolving to flattened body values, without
      old result-boundary wording.
- [x] **Runtime marker value-helper not-awaitable guards**: added compile-error
      e2e coverage proving `TimedOut`, `Cancelled`, `Panicked`,
      `ChannelClosed`, and `LockBusy` marker value helpers remain ordinary
      clone/string/debug/equality/hash surfaces, not `Future`s or async
      operation surfaces. Follow-up guards cover `hash()` and `==` directly for
      every marker so the negative coverage matches the documented value
      surface.
- [x] **Async stdio fixture/example cleanup**: bulk-refreshed run fixtures and
      examples so ordinary output uses the corrected async `print`/`println`
      contract. Straight-line run cases now use `main(): Future<null> async`
      and await output helpers; worker-panic reporting helpers return futures;
      duplicate `Future` imports from `core:prelude`/`core:async` were removed;
      and FFI/atomic-pointer cases compute raw-pointer observations before the
      first output await so stack/raw pointer state is not live across
      suspension. Refcounted/example `Drop` coverage now uses non-waiting
      instrumentation instead of async `println` inside finalizers, preserving
      deterministic drop-order assertions without exposing wait-capable public
      output from `Drop`. The follow-up async runtime native-state slice below
      closes the GC-stress cancellation timeout cluster, and the full e2e suite
      now reaches 723 pass / 0 fail.
- [x] **Async runtime timer/cancellation GC native-state fix**: closed the
      remaining GC-stress cancellation timeout cluster by bracketing private
      reactor/timer mutex contention and timer-driver startup/OnceLock waiters
      in runtime-native/no-root state. The timer thread now captures the
      already-initialized driver rather than re-entering `timer_driver()` during
      startup, so concurrent `sleep` polls cannot leave workers marked
      `RUNNING` while they wait for the shared timer driver to initialize.
      Executor-task cancellation now records a reschedule request when a
      cancellation races with an in-flight poll and cannot take the poll lock,
      ensuring the pending task promptly observes cancellation instead of
      relying on an unrelated timer wake. Added focused runtime regression
      coverage for private timer/reactor lock bracketing and busy-poll
      cancellation rescheduling, refreshed docs/16 and docs/21, and verified
      the three GC-stress cancellation cases plus the full e2e suite at
      723 pass / 0 fail.
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
      native parity; `examples/shared.otter`; 2 CLI tests. Later async-Shared
      slices replaced the old ordinary-result lock path with task-aware
      `lock`/`try_lock` futures, held the mutex across awaits, and completed lock release on
      cancel/panic through the worker/task panic boundary. Internal Shared
      registry/cell/held-lock mutex contention is bracketed as
      runtime-native/no-root, but source-level lock contention remains a task
      suspension. Reentrancy remains undefined (per spec).
- [x] **Async (`docs/21`) — COMPLETE, with an explicit `async`/`await` surface.**
      Surface: user code writes postfix `async` on function, anonymous-function,
      and closure bodies whose signature returns `Future<T>`; `await` is a
      user-visible expression valid only inside async bodies; `async { ... }`
      blocks and `for await` are user-visible; `main` may be async and is polled
      by the runtime until it resolves. The internal root-future driver parks in GC
      native state between `Pending` polls; there is no user-visible `block_on`
      builtin.
      `spawn EXPR` schedules a future on the shared executor and returns a
      cancellable `Future<T>`; `Task.spawn` and `Thread.spawn` are handle APIs
      described in `docs/20`. The `Future`/`Ready`/`Pending`/state-machine
      machinery below is both a visible type contract and the internal lowering
      target.

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
      box whose single vtable slot is `poll`. The runtime-internal
      `lang_drive_root_future` entry polls async `main` and async `Thread.spawn` root
      futures until they resolve; it is not a source-level builtin and user code must
      `await` futures instead. That internal poll loop parks on a condvar waker
      between `Pending` polls, in GC-native state, pinning the future as a GC
      root across polls. **`await` is fully lowered**: an async function
      whose body contains `await` becomes a real suspendable state machine — the
      state struct holds every body local, `poll` dispatches on a saved state
      word to resume at the right `await`, each `await` saves the live locals +
      the inner future and returns `Pending`, and the executor re-polls on wake.
      `yield_now()` (a `Future<null>` that suspends once, self-waking) exercises
      genuine park/resume cycles. `await`s in `if`/`while`/`match` bodies work.
      JIT + native parity; GC-stress clean. **`await` inside `async { … }`
      blocks** works (`await async { … }`, and async `main` is polled by the
      runtime root executor until it resolves). **`spawn EXPR`** schedules a future on the shared
      executor and returns a `Future<T>` whose await observes completion,
      cancellation, or panic propagation. **`for await x
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
      condition suspends exactly once per iteration. `await` in a non-async function
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
      `docs/09` §7) without running `E`; awaiting or spawning it polls the future
      until it resolves. The
      closure's value type is `(p) => Future<T>` — a callable, not a bare
      `Future` — so it stores in a struct/list and passes as a higher-order
      argument. Capture-by-reference mutations made inside the body (across an
      `await`) are visible to the next poll and to the enclosing scope. An
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
      resolves `next_async` through the interface (mirroring the ordinary
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
- [x] **Ordinary `for` loop with an `await` in its body** (`docs/21`): a `for` loop
      that is not itself `for await` but whose body suspends now preserves its
      iteration state across the suspend. The loop's codegen-internal iterable
      pointer(s) + index counter live in Cranelift SSA, which does **not** survive
      a `poll` return — so `async_state_layout` reserves per-loop state-struct
      slots (`(primary, secondary, index)`, keyed by `iter.span` via
      `h_scan_for_state`; the iterable slots are GC-traced) and all four ordinary
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
      system `libffi` since neither `pkg-config` nor autotools is assumed; the
      runtime loads `libffi` lazily through platform SONAMEs such as
      `libffi.so.8`/`libffi.dylib`, so Linux builds do not require a `libffi-dev`
      linker symlink and native builds do not pass `-lffi`). The checker rejects
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
      the front-end (`Compiled` is cached per open buffer and invalidated on
      document mutations; queries are driven by
      a `HirIndex` built over the typed **HIR** — the old span-keyed `CheckResults`
      tables are gone, retired into HIR node fields per Phase 2.5). Features: live
      diagnostics (lex+parse+sema), hover (types +
      symbol/builtin signatures), go-to-definition (name-precise, for
      functions/methods/globals/struct ctors/locals), find-references, rename,
      document symbols (items + struct fields + interface/`extend` methods),
      completion (keywords/builtins/top-level defs/locals), full semantic
      tokens (resolution-driven classes refining a bundled TextMate grammar),
      code-action quick-fixes, formatting, document highlights, code lenses, and
      folding ranges, local type inlay hints, incremental text sync, and cached
      analysis.
      Editor positions are converted UTF-16↔UTF-8 (`LineIndex` on the hot path).
      80 unit tests in `crates/lsp`; the extension compiles (`npm run compile`).
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
    on contention turned out to be the ordinary-`for`+`await` state bug (now fixed,
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
      within a submodule surfaces references in both files).
- [x] **Reverse-importer LSP references + rename**: for file-backed definition
      targets, `references`/`rename` now scan the package source root (nearest
      `project.toml`'s `src/`, or the current directory in direct mode), compile
      candidate `.otter` roots with the same open-buffer overlay used by normal
      analysis, and match targets across independent compilations by normalized
      source file + definition name span instead of unstable per-compilation
      `DefId`s. Importer files that declare/import the current module now
      contribute call/use sites, and named import specifiers are included so
      rename keeps importers compiling. Aliased imports update the imported
      source name while preserving the local alias uses. Focused LSP tests cover
      unaliased importer references/rename and aliased importer behavior.
- [x] **Type-position go-to-definition**: goto on a type name written in a type
      annotation (param / return / field / alias / `extend` target / generic
      bound) or a body type position (`var x: T`, `e as T`, closure
      annotations, struct literals, pattern type names) jumps to that type's
      definition. Type-position names aren't value resolutions in the HIR, so
      the LSP resolves them from the parsed AST: `collect_type_refs` walks every
      item, inline module, function/test/default-method/global body, syntactic
      type, `TypePath`, expression, and pattern to build `(name-span, name)`
      refs; `type_def_span_at(off)` resolves the innermost one to its def's name
      span (`def_name_span`), and `goto_definition` tries it as a fallback after
      value resolution. LSP tests cover item-level param/return/field names and
      body local annotations, casts, closure annotations, struct literals, and
      type-binding/record patterns.
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
      level, closer-leading lines dedented), common intra-line token spacing
      (commas, colons, assignment, comparisons including `<`/`>`, generic/type
      angle spacing in annotations and async return types, async closure-arrow
      spacing, control-keyword spacing before parenthesized expressions and
      unary condition/scrutinee starts such as `if !done` / `match !done`,
      statement-keyword unary operand spacing such as `return -1` and
      `await !ready`, `as`/`is` cast and type-test spacing, iterator `in (` spacing,
      interpolation delimiter edges, logical operators, and clearly binary
      arithmetic), strips trailing whitespace outside ordinary comment trivia,
      collapses blank-line runs, normalizes code around
      balanced inline block comments and multi-line block-comment boundary lines
      while preserving comment text exactly, keeps brace bodies nested under
      call/index delimiters (including multiline async closures, async blocks
      passed to calls, plain block expressions, and block-form macro invocations)
      indented from the opened brace rather than the outer call delimiter, wraps long parenthesized,
      bracketed, type-led brace, generic-angle, named-import brace, named-import paths, named-import closing paths, single-argument attributes, attribute argument lists, inline attribute item forms, stacked inline attribute item forms, macro block bodies, ambient/namespace import paths, extern type declarations, extern var declarations, extern function declarations, function bodies, module bodies, module body member lists, map-literal brace, match-arm brace, record-declaration brace comma lists, record field type-annotation union pipe lists, return expressions, break expressions, await/spawn operand expressions, test/bench declaration headers, test/bench bodies, assignment expressions, for iterator headers, for bodies, while condition headers, if/else-if condition headers, match scrutinee headers, match guard headers, unguarded match arm bodies, top-level parenthesized arrow closure bodies, explicit trailing closure bodies, implicit trailing closure bodies, anonymous function expression headers, async block bodies, block expression bodies, loop bodies, else bodies, if bodies, while bodies, type-alias union pipe lists, var initializer expressions, var type-annotation union pipe lists, parameter type-annotation union pipe lists including full multi-parameter layouts, function return union pipe lists, arrow closure return union pipe lists, interface/extend declaration headers, struct declaration headers, type-alias declaration headers, interface/extend bound plus lists, interface/extend body member lists, generic type-parameter bound plus lists including full multi-parameter layouts, cast chains, single cast expressions, method chains, single method expressions, logical chains, single logical expressions, comparison expressions, single comparison expressions, additive chains, single additive expressions, multiplicative chains, single multiplicative expressions, shift chains, single shift expressions, bitwise-AND chains, single bitwise-AND expressions, bitwise-XOR chains, single bitwise-XOR expressions, bitwise-OR chains, and single bitwise-OR expressions at token boundaries, and ensures a single trailing newline. A string/comment-
      Long parenthesized call/argument-list code with a trailing `//` comment
      or balanced trailing `/* ... */` block comment wraps by formatting the
      code fragment and reattaching the exact comment text to the final wrapped
      line. A string/comment-
      aware single scan computes each line's bracket depth (brackets inside
      strings / `//` / nested `/* */` are ignored; block-comment interiors are left
      verbatim). Broader wrapping contexts remain conservative follow-up work. Every reformat is verified by
      **re-lexing the output and requiring identical parser tokens plus ordinary
      comment trivia** (same kinds + text), so `fmt` can only change whitespace,
      never code or comments (it refuses to write otherwise). `fmt <file|dir>` formats in place (recurses dirs,
      skipping hidden/`target`); `--check` lists unformatted files and exits
      non-zero (CI gate); `--emit stdout` requires a single `.otter` file,
      prints only the formatted source, and leaves the file untouched for editor
      format-on-save integrations. Idempotent; verified across all 22 examples (0 token-
      stream violations; formatted output runs identically). Focused compiler,
      CLI, and LSP tests cover formatting/token-safety, including control
      keywords before unary conditions/scrutinees and a single
      comparison-expression guard that wraps only the top-level comparison and
      leaves nested comparison-like operands intact. CLI coverage also pins
      user-visible normalization of async generic return types and async
      closure arrows from malformed input such as `Future < null >` and
      `Task.spawn(()async => { ... })`, including generic return types inside
      function-type fields such as `(i64) => Future<i64>`. Follow-up: broaden line
      wrapping beyond parenthesized/bracketed/type-led brace/generic-angle/named-import brace/named-import paths/named-import closing paths/single-argument attributes/attribute argument lists/inline attribute item forms/stacked inline attribute item forms/macro block bodies/ambient-namespace import paths/extern type declarations/extern var declarations/extern function declarations/function declaration headers/function bodies/module declaration headers/module bodies/module body member lists/interface-or-extend declaration headers/struct declaration headers/type-alias declaration headers/map-literal brace/match-arm brace/record-declaration brace comma lists/record field type-annotation union pipe lists/return expressions/break expressions/await/spawn operand expressions/test/bench declaration headers/test/bench bodies/assignment expressions/for iterator headers/for bodies/while condition headers/if/else-if condition headers/match scrutinee headers/match guard headers/unguarded match arm bodies/top-level parenthesized arrow closure bodies/explicit trailing closure bodies/implicit trailing closure bodies/anonymous function expression headers/async block bodies/block expression bodies/loop bodies/else bodies/if bodies/while bodies/type-alias union pipe lists/var initializer expressions/var type-annotation union pipe lists/parameter type-annotation union pipe lists, including full multi-parameter layouts, function return union pipe lists/arrow closure return union pipe lists/interface/extend declaration headers, struct declaration headers, type-alias declaration headers, interface/extend bound plus lists/interface-or-extend body member lists/generic type-parameter bound plus lists, including full multi-parameter layouts/cast chains/single cast expressions/method chains/single method expressions/logical chains/single logical expressions/comparison expressions/single comparison expressions/additive chains/single additive expressions/multiplicative chains/single multiplicative expressions/shift chains/single shift expressions/bitwise-AND chains/single bitwise-AND expressions/bitwise-XOR chains/single bitwise-XOR expressions/bitwise-OR chains/single bitwise-OR expressions. The formatter lives in `compiler::fmt` (shared), and the **LSP exposes it as a
      `document_formatting` provider** (format-on-save in the editor): the handler
      formats the open buffer, verifies the token-preservation invariant, and
      returns a whole-document edit (declining if it would change tokens). 72 LSP
      tests; the VS Code extension picks the capability up automatically.
- [x] **LSP folding ranges**: the server advertises `textDocument/foldingRange`
      and returns deterministic ranges for multiline `{...}` code blocks plus
      multiline nested block comments. The scanner is string-, char-, line-comment,
      and block-comment-aware, so braces inside literals or comments do not create
      bogus editor folds. 3 focused LSP tests cover nested code folds, block-comment
      folds, and ignored braces in strings/comments.
- [x] **LSP local type inlay hints**: the server advertises
      `textDocument/inlayHint` and returns inferred type hints for unannotated
      local `var` bindings. Hints are built from HIR `local_decls`/`local_types`
      joined with AST binding spans, so explicit annotations, parameters, and
      non-`var` bindings are not duplicated. 4 focused LSP tests cover inferred
      labels, annotated-local/parameter skips, nested block bindings, and requested
      range filtering.
- [x] **LSP incremental sync + cached analysis**: the server advertises
      incremental `textDocument/didChange`, applies LSP range edits in order using
      UTF-16↔UTF-8 conversion, and still accepts full-document replacement changes.
      Open-document queries reuse a cached `Compiled` result until the document
      text changes; any open-buffer mutation clears the cache globally so unsaved
      file-backed submodule overlays cannot go stale. 3 focused LSP tests cover
      UTF-16 incremental edits, full-document replacement, and cache reuse versus
      text-change recompilation.
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
      `HttpRegistry` client against it over real TCP (publish with metadata
      sidecar/index/download/verify/search/yank).
- [x] **Publish metadata sidecar** — done: packaging generates sparse-index
      dependency metadata from registry dependencies, rejects path/git sources
      for registry-published packages, the HTTP client sends a length-prefixed
      JSON sidecar with the tarball, and `pkg::server` persists those deps into
      the JSON-lines index (legacy tarball-only uploads still decode as empty
      deps for compatibility).
- [x] **Feature-gated optional dependency resolution** — done: the resolver
      computes a package feature closure from `[features] default`,
      `[package] default-features`, nested feature names, `dep:name`, and
      `name/feature`; optional deps are skipped until activated, requested
      dependency features are carried into registry package resolution, and
      published sparse-index metadata now includes the package feature map.
      Focused resolver tests cover disabled optional deps, default-feature
      activation, dependency-feature activation of optional transitive registry
      deps, and malformed `dep:` entries; CLI coverage proves `otter_fusion lock`
      includes a feature-enabled optional path dependency.
- [x] **Git dependency fetching** — done: `{ git = "...", rev/branch/tag = ... }`
      dependencies are fetched through `pkg::git`, which maintains bare mirror
      caches, resolves branch/tag/default refs to exact commits, materializes
      immutable source checkouts under the documented git store layout, strips
      `.git` metadata, computes deterministic source-tree checksums, and records
      `git+url#rev` plus checksum in `project.lock`. Existing lockfiles keep
      moving branch/default refs pinned; update-style resolution refreshes them.
      Unit tests exercise exact rev, branch, tag, idempotent checkout reuse, and
      lock pinning; CLI coverage locks and runs a real local git dependency via
      `pkg:` import.
- [x] **Multi-major package coexistence** — done: registry requirements are
      partitioned by package name, registry, and semver-compatible range, so
      `shared ^1` and `shared ^2` resolve to separate package instances rather
      than conflicting. The resolved graph stores instance IDs plus contextual
      dependency-name edges; the compiler loads dependency package instances
      under unique keys and resolves package-internal `pkg:name` imports against
      the importing package's own dependency map. Lockfile output remains the
      documented package list, sorted deterministically by name/version/source;
      tree/why/vendor handle duplicate names without collisions. Tests cover
      registry multi-major resolution and an e2e path graph where two libraries
      compile against different `shared` packages with the same import name.
- [x] **Custom GC allocator** (`gc_alloc`) — done; no system-`malloc` contention
      during sweep, and the GC-stress suite actually stresses (see GC §).
- [x] **Concurrent-GC reclamation** — DONE via the world-barrier stop-the-world
      (see GC §): the collector runs while multiple mutators are live, the gate is
      removed, and the deterministic heavy-churn repro that previously SIGSEGV'd
      every run is clean 130/130 under stress, with a regression case in the
      suite.
- [ ] Remaining advanced deferrals noted inline: per-thread TLABs are now **done**
      (see the GC section); the full MMTk Immix move is the remaining
      behavior-neutral throughput follow-up.

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
      (Future-returning) and poll it on the isolate's executor, completing the
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
  guest panic containment, async entry polled from a host `await`; JIT + native
  parity + GC-stress. Keep `docs/26`, examples, LSP and ROADMAP consistent.

## Current state (verified 2026-05-30)

**Linux portability refresh (verified 2026-06-08):** the runtime no longer
requires the unversioned `libffi` development linker symlink. Variadic FFI loads
the platform `libffi` shared library lazily (`libffi.so.8`/older Linux SONAMEs,
`libffi.dylib` plus system/Homebrew locations on macOS), so `cargo test` and
native `otter_fusion build` work on stock Linux runtime images while preserving
the macOS call path. Cargo now registers the maintained file-driven CLI suite
explicitly and disables auto-discovery of the historical monolithic
`crates/cli/tests/run.rs` target, whose embedded sync-style snippets predate the
current no-public-blocking async stdlib contract. Current Linux verification:
`cargo test --workspace --no-run`, `cargo test -p runtime variadic -- --nocapture`,
direct JIT and native runs for `tests/cases/examples/hello.otter` and
`tests/cases/ffi/variadic_snprintf.otter`, and
`cargo test -p cli --test suite -- --nocapture` (749/749 e2e) all pass.

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
  (`serve`), LSP + VS Code extension (multi-file, cross-file goto/refs/rename,
  formatting, quick-fixes, folding ranges).

## What's next (drives goals.txt)

**Immediate (active goals):**
1. **Deeper optimizing compiler work** (goals.txt "Push Otter Fusion from the
   current production-safe backend optimization pass toward a deeper optimizing
   compiler"): design and implement a MIR/SSA-oriented optimization pipeline, or
   an equivalent backend optimization layer, with interprocedural escape
   analysis, broader inlining/cost modeling, constant folding/propagation,
   dead-store/dead-branch elimination, loop-aware optimizations where safe,
   richer devirtualization/monomorphization, target-aware lowering improvements,
   and tighter code emission where it fits the module/provider architecture.
   Preserve semantics, GC/rooting/finalizer safety, observability
   (`otter_fusion emit tokens|ast|hir|clif`, DWARF, `run --time`/`exec --time`),
   and JIT≡native parity. *No behavior change; benchmark each slice with
   `bench` and timing where relevant.*
2. **Finish Otter Fusion end to end** (goals.txt "Finish Otter Fusion end to end
   as a production-ready language"): treat this ROADMAP and the design docs as
   the authoritative backlog, then close every remaining core language,
   compiler, runtime, backend, std/core library, package-manager, tooling,
   LSP/editor, documentation, example, and test-suite item one test-gated slice
   at a time. This includes formatter spacing/wrapping work,
   behavior-neutral GC throughput work, provider/stdlib surface completion, and Phase 7
   `std:engine` unless a later design decision explicitly moves an item beyond
   production-ready scope.

**Recently completed:**
- **Formatter control-keyword unary spacing: DONE.** The shared formatter now
  inserts a required space between condition/scrutinee-leading keywords and
  unary expression starters, so forms such as `if!done`, `while!done`, and
  `match!done` format to `if !done`, `while !done`, and `match !done` while
  preserving the existing token/comment safety gate. Added shared formatter,
  CLI `fmt`, and LSP formatting regressions, rebuilt the CLI, and reformatted
  the bundled stdlib source tree to remove the old spacing residue.
- **Formatter statement-keyword unary spacing: DONE.** The same shared spacing
  rule now covers statement/operand-leading keywords, so `return-1`, `break-2`,
  `await!ready`, and `spawn!ready` normalize to `return -1`, `break -2`,
  `await !ready`, and `spawn !ready` without changing tokens or comments.
  Added shared formatter, CLI `fmt`, and LSP formatting regressions.
- **Formatter async closure/block body indentation: DONE.** Multiline brace bodies
  nested under call/index delimiters now indent from the opened brace rather
  than inheriting an extra outer delimiter level, so `Task.spawn(() async => {
  ... })`, async blocks passed to calls, and plain block expressions such as
  `drive({ ... })`, plus block-form macro invocations such as
  `drive(@Trace("op") { ... })`, keep ordinary block indentation. Added shared formatter, CLI
  `fmt`, and LSP formatting regressions, plus long async/block/macro-expression
  fallback expectations.
- **Task.spawn example formatter refresh: DONE.** `examples/task_spawn.otter`
  has been reformatted with the corrected multiline async closure indentation,
  and was re-run through `otter_fusion run` to verify the plain-task,
  async-task, and cancelled-task outputs still match.
- **Example formatter sweep after brace-body fixes: DONE.** The remaining
  example-format drift was cleaned in `async_thread_spawn.otter`,
  `generic_methods.otter`, `interface_devirt_bench.otter`, and
  `worker_panic.otter`, covering async closure indentation, `for x in (`,
  and match-body indentation. `otter_fusion fmt examples --check` now reports
  all examples formatted, and the four touched examples were run directly.
- **Formatter trailing comment wrapping: DONE.** Long call/argument-list lines
  with a trailing `//` comment or balanced trailing `/* ... */` block comment
  now wrap the code fragment while preserving the exact comment text on the
  final wrapped line. Added shared formatter, CLI `fmt`, and LSP formatting
  regressions under the token/comment safety gate.
- **Reserved `std:sync` channel constructors e2e coverage: DONE.** The planned
  bounded/MPMC constructor names remain exported for diagnostics, but using
  `channel_bounded`, `channel_mpmc`, or `channel_mpmc_bounded` as a function
  value, named call, or namespace-qualified call now has an e2e compile-error
  regression proving the compiler emits planned-feature diagnostics instead of
  executing placeholder Otter bodies.
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
  `Future` (`lang_shared_lock_future`) the caller awaits: acquire → run the body
  closure under the lock (awaiting an `async` body's future until it resolves, so the lock is
  HELD across the body's `await`s — fixing the release-before-await footgun) → clone the
  result out *while held* (via a codegen-emitted clone thunk) → release. Cancel/panic
  release via a per-thread held-lock set (`lang_shared_release_all`) — drained by the
  **worker-panic boundary** (now done; see its entry above) so a panicking lock body
  releases the lock with no poisoning. Only ordinary non-async `Thread.spawn`
  workers cannot lock (the narrowed compile error → use an *async* `Thread.spawn` worker or
  the `spawn` keyword); an async worker polls its future with a real executor and may lock.
  A new sema escape/detachment taint pass
  rejects references that outlive the body (`.clone()` detaches; a returned reference is
  cloned at the boundary). Tests: e2e `tests/cases/concurrency/*` (mutual exclusion under
  contention, held-across-`await`, `try_lock` busy/free, non-reentrancy, escape rejection
  ×4, clone hatch, return clone-out, GC-stress) + runtime/sema/backend unit tests.
  *Limitation:* a float-typed protected value/result is rejected with a clear error
  (uniform integer/pointer body ABI) — wrap in a struct. *Deferred:* `RwLock<T>` (separate
  primitive whose wait-capable read/write acquisition must be async/future-returning; immediate
  probes must be explicit non-waiting `try_*` helpers).
- **`@RefCounted` — opt-in deterministic reference counting (`docs/16` §8.1): DONE.**
  Generalizes the channel-endpoint carve-out into a user-facing object kind: atomic
  strong count, immediate non-waiting `Drop` + free at count 0, ARC retain/release across
  codegen, tracing GC as the cycle backstop. See the Phase-5 GC/Drop entry. Deferred:
  `Weak<T>`; deterministic (vs GC-timed) drop for collection/`union`-held values.

**Advanced deferral audit (remaining entries are explicitly marked):**
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
- **Channel close on last-`Sender` drop** + async receiver iteration termination —
  **done.** `Sender`/`Receiver` endpoint handles are deterministically released,
  last-sender close wakes/drains receivers to `ChannelClosed`, and receiver
  `for await` loops terminate after buffered messages are drained. See the Phase-5
  channel entry and the `@RefCounted` carve-out/generalization.
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
  importable and `Task.spawn` now accepts both ordinary non-async `() => R` and async
  `() => Future<R>` closures, returning a task-specific `std:task` `JoinHandle<R>`
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
  `std:thread` `JoinHandle<R>` remains distinct and intentionally has no
  `cancel()` or `abort()` method. `timeout(fut, ms)` now calls the same
  cancellation hook when the timer wins, so cancellable loser futures such as
  `spawn EXPR` release their suspended task state. `Thread.spawn` remains the
  dedicated OS-thread primitive.
  Tests added for ordinary non-async and async `Task.spawn`, by-value capture snapshots for
  both ordinary non-async and async `Task.spawn` closures (JIT/native), compiler/e2e rejection
  of non-shareable mutable captures through `Task.spawn`, async `Task.spawn` with
  `Shared.lock`, spawn-future cancellation releasing a held lock, mass
  `spawn EXPR` future cancellation releasing captured sender endpoints so
  receivers close promptly, including a 512-task stress-GC storm (JIT + native),
  task-handle
  `cancel()`/`abort()` joining as `Cancelled`, `abort()` sharing the same
  suspended-state cleanup path as `cancel()` and releasing task-held `Shared`
  locks (JIT + native), ordinary non-async and async `Task.spawn`
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
  ordinary non-async closure `Task.spawn`
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
  explicit native/JIT parity for `Task.spawn` ordinary non-async workers, async workers,
  `Shared.lock`, `JoinHandle.join()`, and cancellation joining as `Cancelled`,
  negative compiler/e2e coverage proving OS-thread `JoinHandle` has neither
  `cancel()` nor `abort()`, LSP hover/type-table coverage for `Task.spawn`
  returning `JoinHandle<R>` and task-only `join`/`cancel`/`abort` method types
  including `Cancelled`, `examples/task_spawn.otter` covering ordinary non-async workers,
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
  parity and direct stdin/stdout/stderr async e2e coverage.
  Target shape:
  `Task.spawn` has the same
  surface as `Thread.spawn` (ordinary non-async or async closure → `JoinHandle<R>` with
  `join`/`detach`) and the same safety model (by-value capture snapshots for
  cross-task isolation + `Shared<T>`/channels for shared state), but scheduled on
  the executor instead of one OS thread per worker. **Complements, not replaces,**
  `Thread.spawn` (kept for CPU-heavy or OS-thread-affine work). **Folds in cancellation
  teeth** (`docs/21` §8): on the executor, `future.cancel()` stops polling the task
  and drops its state machine (running drops, releasing held `Shared` locks /
  endpoints via the per-task release path); `Task.spawn`'s
  `JoinHandle.cancel()`/`abort()` + a `Cancelled` join result
  (`Joined<R> | Panicked | Cancelled`); `timeout()`/`select!` losers ride the same
  path. Cancellation is **cooperative** (effective at the next `await`) — no
  forceful kill; a `Thread.spawn` OS-thread worker has no hard kill (cooperative
  `Shared<bool>`/channel signal only). Explicitly *not* stackful
  goroutine-style green threads with implicit suspension and no explicit async
  surface: that is a second concurrency model in tension with the
  explicit-async design and needs Go-runtime-level syscall handoff + preemption.
- **FFI tail — done:** `@CallConv` decorator; a managed `CString`/`Buffer` handle
  type with `Drop`; `@Variadic` via `libffi` (Cranelift has no portable varargs
  ABI, so variadic calls are marshalled through `ffi_prep_cif_var`/`ffi_call` —
  see the completed-work entry above).
- **Generic `Drop` types; generic-interface default methods; cross-module
  interface default methods** — **done.** Generic `Drop` finalizers are
  registered per monomorphization, generic-interface defaults substitute
  interface type parameters into copied signatures and bodies, and cross-module
  defaults are expanded from imported `pub` interfaces with ambiguity preserved
  as diagnostics rather than silently selecting the wrong body.
- **Package manager advanced:** advanced deferrals are complete as of multi-major
  coexistence; keep hardening registry/package-manager behavior through tests.
- **GC throughput:** per-thread TLABs **done** (~2× multi-thread alloc); the full
  MMTk Immix move remains (behavior-neutral).
- **`fmt` follow-up:** broaden line wrapping beyond parenthesized/bracketed/type-led brace/generic-angle/named-import brace/named-import paths/named-import closing paths/single-argument attributes/attribute argument lists/inline attribute item forms/stacked inline attribute item forms/macro block bodies/ambient-namespace import paths/extern type declarations/extern var declarations/extern function declarations/function declaration headers/function bodies/module declaration headers/module bodies/module body member lists/interface-or-extend declaration headers/struct declaration headers/type-alias declaration headers/map-literal brace/match-arm brace/record-declaration brace comma lists/record field type-annotation union pipe lists/return expressions/break expressions/await/spawn operand expressions/test/bench declaration headers/test/bench bodies/assignment expressions/for iterator headers/for bodies/while condition headers/if/else-if condition headers/match scrutinee headers/match guard headers/unguarded match arm bodies/top-level parenthesized arrow closure bodies/explicit trailing closure bodies/implicit trailing closure bodies/anonymous function expression headers/async block bodies/block expression bodies/loop bodies/else bodies/if bodies/while bodies/type-alias union pipe lists/var initializer expressions/var type-annotation union pipe lists/parameter type-annotation union pipe lists, including full multi-parameter layouts, function return union pipe lists/arrow closure return union pipe lists/interface/extend declaration headers, struct declaration headers, type-alias declaration headers, interface/extend bound plus lists/interface-or-extend body member lists/generic type-parameter bound plus lists, including full multi-parameter layouts/cast chains/single cast expressions/method chains/single method expressions/logical chains/single logical expressions/comparison expressions/single comparison expressions/additive chains/single additive expressions/multiplicative chains/single multiplicative expressions/shift chains/single shift expressions/bitwise-AND chains/single bitwise-AND expressions/bitwise-XOR chains/single bitwise-XOR expressions/bitwise-OR chains/single bitwise-OR expressions
  (ordinary comment-trivia preservation, common spacing, sensitive comment
  boundary spacing, and the first eighty-one wrapping slices are in place).
- **Embedding engine (`std:engine`, `docs/26`)** — run guest Otter Fusion inside a
  sandboxed isolate (capability whitelist, host bindings, `@Bridge` copy-by-value
  ABI, per-isolate heap/GC + hard limits). Design done (`docs/26`, Phase 7);
  substrate proven by the macro JIT. Largest new piece is per-isolate runtime state.

## Historical: initial vertical-slice target (achieved long ago)
Smallest end-to-end program that exercised the full pipeline at the start:
`function main() { print_int(40 + 2) }` → JIT → prints `42`. The slice has since
been expanded to the entire language.
