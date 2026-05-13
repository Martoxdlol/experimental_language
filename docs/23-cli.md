# 23. CLI and Toolchain

This chapter specifies the command-line interface and toolchain layout. The placeholder binary name is `lang`; the final name is not yet decided.

## 23.0 Shape

One binary, subcommands. Two layers:

- **Front-end as a library** (`liblangc`): lexer, parser, resolver, type-checker, IR lowering, codegen driver, formatter, doc extractor.
- **Driver binary** (`lang`): argument parsing, project loading, dependency resolution, orchestration, file I/O, process spawning (linker, debuggers).

Every subcommand below is a thin orchestrator over `liblangc` + a small set of helpers. The same library is what an LSP, an external build system, or a third-party tool would embed.

## 23.1 Global flags

These apply to every subcommand.

| Flag | Purpose |
|---|---|
| `--manifest <path>` | Override `project.toml` location. Default: walk upward from cwd. |
| `--target-dir <path>` | Override build output root. Default: `<project>/target/`. |
| `--profile <name>` | Named build profile (`debug`, `release`, or any user-defined). Default: `debug`. |
| `--target <triple>` | Cross-compile target. Default: host. |
| `--jobs <N>`, `-j <N>` | Parallelism. Default: physical cores. |
| `--offline` | Forbid network access (registry, git). |
| `--frozen` | `--offline` + refuse to update the lockfile. |
| `--locked` | Allow reads but refuse to write the lockfile. |
| `--config <key=value>` | Override one config setting (repeatable). |
| `--color <auto\|always\|never>` | |
| `--message-format <human\|json\|short>` | Diagnostic output format; `json` is the LSP/IDE feed. |
| `--verbose`, `-v` / `-vv` / `-vvv` | Driver chatter level. |
| `--quiet`, `-q` | Suppress non-error output. |
| `--no-default-features`, `--features <list>` | Conditional compilation toggles (see §23.10). |
| `--cfg <key[=value]>` | Ad-hoc compile-time `cfg` flag. |
| `--sysroot <path>` | Override the built-in stdlib location (see §23.6). |
| `--toolchain <name>` | Pin a specific installed toolchain (see §23.13). |
| `--help`, `-h` / `--version`, `-V` | Standard. |

## 23.2 Subcommand map

```
lang new        <path> [--lib|--bin|--lib+bins]   create a new project
lang init       [--lib|--bin|--lib+bins]          init in the cwd
lang build                                        compile to the default artifact
lang check                                        parse + typecheck, no codegen
lang run        [-- args...]                      build (if bin) + execute
lang test       [filter] [--bench]                build + run tests
lang bench      [filter]                          build + run benches
lang fmt        [paths...]                        format files
lang lint       [paths...]                        run lints
lang fix                                          apply auto-fixable lints / migrations
lang doc        [--open]                          generate API docs
lang repl       [--script file]                   interactive REPL
lang lsp                                          start the language server (stdio)
lang explain    <error-code>                      long-form diagnostic explanation
lang expand     [--macro <name>]                  print macro-expanded source

lang emit       <stage> [paths...]                dump a specific compilation stage (§23.4)
lang inspect    <thing>                           introspection (types, modules, deps)
lang clean      [--profile <p>] [--target <t>]    delete build artifacts

lang add        <pkg>[@<ver>] [--dev]             add a dependency
lang remove     <pkg>                             remove a dependency
lang update     [<pkg>] [--precise <ver>]         refresh the lockfile
lang tree       [--depth N] [--duplicates]        print the resolved dep graph
lang why        <pkg>                             why is <pkg> in the graph?
lang vendor     [--path <dir>]                    copy all deps into the project
lang lock       [--check]                         regenerate / validate the lockfile

lang search     <query>                           search the registry
lang publish    [--dry-run]                       publish to the registry
lang yank       <pkg>@<ver>                       yank a published version
lang login      / logout                          registry credentials
lang audit                                        check deps against vulnerability DB

lang toolchain  list|install|default|remove|run   manage toolchain versions (§23.13)
lang target     list|add|remove                   manage installed cross-targets
lang sysroot    show|build                        inspect / rebuild the stdlib (§23.6)
lang ffi        bindgen|cbindgen|check|layout     FFI binding generation & validation (§23.12)

lang script     <file>                            run a single .lang file shebang-style
lang completions <shell>                          emit shell completion script
lang config     get|set|list                      read/write user/project config
lang env                                          print the effective build environment
lang version    [--all]                           version (driver, frontend, linker, …)
lang self       update|uninstall                  manage the `lang` install itself
```

