# 10. Interfaces and Extensions

Interfaces define behavior contracts. Types acquire methods and interface implementations through `extend` blocks. The `struct` declaration itself contains only data.

## 10.1 Interface declaration

```
pub interface Named {
  function name(self): str
}
```

An interface is a named, exported-or-not collection of method signatures. Method signatures inside an interface look like function signatures without a body. Semicolons after signatures are optional.

### Multiple methods

```
pub interface Shape {
  function area(self): f64
  function perimeter(self): f64
}
```

### Default implementations

A method signature may include a body; the body is the default implementation. Implementors can override it.

```
pub interface Named {
  function name(self): str

  function greet(self): str {
    "Hello, " + self.name()
  }
}
```

### Static methods

A method without a `self` parameter is a static method. It is called via `.` on the type (not on an instance):

```
pub interface Default {
  function default(): Self
}

extend i32: Default {
  function default(): i32 { 0 }
}

var zero = i32.default()
```

### `Self` type

Inside an interface or `extend` block, `Self` refers to the implementing type. Useful in static methods and method signatures whose return type is "the same type":

```
pub interface Clone {
  function clone(self): Self
}
```

## 10.2 Interface composition

An interface can require other interfaces as super-interfaces:

```
pub interface Printable: Named {
  function to_string(self): str
}
```

Implementing `Printable` for a type requires that type to also implement `Named`. Multiple super-interfaces are listed with `+`:

```
pub interface Renderable: Printable + Sized {
  function render(self): str
}
```

## 10.3 `self` parameter

The receiver. Behavior depends on the receiver type:

- For **primitives** (`i32`, `f64`, `bool`, `char`, etc.) — `self` is passed **by value** (a copy). Methods cannot mutate the original.
- For **reference types** (structs, tuples-stored-on-heap, `List`, `Map`, `str`) — `self` is a reference to the heap object (refcount-managed). Mutation through `self.field = ...` mutates the shared object.

`self` is always implicitly typed; you do not write a type annotation for it. The compiler types `self` as `Self` (the receiver type).

There is no separate `&self` / `&mut self` syntax — for managed reference types, mutation is always allowed, and copying requires an explicit `.clone()`.

## 10.4 Extensions — adding methods

`extend` adds methods to a type:

```
struct Person {
  pub name: str,
  age: i32,
}

extend Person {
  function become_older(self) {
    self.age = self.age + 1
  }

  function is_adult(self): bool {
    self.age >= 18
  }
}
```

Multiple `extend` blocks for the same type are allowed and combine additively. There must not be two definitions of the same method name.

## 10.5 Extensions — implementing an interface

```
extend Person: Named {
  function name(self): str {
    self.name
  }
}
```

An `extend T: I` block must provide every method of `I` that doesn't have a default. Methods with defaults can be omitted (the default applies) or overridden.

An `extend T: I` block can also add methods that are not part of `I`:

```
extend Person: Printable {
  function to_string(self): str { ... }
  // additional, non-interface methods are fine
  function debug_dump(self): str { ... }
}
```

But it's clearer to keep interface impl blocks focused; non-interface methods can go in a plain `extend T { ... }` block.

## 10.6 Generic extensions

`extend` blocks can introduce type parameters:

```
struct Wrapper<T> {
  pub value: T,
}

// Applies to every Wrapper<T>
extend<T> Wrapper<T> {
  function get(self): T {
    self.value
  }
}

// Specialization — only for Wrapper<i32>
extend Wrapper<i32> {
  function double(self): i32 {
    self.value * 2
  }
}

// Generic interface impl
extend<T> Wrapper<T>: Named {
  function name(self): str { "Wrapper" }
}

// Constrained generic impl
extend<T: Clone> Wrapper<T>: Clone {
  function clone(self): Wrapper<T> {
    Wrapper { value: self.value.clone() }
  }
}
```

## 10.7 Extending primitives

Primitives can be extended just like structs:

```
extend i32 {
  function is_even(self): bool {
    self % 2 == 0
  }
}

var x: i32 = 4
print(x.is_even() as str)
```

For primitives, `self` is by value (see 10.3).

## 10.8 Extending unions and aliases

Type aliases (including unions) can be extended:

```
struct Red;
struct Blue;
type Theme = Red | Blue

extend Theme {
  function is_cool(self): bool {
    self is Blue
  }
}
```

This adds `is_cool` to values whose static type is `Theme` (equivalently, `Red | Blue`).

Because unions are normalized (order doesn't matter, nested unions flatten), `extend (Red | Blue)` and `extend (Blue | Red)` and `extend Theme` all refer to the same type and stack their methods together.

Methods on tuple aliases and tuple shapes are added to the shape, not the alias name. Because `type Point = (i64, i64)` and `(i64, i64)` are the same type, methods added to either are accessible on both. See [05-tuples.md](./05-tuples.md).

## 10.9 Orphan rule

To prevent ambiguous interface implementations across modules, an `extend T: I` block must originate in **either the type `T`'s defining module or the interface `I`'s defining module**.

```
// utils module
pub interface Printable { ... }

// types module
pub struct Foo { ... }

// some_other module
extend Foo: Printable { ... }   // ERROR — neither Foo nor Printable is local
```

You can always:

- Implement a local interface for a local type.
- Implement a local interface for a foreign type.
- Implement a foreign interface for a local type.

You cannot implement a foreign interface for a foreign type.

This is the standard "orphan rule"; it makes interface coherence local and modules independently compilable.

## 10.10 Coherence

For any pair (type T, interface I), there must be **exactly one** `extend T: I` impl visible in any compilation. The orphan rule plus generic overlap rules (see [11-generics.md](./11-generics.md)) make this enforceable.

If two `extend` blocks could both apply (e.g. `extend<T> List<T>: Named` and `extend List<i32>: Named`), the more specific one wins. See specificity rules in [11-generics.md](./11-generics.md).

## 10.11 Interface objects (dyn dispatch)

An interface type used as a value type (parameter, field, variable) is an **interface object** — a fat pointer to a heap value plus a vtable pointer. Calls through an interface object dispatch dynamically.

```
function print_all(items: List<Printable>) {
  for x in items {
    print(x.to_string())   // dynamic dispatch
  }
}
```

Versus generic-bounded (monomorphized):

```
function print_all<T: Printable>(items: List<T>) {
  for x in items {
    print(x.to_string())   // static dispatch (monomorphized)
  }
}
```

See [11-generics.md](./11-generics.md) for the full story.

## 10.12 Interfaces and Self in interface objects

If an interface's method signature returns `Self` (e.g. `Clone.clone(self): Self`), it cannot be used as an interface object — the return type would have to be erased to "some Self", which can't be expressed concretely. Such interfaces are called *non-object-safe*. The compiler reports the error at the point of use:

```
function bad(x: Clone): Clone { x.clone() }   // ERROR — Clone is not object-safe
```

To use a non-object-safe interface, use generics:

```
function ok<T: Clone>(x: T): T { x.clone() }
```

(In practice, the user-facing rule is: an interface method that mentions `Self` outside the `self` parameter is not object-safe.)

## 10.13 Calling conventions for interface methods

Method-resolution order at a call site `value.method(args)`:

1. Inherent methods on the value's concrete type (from `extend T { ... }` blocks).
2. Methods supplied by any interface implementation visible at the call site.

Ambiguity (two different impls both providing the same method) is a compile error. Disambiguate by calling the method through the interface explicitly:

```
Printable.to_string(person)
```
