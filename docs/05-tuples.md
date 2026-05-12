# 5. Tuples

Tuples are anonymous, structurally typed groupings of values.

```
var point: (i64, i64) = (3, 4)
var row:   (i64, str, bool) = (1, "Alice", true)
```

## 5.1 Structural typing

Two tuple types with the same sequence of element types are the **same type**, regardless of where they appear:

```
function origin(): (i64, i64) { (0, 0) }
function shift(p: (i64, i64)): (i64, i64) { (p.0 + 1, p.1 + 1) }

shift(origin())   // OK — same type
```

Aliases for tuples are fully interchangeable with the underlying shape:

```
type Point = (i64, i64)
type Z = Point | null
// Z is exactly the same as (i64, i64) | null
```

There is no distinction between `Point` and `(i64, i64)` in the type system. They are the same type, just spelled differently. Methods extended onto one apply to the other.

This is intentional: tuples are *anonymous shapes*. Use a `struct` (record or tuple struct) when you want a distinct nominal type.

## 5.2 Positional access

Fields are accessed by integer literal after `.`:

```
var p = (10, "hi", true)
p.0    // 10
p.1    // "hi"
p.2    // true
```

The index must be a literal integer at compile time — `p.i` for a variable `i` is not supported (a tuple's element types may differ, so the index must be statically known).

Indexing out of range is a compile-time error.

## 5.3 Destructuring

```
var (a, b, c) = (1, 2, 3)

var (id, name, _) = row    // ignore third element
var (id, ..) = row         // ignore the rest
```

The wildcard `_` ignores a single position. The rest token `..` ignores any number of trailing positions; it must appear last.

Patterns (e.g. inside `match`) follow the same rules and additionally allow literal patterns:

```
match point {
  (0, 0)   => "origin",
  (x, 0)   => "x-axis",
  (0, y)   => "y-axis",
  (x, y)   => "elsewhere",
}
```

## 5.4 The empty and single-element cases

- Zero-element: not a distinct tuple type — use `null` (see [02-types.md](./02-types.md)).
- Single-element: parentheses around a single expression always denote grouping, not a tuple. There is no one-element tuple type. If you actually want a "one-element tuple" use a tuple struct or a single-field struct.

## 5.5 Memory layout

A tuple is laid out as an anonymous record with positional fields. There is no GC header on a tuple by itself; tuples live wherever they are stored:

- **Stack / inline**: when used as a local variable that does not escape, the compiler may lay the tuple out inline with sequential fields. Primitive elements are stored directly; reference-type elements take a pointer slot.
- **Heap**: when a tuple escapes (returned out of its frame into a generic position, captured by a closure that escapes, stored in a `List<T>` or `Map<K, V>`), it is boxed into a managed-heap allocation with the standard heap layout (GC header + type id + fields).
- **FFI**: tuples cannot be passed across an `extern` boundary directly. To pass tuple-like data to C, wrap it in an `extern struct` (see [19-ffi.md](./19-ffi.md)).

Whether a tuple is stack-inline or heap-boxed is a compiler decision based on escape analysis and is not user-visible at the language level. Semantics are the same either way.

## 5.6 Tuples in generics

A tuple shape can be used as a type argument like any other type:

```
var pairs: List<(i64, str)> = [(1, "a"), (2, "b")]
```

## 5.7 Extension on tuples

You can `extend` a tuple type or one of its aliases (because they are the same type, both extensions add methods to the same shape):

```
type Point = (i64, i64)

extend (i64, i64) {
  function magnitude(self): f64 {
    var x = self.0 as f64
    var y = self.1 as f64
    sqrt(x * x + y * y)
  }
}
```

Calling `(3, 4).magnitude()` works regardless of whether the value's static type is written as `(i64, i64)` or `Point`.
