# 8. Control Flow

Loops use bare keywords — **no parentheses around their headers**. Parens are only needed when the syntax would otherwise be ambiguous (specifically, a struct literal as the top-level expression in the header; see [07-expressions.md §7.2](./07-expressions.md#72-if-expression)). The same rule applies to `if`, `while`, and the `match` scrutinee.

## 8.1 `for` loop

Iterates over any value implementing `Iterator<T>` (see [18-stdlib.md](./18-stdlib.md)):

```
for n in numbers {
  print(n as str)
}
```

Pattern destructuring is supported in the loop variable:

```
for (k, v) in map_entries {        // Iterator<(K, V)>
  print(k + ": " + (v as str))
}

for Entry { key, value } in user_ages {
  print(key + " -> " + (value as str))
}
```

The pattern must be irrefutable (always matches the yielded type).

Collections that aren't themselves iterators expose iterator factories: `list.iter()`, `map.entries()`, `map.keys()`, `map.values()`. As a convenience, lists, maps, and strings are accepted directly in `for ... in ...` and the appropriate iterator is constructed implicitly:

- `for x in list` — yields `T` (list elements)
- `for entry in map` — yields `Entry<K, V>`
- `for ch in s` — yields `char`

## 8.2 `while` loop

```
while i < 10 {
  print(i as str)
  i = i + 1
}
```

The condition must be `bool`. Evaluated before each iteration.

## 8.3 `loop` — infinite loop

```
loop {
  if done { break }
  do_work()
}
```

`loop` runs forever until `break`. Unlike `while true`, the compiler knows `loop` is irrefutably infinite, so it accepts code that depends on `break` for control-flow type inference:

```
var result = loop {
  if found { break value }
}
// `result` has the type of `value`
```

## 8.4 `break`

Exits the nearest enclosing loop.

```
for x in xs {
  if x < 0 { break }
  process(x)
}
```

`break` may carry a value, but **only out of `loop`**:

```
var first_match = loop {
  var item = next_item()
  if item is null { break null }
  if matches(item) { break item }
}
```

For `while` and `for`, `break` cannot carry a value — the loop's value is always `null`, because the loop may exit via the condition without ever hitting `break`.

## 8.5 `continue`

Skips to the next iteration of the nearest enclosing loop.

```
for n in numbers {
  if n < 0 { continue }
  print(n as str)
}
```

`continue` cannot carry a value.

## 8.6 Type of a loop expression

| Loop | Type when used as an expression |
|---|---|
| `for ... in ...` | `null` |
| `while cond` | `null` |
| `loop` (no `break` with value) | `null` (but the loop never returns to its caller, so the surrounding code is unreachable) |
| `loop` (with `break <expr>`) | The unified type of every `break` expression |

Because `loop` may diverge (run forever), the compiler treats a `loop` whose body never breaks as having type **never** — meaning the code following it is unreachable. In type unification, the never type is absorbed:

```
var x = if cond { 5 } else { loop {} }  // x: i64 (the loop branch is never)
```

## 8.7 Nested loops

`break` and `continue` always act on the innermost enclosing loop. There are no labeled loops in this version of the spec. If you need to exit multiple levels, use a function:

```
function search(): Item | null {
  for row in rows {
    for cell in row {
      if matches(cell) { return cell }
    }
  }
  null
}
```

(Labeled loops may be added in a future revision but are not required for completeness.)

## 8.8 Iterator protocol — quick reference

The `for x in v` syntax requires `v` to implement `Iterator<T>` (directly) or to be one of the built-in iterable shapes (`List<T>`, `Map<K, V>`, `str`).

```
interface Iterator<T> {
  function next(self): Item<T> | Done
}

struct Item<T> { value: T }
struct Done;
```

The loop body sees `T`. Iteration ends when `next()` returns `Done`. See [18-stdlib.md](./18-stdlib.md) for details and a worked example.

## 8.9 Examples

```
function sum(xs: List<i64>): i64 {
  var total: i64 = 0
  for x in xs {
    total = total + x
  }
  total
}

function find_first<T>(xs: List<T>, pred: (T) -> bool): T | null {
  for x in xs {
    if pred(x) {
      return x
    }
  }
  null
}

function countdown(from: i64) {
  var n = from
  while n > 0 {
    print(n as str)
    n = n - 1
  }
  print("liftoff")
}

function poll_until_ready<T>(ch: Receiver<T>): T {
  loop {
    var msg = ch.try_recv()
    if msg is null {
      sleep(10)
      continue
    }
    break msg as T
  }
}
```
