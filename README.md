# Language Specification

This document defines the core syntax, type system, memory model, and behavior of the language. The language is designed to be highly expressive, leveraging discriminated unions, robust FFI, and flexible interface-based generics, while providing predictable memory semantics.

---

## 1. Type System

### 1.1 Primitive Types
The language provides the following primitive types:
- **Signed Integers**: `i8`, `i16`, `i32`, `i64`
- **Unsigned Integers**: `u8`, `u16`, `u32`, `u64`
- **Floating Point**: `f32`, `f64`
- **Boolean**: `bool` (implicitly mapped to `True | False` under the hood)
- **String**: `str` (Heap-allocated, immutable)
- **Character**: `char`
- **Empty Type**: `null`

### 1.2 Discriminated Unions & Type Aliases
Unions are a core building block, using the pipe (`|`) operator to indicate a value can hold one of multiple types.

```typescript
type Result = i64 | str | null;
```

#### The Enum Pattern (Unit Structs)
The language does not have a dedicated `enum` keyword. Instead, enumerations are constructed using unit structs and type aliases.

```typescript
pub struct Red;
pub struct Green;
pub struct Blue;

pub type Color = Red | Green | Blue;
```

### 1.3 Structs
Structs group related data. Fields are private to the module by default; visibility is controlled via the `pub` keyword.

```typescript
pub struct Person {
  pub name: str,
  age: i32, // Private default
}
```

### 1.4 Interfaces
Interfaces define behavioral contracts. Interfaces can require the implementation of methods or even specify that a type must also implement other interfaces (composition).

```typescript
pub interface Named {
  function get_name(self): str
}

pub interface Printable: Named {
  function to_string(self): str
}
```

---

## 2. Variables and Mutability

Every variable is declared using the `var` keyword and is inherently **mutable**. There is no `const` or `let` equivalence. The only restrictions on mutability come from public/private boundary encapsulations across modules.

```typescript
var name: str = "John Doe";
var age = 30; // Inferred as i64
name = "Jane Doe"; // Mutated
```

### 2.1 Blocks and Expressions
The language is expression-oriented. A block `{ ... }` evaluates to its last expression without needing a return statement. The same applies to `if` statements and functions.

```typescript
var status = if (age >= 18) {
  "Adult"
} else {
  "Minor"
};
```

---

## 3. Functions and Lambdas

Functions use the `function` keyword. The `return` keyword is available for early exits.

```typescript
pub function add(a: i64, b: i64): i64 {
  a + b // Implicit return
}
```

Lambdas are anonymous functions and follow the same rules:
```typescript
var double = function(x: i32): i32 { x * 2 };
```

---

## 4. Memory Management & Layout

Memory management follows a hybrid approach inspired by languages like JavaScript or Python, heavily optimized for safety and predictable C interop.

### 4.1 Value vs. Reference Semantics
- **Primitives** (`i32`, `f64`, `bool`): Stored inline on the stack and passed by value (copied).
- **Structs and Collections**: Allocated on the managed heap and passed by reference.
- **Strings**: Heap-allocated but immutable. Manipulating them returns new references.

### 4.2 Memory Layout
The precise layout of variables and heap objects allows the garbage collector (GC) to operate seamlessly.

**Stack/Inline Layout (Variables & Fields):**
Variables stored in the stack (or fields inside a struct) take the form of `[union tag | payload]`.
- For a primitive, the payload is the direct value.
- For a reference type (structs/maps), the payload is a raw pointer.

**Heap Layout:**
Managed heap objects follow this structure: `[gc header | type id | object fields]`.
- Crucially, the pointer held by a stack variable points **directly to the object fields**.
- The `gc header` and `type id` are accessed via negative offsets from the base pointer.
- **Compiler Optimizations**: The compiler can elide the `type id` and `gc header` completely if an object's lifetime is provably lexical, localized, and requires no dynamic dispatch.

