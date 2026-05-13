# 15. Operators and Operator Overloading

Most operators are syntactic sugar for method calls on built-in interfaces. Implementing an interface gives a type the corresponding operator.

The built-in interfaces below live in `core:prelude` and are auto-imported.

## 15.1 Arithmetic

```
pub interface Add<Rhs = Self, Output = Self> {
  function add(self, rhs: Rhs): Output
}

pub interface Sub<Rhs = Self, Output = Self> {
  function sub(self, rhs: Rhs): Output
}

pub interface Mul<Rhs = Self, Output = Self> {
  function mul(self, rhs: Rhs): Output
}

pub interface Div<Rhs = Self, Output = Self> {
  function div(self, rhs: Rhs): Output
}

pub interface Mod<Rhs = Self, Output = Self> {
  function mod(self, rhs: Rhs): Output
}

pub interface Neg<Output = Self> {
  function neg(self): Output
}
```

Operator → method:

| Operator | Interface | Method |
|---|---|---|
| `a + b` | `Add` | `a.add(b)` |
| `a - b` | `Sub` | `a.sub(b)` |
| `a * b` | `Mul` | `a.mul(b)` |
| `a / b` | `Div` | `a.div(b)` |
| `a % b` | `Mod` | `a.mod(b)` |
| `-a`    | `Neg` | `a.neg()` |

Primitives `i8`..`i64`, `u8`..`u64`, `f32`, `f64` implement these for matching types. `str` implements `Add<str, str>` (concatenation).

Implementing `Add` for asymmetric types:

```
extend Vec3: Add<f64, Vec3> {
  function add(self, k: f64): Vec3 {
    Vec3 { x: self.x + k, y: self.y + k, z: self.z + k }
  }
}
```

`a + b` resolves by matching the types of `a` and `b` to the most specific `Add` impl.

## 15.2 Logical

```
pub interface Not<Output = Self> {
  function not(self): Output
}
```

| Operator | Interface | Method |
|---|---|---|
| `!a` | `Not` | `a.not()` |

`!`, `&&`, `||` for `bool` are built-in and **not overloadable**. `&&` and `||` short-circuit; they cannot be expressed as method calls (an overload would require eager evaluation).

`Not` does apply to user types — for instance, a `bool`-like wrapper.

## 15.3 Bitwise

```
pub interface BitAnd<Rhs = Self, Output = Self> { function bitand(self, rhs: Rhs): Output }
pub interface BitOr<Rhs = Self, Output = Self>  { function bitor(self, rhs: Rhs): Output }
pub interface BitXor<Rhs = Self, Output = Self> { function bitxor(self, rhs: Rhs): Output }
pub interface Shl<Rhs = Self, Output = Self>    { function shl(self, rhs: Rhs): Output }
pub interface Shr<Rhs = Self, Output = Self>    { function shr(self, rhs: Rhs): Output }
```

| Operator | Interface | Method |
|---|---|---|
| `a & b`  | `BitAnd` | `a.bitand(b)` |
| `a \| b` | `BitOr`  | `a.bitor(b)` |
| `a ^ b`  | `BitXor` | `a.bitxor(b)` |
| `a << b` | `Shl`    | `a.shl(b)` |
| `a >> b` | `Shr`    | `a.shr(b)` |

Shift right on signed integers is arithmetic (sign-extending). Shifting by `>= width(T)` panics in both debug and release.

`|` in type position constructs unions, not bitwise OR. The compiler disambiguates by context — in a type position, `|` is union; in an expression position, `|` is `BitOr`.

There is no separate "logical not" interface — `!` on a `bool` is built-in.

`~` (bitwise NOT) is **not** a separate operator. Use `BitXor` with all-ones or `Not` on integer types (the language provides `Not` for primitive ints to mean bitwise inversion).

```
extend i32: Not<i32> {
  function not(self): i32 { ... }   // bitwise inversion
}

var inverted = !mask    // bitwise NOT on integers; logical NOT on bool
```

## 15.4 Equality

```
pub interface Eq {
  function eq(self, other: Self): bool

  function ne(self, other: Self): bool {
    !self.eq(other)
  }
}
```

