# 17. Modules and Imports

A module is the unit of source organization and visibility. **One source file is exactly one module** (with the exception of inline `mod` blocks; see 17.2). The module tree is built from explicit `mod` declarations and mirrors the filesystem.

## 17.1 Project layout

A project has:

- A **manifest file** at the project root (toolchain-defined name; treat `project.toml` as the placeholder).
- A **source root**, conventionally `src/`, declared in the manifest.
- An **entry file** declared in the manifest:
  - **Binary**: `kind = "binary"`, `entry = "src/main.lang"`. `main.lang` must export `function main()`. The package produces an executable; it is **not** consumable via `pkg:`.
  - **Library**: `kind = "library"`, `entry = "src/lib.lang"`. The package's public API is everything `pub` in `lib.lang`. Consumers `import { ... } from "pkg:<name>"` resolve against `lib.lang`.
  - **Library + binaries**: `kind = "library+bins"`, plus `bins = ["src/bin/<name>.lang", ...]`. The library entry is `lib.lang`; each bin is an additional entry the compiler walks.
- Dependencies declared in the manifest, available under the `pkg:` prefix.

The compiler starts at each declared entry and walks the `mod` tree (see 17.2). Every `.lang` file under `src/` must be reachable from some entry through a chain of `mod` declarations. **A source file that no `mod` declaration mentions is a hard error**:

```
unreferenced source file: src/util/dead.lang
  expected `mod dead` in src/util.lang (or an ancestor)
```

This is the same strictness as Rust: if it's not in the `mod` tree, the compiler doesn't see it.

## 17.2 Module declarations

A file declares its submodules with `mod` or `pub mod`:

```
// src/lib.lang
mod internals          // private — usable from this file, not visible to pkg consumers
pub mod util           // public — pkg:cool-lib/util is a valid import path for consumers
pub mod db
```

Two forms, exactly:

- **`mod <name>`** — submodule exists; addressable from this file (and its other submodules) but **not** from importers of this module.
- **`pub mod <name>`** — submodule exists; addressable both internally and from importers of this module.

### Resolution to files

A `mod foo` in file `path/to/parent.lang` resolves to `path/to/parent/foo.lang`. The submodule directory shares its parent file's basename.

Top-level entries are the one exception: `mod foo` in `src/lib.lang` (or `src/main.lang`, or any file directly named in `bins`) resolves to `src/foo.lang` — siblings of the entry, not `src/lib/foo.lang`. This matches Rust's crate-root convention and keeps the source root flat.

```
src/
  lib.lang                 // entry: mod util, pub mod db, mod internals
  util.lang                // mod helpers, mod parser
  util/
    helpers.lang           // mod text
    helpers/
      text.lang
    parser.lang
  db.lang                  // pub mod query
  db/
    query.lang
  internals.lang
```

There are no `mod.lang` index files. Every module's source file is named after itself.

### Inline modules

A `mod` declaration may carry a body, defining the module inline:

```
mod conversions {
  pub function as_celsius(f: f64): f64 { (f - 32.0) * 5.0 / 9.0 }
  pub function as_fahrenheit(c: f64): f64 { c * 9.0 / 5.0 + 32.0 }
}
```

Inline modules cannot have **external** children: declaring `mod foo` (no body) inside an inline `mod parent { ... }` is a compile error. (To split out children, make `parent` an external module first.) Inline `mod` inside inline `mod` is fine.

### What `mod` does not do

- It does not import names. Use `import` (17.3) to bring names into scope.
- It does not auto-execute the submodule's code (there is no module-load code; see 17.6).
- It does not change a file's contents in any way — it just registers the file's existence in the module tree.

## 17.3 Import forms

```
import "util/helpers"                                   // glob — every public name
import { Foo, bar } from "util/helpers"                 // named
import { Foo as Bar } from "util/helpers"               // named with alias
import { Foo, bar as baz, Qux } from "util/helpers"     // mixed
```

### Glob import

`import "<path>"` makes every `pub` name from the target module visible in the current module under its original name.

### Named import

`import { ... } from "<path>"` brings exactly the listed names into scope. Each may be aliased with `as`.

## 17.4 Path forms

There are four kinds of import paths. The prefix (or its absence) makes the kind unambiguous from a glance:

| Form | Meaning |
|---|---|
| `"core:..."`, `"std:..."` | Built-in module supplied by the toolchain (see 17.8). |
| `"pkg:<name>"`, `"pkg:<name>/<sub>"` | An external dependency listed in the manifest. |
| `"<segment>/<segment>/..."` | **Absolute** path inside the current package — names trace through the `mod` tree from the entry's siblings. |
| `"./<segment>"`, `"../<segment>"` | **Relative** path inside the current package — resolved from the importing file's directory. |

Path examples (with source root `src/`):

```
import { Logger } from "util/log"            // src/util/log.lang
import { open }   from "io/file"             // src/io/file.lang
import { Helper } from "./helper"            // file in same directory
import { Util }   from "../shared/util"      // sibling directory
import { parse }  from "pkg:json/parse"      // external package, subpath
```

### Relative-import escape rule

