# 3. Discriminated Unions

A union is a type whose value is exactly one of a fixed set of variants. Variants are separated by `|`.

```
type Result = i64 | str | null
```

A value `var r: Result = 42` is an `i64`; the variable's static type is `Result`, the runtime tag identifies the inhabited variant.

## 3.1 Anonymous and named unions

A union can appear inline anywhere a type can:

```
function find(id: i64): Person | null { ... }
```

It can also be aliased with `type`:

```
type Maybe<T> = T | null
function find(id: i64): Maybe<Person> { ... }
```

These two forms are fully interchangeable; the alias is purely a name. There is no nominal-vs-structural difference: any two unions with the same set of variants are the same type.

## 3.2 Order does not matter

Union variant order is not part of identity:

```
type A = i64 | str
type B = str | i64
// A and B are the same type.
```

The compiler treats unions as sets of variants.

## 3.3 Duplicate variants are absorbed

```
type Duped = i64 | i64
// Equivalent to: type Duped = i64
```

This matters mostly during substitution into generic types (where two type parameters could collapse to the same concrete type).

## 3.4 Nested unions flatten

A union expression nested inside another union flattens:

```
type AB = i64 | str
type ABC = AB | bool
// Equivalent to: type ABC = i64 | str | bool
```

Flattening applies through type aliases as well: there is no nesting of unions at runtime. A value of type `ABC` carries one tag identifying which of `i64`, `str`, `bool` is inhabited — there is no two-level tag.

## 3.5 Recursive unions

A type alias may reference itself, **but only through a non-union variant** (typically a struct field that holds the recursive type). A recursive union built only out of unions and the alias name itself reduces to a non-recursive union and is normalized.

### 3.5.1 Recursion through a struct field — supported

The standard pattern for recursive types is to wrap the recursive part in a struct:

```
struct Cons { head: i64, tail: List }
type List = null | Cons
```

Here `List` recurses through `Cons.tail`, which is a single non-union variant. The flattening rule does not apply across struct fields, so `List` is the two-variant union `null | Cons`, and `Cons.tail` is itself a `List`.

Tree example:

```
struct Branch { left: Tree, right: Tree }
type Tree = i64 | Branch
```

### 3.5.2 Direct self-reference inside a union expression — normalized away

```
type X = i64 | X
```

After flattening, `X = i64 | X = i64 | i64 | X = i64`. This is a degenerate self-reference — the alias is normalized to its non-recursive core. The compiler accepts the alias and reports it as equivalent to `i64`.

### 3.5.3 Mutual recursion through union expressions — normalized away

```
type X = i64 | Y
type Y = str | X
```

Substituting: `X = i64 | str | X`, `Y = str | i64 | Y`. Both reduce to `i64 | str`. The compiler treats both `X` and `Y` as the same type `i64 | str` after normalization.

If you actually want distinct mutually recursive types, wrap one side in a struct (as in 3.5.1).

### 3.5.4 Normalization algorithm (informal)

Given a union alias, the compiler:

1. Substitutes alias references that resolve to unions, eliminating them.
2. Flattens nested union expressions.
3. Removes occurrences of the alias being defined from its own RHS (self-absorption rule, since `X | X` ≡ `X`).
4. Removes duplicate variants.
5. Checks that the resulting variant set is finite and non-empty.

After normalization, every union alias has a canonical set of non-union variants.

## 3.6 Variant identity

Two variants in a union are considered the same if they refer to the same nominal type. Structural types (tuples) compare by shape (see [05-tuples.md](./05-tuples.md)).

So `(i64, i64) | (i64, i64)` collapses to `(i64, i64)` after deduplication.

## 3.7 Subtyping by union extension

A union `A | B` is implicitly a subtype of `A | B | C`. Values move in this direction without an explicit cast:

```
var x: i64 = 5
var y: i64 | str = x   // OK: i64 ⊆ i64 | str
var z: i64 | str | bool = y // OK: extending the union
```

Going the other way requires `as` and may panic at runtime if the value doesn't match (see [12-type-logic.md](./12-type-logic.md)).

## 3.8 Generic unions

Type parameters are substituted before normalization:

```
type Result<T, E> = T | E

// Result<i64, str> = i64 | str
// Result<i64, i64> = i64           (deduplicated)
// Result<i64 | str, bool> = i64 | str | bool  (flattened)
```

This composes well with `null`:

```
type Maybe<T> = T | null
// Maybe<i64> = i64 | null
// Maybe<i64 | null> = i64 | null   (deduplicated)
```

So `Maybe<Maybe<T>>` is the same as `Maybe<T>`: there is no "double-nullable". This is intentional — there is no `Some<None>` ambiguity. Where the ambiguity actually matters (iterator end signalling, future readiness), use a dedicated wrapper struct (see [18-stdlib.md](./18-stdlib.md), `Item<T>` and [21-async.md](./21-async.md), `Ready<T>`).

## 3.9 Empty unions are not allowed

There is no `type X = ` (zero variants). Every union must have at least one variant. (One-variant "unions" are equivalent to the underlying type.)

## 3.10 Pattern matching on unions

See [07-expressions.md](./07-expressions.md) for `match` syntax and [12-type-logic.md](./12-type-logic.md) for `is`/`as` and flow narrowing.

`match` over a union must be exhaustive — every variant must be covered, either explicitly or by `_`. The compiler reports missing variants.