| Operator | Interface | Method |
|---|---|---|
| `a == b` | `Eq` | `a.eq(b)` |
| `a != b` | `Eq` | `a.ne(b)` |

Primitives have built-in `Eq`. For user types, `Eq` must be implemented explicitly — there is no implicit field-by-field equality, but the compiler can derive an impl via the `@derive(Eq)` macro (see [22-macros.md](./22-macros.md)).

## 15.5 Ordering

```
pub type Ordering = Less | Equal | Greater
pub struct Less;
pub struct Equal;
pub struct Greater;

pub interface Ord: Eq {
  function cmp(self, other: Self): Ordering

  function lt(self, other: Self): bool { self.cmp(other) is Less }
  function le(self, other: Self): bool {
    var o = self.cmp(other)
    o is Less || o is Equal
  }
  function gt(self, other: Self): bool { self.cmp(other) is Greater }
  function ge(self, other: Self): bool {
    var o = self.cmp(other)
    o is Greater || o is Equal
  }
}
```

| Operator | Interface | Method |
|---|---|---|
| `a < b`  | `Ord` | `a.lt(b)` |
| `a <= b` | `Ord` | `a.le(b)` |
| `a > b`  | `Ord` | `a.gt(b)` |
| `a >= b` | `Ord` | `a.ge(b)` |

Numeric primitives and `char` implement `Ord`. `str` implements `Ord` (lexicographic by Unicode scalar values). `bool` does **not** implement `Ord`. `null` does not implement `Ord`.

## 15.6 Indexing

```
pub interface Index<Idx, Output> {
  function index(self, i: Idx): Output
}

pub interface IndexMut<Idx, Output>: Index<Idx, Output> {
  function index_mut(self, i: Idx, v: Output)
}
```

| Form | Interface | Method |
|---|---|---|
| `x[i]` (read)   | `Index`    | `x.index(i)` |
| `x[i] = v` (write) | `IndexMut` | `x.index_mut(i, v)` |

Out-of-bounds behavior is per-type. `List<T>` and `Map<K, V>` panic on `[]` for missing or out-of-range; use `.get(i)`/`.get(k)` for the `T | null` variant.

A type can implement multiple `Index<Idx, Output>` impls for different `Idx` types — e.g. a `Matrix` indexed by both `i64` and `(i64, i64)`.

## 15.7 Hashing

```
pub interface Hash {
  function hash(self): u64
}
```

`Map<K, V>` requires `K: Eq + Hash`. Implement `Hash` so that `a.eq(b)` implies `a.hash() == b.hash()`.

Primitives implement `Hash`. `str` implements `Hash`. Tuples implement `Hash` if every component implements `Hash`.

## 15.8 Cloning

```
pub interface Clone {
  function clone(self): Self
}
```

`Clone.clone` is the deep-copy entry point used by:

- Channel sends when refcount > 1 (see [20-concurrency.md](./20-concurrency.md)).
- `Shared.lock` returning structured data (see [20-concurrency.md](./20-concurrency.md)).
- Cross-thread captures during `spawn` (see [20-concurrency.md](./20-concurrency.md)).
- Any explicit `.clone()` call.

User types implement `Clone` to define their copy semantics. The compiler can derive a deep-copy impl via `@derive(Clone)`.

`Clone` is **not** auto-implemented for user types. If you try to clone a struct that doesn't implement `Clone`, the call site fails to compile.

Primitives implement `Clone` trivially (the copy is the value).

`Clone.clone` must produce a value that's "independent enough" that mutation of one doesn't affect the other in any visible way. Whether the implementation is a true deep copy or a copy-on-write is the implementor's choice; the contract is observable independence.

## 15.9 Drop

```
pub interface Drop {
  function drop(self)
}
```

Called by the runtime just before deallocating an object. Implement this to release foreign resources, log, etc.

`Drop.drop` runs:

- When a refcount hits zero through normal scope exit.
- During cycle collection, in unspecified order between cycle members.
- During stack unwinding from a panic.

`Drop` does **not** run when a thread other than main panics on a value still in scope (the runtime aborts that thread; managed memory is reclaimed by the GC but `Drop` is not guaranteed to run on every object).