Relative paths (`./` / `../`) are convenient for "next to me" and "directly above me" references, **but they cannot escape the package boundary**. The package boundary is the source root.

```
src/
  app/
    main.lang                  // can import "../util/log" (resolves to src/util/log.lang)
    db/
      query.lang               // can import "../../util/log" (resolves to src/util/log.lang)
                               // CANNOT import "../../../something"  ERROR: escapes package
  util/
    log.lang
```

Any `../` chain that resolves above the source root is rejected by the compiler:

```
relative import "../../../outside" escapes package "myapp"
```

The same rule applies inside external packages: a file in `pkg:cool-lib/...` cannot relative-import out of `cool-lib`. Each package is its own escape boundary. The only way to reach another package's contents is its `pkg:` prefix (and only the names that package marks `pub` in its entry/library file).

### Style guidance (lint, not error)

- Prefer **absolute** project paths for cross-cutting modules (logging, types, constants) — the path stays meaningful in error messages and grep results.
- Prefer **relative** for tightly-coupled siblings or children — the path stays valid if the subtree moves to a different location.

## 17.5 Visibility

There are two visibility knobs, no others:

- **`pub` on an item** — the item is visible to importers of *this module*.
- **`pub mod`** — the submodule is visible to importers of *its parent module*.

Together they form a chain. For a name to be reachable from outside the package, **every `mod` along the path from the entry must be `pub mod`, and the leaf item must be `pub`**.

```
// src/lib.lang
pub mod util       // util is part of the public surface

// src/util.lang
pub mod log        // util/log is part of the public surface

// src/util/log.lang
pub struct Logger { ... }    // Logger is visible to outside callers
struct Internal { ... }       // Internal is NOT visible to outside callers
```

If `lib.lang` had written `mod util` (no `pub`), the entire `util/*` subtree would be package-internal, regardless of `pub` markers further down.

### What `pub` exposes

- `pub struct Foo` — the struct type is importable.
- `pub interface I` — the interface is importable.
- `pub function f` — the function is importable.
- `pub type T = ...` — the alias is importable.
- `pub var X` — the module-level variable is importable (and writable; see [06-variables.md](./06-variables.md)).

For structs, **each field has independent visibility**. A `pub struct` with all-private fields can be returned and held but not constructed or destructured outside its module.

`extend` blocks have no `pub` modifier themselves — the methods they add inherit the visibility rules: an `extend Foo: Pub` impl is visible wherever `Foo` and `Pub` are both visible.

### No finer-grained visibility

There is no `pub(crate)`, `pub(super)`, `pub(in path)`, etc. The chain of `pub mod` declarations and per-item `pub` is the entire lattice:

- "Visible within this file": leave the item private.
- "Visible to my package only": `pub` the item; declare its containing `mod` *without* `pub` somewhere along the chain.
- "Visible to external consumers": `pub` the item; `pub mod` the whole chain.

If two sibling files need to share internals without exposing them globally, place them under a common parent that's declared `mod` (not `pub mod`). Items marked `pub` are then reachable from siblings under the same parent but invisible outside the package.

## 17.6 No top-level runtime

There is **no module-load runtime** — modules do not execute code on import. Specifically:

- Module-level `var` initializers must be compile-time constants (see [06-variables.md](./06-variables.md)). Their values are computed at compile time and embedded into static storage.
- There is no equivalent of a JavaScript "module body" or a Python module-level execution.
- There is no implicit `main` runner per module.

Practical consequence: import order, transitively, does not matter at runtime. The compiler builds the symbol graph; the linker fills in addresses; the program starts at `main`.

## 17.7 Circular imports

Circular imports are **allowed**. Because there is no module-level runtime code, there is no possibility of "using a module before its body has run". The compiler resolves type and function references regardless of cycle:

```
// module a
import { B } from "b"
pub struct A { b: B }

// module b
import { A } from "a"
pub struct B { a: A }
```

Two cautions:

