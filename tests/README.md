# Test suite

End-to-end tests for Otter Fusion. Every case under `cases/` is a complete,
self-contained `.otter` program (it declares its own `import`s) that carries its
expected outcome inline as comments. The runner lives in `crates/cli/tests/`.

We **do not test only happy paths**. The suite deliberately covers compile-time
errors, runtime panics, memory/GC stress, concurrency, and — via `known-bug`
markers — the things that *should* fail to compile (or should work) but
currently don't. A suite that is 100% green on an unfinished language is hiding
the truth, so the runner prints a **catalog of known bugs / unimplemented
features** and fails if one silently starts passing.

## Run

```sh
# Whole suite + status + per-category timing report:
cargo test -p cli --test suite -- --nocapture

# A single case is just a normal program:
otter_fusion run cases/arithmetic/precedence.otter --time

# Update expected stdout for passing `run` cases after an intended change:
OTTER_TEST_BLESS=1 cargo test -p cli --test suite -- --nocapture
```

## Case format

Lines starting with `//@` are directives; lines starting with `//~` are the
exact expected stdout (one per line).

```otter
//@ kind: run
//@ description: operator precedence
//@ stdout:
//~ 7
import { println } from "std:io";
function main() { println("${1 + 2 * 3}"); }
```

Directives:

| Directive             | Meaning                                                              |
| --------------------- | -------------------------------------------------------------------- |
| `kind: run`           | (default) compiles, runs, exits 0, prints exactly the `//~` lines.   |
| `kind: compile-error` | must fail to compile (no stdout).                                    |
| `kind: panic`         | runs then aborts (exit 101 by default).                              |
| `exit: <n>`           | expected exit code (overrides the kind default).                     |
| `stderr: <substr>`    | substring required in stderr (repeatable).                           |
| `release`             | run under `--release` (overflow wraps instead of panicking).         |
| `serial`              | run alone, after the parallel batch (for OS-thread-spawning cases).  |
| `env: KEY=VALUE`      | set an env var for the run (repeatable; e.g. `LANG_GC=stress`).       |
| `known-bug: <note>`   | states *desired* behaviour the impl does NOT yet meet — see below.   |
| `description: <text>` | free-form note.                                                      |

`//~ <line>` is one expected stdout line; bare `//~` is an empty line.

## Outcomes the report distinguishes

- **pass** — met its expectations.
- **fail** — did not (a real regression). Fails the suite.
- **XFAIL** (`known-bug`) — a documented gap that is *still present*; expected,
  does **not** fail the suite, and is listed in the known-bug catalog.
- **XPASS** — a `known-bug` case that now meets its (spec-correct) expectations:
  the bug looks fixed. **Fails the suite** so the `known-bug` marker is removed.

This is how the suite catalogs the unfinished surface instead of hiding it.

## Coverage

`cases/` is organised by feature. Beyond the core language, data types,
generics/interfaces/closures/iterators, casts/derives/error-handling and the
`examples/` programs, the suite includes:

- **`panics/`** — overflow (add/sub/mul), divide/rem by zero, shift overflow,
  explicit `panic`/`panic_with`/`exit`, map-missing-key, float→int range/NaN,
  int→char range, list index out of range.
- **`errors/`** — ~50 compile-error cases spanning the checker's diagnostic
  surface: unknown type/value, type mismatch (annotation/return/arg/union),
  arity, non-bool condition, invalid/illegal casts, operator-not-implemented,
  no-method / no-field, missing/extra struct field, generic arity, unsatisfied
  bound, empty-list inference, recursive alias, non-exhaustive match, literal
  range, `self`/`break`/`continue` placement, deref of non-pointer, bad imports,
  duplicate definitions, and more.
- **`gc/`** — `LANG_GC=stress` (collect on every allocation) over list/map/
  string/struct-graph/closure churn, to shake out missing GC roots.
- **`concurrency/`** — `Thread.spawn` + `await join`, channels, `Shared` locks,
  and a 100-thread storm (`serial`, deterministic).
- **`release/`** — release-profile wrapping arithmetic.
- **`known_bugs/`** — the documented gaps (currently none; see below).

## Known bugs / runtime notes (current)

The `known_bugs/` catalog is currently **empty** — every previously-documented
gap has been resolved and its case promoted into the matching category:

- Tuple pattern arity mismatch (too few / too many) → clean compile errors
  (`tuples/pattern_arity_*`); the too-many case no longer crashes the backend.
- Duplicate field in a struct literal → rejected
  (`structs/duplicate_field_in_literal`).
- Record pattern on a tuple struct → clear "not a record struct" diagnostic
  (`match/record_pattern_on_tuple_struct`).
- Named function as a first-class value → works
  (`functions/first_class_value`), lowered via a closure-ABI thunk.
- `Thread.spawn` of a float-returning function → works for `f64`/`f32`
  (`concurrency/thread_float_result`); the worker carries the float bits.
- Spawn captures are independent by-value snapshots per docs/20 §6
  (`concurrency/spawn_capture_is_snapshot`) — a captured local is copied at the
  spawn site, so mutating it afterwards is not observed by the worker.

Note: the previously-observed intermittent abort in thread-spawning programs
was **fixed** — it was not GC or contention but an async-state-machine bug
(a sync `for` loop with an `await` in its body lost its iteration state across
the suspend; see `iterators/for_await_in_body_*`). GC reclamation is still
gated to a single mutator (a separate, deliberate memory-safety measure; see
`ROADMAP.md` GC §). Thread-spawning cases run `serial` to keep cross-process
CPU contention low.

## Add a case

Drop a self-contained `.otter` file with a directive header into the right
`cases/<category>/` folder, then run the suite. For `run` cases you can write
the program and use `OTTER_TEST_BLESS=1` to capture stdout — then review it.
