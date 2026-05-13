# 1. Lexical Structure

## 1.1 Source encoding

Source files are UTF-8. Line breaks are `\n` or `\r\n`; they are equivalent.

## 1.2 Comments

Four forms. Two are ignored by the compiler; two are **doc comments** attached to items.

### Ordinary comments

```
// Line comment — to end of line.

/*
  Block comment.
  /* Block comments nest. */
*/
```

Block comments nest, so a block comment can be used to comment out code that itself contains block comments.

### Doc comments

```
/// Documents the item that follows.
/// Markdown is the convention for body content.
pub struct Person {
  /// The person's name.
  pub name: str,
}

//! Documents the enclosing module.
//! Use this at the top of a file to document the module itself.
```

- **`///`** is an **outer doc comment** — it attaches to the *next* item (struct, function, interface, type, mod, field, ...). Multiple consecutive `///` lines form one doc block.
- **`//!`** is an **inner doc comment** — it attaches to the *enclosing module*. Conventionally placed at the top of a file (the only place `//!` is allowed).

The content is plain text; the toolchain's documentation generator interprets it as Markdown.

Doc comments are first-class syntactic attributes — they are visible to procedural macros via `ctx.docs(input)` (see [22-macros.md](./22-macros.md)) and surface in error messages, hover-info, and generated docs.

A doc comment on an item that has no recipient (a trailing `///` at the end of a file, a `///` on a statement inside a function body) is a compile warning.

## 1.3 Identifiers

Identifiers start with a Unicode letter or `_`, followed by letters, digits, or `_`. Identifiers are case-sensitive and ASCII-recommended for portability.

