# 19. Foreign Function Interface

The FFI crosses the C ABI boundary. It separates three concerns:

- **Layout** — how a type is arranged in memory. Controlled by `extern struct` plus optional layout decorators.
- **ABI** — how a function is called. Controlled by `extern function` plus optional ABI decorators.
- **Allocation** — which heap a value lives on. Controlled by `Foreign.alloc` (foreign heap) versus normal construction (managed heap or stack).

`extern struct` is the only structural-layout form; there is no separate `@Repr`. Any function can be marked `extern` to opt into the C ABI.

## 19.1 The two heaps (recap)

- **Managed heap** — GC-traced. Holds normal structs, lists, maps, strings, closures.
- **Foreign heap** — allocated by extern code (`malloc`, `mmap`, library allocators) or by `Foreign.alloc`. Opaque to the GC; manual lifetime.

`*T` is a raw pointer into either region — the GC inspects the address range at runtime to decide whether to trace it. `extern struct` types **have no GC header** and can live on either heap (or on the stack).

See [16-memory.md](./16-memory.md).

## 19.2 Pointers

### 19.2.1 The pointer type `*T`

`*T` is a raw machine pointer. It is a single machine word with no length, alignment, or ownership tag.

- A `*T` value is obtained from an extern function return, from `&expr` (§19.2.2), from `Foreign.alloc` (§19.8), or from pointer reinterpretation via `as` (§19.2.5).
- Holding a `*T` is the language's marker that you have stepped outside the normal safety guarantees. There is no separate `unsafe` block; the presence of `*T` is itself the marker.
- The compiler does not insert pin/unpin around pointer values; managing managed-heap lifetimes through a `*T` is the user's responsibility (§19.15).

### 19.2.2 `&expr` — address-of

`&expr` produces a `*T` from a place-expression of type `T`.

```
extern function getrusage(who: i32, out: *Rusage): i32

function snapshot(): Rusage | i32 {
  var ru = Rusage {
    ru_utime: Timeval { tv_sec: 0i64, tv_usec: 0i64 },
    ru_stime: Timeval { tv_sec: 0i64, tv_usec: 0i64 },
  }
  var rc = getrusage(0i32, &ru)
  if rc != 0i32 { return rc }
  ru
}
```

Rules:

- The operand must be a place-expression (a local, a struct field, or an array element). Taking `&` of a temporary is a compile error.
- The resulting pointer is valid only for the enclosing extern call site. Storing it in a longer-lived location (return value, struct field, captured closure, channel send) is a compile error.
- If the operand lives on the managed heap, `&` auto-pins for the duration of the call and auto-unpins on return. This is the **only** implicit pin in the language.
- If the operand is an `extern struct` on the stack, no pin is needed.

### 19.2.3 `*ptr` — dereference

`*ptr` is a prefix operator that dereferences a pointer. Combined with field access, `(*ptr).field` reads or writes through the pointer.

```
var p: *Counter = ...
var n = (*p).value
(*p).value = (*p).value + 1
```

- Dereferencing a `*T | null` whose value is `null` panics with the message `null pointer dereference`.
- Dereferencing a pointer to a managed-heap object that is not currently pinned is undefined behavior.
- Dereferencing a wrongly-typed pointer is undefined behavior.

### 19.2.4 `*T | null` and null-pointer optimization

The union `*T | null` is laid out as a single raw pointer where `null` is the bit pattern `0x0`. This is a **general rule**: any union `null | P` where `P`'s representation cannot legitimately be all-zero qualifies for null-pointer optimization (NPO).

NPO applies to:

- `*T` for any `T`.
- Extern function pointer types (`extern (...) -> R`).
- `extern type T` opaque handles passed as `*T`.
- `@Transparent` newtypes wrapping any of the above.

```
extern function malloc(size: c_size_t): *c_void | null
extern function find_user(id: u32): *User | null
```

If a return is declared as `*T` (non-nullable) but the foreign code returns `0x0` at runtime, dereferencing panics. Use `*T | null` for any pointer that can be null.

### 19.2.5 `as` between pointer types

