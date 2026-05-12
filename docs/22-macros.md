# 22. Procedural Macros

Macros transform source code at compile time by manipulating ASTs. They are **always procedural** — written as functions in the language itself. There is no `macro_rules!`-style declarative template form.

## 22.1 Naming

Macro names use **UpperCamelCase**, like struct and interface names: `@JsonSerializable`, `@Route`, `@Derive`. This visually separates a macro invocation from a regular function call (which uses `snake_case`). The compiler rejects `@lowercase_name` invocations with an error.

## 22.2 Invocation forms

A macro can appear in three syntactic positions. The position determines how the input AST is shaped; the same proc-macro signature handles all three.

### Decorator (item) form

```
@JsonSerializable
pub struct Config {
  pub host: str,
  pub port: i32,
}

@Route("/api/users", method = "GET")
function get_users(): str {
  "users"
}

@Derive(Eq, Hash, Clone)
pub struct Person {
  pub name: str,
  pub age:  i32,
}
```

The decorator sits above a top-level item — a `struct`, `function`, `interface`, `type` alias, `extend` block, or `mod` declaration. The decorated item is passed in as the macro's `input`.

### Expression form

```
var v = @Vec(1, 2, 3)
var sql = @Query("SELECT * FROM users WHERE id = ?", id)
```

Used in any expression position. The macro is parsed as `@Name(args...)` and its `input` AST is an expression-shaped node holding the args.

### Block form

```
@Time {
  do_work()
  do_more_work()
}

@Trace("expensive op") {
  compute_thing(input)
}
```

The macro wraps a block of statements. The `input` AST is a block-shaped node.

### Arguments

In every form, a macro invocation may carry positional and keyword arguments inside `(...)`:

```
@Route("/api/users", method = "GET", auth = true)
```

With no arguments, the parentheses may be omitted: `@JsonSerializable`.

### Stacking decorators

A single item may have multiple `@` decorators. They are applied **bottom-up** (innermost first):

```
@Derive(Clone)            // applied second — sees the @JsonSerializable output
@JsonSerializable         // applied first — sees the raw struct
pub struct Config { ... }
```

Equivalent to `Derive(Clone, JsonSerializable(raw_struct))`.

## 22.3 Macro definition

A macro plugin is a normal `pub function` decorated with `@ProcMacro`:

```
import { MacroContext, ASTNode } from "core:compiler"

@ProcMacro
pub function JsonSerializable(ctx: MacroContext, input: ASTNode): ASTNode {
  // inspect input, build a new AST, return it
}
```

Rules:

- The function name (UpperCamelCase) is the macro's invocation name.
- The signature is always `(MacroContext, ASTNode) -> ASTNode`. (Multiple-input macros are not supported; bundle inputs into an args node.)
- The function must be `pub` to be invokable from other modules; `@ProcMacro` functions follow the same `pub mod` chain as runtime code.
- The function may not import any `std:*` module (the compiler refuses to load a macro plugin that pulls in OS-dependent code; see 22.6).

Macros live in the same source tree as runtime code — there is no separate "macro crate". The compiler compiles `@ProcMacro` functions as part of the normal build and loads them as plugins when their invocations are encountered.

## 22.4 Compilation phases

Macro expansion is one of seven compilation phases. Macros run **before** type checking — they see syntax, not semantics.

| Phase | What runs |
|---|---|
| 0 — Lex | Source → tokens. |
| 1 — Parse | Tokens → raw AST. |
| 2 — **Macro expansion** | Expand every `@` invocation; rerun parse on emitted tokens; repeat to a fixed point (depth-limited; see 22.10). |
| 3 — Module resolution | Walk the `mod` tree; check orphan files; resolve `import`s. |
| 4 — Type check | Generic constraints, flow narrowing, overload resolution. |
| 5 — Monomorphization | Specialize generics; lower to IR. |
| 6 — Codegen | LLVM / Cranelift. |

Practical consequences:

