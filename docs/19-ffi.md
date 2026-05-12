# 19. Foreign Function Interface

The `extern` keyword crosses the C ABI boundary. It applies to functions, structs, and function types.

## 19.1 The two heaps (recap)

- **Managed heap** — GC-traced. Holds normal structs, lists, maps, strings, closures.
- **Foreign heap** — allocated by extern code (`malloc`, `mmap`, library allocators). Opaque to the GC; manual lifetime.

`extern struct` values live in the foreign heap. Non-extern struct values live in the managed heap. `*T` is a raw pointer into either region — the GC inspects the address range at runtime to decide whether to trace it.

See [16-memory.md](./16-memory.md).

## 19.2 Extern functions

The `extern` keyword on a function declares it as having the C calling convention. Two forms:

### Import (no body)

```
extern function print_native(value: str)
extern function malloc(size: u64): *Buffer
extern function strlen(s: *u8): u64
```

The implementation is provided by foreign code linked at build time. Calling an import-form extern function calls into C (or a C-compatible binary).

### Export (with body)

```
extern function on_tick(d: *MyStruct) {
  d.counter = d.counter + 1
}
```

The compiler emits the function with C calling convention so foreign code can call it. Typically used for callbacks passed to C libraries.

Exported extern functions are **always top-level**. They **cannot capture variables** — a C function pointer has nowhere to store an environment. C callbacks conventionally pass user state through a `void*` parameter; in this language that role is filled by a `*T` argument.

```
extern function register_timer(cb: extern (*u8) -> null, data: *u8)

extern function my_handler(data: *u8) {
  // `data` is the user pointer passed in at registration
  ...
}

function setup() {
  var state = MyState { ... }
  register_timer(my_handler, pin(state) as *u8)
}
```

## 19.3 Pointer syntax

`*T` is a raw machine pointer. **Inside extern contexts only** (extern function signatures, extern struct fields, extern function-type signatures), a parameter or field may be prefixed with `*` to mark it as a pointer.