`as` already covers union narrowing, primitive conversion, and interface widening/narrowing (see [12-type-logic.md](./12-type-logic.md)). This chapter extends it to **pointer-to-pointer reinterpretation**:

```
var p: *c_void = ...
var q: *MyStruct = p as *MyStruct
```

Both source and destination must be pointer types. The cast is a no-op at runtime; the user vouches for the layout. No other `as` behavior changes.

## 19.3 Extern structs and layout decorators

`extern struct` declares a C-layout struct: fields in declared order, native alignment, no GC header.

```
extern struct Buffer {
  pub data: *u8,
  pub size: u64,
}

extern struct Timeval {
  pub tv_sec:  i64,
  pub tv_usec: i64,
}
```

Consequences (unchanged from the prior spec):

- No GC header. Cannot implement interfaces that require dynamic dispatch (an `extend X: SomeInterface` block is fine; using `X` as an interface object type is not).
- Cannot be a generic type argument to managed-heap collections (`List<X>`, `Map<K, X>`).
- All field types must themselves be extern-compatible.
- May live on the foreign heap (`Foreign.alloc`), on the stack (a local), or as a by-value parameter or return.

Field visibility follows the same `pub`/private rules as regular structs (see [04-structs.md](./04-structs.md)).

### 19.3.1 Layout decorators

Four blessed proc-macros (registered the same way `@Derive` is in [22-macros.md §22.11](./22-macros.md)) modify the layout of an `extern struct`:

```
@Packed(1)
extern struct DiskHeader {
  pub magic:   u32,
  pub version: u16,
  pub flags:   u8,
}

@Align(64)
extern struct CacheLine {
  pub bytes: [u8; 64],
}

@Transparent
struct Handle(i32)

@Union
extern struct FloatBits {
  pub f: f32,
  pub i: u32,
}
```

| Decorator | Applies to | Effect |
|---|---|---|
| `@Packed(N)` | `extern struct` | Fields not aligned beyond `N` bytes. `@Packed` alone means packing 1. |
| `@Align(N)` | `extern struct` | Minimum alignment `N` on the struct. |
| `@Transparent` | tuple struct with exactly one non-zero-sized field | Wrapper has the same ABI as the inner type, including NPO. Applies to both regular and extern structs. |
| `@Union` | `extern struct` | All fields share offset 0; size is the max field size; reads return whatever bits are there. The user tracks which field is "active." |

Decorator placement follows [22-macros.md §22.2](./22-macros.md): above the item, before any `pub`.

### 19.3.2 `ReprC`

`ReprC` is a marker interface in `core:ffi`. It is auto-implemented for every `extern struct` type, every numeric primitive, every pointer type, and every `@Transparent` wrapper around such a type. It is the constraint used by generic FFI helpers that need a known-layout type — most notably `Foreign.alloc<T: ReprC>()`.

## 19.4 Opaque foreign types — `extern type`

```
extern type Sqlite3
extern type Sqlite3Stmt

extern function sqlite3_open(path: *u8, db: **Sqlite3): i32
extern function sqlite3_prepare_v2(
  db: *Sqlite3, sql: *u8, n: i32, stmt: **Sqlite3Stmt, tail: **u8,
): i32
```

`extern type T` declares a foreign-only nominal type with unknown size and alignment. Values of type `T` cannot be constructed, copied, or destructured in this language; only `*T` and `*T | null` are usable forms. Matches the `FILE*`, `sqlite3*` idiom in C.

## 19.5 Foreign globals — `extern var`

```
@Link(lib = "c")
extern var errno: i32

@Link(lib = "c")
extern var environ: **u8
```

`extern var name: T` declares a foreign global. Reads and writes are direct. The compiler emits the platform-appropriate load (TLS on thread-local symbols).