- A cyclic type definition like `pub struct A { b: B }` and `pub struct B { a: A }` is recursive through references; the struct fields are pointers to heap objects, so the recursion is well-founded (each value's size is finite).
- The compiler must still solve generic constraints transitively; pathological generic cycles (e.g. `f<T: g<T>>`) are caught at compile time.

## 17.8 Built-in modules — `core:` and `std:`

Two reserved prefixes identify modules supplied by the toolchain:

- **`core:`** — assumes an allocator, **no OS**. Always available wherever the language runs (including bare metal with a heap). The single module `core:prelude` is auto-imported into every user module.
- **`std:`** — assumes an OS. Sub-modules under `std:` provide IO, threading, synchronization, async runtime, time, filesystem, networking. Must be explicitly imported. Not available on freestanding targets.

The name distinction reflects the realistic minimum the language can run on: an allocator is mandatory (structs are RC-managed on the heap, `str` is heap-allocated, closures box their environments), but an OS is not.

You cannot:

- Create a module whose path starts with `core:` or `std:`.
- Shadow or re-export names from these prefixes.

### `core:prelude`

Auto-imported into every user module. No explicit `import` needed.

Contains everything that the language's syntax desugars into, plus the heap-using container types:

- **Operator interfaces** — `Add`, `Sub`, `Mul`, `Div`, `Mod`, `Neg`, `Not`, `BitAnd`, `BitOr`, `BitXor`, `Shl`, `Shr`, `Eq`, `Ord`, `Index`, `IndexMut`, `Hash`. The `Ordering` type and its `Less` / `Equal` / `Greater` variants.
- **Iterator protocol** — `Iterator<T>`, `Item<T>`, `Done`.
- **Future protocol** — `Future<T>`, `Ready<T>`, `Pending`, `Context`.
- **Error propagation** — `Try<O, R>`, `FromResidual<R>`.
- **Lifecycle** — `Clone`, `Drop`.
- **Stringification** — `ToStr` (used by string interpolation; see [01-lexical.md §1.9](./01-lexical.md#19-string-literals-and-interpolation) and [15-operators.md §15.10](./15-operators.md#1510-stringification--tostr)).
- **FFI** — `ReprC`, `pin<T>`, `unpin<T>`, the `Buffer` extern struct.
- **Panic** — `panic(msg: str)`, `panic_with(value: T)`.
- **Heap collections** — `List<T>`, `Map<K, V>`, `Entry<K, V>`. (These use the allocator, which is always available.)
- **Methods on primitive `str`** — `str` is heap-allocated and its methods are part of the language.

Numeric helper namespaces (`i32.wrapping_add` etc.) are reachable without import: numeric primitives are keywords, and their static methods come along for free.

### `std:*` — explicit-import modules

| Module | What it provides | Why it's not in `core:` |
|---|---|---|
| `std:io` | `print`, `println`, file handles, byte streams | Needs an OS (stdout/stderr, file descriptors). |
| `std:thread` | `Thread.spawn`, `JoinHandle`, `Joined`, `Panicked` | Needs OS threads. |
| `std:sync` | `Shared<T>`, `LockBusy`, channels (`channel`, `channel_bounded`, `channel_mpmc`, `channel_mpmc_bounded`), `Sender<T>`, `Receiver<T>`, `MpmcSender<T>`, `MpmcReceiver<T>`, `ChannelClosed` | Needs OS mutexes / condvars. |
| `std:async` | `spawn`, `block_on`, `timeout`, the default executor, `AsyncIterator` adapters | Needs an executor; default impl uses OS threads. |
| `std:time` | wall-clock, monotonic time, durations | Needs an OS clock. |
| `std:fs` | filesystem | Needs an OS filesystem. |
| `std:net` | sockets, TCP, UDP | Needs an OS network stack. |

Typical imports:

```
import { print, println } from "std:io"
import { Thread, JoinHandle } from "std:thread"
import { Shared, channel } from "std:sync"
import { spawn, block_on } from "std:async"
```

A freestanding (no-OS) target compiles fine without any `std:*` import; `core:prelude` is enough to define types, run pure computation, and interact with FFI.

## 17.9 Shadowing rules at import

- A local definition silently shadows an import. If a module both imports `Foo` and defines its own `Foo`, the local one wins in that module.
- Importing the same local name from multiple imports is an error. You must alias one or both.
- An `import "..."` glob that would introduce a name colliding with an existing import is an error (unless one of the imports aliases the name).

## 17.10 Re-exports — `pub import`

`pub mod` exposes an entire submodule. To re-expose **individual names** from a submodule (without making the whole submodule public), use `pub import`:

```
// src/lib.lang
mod internals                                       // private — pkg consumers can't reach internals/*
pub import { Logger } from "internals/log"          // but Logger is in pkg:cool-lib's public surface
pub import { Db, open } from "internals/db"

pub function version(): str { "1.2.3" }
```

Syntax and forms parallel `import` exactly, with `pub` prepended:

```
pub import "util/helpers"                             // glob re-export
pub import { Foo, bar } from "util/helpers"           // named re-export
pub import { Foo as Bar } from "util/helpers"         // aliased re-export
```

The re-exported names appear in the re-exporting module's `pub` set. Importers of the re-exporting module see them as if they were defined locally.

### Semantics

- `pub import` simultaneously **imports** the named items into the current module (so the current module can use them) and **re-exports** them as part of this module's public API.
- For the re-export to be reachable from an external consumer, the **declaring module itself** must be on a `pub mod` chain — `pub import` doesn't bypass module visibility.
- Re-exporting a name does not produce a new type, struct, or function — it's still the same definition, just reachable under a new path.
- Collision rules (17.9) apply: a `pub import`ed name that collides with a local definition or another import is an error; alias one or both with `as`.

### What this lets you do

- Curate a flat public API at `lib.lang` while keeping the internal directory tree as deep as you want.
- Move implementation files around without changing the public API spelling — consumers always see the `lib.lang` paths.
- Mix `pub mod`-style "expose the whole submodule" with `pub import`-style "lift individual names" without conflict.

## 17.11 Compilation units

A module is a single source file. Multiple files form a project; the build tool composes them into one program (or library). Modules are compiled independently and combined.

`pub` and the orphan rule (see [10-interfaces.md](./10-interfaces.md)) together ensure that the import graph never produces ambiguous or conflicting type/interface configurations.