- A macro cannot ask "what type is this expression?" — types don't exist yet.
- A macro cannot ask "what module is this item in?" — imports aren't resolved yet.
- A macro can ask about **syntactic** shape: "is this a struct?", "what fields does it have?", "what's the literal value of this argument?".
- A macro's output may itself contain macro invocations, which are then expanded recursively.

## 22.5 Hygiene

Macros are **hygienic by default**: identifiers a macro introduces live in a fresh syntax context. A `var temp = ...` emitted by a macro cannot collide with a caller's `temp`, even if the same spelling appears in both.

To deliberately introduce a name in the caller's scope (needed for `@Derive`-style macros that add methods reachable by the original name), use the unhygienic-escape helper:

```
var name = ctx.unhygienic("from_json")   // an identifier the caller can name directly
```

`ctx.unhygienic(name: str)` produces an identifier that resolves in the **invocation site's** scope. Use sparingly — most macros should keep their internal bindings hygienic.

## 22.6 Sandbox

A macro runs as a pure function over the AST. The plugin runtime enforces:

**Allowed:**

- Read the `input` AST and inspect span metadata.
- Construct new AST nodes via `ctx` helpers.
- Emit diagnostics via `ctx.error(span, msg)` / `ctx.warn(span, msg)` / `ctx.note(span, msg)`.
- Return a transformed AST.

**Not allowed:**

- I/O of any kind — no file reads, no network, no environment variables, no clock. (See 22.13 for a future relaxation.)
- Spawning threads or futures. `std:thread` and `std:async` are unimportable from macro code.
- Persisting state across invocations (each call gets a fresh, empty plugin instance).
- Inspecting types or resolved import paths (Phase 2 runs before Phase 3/4).

This makes macro execution deterministic and reproducible: given the same input AST and arguments, a macro always produces the same output AST.

## 22.7 Errors and diagnostics

A macro signals failure by emitting one or more diagnostics through `MacroContext` and returning a sentinel:

```
@ProcMacro
pub function Route(ctx: MacroContext, input: ASTNode): ASTNode {
  var path = ctx.arg_string("path")
  if path is null {
    ctx.error(ctx.invocation_span, "@Route requires a 'path' argument")
    return ASTNode.error_marker()
  }
  // ... build the transformed AST
}
```

`ASTNode.error_marker()` is a sentinel that tells the compiler "I've already reported the problem; suppress downstream checks on this subtree." Compilation continues so the user sees as many errors as possible per build.

`ctx.error` does **not** abort the macro — it accumulates. A macro can emit several errors and still return a real AST if some other check still produces something useful.

## 22.8 `MacroContext` API (sketch)

```
pub interface MacroContext {
  // span of the @MacroName itself
  function invocation_span(self): Span

  // arguments to the @MacroName(...) invocation
  function args(self): List<ASTNode>            // positional, in order
  function kwargs(self): Map<str, ASTNode>      // keyword args

  // argument coercion helpers (return null if missing or wrong shape)
  function arg_string(self, name: str): str | null
  function arg_int(self, name: str):    i64 | null
  function arg_bool(self, name: str):   bool | null
  function arg_ident(self, name: str):  str | null

  // diagnostics
  function error(self, span: Span, message: str)
  function warn(self,  span: Span, message: str)
  function note(self,  span: Span, message: str)

  // hygiene escape
  function unhygienic(self, name: str): ASTNode

  // identifier minting (hygienic temp names)
  function fresh_ident(self, hint: str): ASTNode
}
```

The full surface — argument inspection, span manipulation, AST construction helpers — is part of `core:compiler` and is documented separately from this language spec.

## 22.9 `ASTNode` (deferred)

The exact `ASTNode` schema — the discriminated union of node kinds (struct decl, function decl, expression, pattern, statement, block, ...), their fields, traversal helpers, and pretty-printers — is **deferred to a future specification document**.

What this version of the spec commits to about `ASTNode`:

- It is a discriminated union (per [03-unions.md](./03-unions.md)) with one variant per syntactic category.
- Every node carries a `Span` for diagnostics.
- Nodes are immutable; "modifying" a node means producing a new one.
- A node can be re-parsed from a source string via `core:compiler` helpers (`parse_item`, `parse_expr`, `parse_block`), letting macros generate code by string concatenation when convenient.

