# 9. Functions and Closures

## 9.1 Function declaration

```
pub function add(a: i64, b: i64): i64 {
  a + b
}
```

Parts of the signature:

- `pub` — optional visibility.
- `function` keyword.
- Name.
- Optional generic parameters: `<T, U: Bound>` (see [11-generics.md](./11-generics.md)).
- Parameter list: `(name: T, name: T, ...)`. Parameters cannot have default values. Trailing comma allowed.
- Return type: `: T`. Omitted return type defaults to `null`.
- Body: a block.

The body is an expression-block. Its trailing expression is the return value. Use `return expr` for an early exit.

```
function get_ratio(current: f64, total: f64): f64 {
  if total == 0.0 {
    return 0.0
  }
  current / total
}
```

## 9.2 Generic functions

```
function identity<T>(value: T): T {
  value
}

function bigger<T: Ord>(a: T, b: T): T {
  if a > b { a } else { b }
}

function map<I, O>(xs: List<I>, f: (I) -> O): List<O> {
  var out = List.new<O>()
  for x in xs {
    out.push(f(x))
  }
  out
}
```

See [11-generics.md](./11-generics.md) for bounds, monomorphization rules, and dynamic dispatch.

## 9.3 Function-type syntax

A function type is written `(P1, P2, ...) -> R`:

```
var f: (i64) -> i64 = function(x: i64): i64 { x * 2 }
var combine: (i64, i64) -> i64 = function(a: i64, b: i64): i64 { a + b }
```

Parameter names are not part of the type — only the types are:

```
type IntFn = (i64) -> i64    // same as (x: i64) -> i64 with names elided
```

For function types with the C ABI used in FFI, prefix with `extern`:

```
type CCallback = extern (data: *u8, size: u64) -> i32
```

See [19-ffi.md](./19-ffi.md).

## 9.4 Anonymous functions — long form

The `function` keyword with no name creates an anonymous function:

```
var double = function(x: i32): i32 {
  x * 2
}
```

Anonymous functions follow the same rules as named functions: implicit return of the trailing expression, all parameters typed (unless inference takes over), explicit return type allowed.

## 9.5 Closures — short form

In contexts where the expected function type is known (a typed binding, an argument position, etc.), the short closure form may omit type annotations:

```
|x| x * 2

|a, b| a + b

|x| {
  var y = x + 1
  y * y
}

|x: i32| -> i32 {
  x + 1
}
```

Syntax:

- `|<params>| <body>` — body is an expression or a block.
- Parameter types and the return type are inferred from context when not annotated.
- Parameters between `|...|` are comma-separated and may have type annotations.
- The zero-parameter form is `|| <body>`. In argument position there is no ambiguity with the logical-OR operator because `||` does not begin a valid expression there.

```
spawn(|| producer(tx))      // zero-parameter closure
var f = || 42               // zero-parameter closure returning i64
```

A short-form closure is **exactly** the same kind of value as a `function(...) { ... }` anonymous function — it just has lighter syntax. Both produce a value of some `(P...) -> R` function type.

## 9.6 `it` — implicit single parameter

When a closure with exactly one parameter is expected (from the context), `{ ... }` without `|...|` is a closure body whose single parameter is named `it`:

```
var doubled = numbers.map({ it * 2 })
var positives = numbers.filter({ it > 0 })
```

Equivalent long forms:

```
var doubled   = numbers.map(|x| x * 2)
var doubled_2 = numbers.map(function(x: i64): i64 { x * 2 })
```

Rules:

- The implicit-`it` form requires the surrounding context to provide a unique function type with exactly one parameter. Otherwise the compiler rejects the form with "ambiguous closure parameter".
- `it` is just a regular parameter binding inside the body; it can be shadowed.
- Nested `it`-closures: the inner `it` shadows the outer. To refer to both, use the explicit `|x| ...` form.

## 9.7 Captures

Closures capture variables from their enclosing lexical scope. Capture is **by reference** for reference-type variables (the captured object is the same one the outer scope sees) and **by value** for primitive variables (a copy is taken at closure-creation time).

```
var n: i64 = 0
var inc = function() { n = n + 1 }  // captures `n` by reference — but n is primitive
```

For primitives, the closure captures a binding cell so that mutations persist. The mental model is: every captured variable is captured by reference; primitives just happen to have inline storage in the cell.

Practical implication: a closure that captures a struct shares it with the caller — mutations through the closure are visible to the caller. To capture a snapshot, clone before capture:

```
var snapshot = person.clone()
var greet = function() { print(snapshot.name) }
```

### Capture analysis

Variables are captured automatically — there is no explicit capture list. The compiler determines the capture set from the closure body. A captured reference type's refcount is incremented at closure-creation time and decremented when the closure is dropped.

### `extern` callbacks cannot capture

Functions declared `extern` (so that they can be invoked across the C ABI) **cannot capture**: a C function pointer has no environment slot. To pass state to a C callback, use the conventional `void*` user-data parameter (typed as `*T` in this language). See [19-ffi.md](./19-ffi.md).

## 9.8 Async functions and async closures

`async function f(...) -> T` returns a `Future<T>` rather than executing immediately. Bodies may use `await`. See [21-async.md](./21-async.md).

Async closures use the `async` prefix on either form:

```
var fut = async function(x: i64): str {
  var d = await fetch(x)
  d.body
}

var fut2 = async |x| {
  var d = await fetch(x)
  d.body
}

var fut3 = async { await fetch(it) }   // implicit it
```

The type of an async closure is `(...) -> Future<R>`.

## 9.9 Calling syntax recap

```
foo(a, b)                 // free function
person.greet()            // method (instance)
List.new<i64>()           // static / namespaced function
List<i64>()               // type-as-constructor shorthand for List<i64>.new() — see 9.10
(|x| x + 1)(5)            // call a closure expression
```

Methods and static functions share the same `function` keyword — they differ only by being inside an `extend` block and (for methods) taking `self` as their first parameter. See [10-interfaces.md](./10-interfaces.md).

## 9.10 Type-as-constructor shorthand

For any type `T` that exposes a `new` static method, the expression `T(args)` is a **parse-time rewrite** for `T.new(args)`:

```
var xs = List<i64>()              // → List<i64>.new()
var m  = Map<str, i64>()          // → Map<str, i64>.new()
var c  = Pool<Conn>(size = 16)    // → Pool<Conn>.new(size = 16)
```

### Rules

- The rewrite is **purely syntactic**. The compiler does not require `new` to return `T` — its return type is whatever the method declares. A `new` that returns `T | Error`, `T | null`, `Future<T>`, or any other type works the same way: `T(args)` evaluates to whatever `T.new(args)` evaluates to.

  ```
  extend MyCache: ... {
    function new(size: i64): MyCache | InvalidSize {
      if size <= 0 { InvalidSize } else { MyCache { ... } }
    }
  }

  var c = MyCache(16)?      // c: MyCache, after `?` propagates InvalidSize
  ```

- If `T` does not have a `new` static method, `T(args)` is a compile error: `"type T is not callable: no 'new' static method"`.
- The rewrite applies only when `T` is used as a value-position expression. `T(args)` in a pattern, in a type annotation, or as a generic argument is not a constructor call.
- Generic arguments work as usual. `List<i64>()` rewrites to `List<i64>.new()`; the type arguments are part of the receiver and flow into `new`.

### Tuple structs are not rewritten

Tuple structs (`pub struct Pair(i64, i64)`) already use `Pair(1, 2)` as their literal constructor. The rewrite does **not** apply to them — `Pair(1, 2)` is direct construction regardless of whether anyone defined `Pair.new`. The compiler distinguishes the two cases by the declared kind of `T`.

If you want both literal construction and a smart-constructor on the same type, expose the smart form under a different name (`Pair.make(...)`) or use a record struct instead of a tuple struct.

### Why it's just sugar

Because the rewrite is purely syntactic, there is no `New` / `Constructor` interface in the language and nothing to implement. Defining a `new` static method on a type is the only thing that "opts in." This keeps the type system unchanged and means generic code over "things that can be `T(...)`-constructed" must still go through `T.new` explicitly (no generic bound like `T: New<Args, Out>` is available — design intentional, deferred to a future revision if it becomes needed).

## 9.11 No variadics, no default arguments, no named arguments

The function call grammar is fixed: positional, exact arity. To approximate optional parameters, accept a struct argument with a spread default; to approximate variadic input, accept a `List<T>`.

## 9.12 Recursion

Direct and mutual recursion are supported. Functions are visible throughout their declaring module regardless of declaration order — there is no top-down restriction.

## 9.13 Diverging functions

A function that never returns (always panics, always loops) has an unreachable end-of-body. The compiler infers the return type as the never type in that case:

```
function fail(msg: str): null {
  panic(msg)
}
```

Such a function can be assigned anywhere any return type is expected, because never is a subtype of every type.