Outside extern contexts, `*T` is not a valid type. Managed code uses references implicitly (struct values are heap pointers internally; you don't write `*` for them).

Pointer rules:

- `*T` is a single machine word.
- `*T` does not carry a length, alignment, or ownership tag — it's a raw address.
- Dereferencing a `*T` to a managed type from foreign code requires the pointed-at object to be pinned (see 19.6). If unpinned, the GC may have moved or freed the object.
- Pointer arithmetic is not supported in the source language; use stdlib helpers if needed.

## 19.4 Nullable pointers — `*T | null`

A pointer that may be `null` is written as the union `*T | null`. This is a **special-cased union** at the FFI boundary: it lowers to a single raw pointer where the value `null` corresponds to the bit pattern `0x0`.

```
extern function malloc(size: u64): *Buffer | null
extern function find_user(id: u32): *User | null
```

This is the only union type allowed in the extern signature of a parameter or return value (see 19.7).

If a return type is declared as `*T` (non-nullable) but the foreign code returns `0x0` at runtime, dereferencing panics. Use `*T | null` for any pointer that can be null.

## 19.5 Extern structs

```
extern struct Buffer {
  data: *u8,
  size: u64,
}

extern struct MyCFunctionArgs {
  flags: i32,
  name:  *u8,
}
```

Extern structs are laid out as **C structs**: each field at its native C offset, with platform-standard alignment and padding. No GC header. Visible to C code byte-for-byte.

Consequences:

- An extern struct cannot implement an interface that requires the GC header for dispatch (i.e. nearly all interfaces). Specifically, extern structs do not support interface objects or `as` narrowing through a union variant.
- An extern struct's fields can themselves be pointers to managed values (`*T`), but the GC won't trace them. You're on the hook for keeping the managed targets pinned for the lifetime of the extern struct.
- Construct an extern struct with the usual struct-literal syntax; storage is on the foreign heap if explicitly allocated (`Buffer.alloc`), or on the stack if used as a local value.

Field visibility for extern structs follows the same `pub`/private rules as regular structs.

## 19.6 Pinning

The GC is free to move or reclaim managed objects at any time. Whenever a `*T` pointing into the managed heap is handed to foreign code, the object must be **pinned** so the GC neither moves nor collects it. Pinning is **always manual** — the compiler does not insert `pin`/`unpin`.

```
function pin<T>(value: T): *T
function unpin<T>(ptr: *T)
```

Semantics:

- `pin(value)` registers `value` as a pinned GC root and returns its raw address as `*T`. While pinned, the object is guaranteed not to move and not to be collected.
- `unpin(ptr)` releases the pin. After this call the pointer is no longer valid for foreign use — the GC may move or collect the underlying object on its next cycle.
- Pinning is refcounted per object: nested `pin`/`unpin` pairs on the same value compose correctly.
- `pin` on a value that already lives in the foreign heap (e.g. an extern struct) returns its address unchanged. `unpin` on a foreign address is a no-op.

Passing a managed value to an extern function without first pinning it is a **programmer error** with undefined behavior in the FFI implementation. Nothing happens implicitly.

## 19.7 Restrictions on extern signatures

Extern function signatures must be expressible in the C ABI. Specific rules:

### Allowed parameter / return types

- Numeric primitives (`i8`..`i64`, `u8`..`u64`, `f32`, `f64`, `bool`, `char`, `isize`, `usize`).
- `null` (as a return type, indicating "void").
- `*T` for any `T`.
- `*T | null` (lowered to a nullable pointer).
- `extern struct` types passed by value (the C compiler's rules for struct-by-value apply on the platform).
- `extern (...) -> R` function pointers.
- Generic type parameters constrained to pointer position (see 19.8).

### Not allowed (without an explicit pointer wrap)

- Managed (non-extern) struct types, tuples, lists, maps, `str`, interface objects — passed by value.
- Union types other than `*T | null`. To pass a union, take a pointer to it: `*(A | B)`.

Specifically: an extern function parameter or return type that is a union other than `*T | null` is **rejected by the compiler**, because the runtime tag of a union has no defined C ABI representation. The two ways to work around this are:

- Pass a pointer to the union: `extern function f(x: *(A | B))`. The C side gets a single pointer; the runtime tag is stored in the managed object's GC header.
- Use the nullable-pointer special case: `extern function f(x: *T | null)`.

These two forms cover the common cases. Anything beyond them must be marshalled explicitly into an extern struct.

### Strings

`str` is **not** ABI-compatible with C strings. To pass a string across FFI:

- Use `*u8` for a UTF-8 byte pointer (length-known by another argument or a sentinel — language doesn't enforce).
- Use `Buffer` for a `(ptr, size)` pair.
- Use a custom extern struct describing the layout the C side expects.

There is no implicit conversion. The user marshals, pins, and unpins.

### Function pointers

Function-typed parameters cross only as `extern (...) -> R`. A non-extern closure cannot be passed (it has an environment). To pass a callback, declare it as a top-level `extern function` (or as a closure-free `extern (...) -> R` expression — note: short-form closures cannot be marked `extern`).

## 19.8 Generics across FFI

Generic type parameters appear in extern signatures **only as pointers**:

```
extern function hashmap_set<T>(key: *str, value: *T)
extern function alloc_array<T>(n: u64): *T | null
```

This is because the C ABI fixes the size of every argument at the call site; an unknown `T` has unknown size. Restricting to `*T` keeps every call site a known pointer-sized argument.

## 19.9 The two memory regions in practice

```
extern function malloc(size: u64): *Buffer | null
extern function free(p: *Buffer)

function allocate(size: u64): Buffer | null {
  var p = malloc(size)
  if p is null { return null }
  // We have a foreign-heap Buffer. No pin needed; it's already foreign.
  p as Buffer  // narrowing the *T | null → Buffer is allowed here because Buffer is extern
}

function release(b: Buffer) {
  free(pin(b))   // pin is a no-op for already-foreign values
}
```

Pinning a managed value across a foreign callback:

```
extern function register_callback(cb: extern (*u8) -> null, data: *u8)
extern function unregister_callback(data: *u8)

struct MyState {
  pub counter: i64,
}

extern function on_tick(data: *u8) {
  // The cast/unpin pattern below is conceptual; in practice the language
  // provides accessor helpers for converting *u8 back to a typed pointer.
  // Treat this as illustrative.
  ...
}

function install() {
  var s = MyState { counter: 0 }
  var p = pin(s)              // *MyState
  register_callback(on_tick, p as *u8)
  // s must remain pinned until unregister_callback runs
}
```

(The exact `*u8 → *T` round-tripping helper is a stdlib detail; the FFI specification just requires it be possible.)

## 19.10 Build-time linkage

How extern functions are resolved at link time is a toolchain concern. The language only specifies the source-level binding (`extern function name(...)`). Implementations may support attributes on extern declarations for library naming, linkage, calling conventions other than C, etc., but those are extensions outside this spec.

## 19.11 Summary

- `extern function` — C ABI binding, with or without body.
- `extern struct` — C layout, foreign heap, no GC header.
- `extern (...) -> R` — C ABI function type.
- `*T`, `*T | null` — raw pointer / nullable pointer; allowed only in extern contexts.
- `pin`/`unpin` — manual lifetime control for managed values passed to foreign code.
- Unions are not extern-passable except `*T | null` and `*(A | B)`.
- Exported `extern` functions can't capture.
