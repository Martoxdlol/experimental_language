# Test suite

End-to-end tests for Otter Fusion. Every case under `cases/` is a complete,
self-contained `.otter` program (it declares its own `import`s) that carries its
expected outcome inline as comments. The runner lives in `crates/cli/tests/`.

## Run

```sh
# Whole suite + per-category timing report:
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

| Directive               | Meaning                                                       |
| ----------------------- | ------------------------------------------------------------- |
| `kind: run`             | (default) compiles, runs, exits 0, prints exactly the `//~`.  |
| `kind: compile-error`   | must fail to compile (no stdout).                             |
| `kind: panic`           | runs then aborts (exit 101 by default).                       |
| `exit: <n>`             | expected exit code (overrides the kind default).              |
| `stderr: <substr>`      | substring required in stderr (repeatable).                    |
| `release`               | run under `--release` (overflow wraps instead of panicking).  |
| `description: <text>`   | free-form note.                                               |

`//~ <line>` is one expected stdout line; bare `//~` is an empty line.

## Timing

`run`/`panic` cases run with `--time`, so the runtime prints the pure execution
time (excluding compilation) and the report aggregates it per category.

## Add a case

Drop a self-contained `.otter` file with a directive header into the right
`cases/<category>/` folder, then run the suite. For `run` cases you can write
the program and use `OTTER_TEST_BLESS=1` to capture stdout — then review it.
