# 14. Panics

A panic terminates the current thread immediately. Panics are **not catchable**: there is no `try`/`catch`, no `recover`, no way to convert a panic into a value in the panicking thread. Panics exist only for programming errors and irrecoverable conditions; expected failure modes use `T | Error` and `?` (see [13-error-handling.md](./13-error-handling.md)).

## 14.1 Explicit panic

```
pub function panic(message: str): never
```

`panic(msg)` triggers a panic with the given message.

The declared return type `never` means the function does not return. A `panic(...)` call can appear anywhere any value is expected, because `never` is a subtype of every type.

```
function require_positive(n: i64): i64 {
  if n <= 0 {
    panic("expected positive, got " + (n as str))
  }
  n
}
```

There is also `panic_with(value: T)` for cases where a structured panic value is useful, but the language never inspects the value — it's only available to a debugger or platform-provided handler.

## 14.2 Panic sources

The following operations panic on failure:

| Source | When |
|---|---|
| `panic(...)` | Always |
| `as` between union variants | When the value isn't the requested variant |
| `as` from float to int | When the float is NaN or out of the target's range |
| `as` from int to `char` | When the value isn't a valid Unicode scalar (`0..=0x10FFFF` excluding surrogates) |
| Integer division `/` | When the divisor is `0` |
| Integer modulo `%` | When the divisor is `0` |
| Integer overflow | In **debug** builds (release builds wrap; see 14.5) |
| Shift overflow (`<<`, `>>` by ≥ width) | Always (both debug and release) |
| `[]` indexing on `List<T>` | When the index is out of range |
| `[]` indexing on `Map<K, V>` | When the key is missing |
| `Buffer.set(i, v)` does **not** panic | (silent no-op when out of range; matches stdlib pattern) |
| Stack overflow | Always (best-effort detection) |
| Heap allocation failure | Always (the allocator panics on OOM) |
| FFI: dereferencing a `*T` returned where `*T \| null` would have been correct | Always |
| Dropping an object whose `Drop` impl panics | The thread aborts (no recovery) |

Truncating integer-to-integer `as` casts do **not** panic — they truncate silently. Use `checked_*` functions if you want to detect loss of precision.

## 14.3 Behavior of a panicking thread

A panic in thread T:

1. The panic site stops executing further code in T.
2. The runtime walks T's stack, executing `Drop.drop` for in-scope managed objects in reverse order of construction.
3. T is marked as terminated.
4. The panic message and (in debug builds) a stack trace are written to stderr.

After T terminates:

- If T is the **main thread**: the program aborts. All non-main threads are stopped without their stacks being unwound (best-effort drop only on cleanup).
- If T is a **spawned thread**: only T dies. Joining T via the runtime's join API (see [20-concurrency.md](./20-concurrency.md)) returns a "panicked" status that the joiner can react to.

## 14.4 Drop during a panic

`Drop` implementations should not panic. If a `Drop` impl panics during normal collection, the second panic immediately aborts the entire process — there is no double-unwind support.

A `Drop` impl during stack unwinding (already inside a panic) that itself panics: the process aborts.

This is the standard "no double panic" rule. It is the user's responsibility to keep `Drop` impls infallible.

## 14.5 Integer overflow

Two modes:

- **Debug builds**: every integer arithmetic operation that overflows panics.
- **Release builds**: signed and unsigned arithmetic wraps modularly (two's complement for signed).

To opt out of either default, use explicit stdlib functions:

| Function | Behavior |
|---|---|
| `i32.wrapping_add(a, b)` | Wraps (matches release default). |
| `i32.checked_add(a, b)` | Returns `i32 \| null` (`null` on overflow). |
| `i32.saturating_add(a, b)` | Clamps to `i32.MIN` / `i32.MAX`. |
| `i32.overflowing_add(a, b)` | Returns `(i32, bool)` (wrapped value, did_overflow). |

Same families exist on every integer type for `add`, `sub`, `mul`, `div`, `rem`, `neg`, `shl`, `shr`.

The asymmetry between debug and release matches Rust's choice and is intentional: debug surfaces bugs; release prioritizes speed. Programs that need defined behavior in release should use `wrapping_*` or `checked_*` explicitly.

## 14.6 Float behavior

Floats follow IEEE 754. None of the standard operations panic:

- Division by zero produces `+inf`, `-inf`, or `NaN`.
- Out-of-range arithmetic produces `+inf`, `-inf`, or `NaN`.
- NaN comparisons return `false`.

`f as i*` panics for NaN and out-of-range values.

## 14.7 Catchable boundaries between threads

Spawned threads are the unit of fault isolation. A worker thread that panics can be observed by its spawner via the join API:

```
var handle = spawn(function() { ... })
var result = handle.join()
match result {
  Joined { value }     => print("ok: " + value),
  Panicked { message } => print("worker died: " + message),
}
```

`Joined<T>` and `Panicked` are unit/wrapper structs provided by stdlib.

This is the only way to "recover" from a panic: by isolating the fallible code in its own thread and joining.

## 14.8 Style

- Use `panic(...)` for invariants you believe are unbreakable (the value should never be possible).
- Use `T | Error` and `?` for expected failure paths (user input, network errors, missing keys you intend to handle).
- Reserve `as` between union variants for paths you have already narrowed with `is`/`match`; otherwise prefer `match`.
- Do not use panics for control flow.
