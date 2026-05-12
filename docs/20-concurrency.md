# 20. Concurrency

Concurrency uses **isolated threads** with **message-passing channels** as the primary mechanism, plus an explicit **`Shared<T>` mutex** for genuinely shared mutable state.

Everything in this chapter is in the `std:*` tier — it requires an OS for threads and synchronization primitives. Specifically:

- **`std:thread`** — `Thread.spawn`, `JoinHandle<R>`, `Joined<R>`, `Panicked`.
- **`std:sync`** — `Shared<T>`, `LockBusy`, the channel functions (`channel`, `channel_bounded`, `channel_mpmc`, `channel_mpmc_bounded`), `Sender<T>`, `Receiver<T>`, `MpmcSender<T>`, `MpmcReceiver<T>`, `ChannelClosed`.

All of these require explicit imports. Freestanding (no-OS) targets cannot use them.

```
import { Thread, JoinHandle, Joined, Panicked } from "std:thread"
import { Shared, channel } from "std:sync"
```

## 20.1 Threads (`std:thread`)

```
pub function Thread.spawn<R>(work: () -> R): JoinHandle<R>
```

`Thread.spawn` starts a new OS thread (or runtime worker; the implementation may use a thread pool) running a synchronous closure. It returns a `JoinHandle<R>` for observing completion.

For spawning a `Future<T>` onto the async executor instead, use `spawn(fut)` from the async runtime (see [21-async.md](./21-async.md)). Both return the same `JoinHandle<R>` type; the difference is what kind of work is scheduled.

```
pub struct JoinHandle<R> { ... }

extend<R> JoinHandle<R> {
  function join(self): Joined<R> | Panicked
  function detach(self): null
}

pub struct Joined<R> { pub value: R }
pub struct Panicked  { pub message: str }
```

`join` blocks until the thread finishes:

