# 18. Standard Library

The built-in library is split across two tiers (see [17-modules.md §17.5](./17-modules.md#175-built-in-modules--core-and-std)):

- **`core:prelude`** — auto-imported into every user module. Assumes an allocator; needs no OS. Contains the language-semantic interfaces plus the heap-using built-in types.
- **`std:*`** — explicit-import modules that assume an OS. Cover IO, threading, sync, async runtime, time, filesystem, networking.

This chapter documents the data-oriented portion of `core:prelude` — strings, lists, maps, iterators, the FFI `Buffer`, numeric helpers. Operator and trait interfaces are in [15-operators.md](./15-operators.md). Threading and channels are in [20-concurrency.md](./20-concurrency.md). Async runtime is in [21-async.md](./21-async.md). IO (`print`, `println`, etc.) lives in `std:io` and is summarized in §18.10.

## 18.1 Failure semantics across stdlib

Standard-library methods that can fail (out-of-bounds index, missing key, popping from an empty container) **return `T | null`** rather than panicking. Checking `is null` is the way to handle failure.

The `[]` operator family panics on missing/out-of-range — use `.get(...)` for the `T | null` variant. This split mirrors common usage: indexing is for known-valid access, `get` for fallible access.

## 18.2 `str`

```
str   // heap-allocated, immutable, UTF-8
```

`str` is the language's only string type. It is immutable: every "mutating" operation returns a new `str`.

Construction: string literals (see [01-lexical.md](./01-lexical.md)). Concatenation: `+` (sugar for `concat`).

| Method | Returns | Notes |
|---|---|---|
| `size()` | `i64` | Number of characters (Unicode scalar values). |
| `byte_size()` | `i64` | Number of UTF-8 bytes. |
| `is_empty()` | `bool` | |
| `get(i: i64)` | `char \| null` | Out-of-range → `null`. |
| `contains(s: str)` | `bool` | |
| `starts_with(s: str)` | `bool` | |
| `ends_with(s: str)` | `bool` | |
| `index_of(s: str)` | `i64 \| null` | Character index of first occurrence; `null` if absent. |
| `substring(start: i64, end: i64)` | `str` | Half-open range over character indices. Panics if out of range. |
| `concat(s: str)` | `str` | Same as `+`. |
| `split(sep: str)` | `List<str>` | |
| `trim()` | `str` | |
| `to_upper()` | `str` | |
| `to_lower()` | `str` | |
| `replace(old: str, new: str)` | `str` | Replaces all occurrences. |
| `repeat(n: i64)` | `str` | |
| `chars()` | `Iterator<char>` | Iterator over characters. |
| `bytes()` | `Iterator<u8>` | Iterator over UTF-8 bytes. |

`str` implements `Eq`, `Ord` (lexicographic on Unicode scalar values), `Hash`, `Clone` (identity — `str` is immutable, so cloning returns the same reference).

`str` is iterable directly in `for ch in s`; this is equivalent to `for ch in s.chars()`.

## 18.3 `List<T>`

Dynamic array. Heap-allocated; element type any.

Construction: list literal `[...]` or `List.new<T>()`.

```
var a: List<i64> = [1, 2, 3]
var b = List.new<i64>()
```

| Method | Returns | Notes |
|---|---|---|
| `new()` (static) | `List<T>` | Empty list. `List.new<T>()`. |
| `size()` | `i64` | |
| `is_empty()` | `bool` | |
| `clear()` | `null` | |
| `get(i: i64)` | `T \| null` | |
| `set(i: i64, v: T)` | `null` | Silent no-op if out of range. |
| `push(v: T)` | `null` | |
| `pop()` | `T \| null` | Removes and returns the last element. |
| `insert(i: i64, v: T)` | `null` | Panics if `i > size`. |
| `remove(i: i64)` | `T \| null` | Removes the i-th element and returns it. |
| `truncate(n: i64)` | `null` | Trims to length `n` (no-op if `n >= size`). |
| `contains(v: T)` | `bool` | Requires `T: Eq`. |
| `index_of(v: T)` | `i64 \| null` | Requires `T: Eq`. |
| `iter()` | `Iterator<T>` | |

`List<T>` implements `Index<i64, T>` and `IndexMut<i64, T>` — `list[i]` panics on out-of-range, unlike `.get(i)`.

`List<T>` implements `Iterator<T>` indirectly through `iter()`, and is directly iterable in `for x in list`.

`List<T>` implements `Clone` if `T: Clone`; `Eq` if `T: Eq`; `Hash` if `T: Hash`.

## 18.4 `Map<K, V>`

Hash map with `K: Eq + Hash`.

Construction: map literal `{ "key": value, ... }` or `Map.new<K, V>()`.

```
var a: Map<str, i64> = { "x": 1, "y": 2 }
var b = Map.new<str, i64>()
```

### Map literal vs block

The grammar uses `{ ... }` for both blocks and map literals. The compiler disambiguates by content:

- If the contents look like a sequence of `<key-expression>: <value>` separated by `,` — it's a **map literal**.
- Otherwise it's a **block**.

The decision is local to the `{ ... }` token: the parser attempts a map-literal parse first and falls back to a block parse if it fails. Practical rules of thumb:

- Keys must always be expressions followed by `:`; the parser uses the `:` to commit to the map-literal path. (A leading literal followed by `:` is unambiguous.)
- A bare `{ ... }` used at expression position with no `:` inside is a block.
- An empty map literal cannot be written as `{}` — that is the empty block. Use `Map.new<K, V>()` for empty maps.

Inside function-call argument position the disambiguation is the same: argument `{ "x": 1 }` parses as a map literal.

If a block legitimately needs to start with a value followed by `:`, the user has hit an ambiguous parse and must use explicit `Map.new(...)` or a typed annotation.

| Method | Returns | Notes |
|---|---|---|
| `new()` (static) | `Map<K, V>` | Empty map. |
| `size()` | `i64` | |
| `is_empty()` | `bool` | |
| `clear()` | `null` | |
| `get(k: K)` | `V \| null` | |
| `set(k: K, v: V)` | `null` | Inserts or replaces. |
| `remove(k: K)` | `V \| null` | |
| `contains(k: K)` | `bool` | |
| `keys()` | `List<K>` | Snapshot. |
| `values()` | `List<V>` | Snapshot. |
| `entries()` | `Iterator<Entry<K, V>>` | |

`Map<K, V>` implements `Index<K, V>` and `IndexMut<K, V>` — `map[k]` panics on missing key, unlike `.get(k)`.

`for entry in map` iterates over `Entry<K, V>` values:

```
struct Entry<K, V> {
  pub key:   K,
  pub value: V,
}
```

`Map<K, V>` implements `Clone` if both `K: Clone` and `V: Clone`. It does **not** implement `Eq` or `Hash`.

## 18.5 `Item<T>` and `Done`

```
pub struct Item<T> {
  pub value: T,
}

pub struct Done;
```

Used by the iterator protocol: `Iterator<T>.next` returns `Item<T> | Done`. Wrapping the value in `Item<T>` avoids the ambiguity that would arise if `T` itself could be `null` (or any sentinel).

`Item<T> | Done` is a two-variant union; the `Done` variant carries no payload.

## 18.6 `Iterator<T>`

```
pub interface Iterator<T> {
  function next(self): Item<T> | Done
}
```

The contract: each call to `next` produces either an `Item<T>` (with `value` set) or `Done`. Once `Done` has been returned, subsequent calls should also return `Done` — most consumers stop after the first `Done`.

`for x in v` works for any `v` whose type implements `Iterator<T>`. The desugaring is approximately:

```
{
  var __it = v               // or v.iter() for collections that implement IntoIterator
  loop {
    match __it.next() {
      Item { value } => {
        var x = value
        ... loop body ...
      },
      Done => break,
    }
  }
}
```

### Example iterator

```
pub struct Range {
  pub current: i64,
  pub end:     i64,
}

extend Range: Iterator<i64> {
  function next(self): Item<i64> | Done {
    if self.current >= self.end {
      Done
    } else {
      var v = self.current
      self.current = self.current + 1
      Item { value: v }
    }
  }
}

for n in (Range { current: 0, end: 5 }) {
  print(n as str)
}
```

## 18.7 `Buffer` (extern struct)

`Buffer` is a contiguous byte array with **C layout** — it's an `extern struct`, so it interoperates seamlessly with foreign APIs.

```
extern struct Buffer {
  data: *u8,
  size: u64,
}
```

Because `Buffer` is `extern`:

- It passes to/from extern functions by value or by pointer with no shim.
- Its bytes live on the foreign heap (manual `alloc`/`free`; the GC does not trace them).
- It cannot implement interfaces.
- It cannot be a generic type argument (no GC header).

| Method | Returns | Notes |
|---|---|---|
| `alloc(size: u64)` (static) | `Buffer \| null` | Foreign-heap allocation; `null` on OOM. |
| `get(self, i: u64)` | `u8 \| null` | Out-of-range → `null`. |
| `set(self, i: u64, v: u8)` | `null` | Out-of-range → silent no-op. |
| `free(self)` | `null` | Releases the foreign-heap region. |

```
var maybe = Buffer.alloc(1024u64)
if maybe is null {
  // allocation failed
} else {
  var buf = maybe as Buffer
  buf.set(0u64, 65u8)        // 'A'
  buf.free()
}
```

## 18.8 Numeric helper namespaces

Each numeric primitive has a static namespace for non-default arithmetic:

```
i32.MIN
i32.MAX
i32.wrapping_add(a, b)
i32.checked_add(a, b)
i32.saturating_add(a, b)
i32.overflowing_add(a, b)
// ... similarly for sub, mul, div, rem, neg, shl, shr
```

Float-specific:

```
f64.INFINITY
f64.NEG_INFINITY
f64.NAN
f64.is_nan(x)
f64.is_infinite(x)
f64.is_finite(x)
```

Conversions between numeric types use `as` (see [12-type-logic.md](./12-type-logic.md)).

## 18.9 What lives in `core:prelude` — recap

The chapter sections above all describe `core:prelude` contents:

- `str` and its methods (§18.2).
- `List<T>` (§18.3).
- `Map<K, V>` and `Entry<K, V>` (§18.4).
- `Item<T>`, `Done` (§18.5).
- `Iterator<T>` interface (§18.6).
- `Buffer` extern struct (§18.7).
- Numeric helper namespaces (§18.8).

Plus everything documented in their own chapters: operator and lifecycle interfaces ([15-operators.md](./15-operators.md)), `Future<T>`/`Ready<T>`/`Pending`/`Context` ([21-async.md](./21-async.md)), `Try`/`FromResidual` ([13-error-handling.md](./13-error-handling.md)), `panic`/`panic_with` ([14-panics.md](./14-panics.md)), `pin`/`unpin` and `ReprC` ([19-ffi.md](./19-ffi.md)).

None of these requires an OS. `core:prelude` works on any target that supplies an allocator.

## 18.10 `std:io` — printing and IO

```
import { print, println } from "std:io"

print("hello")        // no trailing newline
println("hello")      // appends '\n'
```

Both take `str`. To print non-strings, convert first via `as str` (numeric primitives) or by implementing a project-local stringification interface.

`std:io` also provides file handles and byte streams; their full API is out of scope for this chapter. The point is: any operation that touches stdin/stdout/stderr or the filesystem requires an OS, so it lives under `std:`.

## 18.11 Other `std:*` modules

| Module | Provides | Documented in |
|---|---|---|
| `std:thread` | `Thread.spawn`, `JoinHandle`, `Joined<R>`, `Panicked` | [20-concurrency.md](./20-concurrency.md) |
| `std:sync` | `Shared<T>`, `LockBusy`, channels, senders, receivers | [20-concurrency.md](./20-concurrency.md) |
| `std:async` | `spawn`, `block_on`, `timeout`, default executor, `AsyncIterator` adapters | [21-async.md](./21-async.md) |
| `std:time` | wall-clock and monotonic time | (future) |
| `std:fs` | filesystem | (future) |
| `std:net` | sockets, TCP, UDP | (future) |

`Future<T>` / `Ready<T>` / `Pending` / `Context` and `AsyncIterator<T>` (as a bare interface) live in `core:prelude` because they are language-semantic shapes — the *runtime* that drives them lives in `std:async`.

## 18.12 What is **not** in this version of the standard library

- No regular expressions.
- No JSON (the user can write a parser; the `extern` interface is available).
- No time / date (likely a future `std:time` module).
- No formatting machinery (no `Display` / `Debug` / `format!` analogues).
- No async filesystem or networking adapters (a future addition layered on `std:async` + `std:fs`/`std:net`).

These are intentional omissions to keep the surface small. They may be added as separate `std:*` modules in later revisions.