## 23.3 `lang build` — the main path

```
lang build [--bin <name>] [--lib] [--all-targets]
           [--emit <kinds>] [--link-args <…>]
           [--out-dir <path>] [--keep-temps]
           [--lto <off|thin|fat>] [--strip <none|debug|all>]
           [--debug-info <none|line|full>] [--codegen-units N]
           [--panic <unwind|abort>] [--threads <N>]
```

Output layout under `target/`:

```
target/
  <profile>/                e.g. debug/, release/, custom-foo/
    <triple>/               e.g. x86_64-darwin/
      bin/<name>            executables
      lib<name>.{a,dylib}   libraries
      deps/                 per-crate object & metadata
      build/                build-script outputs
      incremental/          incremental compilation cache
      examples/, tests/, benches/
  cache/                    content-addressed: parser ASTs, type tables, codegen IR
  doc/                      output of `lang doc`
  tmp/                      transient files when --keep-temps is set
```

`build` is idempotent and incremental: hashes of (source bytes + manifest + toolchain version + cfg set + dep fingerprints) key into `target/cache`.

## 23.4 `lang emit` — intermediate-stage artifacts

This is the "show me the lexer / parser / IR" entry point. It pulls from the pipeline without altering the normal build cache.

```
lang emit <stages> [<file>|<module>|--all] [-o <out>] [--format <fmt>]
```

`<stages>` is a comma-separated list (or `all`). The stages, in pipeline order:

| Stage key | What it produces | Default format |
|---|---|---|
| `tokens` | Lexer token stream | one token per line, or `json` |
| `cst` | Concrete syntax tree (lossless, with trivia) | `s-expr` / `json` |
| `ast` | Abstract syntax tree (post-desugar of literals etc.) | `s-expr` / `json` |
| `resolved-ast` | AST with names resolved (every identifier → symbol id) | `json` |
| `mod-graph` | Module-tree dot graph | `dot` / `json` |
| `import-graph` | Import edges between modules | `dot` / `json` |
| `types` | Type-checked AST with inferred types | `json` / annotated source |
| `hir` | High-level IR (post-typeck, generics still abstract) | `text` |
| `mono` | Generics monomorphized | `text` |
| `mir` | Mid-level IR (CFG, after borrow/RC analysis) | `text` |
| `lir` | Low-level IR (machine-ish, pre-codegen) | `text` |
| `llvm-ir` / `cranelift-ir` | Backend IR | `.ll` / text |
| `asm` | Target assembly | `.s` |
| `obj` | Native object file(s) | `.o` |
| `archive` | Static archive | `.a` |
| `dylib` | Dynamic library | `.so` / `.dylib` / `.dll` |
| `bc` | LLVM bitcode (if LLVM backend) | `.bc` |
| `metadata` | Crate metadata (public surface, types, generics) | `.langmeta` (compact binary) |
| `docs-json` | Doc model (input to `lang doc`) | `json` |
| `deps-make` | Make-style `.d` dep file | `text` |
| `coverage-map` | Source-coverage map | `json` |

Examples:

```
lang emit tokens src/util/log.lang
lang emit ast,types --format json src/lib.lang -o ast.json
lang emit obj --all -o target/objs/        # one .o per module
lang emit llvm-ir,asm --release src/main.lang
lang emit mod-graph --format dot | dot -Tpng > modules.png
```

`lang emit obj` together with `lang emit metadata` is the **separate-compilation contract**: a third-party build system can drive per-module compilation and call the system linker itself. That is the seam for "compile to objects I can link later".

## 23.5 `lang inspect` — introspection over the resolved program

A read-only query interface backed by the same frontend as `lang lsp`. Useful in CI, scripts, and IDE plugins that aren't full LSP clients.

```
lang inspect module      <path>             metadata, submodules, pub set
lang inspect symbol      <fq-name>          definition site, type, doc, references
lang inspect type        <expr-or-name>     resolved type, layout, size, align
lang inspect deps        [--depth N]        full resolved dep graph
lang inspect features                       active feature set
lang inspect cfg                            effective cfg keys/values
lang inspect manifest                       fully-resolved manifest (after profile/cfg merge)
lang inspect targets                        list of build targets in this package
lang inspect coverage    <run-id>           coverage from a prior `test --coverage`
```

