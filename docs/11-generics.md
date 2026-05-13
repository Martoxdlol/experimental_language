# 11. Generics

Generic parameters appear in `< >` after a name. They apply to functions, structs, interfaces, and `extend` blocks.

```
function identity<T>(value: T): T { value }
struct Box<T>     { pub value: T }
interface Container<T> { function get(self): T }
extend<T> Box<T>: Container<T> { function get(self): T { self.value } }
```

## 11.1 Type parameters

A type parameter is a placeholder for a type. Each parameter has a kebab-friendly identifier; conventionally `T`, `U`, `K`, `V`, `E`, etc.

Type parameters can be constrained:

```
function bigger<T: Ord>(a: T, b: T): T { if a > b { a } else { b } }
function dump<T: Printable + Clone>(x: T) { print(x.clone().to_string()) }
```

Multiple bounds separate with `+`. A bound is an interface (possibly with its own generic arguments):

```
function consume<T: Iterator<i64>>(it: T) { ... }
```

## 11.2 Default type parameters

A generic parameter can have a default:

```
interface Add<Rhs = Self, Output = Self> {
  function add(self, rhs: Rhs): Output
}
```

A default lets callers omit the argument: `Add<>` (or just `Add`) means `Add<Self, Self>`. Defaults can refer to earlier parameters and to `Self`.

Defaults are only meaningful in declaration positions; at use sites, omitted arguments simply use the default.

## 11.3 Bidirectional inference

Type arguments are usually inferred from value arguments:

```
function pair<A, B>(a: A, b: B): (A, B) { (a, b) }

var p = pair(1, "hi")          // pair<i64, str>
```

Explicit type arguments are written in `< >` immediately after the function or method name. There is no separate disambiguating prefix (no `::<...>` and no `.<...>`):

```
var p = pair<i64, str>(1, "hi")
var empty = List.new<str>()
```

In expression positions where `name<T>(...)` could in principle be ambiguous with a chained comparison `name < T > (...)`, the parser commits to the generic-call interpretation when:

- the name is a known type, generic function, or static method, **and**
- the `<` is immediately followed by a type expression (and not, say, a value expression unsuitable as a type).

In the rare case that the parser commits to the wrong interpretation, parenthesize: `(a < b) > c` for the comparison; explicit generic arguments are otherwise unambiguous.

## 11.4 Monomorphization

Every generic function and method is **monomorphized**: the compiler emits a separate copy for each distinct set of type arguments used in the program. This means:

- Generic code is as fast as hand-written specialized code.
- Binary size grows with the diversity of instantiations.
- Generic parameters do not carry type tags at runtime — the type is "burned in" per instantiation.

Monomorphization is **always** the default for `<T: Bound>` parameters.

## 11.5 Dynamic dispatch

There is exactly one situation that triggers dynamic dispatch: **an interface name used as a value type**. This includes parameter types, field types, local variable types, and return types.

```
function f(x: Printable) { ... }   // x: Printable — dyn dispatch
var items: List<Printable> = ...   // dyn dispatch per element
```

In these cases, the value is represented as a "fat pointer" — a pair `(object_pointer, vtable_pointer)`. Method calls through this fat pointer use the vtable.

By contrast, a generic-bounded form is monomorphized:

```
function f<T: Printable>(x: T) { ... }   // monomorphized
```

The compiler prefers monomorphization. It uses dynamic dispatch only when the interface appears as a value type. If you want monomorphization, use the generic form; if you need heterogeneous collections or runtime polymorphism, use the interface-type form.

### When does this matter?

- `List<Shape>` — every element can be a different concrete type implementing `Shape`. Dynamic dispatch.
- `List<T>` with `T: Shape` — every element is the **same** concrete type. Monomorphized.

### Narrowing to an interface

A value can be widened to an interface (implicit) and narrowed back with `as` (panics if wrong):

```
var person = Person { ... }
var p: Printable = person       // implicit widen to interface
var back = p as Person          // narrow
```

The widening produces a fat pointer. Narrowing checks the type tag in the GC header (see [16-memory.md](./16-memory.md)) and panics on mismatch.

### Non-object-safe interfaces

An interface that mentions `Self` outside the `self` parameter cannot be used as a value type — there's no way to express the result in a vtable. The compiler rejects this at use sites (see [10-interfaces.md](./10-interfaces.md)).

## 11.6 Generic structs