Reserved identifiers cannot be used as names: see [keywords](#16-keywords).

## 1.4 Integer literals

Integer literals support four bases:

```
42        // decimal
0xFF      // hex
0o17      // octal
0b1010    // binary
```

Digits can be visually grouped with `_`, which is otherwise ignored:

```
1_000_000
0xFFFF_FFFF
0b1010_0101
```

### Suffixes

A literal may carry a type suffix indicating its type:

```
42i8     42i16    42i32    42i64
42u8     42u16    42u32    42u64
42isize  42usize
```

A bare integer literal (no suffix) is `i64` unless context forces another type:

- An explicit type annotation: `var x: u8 = 42`.
- The literal appearing as an operand in a typed context: `var x: i32 = 1; var y = x + 5` makes `5` an `i32`.
- A generic call site whose inferred type parameter pins the literal.

If no suffix and no context, the literal is `i64`.

A suffix that does not fit (`300i8`) is a compile-time error. A suffix combined with a conflicting annotation (`var x: i32 = 1i64`) is a compile-time error.

## 1.5 Floating-point literals

```
3.14      // f64 by default
1e6
2.5e-3
1.0f32
1.0f64
```

A bare float literal is `f64` unless context forces `f32`. Integer-shaped literals can be coerced to floating point only via explicit suffix or annotation; there is no implicit int → float coercion.

## 1.6 Boolean literals

```
true
false
```

Lowercase only. `bool` is a primitive (see [02-types.md](./02-types.md)), not a union of singletons.

## 1.7 Null literal

```
null
```

`null` is both the name of the empty type and its sole value (see [02-types.md](./02-types.md)).

## 1.8 Character literals

A `char` literal is a single Unicode scalar value enclosed in `'`:

```
'a'
'\n'
'\u{1F600}'
'\\'
```

`char` is 32 bits wide (a Unicode scalar value, i.e. a code point in `0..=0x10FFFF` excluding the surrogate range).

Supported escape sequences inside character and string literals:

| Escape | Meaning |
|---|---|
| `\n` | Newline |
| `\r` | Carriage return |
| `\t` | Tab |
| `\\` | Backslash |
| `\'` | Single quote |
| `\"` | Double quote |
| `\$` | Literal `$` (only meaningful inside a string literal — escapes interpolation) |
| `\0` | Null byte |
| `\xHH` | Byte with hex value HH (only valid for bytes 0..=0x7F in string literals; full range in byte literals if any are added later) |
| `\u{H...}` | Unicode scalar value (1–6 hex digits) |

### Quote style — strict split

- **`""`** is always a **string literal** of type `str`.
- **`''`** is always a **character literal** of type `char`. A `''` literal must contain exactly one Unicode scalar value (`'a'`, `'\n'`, `'\u{1F600}'`); zero characters (`''`) or more than one is a compile error.

The two are not interchangeable: `'x'` is never a one-character string and `"x"` is never a character.

## 1.9 String literals and interpolation

```
"hello"
"line1\nline2"
"unicode: \u{1F600}"
```

String literals are UTF-8 and produce `str` values. `str` is immutable and heap-allocated (see [02-types.md](./02-types.md) and [18-stdlib.md](./18-stdlib.md)).

### Interpolation

A string literal may embed expressions whose results are spliced in:

```
var name = "Alice"
print("Hello, $name")                  // Hello, Alice

var user = User { name: "Alice", age: 30 }
print("Hello, ${user.name}, age ${user.age}")

print("Total: ${items.size() + 1}")
```

Two forms:

- **`$<identifier>`** — interpolates the value of `<identifier>`. The identifier is parsed greedily as letters, digits, and underscores starting with a letter or `_`. `$name` is the identifier `name`; `$_x` is the identifier `_x`; `$1` is not valid (no leading digit).
- **`${<expression>}`** — interpolates an arbitrary expression. Use this for field access, method calls, arithmetic, or any expression more complex than a bare identifier.

To produce a literal `$`, escape it: `"\$"`. A `$` not followed by an identifier or `{` is a compile error (so typos are caught rather than silently emitting the literal).

### Stringification rule

Every interpolated value must implement the `ToStr` interface (defined in `core:prelude`; see [15-operators.md](./15-operators.md)). The interpolated form `"x = $x"` desugars to `"x = " + x.to_str()`. Primitives, `str`, and `null` implement `ToStr` out of the box; user types implement it to participate in interpolation.

If `x` does not implement `ToStr`, the compile error points at the `$x` site, not at the desugared `+`.

### Raw strings

Raw strings — opting out of escape and interpolation processing — are not in this version of the spec.

## 1.10 Punctuation and operators

| Tokens | Use |
|---|---|
| `{ }` | Blocks, struct/map literals |
| `( )` | Grouping, call, tuple |
| `[ ]` | List literal, indexing |
| `< >` | Generics, comparison |
| `,` | Separator |
| `;` | Statement terminator |
| `:` | Type annotation, interface bound, map literal key/value |
| `.` | Member access, tuple positional (`t.0`), and static/namespaced access (`List.new`) |
| `=` | Assignment |
| `==` `!=` | Equality |
| `<` `<=` `>` `>=` | Ordering |
| `+` `-` `*` `/` `%` | Arithmetic |
| `&&` `\|\|` `!` | Logical |
| `&` `\|` `^` `~` `<<` `>>` | Bitwise |
| `\|` | Union type construction, closure parameter delimiter |
| `->` | Function return / function-type arrow |
| `=>` | Match arm |
| `?` | Error propagation (postfix) |
| `..` | Spread / rest pattern |
| `@` | Macro / decorator |
| `'` | Character literal delimiter |
| `*` | Multiplication; pointer prefix (in `extern` contexts only) |
| `_` | Wildcard pattern, ignored binding |

Operator precedence follows the conventional ordering; see [15-operators.md](./15-operators.md).

## 1.11 Semicolons and statement terminators

Statements are terminated by `;`. The last expression in a block may omit the trailing `;` to make the block evaluate to its value (see [07-expressions.md](./07-expressions.md)).

A trailing `;` after an expression turns it into a statement, discarding its value. The block then evaluates to `null`.

## 1.12 Whitespace

Whitespace separates tokens but is otherwise insignificant. Indentation has no semantic meaning.

## 1.13 Number parsing — disambiguation rules

`-1` in expression context is the unary negation operator applied to literal `1`, not a negative literal. This matters only at the boundary of pattern syntax and parsing; the practical effect is that `i64.MIN` is `-9_223_372_036_854_775_808` and must be written carefully if pattern-matched.

## 1.16 Namespacing — only `.`

There is no `..` operator for namespacing (`::` in Rust, `::` in C++). Static access uses the same `.` that's used for member access:

```
List.new<i64>()       // static method on the type List
i32.wrapping_add(a,b) // static helper namespaced under i32
Map.new<str, i64>()
```

A leading capital identifier followed by `.` resolves to a static/namespace access if the right-hand side is a static name; otherwise it's member access on the value. The two are syntactically identical.

## 1.14 Reserved character

`@` is reserved for macro invocation. See [22-macros.md](./22-macros.md).

## 1.15 Keywords

Reserved, cannot be used as identifiers:

```
as          async       await       break
continue    else        extend      extern
false       for         function    if
import      in          interface   is
loop        match       mod         null
pub         return      self        Self
struct      true        type        var
while       yield  (reserved, unused)
```

`Self` is reserved with a capital `S`; see [10-interfaces.md](./10-interfaces.md).

`yield` is reserved for future use; the language does not currently support generator syntax (see [21-async.md](./21-async.md)).