All output streams cleanly under `--message-format json`.

## 23.6 Standard library replacement — `--sysroot` and `core:` / `std:`

### Default behavior

The toolchain ships a prebuilt sysroot per installed target:

```
~/.lang/toolchains/<channel>/lib/sysroot/<triple>/
  core/                  source + prebuilt .langmeta + .a
  std/                   source + prebuilt .langmeta + .a
  manifest.toml          versions, supported targets, ABI hash
```

The driver locates the sysroot by:

1. `--sysroot <path>` CLI override.
2. `[build] sysroot = "..."` in the project manifest.
3. `LANG_SYSROOT` env var.
4. The active toolchain's default.

### Replacing the stdlib

Two distinct things to allow replacing:

| You want… | Mechanism |
|---|---|
| A different `core:` (e.g. freestanding, custom allocator interfaces) | `[build] sysroot = "<path>"`, where `<path>/core/manifest.toml` declares it provides `core`. |
| A different `std:` (e.g. embedded `std` shim) | Same, providing `std`. |
| Build the sysroot **from source** (e.g. with custom features) | `lang sysroot build --features <…> --target <triple>` — analogous to Rust's `-Z build-std`. |
| Drop `std:` entirely (freestanding) | Manifest `[package] no-std = true`. Importing `std:*` becomes a hard error; `core:` remains. |
| Pin a specific sysroot per dependency | Manifest `[dependencies.foo] sysroot = "..."` (rare; mainly for FFI-shim crates). |

A sysroot is a **versioned, ABI-tagged artifact**. The driver refuses to link an object compiled against sysroot ABI `X` into a binary linking sysroot ABI `Y` (mismatch → diagnostic with the two hashes).