- Normal exit → `Joined { value }`.
- Thread panicked → `Panicked { message }` (the panic message; the thread's stack has been unwound and resources released).

`detach` releases the handle without joining; the thread continues to completion independently. Detached threads cannot be re-joined and their result is lost.

### Cross-thread capture

A closure passed to `Thread.spawn` may capture variables from the spawning thread's scope, but those captures are subject to **isolation rules**:

- For each captured reference, the compiler inserts a deep clone at the spawn site (uses `Clone.clone`, requires `T: Clone`).
- For primitive captures, a value copy is taken (no clone needed).
- The new thread receives independent copies. The spawner can continue using its originals safely.

This is the **default isolation boundary**: each thread gets its own copy. To share, use channels or `Shared<T>`.

If a captured type does not implement `Clone`, the spawn site fails to compile, with a diagnostic pointing at the offending capture.

## 20.2 Channels (`std:sync`)

A channel is a typed FIFO message queue. Construction returns a `(Sender, Receiver)` pair.

### MPSC (default)

```
pub function channel<T>(): (Sender<T>, Receiver<T>)
pub function channel_bounded<T>(capacity: usize): (Sender<T>, Receiver<T>)
```

- `Sender<T>` is `Clone` — multiple producers.
- `Receiver<T>` is **not** `Clone` — single consumer.

This is multi-producer, single-consumer (MPSC), the most common pattern.

### MPMC

```
pub function channel_mpmc<T>(): (MpmcSender<T>, MpmcReceiver<T>)
pub function channel_mpmc_bounded<T>(capacity: usize): (MpmcSender<T>, MpmcReceiver<T>)
```

Both `MpmcSender<T>` and `MpmcReceiver<T>` are `Clone`. Multiple producers and consumers can share the same channel; each message is delivered to exactly one consumer.

### Sender API

```
extend<T> Sender<T> {
  function send(self, value: T): null | ChannelClosed
}

pub struct ChannelClosed;
```

`send` returns `null` on success, `ChannelClosed` if the receiver has been dropped.

On a bounded channel, `send` blocks if the channel is full (or returns an error variant in async contexts; see [21-async.md](./21-async.md) for the async variant).

### Receiver API

```
extend<T> Receiver<T> {
  function recv(self): T | ChannelClosed
  function try_recv(self): T | null | ChannelClosed
}
```

- `recv` blocks until a message arrives or the channel closes.
- `try_recv` returns immediately: `T` if a message is available, `null` if not, `ChannelClosed` if closed and empty.

A channel is "closed" when **all senders have been dropped**. Subsequent `recv` calls drain remaining messages, then start returning `ChannelClosed`.

### Iteration

`Receiver<T>` implements `Iterator<T>` (the iterator yields until the channel closes):

```
for msg in rx {
  process(msg)
}
```

The iterator yields `Item<T>` until `recv` would return `ChannelClosed`, then `Done`.

## 20.3 Zero-copy on send

When the runtime can prove the value being sent has refcount exactly 1 at the moment of send, the pointer is **moved** rather than cloned. This avoids the deep clone that cross-thread transfer would otherwise require.

The mechanism:

1. The compiler tags the `tx.send(p)` call site with "p is at last use" if static analysis determines that `p` is not used afterward in its scope (no subsequent read, no later branch that uses it).
2. At runtime, `send` reads the object's refcount.
3. If the static analysis confirmed last-use **and** the dynamic refcount is exactly 1, the send transfers the pointer to the receiver thread without cloning. The sender's local binding is invalidated by being dropped at the send site (the compiler emits the drop as part of the send sequence).
4. Otherwise, `send` invokes `Clone.clone(p)` and transfers the clone. The sender's reference remains valid.

In effect: **sending a value at its last use point is zero-copy whenever no other references exist**. This is the optimization referenced in the language's "send and forget" idiom:

```
var (tx, rx) = channel<Person>()
var p = Person { name: "John", age: 30 }
tx.send(p)   // p is at its last use; if refcount == 1, this is a pointer move
```

### Requirements

- The value's type must implement `Clone` (because the fallback path needs to clone).
- The compiler's last-use analysis is conservative: any later reference (including indirect, e.g. via a closure) defeats the optimization. The send then clones.
- Cross-thread refcounts are atomic, so the runtime check is race-free.

## 20.4 `Shared<T>` (`std:sync`)

`Shared<T>` is a mutex-protected handle to a value that needs to be shared between threads without channels.

```
pub struct Shared<T> { ... }

extend<T> Shared<T> {
  static function new(value: T): Shared<T>
  function lock<R>(self, body: (T) -> R): R
  function try_lock<R>(self, body: (T) -> R): R | LockBusy
}

pub struct LockBusy;
```

`lock` blocks until the lock is acquired, then runs `body` with the inner value as the argument. The lock is released when `body` returns.

```
var state = Shared.new(Counter { value: 0 })

state.lock(|c| { c.value = c.value + 1 })

var snapshot = state.lock(|c| { c.value })
```

### Detachment rule

`body`'s return value is the only thing that "escapes" the lock. For reference types, the runtime forces a `Clone` at the return boundary:

- If `body` returns a primitive, it is returned by value (cheap copy).
- If `body` returns a reference type, the runtime calls `Clone.clone` on it before releasing the lock.

This prevents handing out a raw reference into the protected region that could be used after the lock is released. If the return type doesn't implement `Clone`, the lock body fails to compile at the return type position.

The reference passed to `body` is **valid only inside the lock**. Storing it in an outer-scoped variable that outlives the `body` call is rejected by the compiler (escape analysis).

### Reentrancy

`Shared.lock` is **not reentrant**. A thread that holds the lock and calls `lock` again on the same `Shared<T>` will deadlock. Use `try_lock` if uncertain.

### Poisoning

If `body` panics with the lock held, the runtime unwinds the panic and releases the lock. The lock is **not poisoned** — future lockers see the inner value in whatever state the panicking body left it. Users who care about consistency under panic should use `try_lock` and structure operations transactionally.

## 20.5 Atomics

The initial spec does not expose individual atomic primitives. `Shared<T>` covers the common cases for primitives:

```
var counter = Shared.new(0i64)
counter.lock(|n| { /* mutate n */ })
```

For high-performance lock-free patterns, users can drop down to FFI atomics. Standalone atomic primitives may be added in a later revision.

## 20.6 Thread safety summary

| Construct | Thread safety |
|---|---|
| Local `var` | Thread-local — never shared by default. |
| Module-level `var` (no wrapper) | **Unsafe** under concurrent access. The language does not synchronize. |
| Captured-by-`spawn` value | Deep-cloned at spawn — each thread has its own copy. |
| Channel | Safe by construction; one message has exactly one owner at a time. |
| `Shared<T>` | Safe; mutex-protected; one writer at a time. |
| `extern struct` / foreign memory | Whatever C semantics apply — language doesn't help. |

For module-level mutable state shared across threads, wrap it: `pub var COUNTER: Shared<i64> = Shared.new(0i64)` if `Shared.new` could be const-evaluated, otherwise initialize via lazy access pattern (see [06-variables.md](./06-variables.md)).

## 20.7 Diagram — the data isolation story

```
   Thread A                  channel/Shared/spawn boundary               Thread B
   --------                                                              --------
   var p = ...                                                           (no view of p)

                            tx.send(p)
   ----- A's p moves or clones ----------------------------------------> rx.recv()
                                                                         var q = ...

                            (no view of q)                               tx.send(q)
   rx.recv() <----------------------------------------- B's q moves or clones -----
   var ...
```

Threads do not share managed-heap state by default. All cross-thread communication goes through one of: channel (move or clone), `Shared<T>` (locked access + cloned escape), or `spawn` capture (deep clone at boundary).

## 20.8 Cancellation

There is no built-in thread cancellation. To stop a worker, send it a sentinel message or close its input channel and let it observe `ChannelClosed`.

For async, see [21-async.md](./21-async.md).

## 20.9 Examples

```
import { println } from "std:io"
import { Thread } from "std:thread"
import { channel } from "std:sync"

function producer(tx: Sender<i64>) {
  for n in (Range { current: 0, end: 5 }) {
    tx.send(n)
  }
  // tx drops here; channel closes when all senders are dropped
}

function consumer(rx: Receiver<i64>) {
  for n in rx {
    println("got " + (n as str))
  }
}

function main() {
  var (tx, rx) = channel<i64>()
  var h1 = Thread.spawn(|| producer(tx))
  var h2 = Thread.spawn(|| consumer(rx))
  h1.join()
  h2.join()
}
```

```
// Shared counter incremented by many threads
import { println } from "std:io"
import { Thread, JoinHandle } from "std:thread"
import { Shared } from "std:sync"

function main() {
  var state = Shared.new(0i64)
  var handles = List.new<JoinHandle<null>>()
  for _ in (Range { current: 0, end: 8 }) {
    var s = state.clone()    // Shared<T> is Clone (clones the handle, same inner)
    handles.push(Thread.spawn(|| {
      for _ in (Range { current: 0, end: 1000 }) {
        s.lock(|n| { /* primitive mutation through ref */ })
      }
    }))
  }
  for h in handles { h.join() }
  println(state.lock(|n| n) as str)
}
```

(Note: `Shared<T>.clone` clones the handle, not the underlying value. The handle is a refcounted pointer to the mutex + inner data; both clones lock the same mutex.)