There is no new `const` keyword: a constant from a C header is emitted by `bindgen` as a module-level `pub var`, whose initializer is a compile-time constant per [17-modules.md §17.6](./17-modules.md#176-no-top-level-runtime).

```
pub var PATH_MAX: usize = 4096usize
pub var O_RDONLY: i32   = 0i32
pub var O_WRONLY: i32   = 1i32
```

## 19.6 Fixed-size arrays — `[T; N]`

`[T; N]` is a fixed-size array type with `N` resolved at compile time. **Its scope is narrow**: it is allowed only as an `extern struct` field type and as an extern function parameter or return type (typically via `*[T; N]`).

```
extern struct InAddr {
  pub bytes: [u8; 4],
}

extern struct Sockaddr {
  pub family: u16,
  pub data:   [u8; 14],
}

extern function read_into(buf: *[u8; 256], n: usize): isize
```

This is the minimum addition needed for `extern struct`s to mirror C structs; no general-purpose array type or const-generics is introduced into the language.

## 19.7 C-width aliases — `core:ffi`

C's `int`, `long`, `char` are platform-dependent. The C-width aliases live in `core:ffi`:

```
import {
  c_int, c_uint, c_long, c_ulong, c_longlong, c_ulonglong,
  c_short, c_ushort, c_char, c_schar, c_uchar,
  c_float, c_double,
  c_size_t, c_ptrdiff_t, c_intptr_t, c_uintptr_t,
  c_void, c_va_list,
} from "core:ffi"
```

- Numeric aliases lower to the appropriate fixed-width type for the target ABI.
- `c_void` is declared as `extern type c_void`; only `*c_void` is a valid value form. Replaces `*u8`-for-everything as the "type-erased pointer."
- `c_va_list` is declared as `extern type c_va_list`; used to wrap C variadic functions through the va_list interface.

`core:ffi` is part of `core:` (no OS dependency); freestanding targets get it without an OS.

## 19.8 Foreign allocation — `Foreign`

`Foreign` is a namespace struct in `core:ffi` whose static methods are the foreign-heap allocator API:

```
pub struct Foreign;

extend Foreign {
  function alloc<T: ReprC>():                                  *T | null
  function alloc_zeroed<T: ReprC>():                           *T | null
  function alloc_flex<T: ReprC, E: ReprC>(extra_count: usize): *T | null
  function realloc<T: ReprC>(p: *T, new_size: usize):          *T | null
  function free<T: ReprC>(p: *T)
}
```

All methods are static (no `self` parameter, per [10-interfaces.md §10.1](./10-interfaces.md#101-interface-declaration)). All allocating methods return `null` on failure — foreign allocation is fallible, unlike managed allocation (see [16-memory.md §16.9](./16-memory.md#169-allocation-failure)).

`Buffer.alloc(size)` is reimplemented on top of `Foreign.alloc`; existing code that uses `Buffer.alloc` does not change.

### 19.8.1 Flexible array members

The C idiom `struct foo { int n; char data[]; }` is expressed by giving the trailing field a regular pointer type and using `Foreign.alloc_flex` for a single allocation:

```
extern struct Message {
  pub kind:   u32,
  pub length: u32,
  pub data:   *u8,
}

function make_message(kind: u32, payload: Buffer): *Message | null {
  var p = Foreign.alloc_flex<Message, u8>(payload.size as usize)
  if p is null { return null }
  // initialize header fields and copy payload bytes after the header
  ...
  p
}
```

No field-level decorator and no new field marker.

### 19.8.2 Bitfields

C bitfields are not supported directly. Treat the underlying integer as a regular field and mask/shift in user code:

```
extern struct Flags {
  pub raw: u32,
}

extend Flags {
  function kind(self):   u32  { self.raw & 0x07u32 }
  function dirty(self):  bool { (self.raw & 0x08u32) != 0u32 }
  function serial(self): u32  { self.raw >> 4 }
}
```

If a C library forces bitfield interop, `bindgen` (§19.18) emits accessor methods like the above and notes that the underlying layout is platform-dependent.

## 19.9 Strings — `CStr`, `CString`, `Buffer`

`str` is **not** ABI-compatible with C strings (it is a managed-heap UTF-8 buffer with a length header, not NUL-terminated). `core:ffi` ships the boundary types.

### 19.9.1 `CStr` — borrowed NUL-terminated pointer

```
extern struct CStr {
  pub ptr: *u8,
}
```

A borrowed view of a NUL-terminated UTF-8 byte sequence. Lifetime is unspecified; the caller is responsible for keeping the underlying buffer alive.

| Method | Returns | Notes |
|---|---|---|
| `CStr.from_ptr(p: *u8)` | `CStr` | Wraps a raw pointer; no copy, no validation. |
| `CStr.byte_len(self)` | `usize` | Walks to the NUL terminator. |
| `CStr.to_str(self)` | `str \| InvalidUtf8` | Copies into the managed heap and validates UTF-8. |

### 19.9.2 `CString` — owned NUL-terminated buffer

```
pub struct CString { ... }
```

| Method | Returns | Notes |
|---|---|---|
| `CString.from_str(s: str)` | `CString \| null` | Foreign-heap copy with appended NUL. `null` if `s` contains an interior NUL or allocation fails. |
| `CString.as_cstr(self)` | `CStr` | Borrow. |
| `CString.free(self)` | `null` | Releases the foreign-heap buffer. Also runs on `Drop`. |

`CString` implements `Drop`, so a value going out of scope frees the underlying buffer automatically.

### 19.9.3 `Buffer` — `(ptr, size)` pair

`Buffer` (in `core:prelude`) covers the case where length matters separately from a NUL terminator:

```
extern struct Buffer {
  pub data: *u8,
  pub size: u64,
}
```

See [18-stdlib.md §18.7](./18-stdlib.md#187-buffer-extern-struct) for the full API. Both `CStr` and `Buffer` are `extern struct`s, so they pass to C by value with no shim.

## 19.10 Extern functions

```
extern function name(args): R          // import (no body)
extern function name(args): R { ... }  // export (with body)
```

### 19.10.1 Imports

```
extern function malloc(size: c_size_t): *c_void | null
extern function free(p: *c_void)
extern function strlen(s: *u8): usize
```

The implementation is provided by foreign code linked at build time. Calling an imported extern function compiles to a C call. The compiler wraps the call in a thread-blocked transition (§19.16.1).

### 19.10.2 Exports

```
extern function on_tick(d: *MyStruct) {
  (*d).counter = (*d).counter + 1
}
```

The compiler emits the function with C calling convention so foreign code can call it. Exported extern functions are **always top-level** and **cannot capture variables** — a C function pointer has no environment slot. C callbacks conventionally pass user state through a `void*` parameter; in this language that role is filled by `*c_void` (or `*T`).

## 19.11 Function decorators

Six decorators compose on top of `extern function`. Each is a compiler-blessed proc-macro; placement follows [22-macros.md §22.2](./22-macros.md) (above the item, before `pub`).

```
@Link(lib = "z", kind = "dynamic", version = ">=1.2")
@Symbol("inflateInit_")
extern function zlib_inflate_init(strm: *ZStream, ver: *u8, sz: i32): i32

@CallConv("system")
extern function WinMain(hInst: *c_void, hPrev: *c_void, cmd: *u8, show: i32): i32

@Variadic
extern function printf(fmt: *u8): c_int

@Reentrant("any")
@OnPanic("abort")
extern function on_tick(data: *c_void) {
  ...
}
```

| Decorator | Applies to | Effect |
|---|---|---|
| `@Link(lib, kind, version?, path?)` | imports | References a `[package.links]` entry by name. If absent, the symbol resolves from the host process. |
| `@Symbol("name")` | imports, exports | Override the symbol name; default is the function name. |
| `@CallConv("c" \| "system" \| "stdcall" \| "fastcall")` | imports, exports | Calling convention. Default `"c"`. |
| `@Variadic` | imports | Function takes additional arguments at call sites; C default argument promotions apply. See §19.12. |
| `@Reentrant("managed-only" \| "any")` | exports | Whether foreign threads may invoke this. See §19.16.2. |
| `@OnPanic("abort" \| value)` | exports | Behavior on panic in the body. See §19.16.3. |

## 19.12 Variadic imports

`@Variadic` declares an import that takes additional unspecified arguments after the fixed prefix. There is no `...` token in the signature — the decorator alone carries the meaning. This is the one exception to "no variadics" ([09-functions.md §9.11](./09-functions.md#911-no-variadics-no-default-arguments-no-named-arguments)), and it exists only for FFI interop.

```
@Variadic
extern function printf(fmt: *u8): c_int

function example() {
  var fmt = (CString.from_str("%d + %d = %d\n") as CString).as_cstr()
  printf(fmt.ptr, 1i32, 2i32, 3i32)
}
```

The compiler applies C default argument promotions to each variadic argument at the call site: `i8`/`i16` → `c_int`, `u8`/`u16` → `c_uint`, `f32` → `f64`. Other types pass unchanged.

Defining a variadic body in this language is not supported. To wrap a C variadic, take a `*c_va_list` parameter and pass it to libc's `vprintf` family.

## 19.13 Function pointers and callbacks

The extern function-type form `extern (T1, T2, ...) -> R` is already part of the language ([09-functions.md §9.3](./09-functions.md#93-function-type-syntax)). Closures cannot be passed across the C ABI (they carry an environment a C function pointer has no slot for); only top-level `extern function` symbols are callable as `extern (...) -> R` values.

For callbacks with state, `core:ffi` provides a generic bundle:

```
extern struct Callback<C, R> {
  pub fn:   extern (*c_void, C) -> R,
  pub data: *c_void,
}
```

Many C APIs accept `(callback, userdata)` as two separate parameters — pass them separately. `Callback<C, R>` is for the APIs that expect them bundled in a struct.

## 19.14 C patterns expressed without new syntax

### 19.14.1 C enums

A C enum is an integer with named constants. Express it as a type alias plus module-level `pub var` values:

```
pub type FileMode = i32

pub var MODE_READ:  FileMode = 0i32
pub var MODE_WRITE: FileMode = 1i32
pub var MODE_RW:    FileMode = 2i32
```

If exhaustive matching matters, write a language-side discriminated union on top and translate at the boundary:

```
pub type ParsedMode = Read | Write | ReadWrite | UnknownMode

function parse_mode(m: FileMode): ParsedMode {
  if m == MODE_READ       { Read }
  else if m == MODE_WRITE { Write }
  else if m == MODE_RW    { ReadWrite }
  else                    { UnknownMode }
}
```

`bindgen` (§19.18) emits this pattern automatically from a C `enum`.

### 19.14.2 C unions

Use `@Union extern struct` (§19.3.1).

### 19.14.3 Flexible array members

Use `Foreign.alloc_flex` (§19.8.1).

### 19.14.4 Bitfields

Use mask/shift methods (§19.8.2).

## 19.15 Pinning

The GC may move or reclaim managed objects at any time. Whenever a `*T` pointing into the managed heap is handed to foreign code, the object must be **pinned** so the GC neither moves nor collects it.

Three complementary APIs cover the cases.

### 19.15.1 `&expr` — auto-pin for a single call

`&expr` (§19.2.2) pins automatically for the duration of the enclosing extern call. This is the most common pattern (output parameters, scalar handoff) and requires no extra API.

### 19.15.2 `with_pin` — scoped pin

For passing a managed value into a closure body that does extra work:

```
import { with_pin } from "core:ffi"

with_pin(state, |ptr| {
  call_native_op(ptr)
})
```

```
pub function with_pin<T, R>(value: T, body: (*T) -> R): R
```

- Pins `value` before invoking `body`.
- Unpins on every exit path, including panic-unwind.
- The pointer is valid only inside `body`; the compiler enforces this with the same rule that gates `&expr`.

The closure is passed inside the call parentheses — there is no trailing-lambda syntax in the language ([09-functions.md §9.5](./09-functions.md#95-closures--short-form)).

### 19.15.3 `Pin.acquire` and `PinHandle<T>` — long-lived pin

For callback registrations that outlive any single call:

```
import { Pin, PinHandle } from "core:ffi"

var handle = Pin.acquire(state)
register_callback(on_tick, handle.ptr() as *c_void)
// ... later ...
unregister_callback(handle.ptr() as *c_void)
handle.release()
```

```
pub struct Pin;
extend Pin {
  function acquire<T>(value: T): PinHandle<T>
}

pub struct PinHandle<T> { ... }
extend<T> PinHandle<T> {
  function ptr(self):     *T
  function release(self): null
}
extend<T> PinHandle<T>: Drop {
  function drop(self) { /* releases the pin if not already released */ }
}
```

`PinHandle<T>` implements `Drop`. If `release()` is never called explicitly, the handle's `Drop` impl releases the pin and (in debug builds) emits a warning. The resource is owned by the handle — there is no manual `pin` / `unpin` user-facing API.

### 19.15.4 Pinning rules

- Pinning is refcounted per object: nested `with_pin` calls or multiple `PinHandle`s on the same value compose correctly.
- Pinning a value that already lives on the foreign heap (an `extern struct` constructed via `Foreign.alloc`, or a stack-local `extern struct`) is a no-op — the value is already non-moving.
- The `&` auto-pin and `with_pin` borrows do not require explicit release; the compiler emits the unpin.

## 19.16 Runtime semantics

### 19.16.1 Safe-points around imports

Every call to an imported extern function is compiled with a thread-blocked transition on either side:

- Before the call: the thread's stack is snapshotted; the thread declares itself blocked. Other threads can run GC without waiting for this thread to reach a safe-point.
- During the call: pinned values remain rooted. Nothing else on the calling thread's stack is reachable for tracing.
- After the call: the thread re-acquires its mutator state. If GC ran during the call, the thread re-checks its roots.

This is the JNI / Cgo / .NET pinvoke model. It is not exposed at the source level; the compiler emits the transitions automatically. Long-running C calls do not stall the rest of the program.

### 19.16.2 Foreign threads calling exports

A thread the runtime did not spawn must **attach** before invoking an exported extern function. Two ways.

#### Manual

```
import { attach_thread, detach_thread } from "core:ffi"

extern function on_tick(data: *c_void) {
  attach_thread()
  do_work(data)
  detach_thread()
}
```

#### Automatic via `@Reentrant("any")`

```
@Reentrant("any")
extern function on_tick(data: *c_void) {
  do_work(data)
}
```

| `@Reentrant` value | Meaning |
|---|---|
| `"managed-only"` (default) | Only threads the runtime created may call this. Calling from an unattached thread panic-aborts in debug, is UB in release. |
| `"any"` | Compiler emits attach/detach automatically. Safe to call from any thread; small per-call cost. |

Attach behavior:

- Registers the thread with the runtime so the GC can scan its stack.
- Refcount operations on objects reachable from the thread become atomic (matches the cross-thread rule in [16-memory.md §16.4](./16-memory.md#164-refcount-semantics)).
- Detach removes the thread from the GC root set.

There is no `defer`-style scope marker; users who want lexical cleanup either wrap with `@Reentrant("any")` (recommended) or pair the calls themselves.

### 19.16.3 Panics across the boundary

Exported extern functions never unwind through C frames. `@OnPanic` picks the policy:

| Value | Meaning |
|---|---|
| `@OnPanic("abort")` (default) | Panic in body → process abort at the FFI boundary. Message goes to stderr. |
| `@OnPanic(value)` | Panic in body → log the message; return `value` (must type-match the declared return type). |

```
@OnPanic(-1i32)
extern function process_chunk(buf: *u8, n: usize): i32 {
  if n > MAX { panic("chunk too large") }
  ...
}
```

Imports have no in-language panic policy; if a C function aborts or longjmps, the platform's rules apply.

### 19.16.4 GC interaction with foreign memory

- The GC ignores foreign-heap addresses (already in [16-memory.md](./16-memory.md)).
- Managed pointers inside an `extern struct` are **not** traced. The user keeps the target pinned for the struct's lifetime.
- The cycle collector assumes every mutator thread is at a safe-point or blocked. Threads inside an FFI call (auto-blocked per §19.16.1) do not stall collection.

### 19.16.5 Signal handlers

Running language code from a POSIX signal handler is undefined behavior unless the handler is on a thread that is currently attached and the body is `@Reentrant("any")` — and even then, the usual signal-safety rules (no allocation, no locks held by the interrupted thread) apply. Practical advice: don't.

## 19.17 Linkage and the manifest

```toml
[package.links]
zlib    = { lib = "z",      kind = "dynamic", version = ">=1.2" }
mylib   = { lib = "mylib",  kind = "static",  path = "vendor/libmylib.a" }
crypto  = { lib = "crypto", kind = "dynamic" }

[package.ffi]
bindings = [
  { header    = "vendor/zlib.h",
    output    = "src/bindings/zlib.lang",
    allowlist = ["inflate*", "deflate*"] },
  { header = "vendor/mylib.h",
    output = "src/bindings/mylib.lang" },
]

exports = [
  { module = "lib", output = "target/include/mylib.h" },
]
```

`@Link(lib = "name")` in source refers to a `[package.links]` entry. A function with only `@Symbol("...")` and no `@Link` resolves from the host process — useful for libc and platform symbols.

`lang build` runs `bindgen` automatically for any `[package.ffi.bindings]` entry whose header is newer than its output, and `cbindgen` for `[package.ffi.exports]`. See [23-cli.md §23.12](./23-cli.md#2312-ffi-and-linking).

## 19.18 Tooling — `lang ffi`

```
lang ffi bindgen <header>      Generate extern struct, extern type, extern function,
                               extern var declarations, and pub var constants from a C
                               header. Powered by libclang. Output is deterministic.

lang ffi cbindgen              Emit a C header declaring every pub extern function and
                               pub extern struct in the given module. Round-trips with
                               bindgen.

lang ffi check                 Verify that this crate's extern signatures still match a
                               named C header. CI gate.

lang ffi layout                Print size, alignment, and field offsets for one or all
                               extern struct types in the current crate, for a target.
                               Debugging ABI mismatches.
```

See [23-cli.md §23.12](./23-cli.md#2312-ffi-and-linking) for the full CLI reference.

## 19.19 Restrictions on extern signatures

Extern function signatures must be expressible in the C ABI.

### Allowed parameter / return types

- Numeric primitives (`i8`..`i64`, `u8`..`u64`, `f32`, `f64`, `bool`, `char`, `isize`, `usize`).
- C-width aliases from `core:ffi` (`c_int`, `c_long`, etc.).
- `*T` for any `T`, and `*T | null` (NPO).
- `extern struct` types passed by value.
- `@Union extern struct` types passed by value (rare; alignment can be subtle).
- `@Transparent` wrappers around any of the above.
- `[T; N]` (typically through `*[T; N]`).
- `extern (...) -> R` function pointer types.
- `extern type T` opaque handles, only through `*T`.
- Generic type parameters constrained to pointer position (see §19.20).

### Not allowed

- Non-extern struct types, `str`, `List<T>`, `Map<K, V>`, interface objects — by value.
- Discriminated unions other than NPO-eligible `null | P` forms. To pass a discriminated union to C, take a pointer: `*(A | B)`.
- Closures — use `extern (...) -> R` function pointers or `Callback<C, R>`.

A discriminated union other than NPO-eligible forms is rejected at the boundary; the runtime tag has no defined C ABI representation.

### Return type omission

A function with no declared return type returns `null` (see [09-functions.md §9.1](./09-functions.md#91-function-declaration)). At the extern boundary, an omitted return type lowers to C `void`.

## 19.20 Generics across FFI

Generic type parameters appear in extern signatures **only as pointers**:

```
extern function hashmap_set<T>(key: *str, value: *T)
extern function alloc_array<T: ReprC>(n: u64): *T | null
```

The C ABI fixes the size of every argument at the call site; an unknown `T` has unknown size. Restricting to `*T` keeps every call site a known pointer-sized argument.

`T: ReprC` is the common constraint when the function intends the pointee to be C-layout-compatible.

## 19.21 Worked examples

### 19.21.1 Round-trip with libc

```
import { c_size_t, c_void } from "core:ffi"

extern function malloc(size: c_size_t): *c_void | null
extern function free(p: *c_void)
extern function memcpy(dst: *c_void, src: *c_void, n: c_size_t): *c_void

function copy_to_foreign(src: Buffer): Buffer | null {
  var p = malloc(src.size as c_size_t)
  if p is null { return null }
  memcpy(p as *c_void, src.data as *c_void, src.size as c_size_t)
  Buffer { data: p as *u8, size: src.size }
}
```

### 19.21.2 Output parameter via `&`

```
extern struct Timeval {
  pub tv_sec:  i64,
  pub tv_usec: i64,
}

extern struct Rusage {
  pub ru_utime: Timeval,
  pub ru_stime: Timeval,
}

extern function getrusage(who: i32, out: *Rusage): i32

function snapshot(): Rusage | i32 {
  var ru = Rusage {
    ru_utime: Timeval { tv_sec: 0i64, tv_usec: 0i64 },
    ru_stime: Timeval { tv_sec: 0i64, tv_usec: 0i64 },
  }
  var rc = getrusage(0i32, &ru)
  if rc != 0i32 { return rc }
  ru
}
```

### 19.21.3 Callback registration with a long-lived pin

```
import { Pin, PinHandle, c_void } from "core:ffi"

extern function register_callback(cb: extern (*c_void) -> null, data: *c_void)
extern function unregister_callback(data: *c_void)

struct MyState {
  pub counter: i64,
}

@Reentrant("any")
@OnPanic("abort")
extern function on_tick(data: *c_void) {
  var state = data as *MyState
  (*state).counter = (*state).counter + 1
}

function install(): PinHandle<MyState> {
  var s = MyState { counter: 0i64 }
  var handle = Pin.acquire(s)
  register_callback(on_tick, handle.ptr() as *c_void)
  handle
}

function uninstall(handle: PinHandle<MyState>) {
  unregister_callback(handle.ptr() as *c_void)
  handle.release()
}
```

### 19.21.4 Scoped pin via `with_pin`

```
import { with_pin, c_size_t, c_void } from "core:ffi"

extern function call_native_op(input: *u8, len: c_size_t, ctx: *c_void): i32

struct Workload {
  pub items: i64,
}

function run(input: Buffer, w: Workload): i32 {
  with_pin(w, |w_ptr| {
    call_native_op(input.data, input.size as c_size_t, w_ptr as *c_void)
  })
}
```

## 19.22 Summary

- **Layout** — `extern struct` declares C layout. `@Packed(N)`, `@Align(N)`, `@Transparent`, `@Union` are layout decorators.
- **Pointers** — `*T` raw pointer. `*T | null` via NPO. `&expr` for address-of at extern call sites (auto-pins managed values for the call). `*ptr` and `(*ptr).field` for read/write through a pointer. `as` for pointer-to-pointer reinterpretation.
- **Foreign types** — `extern type T` for opaque handles. `[T; N]` for fixed-size arrays in extern positions.
- **Foreign globals** — `extern var name: T` with optional `@Link`.
- **Allocation** — `Foreign.alloc` family from `core:ffi`. `Buffer`, `CStr`, `CString` are the boundary types.
- **C-width aliases** — `c_int`, `c_long`, ..., `c_void`, `c_va_list` from `core:ffi`.
- **Functions** — `extern function` for imports and exports. Decorators `@Link`, `@Symbol`, `@CallConv`, `@Variadic`, `@Reentrant`, `@OnPanic`.
- **C patterns without new syntax** — C enums via `type` alias + `pub var` constants. C unions via `@Union extern struct`. Flexible arrays via `Foreign.alloc_flex`. Bitfields via mask/shift methods.
- **Pinning** — `&` auto-pin for the call, `with_pin` for scoped, `Pin.acquire`/`PinHandle<T>` for long-lived.
- **Runtime** — auto safe-point on imports, manual or `@Reentrant` attach for foreign threads, `@OnPanic` for export panic policy.
- **Manifest** — `[package.links]` and `[package.ffi]` drive linkage and tooling.
- **Tooling** — `lang ffi bindgen` / `cbindgen` / `check` / `layout`.

Net-new syntax: `&expr`, `*ptr`, `(*ptr).field`, `[T; N]` (extern positions only), `extern type T`, `extern var name: T`, `as` extended to pointer-to-pointer reinterpretation. Everything else uses existing language forms.