## 22.10 Recursion and depth limit

A macro's output may contain further `@` invocations. The compiler re-expands them. Recursion terminates when no `@` invocations remain in the tree.

The default recursion depth limit is **128 levels**. Going deeper produces an error pointing at the deepest expansion. The limit is configurable per project in the manifest:

```toml
[macros]
recursion_limit = 256
```

A macro that recursively emits itself with no termination hits this limit and is rejected; the error includes the invocation chain.

## 22.11 Built-in `@Derive`

`@Derive(Trait, ...)` is the one macro shipped with `core:prelude`. It synthesizes interface implementations for a struct.

```
@Derive(Eq, Hash, Clone, Ord)
pub struct Person {
  pub name: str,
  pub age:  i32,
}
```

Initial supported set:

| Derivable | Requires | Synthesizes |
|---|---|---|
| `@Derive(Eq)` | every field `Eq` | `extend T: Eq` — field-by-field equality |
| `@Derive(Ord)` | every field `Ord`; implies `Eq` | `extend T: Ord` — lexicographic comparison by declaration order |
| `@Derive(Hash)` | every field `Hash` | `extend T: Hash` — combined hash of all fields |
| `@Derive(Clone)` | every field `Clone` | `extend T: Clone` — deep copy of every field |

`@Derive` for additional interfaces (`Drop`, project-specific traits) is not built-in — users write their own `@ProcMacro` to synthesize them. `@Derive(MyTrait)` resolves to a macro named `Derive_MyTrait` (the compiler concatenates the dispatch name); the user defines that macro the same way they define any other.

## 22.12 No declarative macros

The language does **not** have `macro_rules!`-style declarative macros. Every macro is a procedural function. Rationale: one mental model, full generality, no fragment-specifier mini-grammar to learn. The tradeoff is more boilerplate for simple template substitutions; for those, write a small `@ProcMacro` that calls `parse_expr` / `parse_item` over a formatted string.

## 22.13 Future work

These items are deliberately deferred so the initial design stays small:

- **Exact `ASTNode` schema** — the variant list, field accessors, and construction API are spec'd separately. See 22.9.
- **Macro debugging tools** — pretty-print expanded AST, expansion tracing, span-aware diffs.
- **`@Derive(Debug)`** — pending a `Debug` interface in stdlib.
- **Allowing I/O / async in privileged macros** — some macros legitimately need to read files (e.g., `@IncludeBytes("./asset.bin")`, `@SqlSchema("schema.sql")`) or perform other "build-script-like" actions. The current rule (22.6) blocks them entirely. A future revision may introduce a **tooling-level whitelist**: the project manifest opts specific macros into limited I/O capabilities, enforced by the toolchain at macro-load time. The set of capabilities (read-files-under-path, read-env-var `X`, etc.) is per-macro and per-project; macros without explicit whitelist entries remain sandboxed. Async-in-macros could follow the same pattern but is lower priority and likely will reuse the same whitelist mechanism. The shape of the whitelist (manifest table, capability tokens, error reporting) is left for a future revision.

## 22.14 Summary

- `@UpperCamelCase` is the invocation form. Three positions: decorator on items, expression, block.
- Multiple decorators apply bottom-up.
- Macros are `pub function Name(ctx: MacroContext, input: ASTNode): ASTNode` marked `@ProcMacro`.
- Macros run in Phase 2 — before type checking and import resolution.
- Hygienic by default; `ctx.unhygienic(...)` to deliberately leak a name to the caller.
- Sandboxed: no I/O, no threads, no type info, no global state. Pure AST-to-AST. (See 22.13 for future privileged-macro mechanism.)
- Errors via `ctx.error(span, msg)`; return `ASTNode.error_marker()` to suppress downstream noise.
- Recursion limit 128, configurable.
- `@Derive(...)` is the only blessed macro in `core:prelude` (initial set: `Eq`, `Ord`, `Hash`, `Clone`).
- No declarative `macro_rules!`-style form.
- `ASTNode` schema, debugging tools, and privileged-macro whitelist are future work.
