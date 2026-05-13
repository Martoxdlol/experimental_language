# 2. Primitive Types

## 2.1 Integers

Sizes are always explicit. There is no architecture-dependent default integer type.

| Type | Width | Range |
|---|---|---|
| `i8`  |  8 bits | −128 .. 127 |
| `i16` | 16 bits | −32_768 .. 32_767 |
| `i32` | 32 bits | −2_147_483_648 .. 2_147_483_647 |
| `i64` | 64 bits | −2⁶³ .. 2⁶³−1 |
| `u8`  |  8 bits | 0 .. 255 |
| `u16` | 16 bits | 0 .. 65_535 |
| `u32` | 32 bits | 0 .. 4_294_967_295 |
| `u64` | 64 bits | 0 .. 2⁶⁴−1 |
| `isize` | pointer-sized | platform's signed pointer size |
| `usize` | pointer-sized | platform's unsigned pointer size |

`isize` / `usize` are the only types whose width depends on the target. They are intended for memory offsets, sizes, and collection indices. They are not interchangeable with the fixed-size integers without explicit `as` casts.

Default integer literal type is `i64` (see [01-lexical.md](./01-lexical.md)).

### Arithmetic, overflow, division by zero

- Integer overflow:
  - **Debug builds**: panics (see [14-panics.md](./14-panics.md)).
  - **Release builds**: wraps (two's complement for signed, modular for unsigned).
- Integer division by zero: panics in both debug and release.
- Modulo by zero: panics in both debug and release.

Explicit non-default overflow behavior is provided by stdlib functions on each numeric type:

```
i32.wrapping_add(a, b)
i32.checked_add(a, b)   // returns i32 | null
i32.saturating_add(a, b)
i32.overflowing_add(a, b) // returns (i32, bool)
```

Same families exist for `sub`, `mul`, `div`, `rem`, `neg`, `shl`, `shr`.

### Conversions

All numeric conversions are explicit, via `as`:

```
var a: i64 = 1000
var b: i8  = a as i8   // truncates, never panics
var c: f64 = a as f64  // exact for small-enough values
var d: i32 = 3.7 as i32 // truncates toward zero
```

Truncating `as` between integer types never panics. `f → i` truncates toward zero; `NaN as i*` panics; out-of-range `f → i` panics.

## 2.2 Floating point

`f32` and `f64`, both IEEE 754. Default float literal type is `f64`.

There is no implicit promotion between `f32` and `f64`; use `as`.

Float operations follow IEEE 754: division by zero produces ±∞, `0.0/0.0` produces NaN, comparisons with NaN return false. None of these panic.

## 2.3 Booleans

```
bool   // primitive, two values: true and false
```

`bool` is a primitive type. It is **not** a union of singleton structs. Literals are lowercase `true` and `false`.

The only operations on `bool` are logical: `&&`, `||`, `!`, `==`, `!=`. `&&` and `||` short-circuit.

## 2.4 Characters

```
char   // 32-bit Unicode scalar value
```

A `char` is exactly one Unicode scalar value, encoded as a 32-bit value. Surrogate code points are not valid `char` values.

`char` literals use **single quotes** (`'a'`, `'\n'`, `'\u{1F600}'`) and must contain exactly one Unicode scalar value. `''` (empty) or `'ab'` (more than one) is a compile error. `char` and `str` literals are not interchangeable: `"a"` is always `str`, `'a'` is always `char`. See [01-lexical.md](./01-lexical.md).

`char` does not auto-convert to or from integers; use `as`:

```
var ch = 'A'
var n  = ch as u32  // 65
var c2 = 65u32 as char  // 'A' (panics if value is not a valid Unicode scalar)
```

## 2.5 Strings

```
str   // heap-allocated, immutable, UTF-8
```

`str` is a managed-heap object containing a UTF-8-encoded byte buffer. It is immutable: all string operations that appear to mutate produce a new `str`.

String length is conceptually defined two ways:

- `s.byte_size()` returns the number of UTF-8 bytes (`i64`).
- `s.size()` returns the number of Unicode characters (`i64`); this requires a scan.

`str` indexing is **not** done by integer index because UTF-8 makes that ambiguous. Use `s.get(i)` to get the `i`-th character, or `s.substring(start, end)` for slices. See [18-stdlib.md](./18-stdlib.md).

`+` on two `str` is a shortcut for `concat`.

String literals support **interpolation**: `"Hello $name"` or `"Hello, ${user.name}, age ${user.age + 1}"`. Interpolated values must implement `ToStr`. See [01-lexical.md §1.9](./01-lexical.md#19-string-literals-and-interpolation) for the lexical rules and [15-operators.md §15.10](./15-operators.md#1510-stringification--tostr) for `ToStr`.

## 2.6 The empty type — `null`

```
null
```

`null` is simultaneously the type name and its only value. It is the language's unit type: a function that returns nothing returns `null`, and a discarded expression evaluates to `null`.

```
function side_effect(): null {
  print("hi")
}
```

Writing `(): null` is rarely necessary because functions without an explicit return type default to `null`:

```
function side_effect() {
  print("hi")
}
```

`null` appears in unions to express absence: `i64 | null` (an `i64` or nothing).

There is no `void`. There is no `unit`. `null` covers both roles.

## 2.7 Equality and ordering

Primitives have built-in equality (`==`, `!=`).

Numeric primitives and `char` have built-in ordering (`<`, `<=`, `>`, `>=`).

`bool` does not have an ordering operator.

`str` has both: equality is byte-wise; ordering is lexicographic by Unicode scalar values.

`null` only has equality with itself: `null == null` is `true`; `null != null` is `false`. Ordering of `null` is not defined.

User types acquire equality and ordering by implementing the `Eq` and `Ord` interfaces (see [15-operators.md](./15-operators.md)).

## 2.8 Default values

The language does not have implicit default values for primitives — every binding must be explicitly initialized. There is no `var x: i32`; you must write `var x: i32 = 0` (or similar). See [06-variables.md](./06-variables.md).

## 2.9 Sizes and alignment

All sizes and alignments below apply to non-`extern` (managed) values. For `extern struct` layout, see [19-ffi.md](./19-ffi.md).

| Type | Size | Alignment |
|---|---|---|
| `i8` / `u8` / `bool` | 1 | 1 |
| `i16` / `u16` | 2 | 2 |
| `i32` / `u32` / `f32` / `char` | 4 | 4 |
| `i64` / `u64` / `f64` | 8 | 8 |
| `isize` / `usize` | pointer-sized | pointer-aligned |
| `null` | 0 | 1 |
| `str` | pointer-sized (heap reference) | pointer-aligned |
| Other heap types (`List<T>`, `Map<K,V>`, structs) | pointer-sized (heap reference) | pointer-aligned |

This is what an inlined field "costs" in a struct. The heap object behind a reference type has its own layout (see [16-memory.md](./16-memory.md)).