`core:prelude` auto-import (see [17-modules.md §17.8](./17-modules.md#178-built-in-modules--core-and-std)) is implemented as an implicit `import * from "core:prelude"` prepended by the resolver. Replaceable sysroots replace the *content* but not the *injection*.

### Subcommands

```
lang sysroot show                                 prints the resolved sysroot path
lang sysroot show --json                          full manifest + ABI hash
lang sysroot build [--features …] [--target …]    rebuild from source into target/sysroot/
lang sysroot verify                               sanity-check that core/std link cleanly
```

## 23.7 How dependencies and libraries load

### Background — how Rust does it

Rust separates **where source comes from** from **where compiled artifacts live**. Three locations matter:

1. **`CARGO_HOME`** (default `~/.cargo/`): user-global state.
   - `registry/index/<registry-host>/` — sparse-index cache (since 1.70 this replaces the old full git clone of the index).
   - `registry/cache/<registry-host>/` — downloaded `.crate` tarballs.
   - `registry/src/<registry-host>/` — extracted source trees (read-only; this is what the compiler reads).
   - `git/` — checkouts of git dependencies, keyed by URL + rev hash.
   - `bin/` — installed binaries from `cargo install`.
   - `config.toml` — user-global Cargo config (registry mirrors, build settings, credentials path).
   - `credentials.toml` — registry tokens.

2. **`target/`** (per-project): compiled artifacts, lockfile-driven, never shared across projects by default. `target/<profile>/deps/lib<crate>-<hash>.rlib` is one compiled dep. The hash is a fingerprint of (source + features + profile + rustc version + dep fingerprints) — that is how Cargo achieves "rebuild only what changed" and "different projects don't poison each other".

3. **`RUSTUP_HOME`** (default `~/.rustup/`): toolchains themselves.
   - `toolchains/<channel>/bin/{rustc,cargo,…}` — the compiler binaries.
   - `toolchains/<channel>/lib/rustlib/<triple>/lib/lib{core,std,…}-<hash>.rlib` — the **sysroot**. This is how `use std::…` resolves: rustc has `--sysroot` baked into its config and finds `libstd` there.

The resolution flow for `use foo;`:

1. Cargo reads `Cargo.toml`, resolves the dep graph against `Cargo.lock`, downloads any missing crates to `CARGO_HOME/registry/src/`, and tells rustc: "the crate named `foo` is at this path, here is its `--extern foo=<path-to-rlib>`".
2. rustc never searches a `node_modules`-like directory at compile time. It only ever looks at what `--extern` flags it was given, plus the sysroot.
3. The lockfile pins exact versions. The build is reproducible given the same lockfile + toolchain + sysroot.

Key properties that fall out:

- **No nested copies.** Each version of each crate is downloaded once into `CARGO_HOME` and compiled (per profile / features / target) once into `target/`.
- **One version per (name, semver-compatible range) in a build.** Cargo's resolver unifies `^1.2` and `^1.4` → `1.4`. Two majors of the same crate **can** coexist (`foo` 1.x and `foo` 2.x both linked, distinguished by symbol mangling). This is fundamentally different from npm.
- **Lockfile is the source of truth, manifest is the constraint.** `Cargo.toml` says "I want `foo ^1.2`"; `Cargo.lock` says "we resolved to `foo 1.4.7`".
- **No runtime path resolution.** By the time the binary runs, all dependency code is statically embedded (or dynamically linked at known paths). There is no `require()` walk.

### Why `node_modules` is a problem

- Filesystem nesting (pre-npm-3) or hoisting + phantom deps (npm 3+) — both leak un-declared deps into your code.
- Resolution happens at *runtime* via `require` / `import` walking parent dirs. Slow, ambiguous, and a security surface (typosquatting + transitive execution).
- One copy per (parent, name) in old npm, hoisted in new npm — same package can be loaded multiple times under different paths and fail `instanceof` checks.
- `postinstall` scripts run untrusted code on `npm install`. Cargo deliberately doesn't have this; build scripts only run when you actually build a project that uses the crate.
- Lockfile (`package-lock.json`) historically diverged from `node_modules` state. pnpm and yarn pnp fix parts of this with content-addressed stores + symlinks / virtual FS, but the model is still walk-based at runtime.

The good idea in node is the content-addressed global cache (pnpm). The bad ideas are runtime path resolution, hoisting, and install-time scripts.

### This language's model

Adopt the Rust model with two refinements:

1. **Sparse HTTP registry index by default** (no giant git clone — Rust learned this the hard way).
2. **Content-addressed global cache, hardlinked into project** (pnpm's good idea) — saves disk and makes "compile from source" cheap. The compiler still receives explicit `--extern` flags; the filesystem layout is an implementation detail.

Concretely:

```
~/.lang/
  toolchains/<channel>/                  toolchain binaries + sysroot (§23.6)
  registry/
    index/<registry-host>/               sparse-index cache
    src/<registry-host>/<pkg>/<ver>/     extracted source (content-addressed)
    cache/<registry-host>/<pkg>/<ver>/   raw tarballs
  git/<url-hash>/<rev>/                  git deps
  bin/                                   `lang install` targets
  config.toml                            user-global config
  credentials.toml                       registry tokens

<project>/
  project.toml                           manifest
  project.lock                           lockfile (committed for bins, optional for libs)
  target/                                build artifacts (§23.3)
  .lang/                                 per-project local state (vendoring opt-in)
```

Resolution flow at build time:

1. Driver reads `project.toml`, computes the dep graph, resolves against `project.lock` (writing it on first build or after `lang update`).
2. Missing packages: fetch into `~/.lang/registry/`.
3. Compile each `(pkg, version, profile, features, target)` once into `target/deps/`, fingerprinted exactly like Cargo. Hardlink from the content-addressed source dir; never copy.
4. Pass explicit `--extern pkg=<path-to-metadata>` to each module compilation. The compiler **never** walks the filesystem looking for packages. The `pkg:` prefix in source resolves only through `--extern`.

Path forms (see [17-modules.md §17.4](./17-modules.md#174-path-forms)) map cleanly:

| Source syntax | Compiler input |
|---|---|
| `import "x/y"` | absolute in current crate → known file |
| `import "./x"` | relative in current crate → known file |
| `import "pkg:foo/bar"` | `--extern foo=<meta>`, then lookup `bar` in foo's pub tree |
| `import "core:prelude"` | sysroot |
| `import "std:io"` | sysroot |

### Lockfile rules

- Binary packages: `project.lock` **must** be committed.
- Library packages: `project.lock` is generated for the lib's own dev/test builds but is **ignored by consumers**.
- `--locked` / `--frozen` / `--offline` give CI / reproducibility levers (see §23.1).
- `lang lock --check` is the CI gate: fails if a build would mutate the lockfile.

### Vendoring (for air-gapped or supply-chain-locked environments)

```
lang vendor --path third_party/      copy all resolved deps into third_party/
                                     write [source.vendored-sources] override into config
```

After `vendor`, the project builds with no network and no `~/.lang/registry` reads. Identical model to `cargo vendor`.

### Workspaces

`project.toml` at the root with `[workspace] members = ["crates/*", "tools/foo"]`. One `target/`, one lockfile, shared dep resolution. Each member has its own `project.toml` declaring its kind (`binary` / `library` / `library+bins`).

## 23.8 Manifest — `project.toml`

The parts the CLI consults:

```
[package]
name = "myapp"
version = "0.3.1"
kind = "binary"                # or "library", "library+bins"
entry = "src/main.lang"
edition = "2026"               # language edition pin; CLI rejects mismatch
license = "MIT"
authors = [...]
description = "..."
repository = "..."
no-std = false                 # if true, std: imports are forbidden
sysroot = "..."                # optional override
default-features = ["json"]

[bins]                         # only when kind = "library+bins"
files = ["src/bin/foo.lang", "src/bin/bar.lang"]

[dependencies]
serde = "1.2"
http = { version = "0.4", features = ["tls"] }
foo-local = { path = "../foo" }
bar-git = { git = "https://...", rev = "abc123" }
plugin = { registry = "internal" }

[dev-dependencies]
test-utils = "0.1"

[build-dependencies]
codegen = "0.5"

[features]
json = ["dep:serde"]
tls = []
default = ["json"]

[profile.debug]
opt-level = 0
debug-info = "full"
panic = "unwind"
incremental = true

[profile.release]
opt-level = 3
lto = "thin"
debug-info = "line"
strip = "debug"
panic = "abort"
codegen-units = 1

[profile.bench]
inherits = "release"
debug-info = "full"

[target.x86_64-linux-gnu]
linker = "lld"
langflags = []                 # per-target extra flags

[registries]
internal = { index = "https://internal-registry/index" }

[build]
target-dir = "target"
jobs = 0                       # 0 = auto

[scripts]                      # `lang run-script <name>`
ci = "lang fmt --check && lang lint && lang test"
```

`lang config get/set` reads/writes the user-global `~/.lang/config.toml` with the same schema; project manifest wins on conflict; CLI `--config` wins over both.

## 23.9 Build profiles

- `debug`, `release` are predefined; both fully overridable in the manifest.
- User-defined profiles inherit via `inherits = "release"`. Activated with `--profile <name>`.
- Profile + target triple + feature set form the fingerprint key — switching profiles never invalidates the other profile's cache.

## 23.10 Features and cfg

Two layers:

- **Features**: declared in the manifest, additive booleans that gate optional code and optional deps. Resolved at dep-resolution time; the resolver unifies the feature set across the graph.
- **cfg**: arbitrary compile-time predicates: `cfg(target_os = "linux")`, `cfg(feature = "json")`, `cfg(debug_assertions)`, `cfg(any(unix, windows))`. Source uses `#[cfg(...)]` attributes; CLI exposes `--cfg key=value`.

CLI levers:

```
--features a,b,c
--no-default-features
--all-features
--cfg my_flag
--cfg target_endian=little
```

`lang inspect cfg` prints the effective set.

## 23.11 Cross-compilation and targets

```
lang target list                          installed targets
lang target add aarch64-linux-musl        download/build the sysroot for that triple
lang target remove <triple>
lang build --target aarch64-linux-musl
```

Per-target manifest section (`[target.<triple>]`) sets linker, link args, runner (for `lang test` on a foreign target via QEMU, etc.).

Freestanding / no-OS targets (e.g. `thumbv7em-none-eabi`) imply `no-std = true` automatically; importing `std:*` fails at resolve time with a target-specific diagnostic.

## 23.12 FFI and linking

[19-ffi.md](./19-ffi.md) defines the source-level FFI surface. The CLI layer covers three concerns: declaring which native libraries to link, generating bindings from C headers, and emitting C headers for exported functions.

### Manifest — `[package.links]` and `[package.ffi]`

```toml
[package.links]
zlib    = { lib = "z",      kind = "dynamic", version = ">=1.2" }
mylib   = { lib = "mylib",  kind = "static",  path = "vendor/libmylib.a" }
crypto  = { lib = "crypto", kind = "dynamic" }

[package.ffi]
# Bindings auto-generated from C headers, regenerated when the header changes.
bindings = [
  { header    = "vendor/zlib.h",
    output    = "src/bindings/zlib.lang",
    allowlist = ["inflate*", "deflate*"] },
  { header = "vendor/mylib.h",
    output = "src/bindings/mylib.lang" },
]

# Headers auto-generated from this crate's pub extern functions.
exports = [
  { module = "lib", output = "target/include/mylib.h" },
]
```

`@Link(lib = "name")` in source refers to a `[package.links]` entry by name. A function declaring only `@Symbol("...")` with no `@Link` resolves from the host process (libc, platform symbols).

`lang build` runs `bindgen` automatically for any `[package.ffi.bindings]` entry whose header is newer than its output. Likewise for `[package.ffi.exports]` and `cbindgen`.

### `lang ffi` subcommands

```
lang ffi bindgen <header> [--output <file>] [--allowlist <pat>] [--blocklist <pat>]
                          [--types-only] [--lang-edition <year>]
        Generate extern struct, extern type, extern function, and extern var
        declarations plus pub var constants from a C header. C enums lower to a
        type alias + pub var constants; C unions lower to @Union extern struct.
        Powered by libclang. Output is deterministic — safe to commit.

lang ffi cbindgen [--module <path>] [--output <file>] [--lang <c|c++>]
                  [--header-guard <name>]
        Emit a C header declaring every pub extern function and pub extern struct
        reachable from <module>. Round-trips with bindgen.

lang ffi check [--against <header>] [--strict]
        Verify this crate's extern signatures match the named C header.
        Reports missing symbols, changed argument types, struct layout diffs.
        Exit 0 if clean, 1 on drift. CI gate.

lang ffi layout [<type>] [--target <triple>] [--format <human|json>]
        Print size, alignment, and field offsets for one or all extern struct types
        in the current crate, computed for the named target. Debugging ABI mismatches.
```

Examples:

```
lang ffi bindgen vendor/zlib.h --output src/bindings/zlib.lang --allowlist "inflate*"
lang ffi cbindgen --module lib --output target/include/mylib.h
lang ffi check --against vendor/zlib.h
lang ffi layout Sockaddr --target aarch64-apple-darwin
```

### Build scripts

For native libraries whose build process must run before the main build (autoconf, cmake, generated headers), declare a build script:

```toml
[build-script]
file = "build.lang"           # compiled and run before the main build, emits link directives
```

A build script writes directives on stdout that the driver consumes:

```
lang:link-lib=dylib=z
lang:link-search=native=/opt/lib
lang:rerun-if-changed=vendor/libmylib.a
lang:emit-cfg=has_openssl
lang:ffi-bindgen=vendor/generated.h:src/bindings/generated.lang
```

Same model as Cargo build scripts, with the same security caveat: build scripts are arbitrary code. The driver:

- Does not run build scripts under `lang check`.
- Sandboxes them when possible (filesystem read-only outside `OUT_DIR`).
- Prints a clear "running build script for <pkg>" line so they are never silent.

## 23.13 Toolchain management

```
lang toolchain list
lang toolchain install stable
lang toolchain install nightly-2026-05-01
lang toolchain default stable
lang toolchain run nightly -- build
lang toolchain remove <name>
```

Project-pin file at `<project>/.lang-toolchain.toml`:

```
[toolchain]
channel = "stable"
components = ["fmt", "lsp", "src"]
targets = ["x86_64-linux-gnu", "aarch64-apple-darwin"]
```

`lang` is itself a **proxy** that dispatches to the active toolchain's real binary — same shape as `rustup`. This is what lets multiple projects on one machine use different language versions without conflict.

## 23.14 Testing, benching, fuzzing, coverage

```
lang test                                run all tests
lang test mymod::                        filter by path
lang test --bench                        include benches
lang test --coverage                     emit coverage map; `lang inspect coverage` reads it
lang bench
lang fuzz <target> [--time 60s]          if a fuzz subcommand ships
```

Test output formats: `--format human|json|junit|tap`.

## 23.15 LSP and editor integration

```
lang lsp                                  speaks LSP over stdio
lang lsp --port 9999                      TCP (debugging only)
lang lsp --log <path>                     diagnostic log
```

The same `liblangc` powers it. `--message-format json` on `build` / `check` / `emit` is the IDE feed when LSP isn't an option.

## 23.16 Macro expansion and debugging

```
lang expand src/main.lang                 print fully macro-expanded source
lang expand --macro my_macro              only expand that macro
lang expand --stage parse|typeck          expansion at different pipeline points
```

This is non-negotiable for a language with macros (see [22-macros.md](./22-macros.md)). The macro author and the macro user both need to see post-expansion source to debug.

## 23.17 Docs

```
lang doc                                  generate target/doc/<pkg>/index.html
lang doc --open
lang doc --emit json                      structured doc model (target/doc-json/)
lang doc --private                        include non-pub items
lang doc --examples                       run examples as tests too
```

The doc model is `liblangc`'s public API — third-party doc tools can consume `docs-json` (§23.4) instead of re-parsing.

## 23.18 REPL and scripting

```
lang repl                                  interactive
lang repl --load src/lib.lang              with a module pre-loaded
lang script foo.lang                       single-file run, no manifest needed
                                           uses an implicit project with no deps
```

For shebang use: `#!/usr/bin/env -S lang script` at the top of `.lang` files.

## 23.19 Config precedence (highest wins)

1. CLI `--config key=value` and `--feature` etc.
2. Environment variables (`LANG_*`).
3. `<project>/.lang/config.toml` (per-project, not committed).
4. `project.toml`.
5. `~/.lang/config.toml`.
6. Built-in defaults.

`lang env` prints the fully resolved configuration with each value's source. Critical for "why did the build do that?" debugging.

## 23.20 Exit codes

Every subcommand follows the same convention.

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic failure (the subcommand's normal "didn't work") |
| 2 | Usage / argument error |
| 101 | Internal compiler error (ICE) — emit a bug-report prompt |
| 130 | Interrupted (SIGINT) |
| Custom | Test failures pass the test runner's count |

## 23.21 Things easy to miss

A checklist of small things that bite later if not designed in early.

- **Stable diagnostic codes** (`E0123`) — wire `lang explain E0123` from day one; users come to rely on stable codes for grep, suppressing, and CI rules.
- **Determinism flag**: `--deterministic` forbids timestamps, ASLR'd hashes, parallel-codegen reordering. Reproducible-build people will thank you.
- **`--print` introspection**: rustc-style `lang --print sysroot|target-list|cfg|target-features|file-names`. Build-system integrations live on this.
- **Color / no-color env vars**: respect `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`.
- **CI-friendly progress**: `--message-format short` for grep, `json` for parsers, plain human elsewhere; never emit ANSI control codes in non-TTY without `--color always`.
- **Editor save-on-format**: `lang fmt --check` for CI, `lang fmt --emit stdout` for editor "format on save" without writing.
- **`--dry-run`** for `publish`, `update`, `clean`, `vendor`, `remove`. Every destructive command should have it.
- **`lang clean`** must accept granularity: `--profile`, `--target`, `--package`, `--doc`. Wiping the whole `target/` is annoying when you've got a half-hour LLVM build cached.
- **First-class plugins / external subcommands**: `lang foo` falls back to `lang-foo` on `$PATH`. Lets community tools ship without core changes.
- **Hook points for build scripts vs. proc macros vs. plugins** should be three distinct, documented mechanisms — don't conflate them like npm conflates `postinstall` with everything.
- **`lang audit`** — wire it to a vulnerability DB endpoint from day one; retrofitting security tooling is painful.
- **`lang self update`** — the toolchain manages itself. If you don't ship this, users will fall behind and the ecosystem will fragment.
- **Standard `OUT_DIR` env var** for build scripts and the standard set of `LANG_*` envs the compiler exports — document these in this chapter and in [19-ffi.md](./19-ffi.md) so they're a stable contract.
- **"What would change" resolver mode**: `lang update --dry-run --verbose` printing the diff of `project.lock`.
- **Single-file ABI for shipping libraries**: pick `.langmeta` (compact, versioned, includes the public surface + generics + cfg gates). Document its compatibility policy explicitly — "metadata is stable across patch versions, may break across minors, definitely breaks across editions" — or the ecosystem will fight you.
- **Shell completions**: `lang completions bash|zsh|fish|powershell|nushell` — small effort, large ergonomics win.