```
struct Pair<A, B> {
  pub first:  A,
  pub second: B,
}

struct Cache<K: Eq + Hash, V> {
  inner: Map<K, V>,
}
```

Bounds on struct generics ensure that all uses (constructions and methods) honor those constraints.

## 11.7 Generic interfaces

```
interface Iterator<T> {
  function next(self): Item<T> | Done
}

interface Add<Rhs = Self, Output = Self> {
  function add(self, rhs: Rhs): Output
}
```

A generic interface can be implemented multiple times for the same type with different type arguments:

```
extend i32: Add<i32, i32> { function add(self, rhs: i32): i32 { ... } }
extend i32: Add<f64, f64> { function add(self, rhs: f64): f64 { ... } }
```

Resolving `x.add(y)` uses argument types to pick the impl.

## 11.8 Generic `extend` blocks

Already shown in [10-interfaces.md](./10-interfaces.md). Examples:

```
extend<T> Wrapper<T> { ... }                     // for all T
extend<T: Clone> Wrapper<T>: Clone { ... }       // constrained
extend Wrapper<i32> { ... }                      // specialization
extend<T> List<T>: Iterator<T> { ... }           // generic interface impl
```

## 11.9 Overlap and specificity

Two `extend` blocks for the same type-and-interface combination can overlap (e.g. `extend<T> List<T>: Foo` and `extend List<i32>: Foo`). The compiler picks the **most specific** impl. Specificity is defined as follows.

Let A and B be candidate impls applicable to a given concrete type/interface pair. A is **strictly more specific** than B iff:

1. There exists a substitution σ from B's type parameters to types such that σ(B's type pattern) = A's type pattern, **and**
2. There exists no substitution from A's type parameters to types that yields B's type pattern.

Informally: A's pattern can be obtained as an instance of B's pattern, but not vice versa.

Examples:

| B's pattern | A's pattern | A more specific than B? |
|---|---|---|
| `extend<T> List<T>` | `extend List<i32>` | yes (substitute T=i32) |
| `extend<T> Wrapper<T>` | `extend<T: Clone> Wrapper<T>` | yes (added bound makes A's pattern an instance) |
| `extend<T> Pair<T, T>` | `extend<A, B> Pair<A, B>` | the first is more specific (forces A=B) |
| `extend<T: Foo> Box<T>` | `extend<T: Bar> Box<T>` | neither (incomparable) |

If two impls overlap and neither is strictly more specific, the compiler reports an **ambiguity error** at the call site that exercised the overlap. To resolve, narrow one impl's bounds or remove one.

This procedure is sound because:

- It's a syntactic test on pattern shapes and bound sets.
- It does not depend on the call site's surrounding code.
- It rejects all true ambiguities and accepts all clearly-ordered cases.

### Bound subsumption

When comparing two impls with the same type pattern but different bounds:

- If A's bounds are a strict superset of B's bounds, A is more specific. (Adding bounds narrows applicability, so A applies to fewer types — exactly the cases where B also applies.)
- If A's bound set is incomparable with B's, the impls are incomparable.

### Practical guidance

Most code never hits overlap. When overlap is intentional (e.g. specialized fast paths), follow the pattern of (1) a fully generic blanket impl plus (2) a single concrete specialization. The compiler picks the specialization for matching concrete types and the blanket for everything else.

## 11.10 Lifetimes

There are no lifetimes. The garbage collector handles managed memory; FFI pointers are managed through the pinning APIs (`&`, `with_pin`, `Pin.acquire`) — see [19-ffi.md §19.15](./19-ffi.md#1915-pinning).

## 11.11 Variance

Generic parameters are **invariant** unless the language specifies otherwise. There is no covariance/contravariance system in user code.

The standard library uses invariance throughout: `List<Animal>` is not assignable to `List<Cat>` or vice versa.

The one exception is union widening: `T` is implicitly assignable to `T | U`. This applies position-by-position in concrete type expressions but does **not** propagate through generic containers.

```
var dog: Dog = ...
var any: Dog | Cat = dog          // OK: union widening on the bare value

var dogs: List<Dog> = ...
var any_list: List<Dog | Cat> = dogs   // ERROR: List is invariant
```

If you need that effect, build a new collection explicitly (e.g. `dogs.map({ it as Dog | Cat })`).

## 11.12 Generic functions in extern signatures

Generics may appear in `extern function` declarations only in pointer position (`*T`), because the size of `T` is not known at the call site. See [19-ffi.md](./19-ffi.md).
