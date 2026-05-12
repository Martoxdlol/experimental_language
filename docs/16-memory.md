# 16. Memory Model

## 16.1 The two heaps

The runtime owns two disjoint memory regions:

- **Managed heap** — every non-`extern` heap value lives here. The garbage collector allocates, traces, refcounts, and frees these objects.
- **Foreign heap** — anything allocated by `extern` code (`malloc`, `mmap`, library allocators, arenas, stack buffers passed in by C). Opaque to the GC. Ownership is manual.

The two regions occupy distinct address ranges. The GC can identify a pointer as "managed" or "foreign" by an address-range check at runtime; foreign pointers are ignored by the GC entirely.

See [19-ffi.md](./19-ffi.md) for the foreign-heap side and pinning.

## 16.2 Value vs reference semantics

- **Primitive types** (`i8`..`i64`, `u8`..`u64`, `f32`, `f64`, `bool`, `char`, `null`, `usize`, `isize`): stored inline (stack, struct fields, registers). Assignment copies the value.
- **Reference types** (`str`, `List<T>`, `Map<K, V>`, structs, interface objects, function values that capture state, "boxed" tuples): the variable holds a pointer to a managed-heap object. Assignment copies the pointer (and increments the refcount). Mutation through one binding is visible through every other binding that holds the same pointer.

To get an independent copy, call `.clone()` (requires `Clone`; see [15-operators.md](./15-operators.md)).

## 16.3 Object layout

Every managed-heap object has the same header layout. The pointer held by a variable points **to the start of the object fields**; the header lives at negative offsets.

```
   (low addresses)
   +------------------+
   | GC header (8B?)  |  refcount, color, mark bits, etc.
   +------------------+
   | type id (4B)     |  uniquely identifies the runtime type
   +------------------+
   | (padding to ptr) |
   +------------------+  <-- pointer value held by managed code
   | field 0          |
   | field 1          |
   | ...              |
   +------------------+
   (high addresses)
```

This is deliberate: pointer dereferences for normal field access do not need to skip the header. Operations that need the header (refcount updates, drop dispatch, `as` narrowing to a struct type) read the header at known negative offsets.

Exact byte sizes of the header are implementation-defined; treat the layout above as conceptual. Implementations may pack the refcount and color/mark bits into a single 8-byte word.

`extern struct` types **do not** have a GC header (see [19-ffi.md](./19-ffi.md)).

## 16.4 Refcount semantics

Every managed object has a refcount stored in its header. Refcount operations:

- **Increment** when a new strong reference is created: assignment to a new binding, capture into a long-lived closure, push into a collection, send over a channel, etc.
- **Decrement** when a strong reference is destroyed: scope exit, reassignment, replacement in a collection, etc.
- **Atomic** when the object is reachable from more than one thread (after a send to a channel, after a capture by a spawned thread). The compiler is conservative — objects that *could* be shared use atomic ops.

When a refcount hits 0:

1. `Drop.drop(self)` is invoked (if `Self` implements `Drop`).
2. For each strong-referencing field, the field's refcount is decremented (recursively destroying chains).
3. The object's memory is returned to the allocator.

### Compiler optimizations

The compiler tracks **last use** of every binding and tries to avoid unnecessary refcount traffic:

- If a binding `x` is passed to a function and not used afterward in its scope, the compiler can hand off the existing reference without an inc/dec pair.
- If a binding `x` is returned, the inc/dec at the return site cancels with the call-site receive.
- Static analysis identifies these and emits a **move** (pointer transfer) instead of an inc/dec pair.

This makes refcount overhead near-zero for typical straight-line code.

## 16.5 Cycles and the cycle collector

Pure refcounting cannot reclaim cycles (object A holds B, B holds A; nothing reaches zero). The runtime includes a **cycle collector** that runs periodically.

Algorithm (informal):

- The runtime tracks a set of "suspect" objects — objects whose refcount was decremented but did not reach zero. These are candidates for being part of a cycle.
- The collector runs on a background thread (or at scheduled safe points), performing a partial trace from the suspect set:
  - It marks reachable objects.
  - Any suspect not reached from a global root is part of a garbage cycle and is collected.

Collection of cycles:

- `Drop.drop` is called on each cycle member, in **unspecified order**.
- Fields holding references to other cycle members are zeroed before drop, so a cycle member's `Drop` impl observes the object's references as `null` or already-dropped.