### 4.3 External / FFI Allocations
Structs and buffers allocated by external sources (e.g., C's `malloc`) reside in a distinct virtual address space.
- The GC verifies the address range of any pointer. If a pointer falls into the foreign address space, the GC entirely ignores it.
- Foreign structs lack the `[gc header | type id]` prefixes.

---

## 5. Implementations and Extensions

The `extend` keyword adds methods or interface implementations to types. It can be applied to structs, type aliases, and even primitive types.

### 5.1 Basic Extensions
When extending, `self` is automatically inferred and passed by reference, permitting mutation of the struct's state.

```typescript
extend Person {
  function become_older(self) {
    self.age = self.age + 1;
  }
}
```

### 5.2 Interface Implementations
```typescript
extend Person: Named {
  function get_name(self): str {
    self.name
  }
}
```

### 5.3 Extending Primitives
Primitives can be extended just like structs. To access primitive extensions, the module defining them must be in scope.

```typescript
extend str: Printable {
  function to_string(self): str {
    self // A string is already a string
  }
}
```

### 5.4 Extending Type Aliases
You can extend specific union aliases. The methods are only accessible if the compiler knows the variable is definitively of that alias type. structurally identical aliases share assignability, but *not* extensions.

```typescript
pub struct Red; pub struct Blue;
pub type Theme = Red | Blue;

extend Theme {
  function is_cool(self): bool {
    self is Blue
  }
}
// 'is_cool' is only accessible on variables typed strictly as 'Theme'.
```

---

## 6. Visibility and Modules

Visibility mimics Rust's scoping rules. Everything is private to the file/module by default.
- `pub struct Foo` makes the struct available to importers.
- Struct fields must individually be marked `pub` to be accessed or initialized from outside the module.
- `extend` blocks located outside the struct's module cannot access private fields.

Modules are imported by string paths:

```typescript
import { Foo, bar } from "utils";
import { Foo as Bar } from "utils";
```

---

## 7. Generics

Generics operate on functions, structs, and interfaces.

### 7.1 Interface Bounds
Generic type parameters can be constrained using interfaces via the `+` syntax.

```typescript
function process<T: Named + Printable>(item: T) {
  print(item.get_name() + ": " + item.to_string());
}
```

### 7.2 Generic Inference capabilities
Explicit type parameters are allowed but usually unnecessary. The compiler fully supports bidirectional type inference.

```typescript
var list = List.new(); // Inferred as List<i32> later
list.push(42);
```

### 7.3 Overlapping Interface Priority
Extensions mapping interfaces to generic types might overlap (e.g., implementing an interface for `<T>` and specifically for `i32`).
- **Priority Rule**: The more specific implementation always wins (Rust trait resolution semantics).
- If the compiler detects an ambiguous overlap with no clear winner, it halts with a compilation error. Overlapping implementations must clearly prioritize one path.

---

## 8. Type Logic (`is` and `as`)

### 8.1 Type Checking (`is`)
The `is` keyword checks the current inhabited type of a variable at runtime, evaluating to a `bool`.

### 8.2 Casting and Narrowing (`as`)
The `as` keyword serves dual purposes: Type Casting and Type Narrowing.

**Type Narrowing:**
When combined with union types, `as` forces the compiler to treat a variable as a specific variant. If the variable does not hold that variant at runtime, the program panics (crashes).

```typescript
var val: str | i32 = "hello";
if (val is i32) {
  var num = val as i32; // Valid Narrowing
}
```

**Primitive Type Casting:**
`as` translates between primitive numeric boundaries.
- **Safe casts (Upcasting)**: `i8 as i32`, `f32 as f64`. Still require the `as` keyword.
- **Unsafe casts (Downcasting)**: `i64 as i8`, `f64 as i32`. Evaluates via truncation. Data loss will occur silently if out of bounds.

Allowed Conversions:
- Any integer to any integer (truncates or zero/sign-extends).
- Any float to any float.
- Integer to float (precision may be lost).
- Float to integer (truncates decimal, saturates at max bounds).

---

## 9. Built-in Interfaces (Traits)

The core language (`of:core` prelude) provides specific marker and behavioral interfaces that tie deeply into compiler logic.

### 9.1 Data Operations
- **`Eq`**: Controls behavior of the `==` and `!=` operators.
- **`Hash`**: Required for a type to be used as a key in a `Map<K, V>`.
- **`Index<Idx, Output>`**: Overloads read access for `collection[index]`.
- **`IndexMut<Idx, Output>`**: Overloads write access for `collection[index] = val`.
- **`Iterator<T>`**: Enables usage inside `for (item in collection) { ... }` loops. Requires `function next(self): T | null`.

### 9.2 Memory Operations
- **`Clone`**: Defines deep copy semantics. By default, the language provides an automatic structural deep clone (like JavaScript `structuredClone`) when crossing certain multi-threading/isolation boundaries. Implementing `Clone` overrides this behavior with a custom copy routine.
- **`Drop`**: Triggered immediately before the GC deallocates the object.
  - *Note on Escape/Resurrection:* If the `drop(self)` method assigns `self` to an outer-scoped variable or global list, the deallocation is canceled, and the object is "resurrected".

### 9.3 Foreign Markers
- **`ReprC`**: A marker interface. Implementing `ReprC` on a struct disables `[gc header | type id]` generation, strips pointer indirections internally, and guarantees standard C struct memory layout. Useful for maintaining managed instances that are byte-for-byte compatible with C without strictly living in the foreign heap.

### 9.4 Operator Overloads
Arithmetic logic can be extended: `Add`, `Sub`, `Mul`, `Div`, `Mod`.

---

## 10. Core Standard Library

The auto-imported `of:core` prelude provides immediate access to essential types.

### 10.1 `List<T>`
Dynamic array literal `[...]` resolves to `List<T>`.
Methods: `size()`, `is_empty()`, `clear()`, `push(v: T)`, `pop() -> T | null`, `insert(i, v)`, `remove(i)`, `contains(v)`.

### 10.2 `Map<K, V>`
Hash map literal `{ "key": value }` resolves to `Map<K, V>`. Key formatting requires quotes to differentiate from a code block.
Methods: `size()`, `is_empty()`, `clear()`, `get(k) -> V | null`, `set(k, v)`, `remove(k)`, `contains(k)`, `keys()`, `values()`.

### 10.3 String (`str`)
Strings are immutable. Methods include:
`size()`, `get(i)`, `contains(s)`, `starts_with(s)`, `ends_with(s)`, `substring(start, end)`, `split(sep)`, `trim()`, `to_upper()`, `replace(old, new)`.

### 10.4 Error & Null Handling
There are no global exceptions. Operations that fail (out-of-bounds, etc.) return `T | null`. User logic handles structured errors via generic unions `T | ErrorStruct`.

---

## 11. Foreign Function Interface (FFI)

The `extern` keyword defines C ABI boundaries. Only top-level items can be `extern`.

### 11.1 Pointers and Nullability
- **`*T` syntax**: Specifies a raw machine pointer. Over FFI boundaries, this represents a raw memory address.
- **Nullable pointers (`*T | null`)**: The underlying C layer cannot represent standard tagged unions. Thus, `*T | null` is syntactic sugar. The language treats the variable seamlessly via `is null` / `as T` paradigms, but over the FFI boundary, it resolves to a pure pointer that equates to `0x0` for null. Traditional unions (`i32 | str`) cannot be passed to FFI functions.

### 11.2 Extern Structs and Ownership
Extern structs natively inhabit the foreign heap.
- They cannot implement interfaces (missing GC headers).
- They must be manually allocated and freed via external functions (e.g., C's `malloc` / `free`).

```typescript
extern struct Buffer {
  data: *u8,
  size: u64,
}
```

### 11.3 Passing Managed Structs to FFI
You can pass managed, regular structs into FFI bounds:
1. **By Value**: Extracted raw fields are passed in C layout. GC headers and IDs are stripped.
2. **By Pointer (`*T`)**: The underlying struct pointer is passed. Because foreign functions may retain the pointer, **Managed memory must be pinned**.

```typescript
function pin<T>(value: T): *T;
function unpin<T>(ptr: *T);
```
- `pin` registers a value as an immovable GC root.
- `unpin` releases it. Nested `pin` calls increment a reference counter safely.

It is a logic error to pass a managed pointer unpinned to an asynchronously retained C state.

---

## 12. Pending Design / Future Specifications

The following features and specifications are acknowledged but pending formal design integration:

1. **Global Variables**: Syntax, thread-locality, and initialization order constraints.
2. **Compile Time Execution**: Macros or `comptime` blocks for evaluating logic ahead-of-time.
3. **Threads and Channels**: Actor models vs shared memory models, Mutex structures.
4. **Clone vs. Reuse on Single Reference**: Definitively tracking if a single-owner reference can be mutated in place vs. implicitly cloned to prevent side-effects.
5. **Async/Await Capabilities**: Event loop integration, `Promise`/`Future` semantics, and coroutine stack management.