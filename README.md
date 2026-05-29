# Otter Fusion Programming Language

This is the implementation of a programming language I designed. The design is my creation. The implementation was done by Claude (I don't have the skill or time to do it myself right now)

I started doing this because I always wanted to build a programming language. Not any programming language but one with specific set of features.

## Name

I choose "Otter Fusion" because otters are cute and didn't have much idea of other super tech names. I realized the name "Otter" is already in use.

I also choose `.otter` extension because it is nice and not used by any other commonly known language.

## Architecture

Otter Fusion is held to the same completeness and quality bar as a production
language (Rust, C, Java). No stage is skipped, no hard case is deferred, and the
compiler is **debuggable and observable** at every layer — every intermediate
representation can be inspected, and every node traces back to its source.

### Compilation pipeline

```
 lex ─► parse ─► AST ──────────────► HIR ──────────────► codegen ─► Cranelift ─► JIT / object+link
 (tokens)      (purely         (typed · resolved ·     (consumes                 (run / build)
                syntactic)      desugared)              HIR only)
                                    ▲                       ▲
                  resolve + type-check produces it       LSP also consumes HIR
                                                  [later, only for a self-hosted optimizer:]
                                                       HIR ─► MIR (CFG) ─► optimize ─► codegen
```

| Stage | Crate / module | Role |
|-------|----------------|------|
| **Lexer** | `compiler/lexer` | source text → tokens, with spans |
| **Parser** | `compiler/parser` | tokens → `ast` (purely syntactic, untyped) |
| **Collect** | `sema/symbols` | assign a `DefId` to every item/field/param/module; build the module tree |
| **Resolve + type-check** | `sema/check` | bidirectional checking, flow narrowing, exhaustiveness; **produces HIR** (resolve stays fused into check — this is correct and matches clang) |
| **HIR** | `hir` | the **typed, resolved, desugared tree** — the single source of truth codegen and the LSP consume |
| **Monomorphize** | `backend` worklist | instantiate generic HIR per `(DefId, type-args)`; stays at the HIR→codegen boundary |
| **Codegen** | `backend` | HIR → Cranelift IR → JIT (`run`) or object + system linker (`build`) |
| **Runtime** | `runtime` | GC, `str`/`List`/`Map`, channels, threads, async executor, FFI — linked into compiled programs |

### The central decision: a typed HIR, not span side-tables

Every language at this level (clang's typed AST, rustc's THIR, Swift's SIL,
javac's attributed AST) shares one property: **the representation codegen
consumes is typed and self-describing** — each node carries its type, its name
resolution, and its call/dispatch kind. Codegen never re-derives structure the
type-checker already computed.

Otter Fusion's HIR enforces exactly this:

- every expression node carries its `Ty`;
- every name carries its resolution (`Local` / `Function` / `Method` / …);
- every call carries its dispatch kind as an explicit variant
  (`Direct` / `Method` / `Dyn` / `Intrinsic` / `Closure` / `Extern`);
- coercions are explicit nodes (`Widen` / `Unbox` / `WidenDyn`), not lookups;
- builtins are explicit `Intrinsic` variants, not per-feature marker tables;
- desugaring lives here (`?`, `for`, operator overloading, string
  interpolation, async/await lowering).

This retires the implicit, span-keyed side-table contract (the former
`CheckResults` hashmaps): that data becomes **fields on HIR nodes**, so the
checker↔codegen agreement is a typed Rust structure the compiler verifies,
and an entire class of span-collision bugs becomes structurally impossible.

### MIR (deferred, on demand)

MIR is a control-flow-graph form whose only job is enabling a **self-hosted
optimizer** (dead-code elimination, inlining, escape analysis,
devirtualization) and dataflow-based checks. Like clang and javac, Otter Fusion
leans on the backend (Cranelift) for low-level optimization for now, so MIR is a
**separate, later project** introduced only when we optimize *above* Cranelift —
never bundled with the HIR work.

### Debuggability & observability (first-class)

- **Inspect every IR**: `--emit=tokens|ast|hir|clif|obj` with stable,
  deterministic pretty-printers for each stage.
- **Provenance everywhere**: every HIR (and later MIR) node preserves its source
  `Span`, so any value, error, or lowering traces back to the exact source.
- **Pass visibility**: optional per-pass tracing/timing of the pipeline.
- **Source-level debug info**: compiled programs emit DWARF line tables via
  Cranelift so native binaries are steppable in `lldb`/`gdb`.
- **Diagnostics**: full-fidelity caret diagnostics with stable codes.

### Migration method (staged, test-gated — no big-bang)

The move from the span-side-table model to HIR is done incrementally, with the
full test suite as the regression gate at every step:

1. define the complete HIR types (additive, zero behavior change);
2. lower `AST + CheckResults → HIR` losslessly (checker untouched);
3. repoint codegen to consume **HIR only** (no AST / side-table access);
4. repoint the LSP to HIR;
5. make the checker emit HIR directly, deleting `CheckResults` tables one at a
   time until none remain.

Every step keeps all existing tests green and adds HIR-level tests.