`Drop.drop` must not panic (see [14-panics.md](./14-panics.md)). It runs with `self` as a reference; you may read or mutate `self`. You should not resurrect `self` by stashing it into a long-lived variable, but the language does not prevent it.

See [16-memory.md](./16-memory.md) for the full drop / GC interaction.

## 15.10 Stringification — `ToStr`

```
pub interface ToStr {
  function to_str(self): str
}
```

`ToStr` produces a user-readable string for a value. It is the one interface used by **string interpolation** (see [01-lexical.md §1.9](./01-lexical.md#19-string-literals-and-interpolation)). The compiler desugars `"x = $x"` to:

```
"x = " + x.to_str()
```

Every interpolated value's type must implement `ToStr` (or implement it via a `pub mod` chain visible at the interpolation site), otherwise the compile error points at the `$x` / `${...}` site.

### Built-in implementations

- Numeric primitives (`i8`..`i64`, `u8`..`u64`, `usize`, `isize`, `f32`, `f64`) — decimal representation (matches `as str` on primitives).
- `bool` — `"true"` / `"false"`.
- `char` — a single-character `str`.
- `str` — identity (`s.to_str()` is `s`).
- `null` — `"null"`.
- `List<T>` — `"[a, b, c]"` (requires `T: ToStr`).
- `Map<K, V>` — `"{k1: v1, k2: v2}"` (requires `K: ToStr` and `V: ToStr`).
- Tuples — `"(a, b, c)"` (requires every component to implement `ToStr`).

### Relationship to `as str`

The `as str` cast (defined in [12-type-logic.md](./12-type-logic.md)) for numeric primitives produces the same string as `ToStr::to_str`. For user types, `as str` is **not** defined unless they implement `ToStr` — in which case `value as str` is sugar for `value.to_str()`. This keeps one stringification path through the type system.

### Auto-derive

`@Derive(ToStr)` (see [22-macros.md](./22-macros.md)) synthesizes a field-by-field implementation:

```
@Derive(ToStr)
pub struct Person { pub name: str, pub age: i32 }

// Person { name: "Alice", age: 30 } produces:
// "Person { name: Alice, age: 30 }"
```

The derived form is intended for diagnostics and `print` debugging. For user-facing formatting (locale-aware numbers, custom separators, etc.), implement `ToStr` by hand.

### No `Display` / `Debug` split

Some languages distinguish a user-facing `Display` from a developer-facing `Debug` (Rust). This spec collapses them into one `ToStr`. If a project needs the distinction, it can define its own `Debug` interface as user code — the language doesn't bless one. The rationale: most string output is either logs (`Debug`-ish) or user UI (which usually goes through a localization layer anyway, where neither built-in is enough). One interface keeps the surface small.

## 15.11 `ReprC` — disabling GC header for FFI

```
pub interface ReprC {
}
```

Marker interface (no methods). Implementing `ReprC` on a struct asks the compiler to lay out the struct without the GC header, producing standard C struct layout suitable for FFI.

A `ReprC` struct loses the ability to be referenced from managed code as a normal heap object — the GC cannot trace it. In practice, prefer using `extern struct` (see [19-ffi.md](./19-ffi.md)). `ReprC` is for the rare case where a struct must be both managed-accessible and C-layout. Cross-tracing rules are unspecified for `ReprC` structs; mark them only if you understand the implications.

## 15.12 Precedence summary

The expression-precedence table in [07-expressions.md](./07-expressions.md) gives the canonical ordering. Calls to overload methods do **not** see their own precedence — precedence is fixed at the operator level.

`==` and `!=` are non-associative. `<`, `<=`, `>`, `>=` are also non-associative; `a < b < c` is a parse error.

## 15.13 Operator overloading and dynamic dispatch

Operator method calls follow the same monomorphization / dyn-dispatch rules as any other method call (see [11-generics.md](./11-generics.md)). Expressions involving operators on generic-bounded types compile to monomorphized direct calls. Expressions involving operators on interface-typed values use vtable dispatch.

## 15.14 No bespoke operator definitions

You can only overload the operators listed above by implementing the corresponding interface. There is no way to define new operator symbols or to override operator precedence per-type.
