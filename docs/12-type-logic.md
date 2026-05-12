# 12. Type Logic — `is`, `as`, and Flow Narrowing

Discriminated unions are first-class; `is` and `as` are the runtime + compile-time interface to them.

## 12.1 `is` — runtime type check

```
v is T
```

Evaluates to `bool`. True if and only if the runtime tag of `v` indicates that the value is currently inhabiting type `T`.

```
function describe(v: i64 | str | null) {
  if v is i64 {
    print("number")
  } else if v is str {
    print("string")
  } else if v is null {
    print("nothing")
  }
}
```

`is` works for:

- Primitive types (`i64`, `f64`, `bool`, `char`, `str`).
- Struct types (including unit structs and tuple structs).
- Tuple shapes (`v is (i64, str)`).
- Type aliases (resolved transparently).
- `null`.

`is` does **not** match through pattern destructuring — for that, use `match` (see [07-expressions.md](./07-expressions.md)).

## 12.2 `as` — narrowing and conversion

`as` plays two roles depending on the target type.

### Union narrowing

When `v: U` for some union `U` and `T` is a variant of `U`:

```
var v: i64 | str = ...
var n: i64 = v as i64    // narrows to the i64 variant
```

If `v` is actually inhabiting `T`, the result is the underlying value. Otherwise the program **panics** (see [14-panics.md](./14-panics.md)).

If the compiler has already narrowed `v` to `T` via flow analysis (section 12.4), the `as T` is redundant but still allowed; it does not generate a runtime check.

### Primitive conversion

`as` between primitive numeric types is a value conversion:

- Integer → integer: bit-truncating cast. Never panics.
- Integer → float: closest representable value.
- Float → integer: truncates toward zero. Panics if the value is NaN or out of the target's range.
- Float → float: closest representable value (may lose precision).
- Integer ↔ `char`: see [02-types.md](./02-types.md). `i → char` panics if the value isn't a valid Unicode scalar.
- Any numeric → `str`: produces the decimal string. Never panics.

```
var n: i64 = 1000
var b: i8  = n as i8        // truncated to -24
var f: f64 = n as f64       // exact
var i: i32 = 3.7 as i32     // truncated to 3
```

### Interface widening and narrowing

Widening a concrete type to an interface is implicit (no `as` needed):

```
var p: Person = ...
var n: Named = p     // implicit widen
```

Narrowing back uses `as`:

```
var back: Person = n as Person   // panics if n is not actually a Person
```

### Disallowed casts

`as` between unrelated types is a compile error:

```
var s: str = "hi"
var n: i64 = s as i64    // ERROR: no defined cast from str to i64
```

To parse a number from a string, use stdlib functions; `as` is not a parser.

## 12.3 Combining `is` and `as`

```
if v is i64 {
  var n = v as i64
  // ...
}
```

In a flow-narrowed branch (section 12.4), the `as` is redundant — `v` is already statically `i64`. But you may still write it for clarity, and the compiler should not warn.

## 12.4 Flow narrowing

Inside the branches of `if`, `else`, `match`, and short-circuited boolean expressions, the compiler tracks the runtime tag(s) a variable could hold and **narrows the variable's type** accordingly. This is "occurrence typing" or "flow typing".

### `if v is T`

Inside the `then` block, `v` has the narrowed type `T`. Inside the `else` block (if present), `v` has the type `U \ T` (the original type minus `T`).

```
function describe(v: i64 | str | null): str {
  if v is i64 {
    // v: i64 here
    "number: " + (v as str)
  } else {
    // v: str | null here
    if v is null {
      // v: null here
      "nothing"
    } else {
      // v: str here
      "string: " + v
    }
  }
}
```

### Negation

`if !(v is T) { ... } else { ... }` — the `then` branch has `v: U \ T`, the `else` branch has `v: T`.

`if v is T { return ... }` — code after the `if` block has `v: U \ T` (because the early return removes the `T` case from the post-`if` flow).

### `&&` short-circuit

```
if v is i64 && v > 0 {
  // v: i64, then v > 0 — second condition compiles because v is known i64
}
```

When `&&` chains a type check on the left, the right-hand side is type-checked with the narrowing already applied. The body of the `if` has the cumulative narrowing.

### `||` short-circuit

```
if v is i64 || v is i32 {
  // v: i64 | i32 here
}
```

When both sides of `||` narrow the same variable to compatible types, the union is taken. If the sides narrow incompatibly (e.g. one narrows to `i64`, the other doesn't narrow that variable at all), no narrowing applies inside the body.

### `match`

Inside a match arm, the scrutinee variable is narrowed to whatever the arm pattern matched. After the entire `match` expression, the scrutinee is its declared type (assuming control flow falls through; if every arm `return`s, the post-match flow is dead).

```
match v {
  i64 n => /* v: i64, also bound to n */ ...,
  str s => /* v: str, also bound to s */ ...,
  null  => /* v: null */ ...,
}
```

### Effect through reassignment

A reassignment to a variable resets its narrowed type to its declared type (or to the type of the new RHS, whichever is more specific):

```
var v: i64 | str = 1
if v is i64 {
  v = "x"     // v's narrowed type is now str
  // v: str here, regardless of the surrounding `if v is i64`
}
```

### Limits of narrowing

- Narrowing only applies to **simple bindings** (`v`) — not to field accesses (`x.y`) or computed expressions. To narrow a field, copy it to a local first.
- Narrowing does not cross function boundaries; if you pass a variable into a function, the function sees its declared type.
- Narrowing is reset by reassignment to the variable.

### Aliasing

When a variable is captured by a closure or stored in a mutable structure, mutations from elsewhere can invalidate the narrowing. The compiler reasons conservatively: narrowing of a variable is preserved only as long as the narrowed variable itself is not reassigned in the current scope, and no closure that might mutate it is invoked. (Practically: don't pass `v` into a closure inside a narrowed branch and expect the narrowing to survive across the call.)

## 12.5 Examples

```
function process(input: i64 | str | null) {
  if input is null { return }

  // input: i64 | str

  if input is i64 && input > 0 {
    // input: i64, additionally positive
    print(("positive: " + (input as str)))
  } else if input is str {
    // input: str
    print("string: " + input)
  } else {
    // input: i64 (and not positive)
    print("non-positive: " + (input as str))
  }
}

function unwrap_or<T>(v: T | null, default: T): T {
  if v is null { default } else { v }   // v: T in the else branch
}
```
