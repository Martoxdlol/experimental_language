# 4. Structs

Structs are nominal types that group named (or positional) fields.

There are three forms:

- **Record structs** — named fields.
- **Tuple structs** — positional fields.
- **Unit structs** — no fields.

## 4.1 Record structs

```
pub struct Person {
  pub name: str,
  age: i32,             // private to the defining module
}
```

Every field has its own visibility. Fields default to private; `pub` exposes them.

Field separators: comma. Trailing comma is allowed.

### Construction

```
var alice = Person {
  name: "Alice",
  age:  30,
}
```

All fields must be initialized (no defaults). If any field is private to another module, construction from outside the module is rejected by the compiler.

### Field init shorthand

When a local variable's name matches a field name, the field name may stand alone:

```
var name = "Alice"
var age  = 30
var alice = Person { name, age }   // shorthand for { name: name, age: age }
```

### Spread (functional update)

```
var older = Person { ..alice, age: 31 }
```

`..expr` copies all fields from `expr` (which must be of the same struct type). Explicit fields override the spread. The spread must appear at the end (or interleaved with overrides; trailing position is recommended for clarity).

A spread does not move or invalidate the source — it copies field values. For reference-type fields, the copy is a reference copy (refcount increment).

### Field access

```
alice.name
alice.age = 31    // mutation; see [06-variables.md]
```

External access to private fields is rejected by the compiler.

## 4.2 Tuple structs

A nominal type whose fields are positional:

```
pub struct Pair(pub i64, pub i64)
pub struct Wrapper<T>(T)
```

Constructed by name, like a function call:

```
var p = Pair(3, 4)
var w = Wrapper(42)
```

Accessed positionally with `.0`, `.1`, ...

```
print(p.0 as str)
print(p.1 as str)
```

Tuple structs are useful when you want a distinct nominal type but no meaningful field names. They are *not* the same as anonymous tuples (see [05-tuples.md](./05-tuples.md)): tuple structs are nominal, anonymous tuples are structural.

## 4.3 Unit structs

A struct with no fields:

```
pub struct Red;
pub struct Green;
pub struct Blue;
```

Constructed by name without parentheses:

```
var c: Red | Green | Blue = Red
```

(Mentioning the name in an expression context constructs the unique value.)

Unit structs are the standard way to enumerate distinguishable variants:

```
pub type Color = Red | Green | Blue
```

## 4.4 Generic structs

```
pub struct Box<T> {
  pub value: T,
}

pub struct Pair<A, B> {
  pub first:  A,
  pub second: B,
}
```

Construction infers type parameters from the field values when possible:

```
var b = Box { value: 42 }       // Box<i64>
var p = Pair { first: 1, second: "x" }   // Pair<i64, str>
```

Explicit form:

```
var b = Box<i64> { value: 42 }
```

## 4.5 Visibility recap

| Modifier | Effect |
|---|---|
| (none) on struct | Struct visible only to its defining module |
| `pub` on struct  | Struct visible to importers |
| (none) on field  | Field readable/writable only from defining module |
| `pub` on field   | Field readable/writable from anywhere the struct is visible |

A `pub struct` with all-private fields can be returned from functions and held by reference, but not constructed or destructured outside its module.

## 4.6 Methods

Methods are added with `extend` blocks; see [10-interfaces.md](./10-interfaces.md). A struct definition does **not** contain its own method bodies — they live in `extend` blocks. (The original prototype allowed methods inline in `struct` definitions; the current spec requires `extend`.)

## 4.7 Equality, hashing, comparison

Structs are not automatically `Eq`, `Hash`, or `Ord`. Implement the corresponding interface (see [15-operators.md](./15-operators.md)) to enable `==`, hashing, and ordering.

Two struct values of the same type with the same field values are not implicitly equal — equality is whatever `Eq` says it is, or `==` is a compile error.

## 4.8 Memory representation

Non-extern structs live on the managed heap with a GC header (see [16-memory.md](./16-memory.md)). A struct value is referenced by a pointer to the object's fields; the GC header sits at a negative offset.

`extern struct` is laid out as a C struct, lives on the foreign heap, has no GC header. See [19-ffi.md](./19-ffi.md).

## 4.9 Destructuring

Record structs are destructurable in `var` bindings and patterns:

```
var Person { name, age } = alice
```

In patterns (e.g. inside `match`), the `..` rest token can ignore remaining fields:

```
match alice {
  Person { name, .. } => print(name),
}
```

Tuple structs destructure positionally:

```
var Pair(x, y) = p
```

Unit structs destructure by name only and bind nothing:

```
match c {
  Red   => "red",
  Green => "green",
  Blue  => "blue",
}
```
