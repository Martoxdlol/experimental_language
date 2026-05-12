# 6. Variables and Mutability

Every binding is declared with `var`. There is no `let` / `const`. Every binding is mutable.

```
var name: str = "John Doe"
var age = 30           // type inferred — i64 (default integer literal type)
var price: f32 = 19.99

name = "Jane Doe"      // mutation
```

Restrictions on what an outside caller can mutate come from **module visibility** (see [17-modules.md](./17-modules.md)), not from variable annotations.

## 6.1 Declaration syntax

```
var <name>          : <type> = <expr>     // explicit type + initializer
var <name>                    = <expr>     // inferred type
```

Both forms require an initializer. Uninitialized bindings (`var x: i64`) are not allowed; primitives have no implicit default value.

A `var` statement is terminated by `;` unless it is the last item in a block, where the `;` is optional but conventional.

## 6.2 Mutation

Any binding can be reassigned, including across types when the new value fits the declared (or inferred) type:

```
var x: i64 | str = 1
x = "hello"            // OK — fits i64 | str
x = true               // compile error — bool not in the union
```

Reassignment to a value of a different concrete type is fine; reassignment that escapes the declared type is rejected by the type checker.

Mutating a struct field uses `=`:

```
person.age = person.age + 1
```

For overloaded indexing, see [15-operators.md](./15-operators.md).

## 6.3 Scoping

Lexical scoping. A block `{ ... }` introduces a new scope. Bindings in an inner scope shadow same-named bindings in outer scopes.

```
var x = 1
{
  var x = 2     // shadows outer
  print(x as str)  // 2
}
print(x as str)     // 1
```

Two `var`s with the same name in the same scope are an error.

A binding is in scope from the point of declaration until the end of the enclosing block.

## 6.4 Module-level variables

Variables declared at module top level are persistent storage:

```
pub var MAX_RETRIES: i32 = 5
var counter: i64 = 0
pub var DEFAULT_CONFIG = Config { debug: false, port: 8080 }
```

### Initializer restrictions

A module-level `var` must be initialized with a **compile-time constant expression**:

- Literals (`42`, `"hello"`, `true`, `null`, `3.14`).
- Struct construction whose every field is itself a compile-time constant.
- Tuple construction whose every element is itself a compile-time constant.
- Arithmetic, bitwise, and logical operations on the above (the compiler evaluates these).
- Other module-level `var` references whose initializers are themselves constant.

The following are **not** allowed at module level:

- Function calls (even pure ones — there is no way to mark a function `const`).
- `as` casts that can fail (union narrowing).
- `await`, `spawn`, `pin`, channel construction.
- Indexing into collections.
- `if`/`match` expressions (control flow is for function bodies).

If you need lazy initialization, expose a function and call it from `main`:

```
var get_start_time_initialized: bool = false
var start_time_storage: i64 = 0
function start_time(): i64 {
  if !get_start_time_initialized {
    start_time_storage = clock_now()
    get_start_time_initialized = true
  }
  start_time_storage
}
```

(Note: `Shared<T>` is the right tool if multiple threads might race.)

### Initialization order

Because there is no module init code (see [17-modules.md](./17-modules.md)), all module-level `var` values are computed at compile time and embedded directly into static storage. There is no runtime "load order" between modules. This makes circular imports trivially safe.

### Visibility

- `var FOO` — module-private.
- `pub var FOO` — visible to importers.

A `pub var` is read **and** writeable by importers. The language does not distinguish read-only from read-write at the visibility layer. If you want read-only-from-outside semantics, expose a getter function and keep the `var` private.

This is intentional: avoids the design complexity of `pub` / `pub(crate)` / `pub(read)` etc., and the visibility system is exclusively about access, not about mutability. Mutating shared module state is unsafe across threads unless wrapped (see `Shared<T>` in [20-concurrency.md](./20-concurrency.md)).

## 6.5 Shadowing

Inner scopes may declare a same-named binding:

```
function example() {
  var x: i64 = 5
  if x > 0 {
    var x: str = "positive"   // shadows; type can differ
    print(x)
  }
  // x is still i64 here
}
```

Shadowing only crosses block boundaries — within a single block, redeclaring is an error.

## 6.6 Assignment, expressions, statements

Assignment is a statement, not an expression. It does not produce a value. You cannot write `var y = (x = 5)` — that's a parse error.

`var` declaration is also a statement.

For expression-vs-statement rules, see [07-expressions.md](./07-expressions.md).

## 6.7 Pattern bindings

`var` accepts destructuring patterns:

```
var (x, y) = point
var Person { name, age } = alice
var Pair(a, b) = pair
```

In each case the names introduced are normal `var` bindings, individually mutable.

A pattern in `var` must be **irrefutable** — it must always match. To match conditionally, use `match` or an `if v is T` guard.

## 6.8 Reference semantics — what assignment does

For primitive types (`i8`..`i64`, `u8`..`u64`, `f32`, `f64`, `bool`, `char`, `null`), assignment copies the value.

For reference types (`str`, structs, `List<T>`, `Map<K, V>`, tuples that have been boxed), assignment copies the **reference**, incrementing the underlying object's refcount. Both bindings then refer to the same heap object. Mutation through one is visible through the other.

```
var a = Person { name: "Alice", age: 30 }
var b = a             // b refers to the same heap object as a
b.age = 31
print(a.age as str)   // 31 — a and b alias
```

To get a deep copy, call `.clone()` (requires the type to implement `Clone`; see [15-operators.md](./15-operators.md)):

```
var b = a.clone()
b.age = 31
print(a.age as str)   // 30 — independent
```

This aliasing-with-mutation model is the standard one for refcounted languages and matches the memory model in [16-memory.md](./16-memory.md).
