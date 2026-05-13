# 7. Expressions

The language is expression-oriented: most constructs produce a value.

## 7.1 Block expressions

A block `{ ... }` is a sequence of statements ending in an optional trailing expression. The block evaluates to the trailing expression, or to `null` if there is none.

```
var v = {
  var x = 10
  var y = 20
  x + y        // no `;` — this is the block's value
}
// v == 30
```

Adding `;` to the last expression turns it into a statement and makes the block evaluate to `null`:

```
var v = {
  var x = 10
  x + 1;       // `;` discards the value
}
// v == null
```

Blocks introduce a new lexical scope (see [06-variables.md](./06-variables.md)).

## 7.2 `if` expression

`if` is an expression. Parentheses around the condition are **not** required and not conventional. Parens are only needed when the surrounding syntax would otherwise be ambiguous (see 7.2 "Struct-literal ambiguity in headers" below).

```
var status = if age >= 18 {
  "Adult"
} else {
  "Minor"
}
```

Chains:

```
var grade =
  if score >= 90 { "A" }
  else if score >= 80 { "B" }
  else if score >= 70 { "C" }
  else { "F" }
```

### Statement form

The `else` branch is required when the `if` expression's value is used. Without `else`, the value of `if` is `null` and the type of both branches must unify (with `null` as an automatic completion):

```
if x > 0 {
  print("positive")
}
// Same as:
//   if x > 0 { print("positive") } else { null }
```

### Type of the result

The type of an `if` expression is the union of its branch types (after normalization). Both branches contribute; if they agree, the result type is exactly that type.

```
var v = if cond { 1 } else { "hi" }   // v: i64 | str
```

### Condition

The condition must be of type `bool`. There is no implicit truthiness for integers, strings, or pointers.

`if v is T { ... }` is a special case for flow narrowing: see [12-type-logic.md](./12-type-logic.md).

### Struct-literal ambiguity in headers

`if`, `while`, `for ... in ...`, and `match` scrutinees normally use **bare headers** — no parentheses. The only case where parens are needed is when the top-level expression of the header would otherwise be ambiguous with a struct literal (also written `Name { ... }`), because the parser cannot tell where the header ends and the body begins.

To disambiguate, wrap the offending expression in parens:

```
// ambiguous — REJECTED:
//   for n in Range { current: 0, end: 5 } { print(n as str) }

// explicit — OK:
for n in (Range { current: 0, end: 5 }) {
  print(n as str)
}
```

Struct literals nested inside parentheses, function calls, indexing, or other contexts are unaffected. This restriction only applies to the bare top-level expression directly before the body block.

## 7.3 `match` expression

`match` is the structured way to dispatch on a value. It works on any type, but is most useful for unions. Arms are separated by `,`.

```
match value {
  Pattern1 => expr1,
  Pattern2 => expr2,
  ...
  _        => default_expr,
}
```

A trailing `,` after the last arm is allowed and conventional. If an arm's body is a block, the `,` is still required:

```
match value {
  i64 n => {
    print("number")
    n * 2
  },
  str s => 0,
}
```

### Patterns

Supported pattern forms:

| Pattern | Matches |
|---|---|
| `_` | Anything (no binding) |
| `name` | Anything; binds value to `name` (only when the static type of the scrutinee is monomorphic) |
| `i64` (etc.) | A value of this primitive type |
| `i64 n` | A value of this primitive type, bound to `n` |
| `42` | The literal `42` of the scrutinee's integer type |
| `"hello"` | The literal string `"hello"` |
| `true` / `false` | Literal booleans |
| `null` | The `null` value |
| `'a'` | The literal character `'a'` |
| `Red` | The unit struct `Red` |
| `Some(n)` (tuple-struct pattern) | A tuple struct, destructuring positionally |
| `Person { name, age }` | A record struct, destructuring named fields |
| `Person { name, .. }` | A record struct, ignoring other fields |
| `Person { name: "Alice", .. }` | Record struct with a literal field pattern |
| `(a, b)` | A tuple, destructuring positionally |
| `(0, y)` | A tuple with a literal component |
| `(a, ..)` | A tuple, ignoring trailing positions |
| `(a, ..rest)` | A tuple, binding the trailing positions to `rest` as a sub-tuple |
| `[a, b, c]` | A list of exactly three elements |
| `[]` | An empty list |
| `[head, ..tail]` | A non-empty list; `head` is element 0, `tail` is a `List<T>` of the rest |
| `[a, b, ..]` | A list of length ≥ 2; trailing elements ignored |
| `[..init, last]` | A non-empty list; `init` is a `List<T>` of all but the last, `last` is the last element |
| `[a, ..mid, z]` | A list of length ≥ 2; `a` is first, `z` is last, `mid` is a `List<T>` of the middle |
| `T x` | The variant `T` of the scrutinee's union, bound to `x` (an irrefutable form for unions) |
| `P1 \| P2` | Either pattern (or-pattern); both must bind the same names with the same types if any |