This means `Drop` impls **must not** assume that referenced objects are still alive when called as part of cycle collection. Practically: don't dereference children in a `Drop` impl unless you've also followed the rule below.

### Drop guidance

Safe patterns inside `Drop.drop`:

- Touch only primitive fields of `self`.
- Release foreign resources (file descriptors, `malloc`'d buffers, etc.) — these are not subject to cycle-collection ordering.
- Log or count (provided logging is itself drop-safe).

Unsafe patterns:

- Call methods on referenced managed objects (they may already be dropped).
- Resurrect `self` by storing it into a long-lived location.
- Panic.

### Reachability roots

The set of GC roots includes:

- Local variables on every thread's stack at every safe point.
- Module-level `var` storage.
- Captured environments of live closures.
- Pinned objects (`pin` adds a root entry; `unpin` removes it).

## 16.6 Determinism of drop

`Drop` is **not guaranteed** to be deterministic. Specifically:

- **Linear (non-cyclic) reference chains**: when a binding goes out of scope and was the last owner, `Drop` is called at the point of scope exit. This is deterministic and predictable in straight-line code.
- **Cycles**: `Drop` runs eventually, in unspecified order, when the cycle collector runs. There is no guaranteed wall-clock bound.
- **At program exit**: the runtime makes a best-effort pass to drop reachable objects, but no guarantee. Long-lived resources should be released explicitly before main returns.
- **Thread death**: if a thread terminates (panic or normal exit), the runtime drops its stack roots. Objects reachable only from a panicked thread that the runtime can't unwind may leak until cycle-collected.

Programs that depend on deterministic finalization (e.g. file flushing) should release resources explicitly rather than relying on `Drop`.

## 16.7 Inline / stack allocation

The compiler may "inline" small heap objects onto the stack when their lifetime is provably bounded by a single frame. This is an optimization invisible at the language level — the user-visible semantics are always as if the object lived on the managed heap.

Tuples and small structs are the most common candidates. The compiler proves the value does not escape (does not appear in any returned value, captured closure, channel send, or assigned-to-collection element) and replaces the heap allocation with a stack slot.

## 16.8 Type identification

The `type id` field in each object header is a compile-time-assigned integer identifying the object's concrete type. This is what `is`/`as` consult at runtime to perform narrowing on union types.

Type IDs are stable within a single compilation. Across separate compilations or dylib boundaries, a runtime registry maps type identity by structure (for nominal types, this is the fully-qualified module-and-name; for tuple shapes, it's a structural hash).

Interface dispatch is *not* based on type id alone — it uses a vtable pointer that is part of the interface-object representation (a fat pointer). See [11-generics.md](./11-generics.md).

## 16.9 Allocation failure

The managed allocator panics on out-of-memory. There is no fallible `alloc` for managed code. (Foreign allocation via `malloc` is fallible and returns `null` to the user; see [19-ffi.md](./19-ffi.md).)

## 16.10 Concurrency interactions

The refcount field is updated atomically when the runtime determines that an object is reachable from more than one thread. This is normally as soon as the object is sent to a channel or captured by a spawned thread.

For thread-local objects, refcount updates are non-atomic (a single-thread optimization).

Cross-thread reads and writes of struct fields are **not synchronized** by the language — use `Shared<T>` or channels (see [20-concurrency.md](./20-concurrency.md)). The language guarantees memory safety (no use-after-free) for managed pointers across threads, but not race-freedom on object contents.

## 16.11 Pinning

See [19-ffi.md](./19-ffi.md). Pinning a managed object adds a strong root that prevents the cycle collector from moving (if a moving collector is in use) or reclaiming the object until `unpin`.

Pinning is **refcounted per object**: nested `pin`/`unpin` pairs on the same value compose correctly. `pin` on a foreign value is a no-op (foreign memory is not managed; pinning is unnecessary).

## 16.12 Summary

- Refcount + cycle collector hybrid GC.
- Managed objects share a uniform header: `[gc header | type id | fields]`, pointer at the field boundary.
- Primitives by value; references inc/dec a refcount.
- Compiler elides redundant inc/dec via last-use analysis (used by channels for zero-copy moves).
- Drop is best-effort, not guaranteed; don't depend on it for correctness.
- Two heaps; the GC ignores foreign pointers.
