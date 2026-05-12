# 21. Async

Asynchronous programming is built on **state-machine futures**, an explicit executor, and `await` as the suspension point. There is no built-in generator (`yield`) syntax — `Iterator<T>` and `AsyncIterator<T>` are implemented by hand.

The pieces are split across two tiers:

- **`core:prelude`** (auto-imported) — the *shape* of async: the `Future<T>` interface, `Ready<T>` / `Pending`, the `Context` extern struct, `AsyncIterator<T>`. These are language-semantic; `async` / `await` desugar against them.
- **`std:async`** (explicit import) — the *runtime* that drives futures: `spawn`, `block_on`, `timeout`, the default executor, the `for await` desugaring's helpers. Requires an OS.

A freestanding (no-OS) target can still write `async function` and `await`, and define custom `Future<T>` impls. It just can't `spawn` or `block_on` without supplying its own executor.

```
import { spawn, block_on, timeout } from "std:async"
```

## 21.1 `Future<T>` (`core:prelude`)

```
pub struct Pending;
pub struct Ready<T> { pub value: T }

pub interface Future<Output> {
  function poll(self, ctx: *Context): Ready<Output> | Pending
}
```

A `Future<T>` is **inert**: calling `poll` advances it, but a future that is never polled never runs.

`poll` returns either `Ready<T>` (the future completed; `value` holds the result) or `Pending` (not yet; try again later — usually after the runtime has been notified through the context's waker).

Type ergonomics:

- A `Future<T>` whose output is `null` (no useful return value) is just `Future<null>`.
- A `Future<T | E>` is the standard shape for a fallible async operation.

## 21.2 `Context` and wakers

```
extern struct Context {
  waker_data: *u8,
  wake_fn:    extern (*u8) -> null,
}
```

The `Context` is the bridge between a future and the executor's scheduler:

- `waker_data` is an opaque pointer the runtime uses to identify the task.
- `wake_fn` is the function the future (or an underlying I/O source) calls to tell the runtime "I'm ready to make progress."

Calling `wake_fn(waker_data)` schedules the task that originally polled this future for re-polling. A future returning `Pending` is expected to ensure something will eventually call `wake_fn` — otherwise the task hangs forever.

The `Context` is an extern struct so that event loops written in C (e.g. `libuv`) can supply waker callbacks natively without language-runtime trampolining.

## 21.3 `async function`

```
async function fetch_data(url: str): str {
  var response = await http_get(url)
  response.body
}
```

The `async` prefix on a function definition rewrites the function so that its body is compiled into a state machine. Effects:

- The return type `T` in the source becomes `Future<T>` in the elaborated type. So `fetch_data` above has type `(str) -> Future<str>`.
- The body may use `await`. Each `await` is a suspension point; the compiler turns the code between awaits into states of the state machine.
- Calling the function does **not** execute the body; it returns a fresh `Future<T>`.

There is no `async` on struct methods that need to be called through an interface — async interface methods are written with explicit `Future<...>` return types instead of the `async` prefix, to keep the trait object representation predictable.

```
interface Fetcher {
  function fetch(self, url: str): Future<str>
}
```

Implementors may use the `async` prefix on the implementing method body if they like; the return type still appears as `Future<str>` at the interface level.

## 21.4 `await`

```
var v = await fut
```

`await` polls the future in the current async context. Semantics:

- If the future's `poll` returns `Ready<T>`, `await` evaluates to the `value` field.
- If it returns `Pending`, the current async function suspends, returning `Pending` from its own `poll` after recording the inner future's waker.

`await` is **only valid inside an `async function` or async closure**. Using it elsewhere is a compile error.

`await` is also **not valid at module level**. Module-level code can only be compile-time constants.

`await` does not propagate errors automatically. If `fut: Future<T | E>` and you `await` it, the result is `T | E`; use `?` (see [13-error-handling.md](./13-error-handling.md)) to propagate the error variant.

```
async function go(url: str): str | NetError {
  var body = (await http_get(url))?    // ? after the await
  parse(body)?
}
```

## 21.5 "Forgot to await" check

A `Future<T>` value that is created but never `await`ed, never passed to `spawn`, and never returned is suspicious — it represents work that will never run. The compiler emits a **lint** (error by default) on a `Future<T>` expression whose value is silently discarded:

```
async function fetch_data(): str { "data" }

function main() {
  fetch_data()         // ERROR — Future created but unused
  spawn(fetch_data())  // OK
  var f = fetch_data() // OK — bound to a variable
}
```

The rule is conservative: explicitly assigning to `_` (`var _ = fetch_data()`) silences the diagnostic, since the user has acknowledged the discard.

## 21.6 Executor (`std:async`)

The standard executor lives in `std:async`:

```
pub function spawn<T>(fut: Future<T>): JoinHandle<T>
pub function block_on<T>(fut: Future<T>): T
```

- `spawn(fut)` schedules a future onto the current runtime. Returns a `JoinHandle<T>` (same `JoinHandle` as in [20-concurrency.md](./20-concurrency.md); the implementation is unified).
- `block_on(fut)` runs the current thread's mini-runtime until the future completes and returns its result. Typically used at the top of `main`:

```
import { block_on } from "std:async"
import { println }  from "std:io"

function main() {
  block_on(async {
    var r = await some_top_level_task()
    println(r)
  })
}
```

The default executor is a work-stealing thread pool with one worker per logical core. The implementation also exposes hooks for custom executors, but they are an extension rather than a core language feature.

### Where futures actually run

A future polled by an executor runs on the executor's worker thread. The future's `poll` body must not block the OS thread for long — it should return `Pending` and rely on a waker. Blocking operations (file I/O, synchronous database calls) should be performed via a "blocking task" mechanism the executor provides; this is implementation-defined.

## 21.7 Async closures

The `async` prefix can be used on any closure form. The closure's return type becomes `Future<T>`.

```
var f1 = async function(x: i64): str {
  await sleep(100)
  x as str
}

var f2 = async |x| {
  await sleep(100)
  x as str
}

var f3 = async { await fetch(it) }   // implicit `it`, single param
```

The type of `f1`/`f2`/`f3` is `(i64) -> Future<str>` (for some appropriate signature).

Async closures may capture variables from the enclosing scope under the same rules as ordinary closures (see [09-functions.md](./09-functions.md)). When passed to `spawn`, the captured set is subject to the cross-thread isolation rules (see [20-concurrency.md](./20-concurrency.md)) — captured reference types are deep-cloned at spawn time.

Async closures cannot be marked `extern` (they have an environment plus a state machine).

## 21.8 Cancellation

A future is cancelled by **dropping** it. Dropping a future invokes its `Drop` impl, which in turn must release any resources it had acquired (open file descriptors, pending I/O registrations, etc.).

`select!`-style combinators (when supplied by stdlib or a third-party library) typically work by polling multiple futures and dropping the losers when a winner completes.

There is **no built-in cancellation token**. If you need cooperative cancellation, pass a `Shared<bool>` or a `Receiver<Cancel>` to the future and let it check.

## 21.9 Timeouts

The stdlib provides a `timeout` combinator:

```
pub function timeout<T>(fut: Future<T>, duration_ms: i64): Future<T | TimedOut>

pub struct TimedOut;
```

`timeout` produces a future that races `fut` against a timer. Whichever finishes first wins; the other is dropped.

## 21.10 `AsyncIterator<T>` (`core:prelude`)

Asynchronous iteration uses a parallel interface:

```
pub interface AsyncIterator<T> {
  function next_async(self): Future<Item<T> | Done>
}
```

Each call to `next_async` returns a future that resolves to either `Item<T>` (with value) or `Done`.

The `for await` loop iterates an async iterator:

```
async function process(stream: AsyncIterator<i64>) {
  for await n in stream {
    println(n as str)
  }
}
```

Desugaring (approximate):

```
{
  var __it = stream
  loop {
    match (await __it.next_async()) {
      Item { value } => { var n = value; ... loop body ... },
      Done           => break,
    }
  }
}
```

`for await` is only valid in an async context.

## 21.11 Streams

The language does **not** provide built-in generator syntax. Where Rust has `gen` or Python `yield`, this language requires explicit `AsyncIterator<T>` implementations. The `yield` keyword is reserved but unused.

To produce a stream, implement the interface by hand. A small example:

```
pub struct Ticker {
  pub current:  i64,
  pub end:      i64,
  pub delay_ms: i64,
}

extend Ticker: AsyncIterator<i64> {
  function next_async(self): Future<Item<i64> | Done> {
    async {
      if self.current >= self.end {
        Done
      } else {
        await sleep(self.delay_ms)
        var v = self.current
        self.current = self.current + 1
        Item { value: v }
      }
    }
  }
}

async function process() {
  var t = Ticker { current: 0, end: 5, delay_ms: 100 }
  for await n in t {
    println(n as str)
  }
}
```

This is verbose by design — explicit state machines are easier to reason about and integrate with cancellation.

## 21.12 Async and panics

A future that panics during `poll` propagates the panic to whoever was driving it:

- `block_on(fut)` — the calling thread panics.
- `spawn(fut)` — the spawned task ends; `JoinHandle.join` returns `Panicked`.

Other in-flight futures on the same executor are unaffected.

## 21.13 Composition primitives

Specific combinators (`join`, `race`, `select`) are stdlib additions and not part of the core language semantics. The minimum guarantees are:

- `await` exists and behaves as described.
- `spawn` exists and returns a `JoinHandle`.
- `Future<T>` has the trait shape above.
- `Context` interoperates with C event loops.

Everything above this baseline is library design.

## 21.14 Summary

- `Future<T>` is an inert state machine.
- `async function` produces one; `await` drives one inside another.
- Futures must be `spawn`ed or `await`ed; ignoring them is a compile error.
- The default executor is a work-stealing pool; `block_on` runs a top-level future.
- `AsyncIterator<T>` + `for await` for async streams. No generator syntax.
- Cancellation by drop; timeouts via `timeout(...)`.
- Async closures with `async` prefix, including the `it`-shorthand form.