### List and tuple rest patterns

`..` in a tuple or list pattern stands for "the remaining positions." It may appear **at most once** per pattern, and may carry a binding name:

- `..` (no name) — discards the rest.
- `..rest` (with name) — binds the rest:
  - In a **tuple** pattern, `rest` is a sub-tuple of the unmatched positions. Its type is statically determined by what the source tuple has left.
  - In a **list** pattern, `rest` is a `List<T>` containing the unmatched elements. The compiler emits the necessary slicing.

List patterns work with any value of type `List<T>`. They do not work on arbitrary `Iterator<T>` — iterators don't have a known length and aren't randomly addressable.

Guards:

```
match n {
  i64 x if x > 0 => "positive",
  i64 x if x < 0 => "negative",
  i64 _          => "zero",
}
```

Guards are arbitrary boolean expressions evaluated after the structural pattern matches.

### Exhaustiveness

`match` must cover every possible value of the scrutinee's static type. The compiler checks this:

```
match color {        // color: Red | Green | Blue
  Red   => 1,
  Green => 2,
                     // ERROR: non-exhaustive — missing Blue
}
```

Add `_ => default` to catch the rest. For unions, listing every variant satisfies exhaustiveness without a wildcard.

For primitive types like `i64`, exhaustiveness requires a `_` arm (you cannot enumerate every integer).

For structs, matching with `_ => ...` or `Person { .. } => ...` is exhaustive.

### Reachability

Arms are tried top-to-bottom. The compiler warns on unreachable arms (an arm whose pattern is fully covered by earlier arms).

### Type of the result

The result type of `match` is the union of all arm body types (after normalization).

### Binding modes

Bindings in patterns are by-reference for reference-type fields (no clone), by-value for primitives. The bound name is a regular `var` and is mutable inside the arm body.

## 7.4 Operator expressions

Standard precedence, highest to lowest (loosely):

| Precedence | Operators | Associativity |
|---|---|---|
| 1 | `.` `()` `[]` `?` (postfix) | left |
| 2 | unary `-` `!` `~` | right |
| 3 | `as` `is` | left |
| 4 | `*` `/` `%` | left |
| 5 | `+` `-` | left |
| 6 | `<<` `>>` | left |
| 7 | `&` | left |
| 8 | `^` | left |
| 9 | `\|` | left |
| 10 | `==` `!=` `<` `<=` `>` `>=` | non-associative |
| 11 | `&&` | left |
| 12 | `\|\|` | left |
| 13 | `=` (assignment statement, not expression) | — |

`==` and `!=` are non-associative: `a == b == c` is a parse error. Use explicit grouping or chained comparisons.

`as` and `is` bind tighter than arithmetic: `x + y as i32` parses as `x + (y as i32)`.

## 7.5 Call expression

```
foo(a, b, c)
list.push(x)
Map.new<str, i64>()    // static method (no receiver)
```

`.` is used for both member access (on a value: `person.name`) and static/namespaced access (on a type or module: `Type.function()`, `Module.name`). There is no separate `::` operator.

## 7.6 Method-call expression

```
person.greet()
person.set_age(30)
```

The receiver `person` is passed as `self`. Method resolution looks for methods on the type, then any `extend` impls of interfaces, with overlap rules from [11-generics.md](./11-generics.md).

## 7.7 Index expression

```
list[0]
map["key"]
```

Calls `Index.index` (read) or `IndexMut.index_mut` (write); see [15-operators.md](./15-operators.md). The element types come from the interface implementation. Out-of-bounds behavior is implementation-defined per type — collections return `T | null` from `get`/`set` but `[]` panics on out-of-bounds for `List<T>` and panics on missing key for `Map<K, V>`.

## 7.8 Statements

Statements terminate with `;`:

- `var <name> [: T] = expr ;` — binding
- `<lvalue> = expr ;` — assignment
- `return expr ;` — early function exit
- `break [expr] ;` — exit a loop (see [08-control-flow.md](./08-control-flow.md))
- `continue ;` — next loop iteration
- `expr ;` — expression statement; result is discarded

The last expression in a block may omit `;` to be the block's value.
