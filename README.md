# Language Specification

This document defines the core syntax, type system, memory model, and behavior of the language. The language is designed to be highly expressive, leveraging discriminated unions, robust FFI, flexible interface-based generics, and powerful concurrency paradigms, all while providing predictable memory semantics.

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

### 1.4 Tuples
Tuples are anonymous, statically typed groupings of values. They are structurally typed, meaning a tuple `(i32, str)` in one module is strictly identical to `(i32, str)` in another.

```typescript
// Assignment and inference to type (i32, str, bool)
var data = (42, "hello", true);

// Destructuring
var (id, name, is_active) = data;

// Ignore specific fields using underscore
var (just_id, _, _) = data;
```

**Memory Layout:**
Tuples are modeled by the compiler as anonymous structs.
- **Stack / Inline Layout**: When passed by value or stored locally, the compiler lays out the data blocks contiguously `[payload_1 | payload_2 | ...]`. Primitives sit inline; reference types take up a pointer-sized slot.
- **Heap Layout**: If a tuple escapes its lexical scope (e.g., returned into a generic dynamic object), it is heap-allocated adopting the standard struct layout.
- **FFI Boundary**: Tuples passed to FFI (`extern` functions) are directly translated to standard C `struct` representations with native C alignment (`repr(C)`).

### 1.5 Interfaces
Interfaces define behavioral contracts. Interfaces can require the implementation of methods or even specify that a type must also implement other interfaces (composition).

```typescript
pub interface Named {
  function get_name(self): str;
}

pub interface Printable: Named {
  function to_string(self): str;
}
```

---

## 2. Variables and Mutability

### 2.1 Local Variables
Every local variable is declared using the `var` keyword and is inherently **mutable**. There is no `const` or `let` equivalence for local scopes. Restrictions on mutability are handled via module encapsulation boundaries.

```typescript
var name: str = "John Doe";
var age = 30; // Inferred as i32/i64 based on architecture
name = "Jane Doe"; // Mutated
```

### 2.2 Global / Module-Level Variables
Variables declared at the module level are placed in static memory segments.
- **Initialization**: Module-level `var` declarations can strictly only be initialized with compile-time literals (primitives, strings, or constant struct initializers). Function calls or dynamic allocations at the module level yield a compiler error, ensuring safe initialization order.
- **Thread Safety**: Accessing unsynchronized module-level variables across concurrent threads is unsafe unless properly wrapped (see `Shared<T>` in Concurrency).

```typescript
// Allowed: Compile-time literals
var MAX_RETRIES: i32 = 5;
var DEFAULT_CONFIG = Config { debug: false };

// Error: Function calls not allowed in module-level declarations
// var start_time = get_internal_time(); // COMPILER ERROR
```

### 2.3 Blocks and Expressions
The language is expression-oriented. A block `{ ... }` evaluates to its last expression without needing a return statement. The same applies to `if` statements and functions.

```typescript
var status = if age >= 18 {
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
var double = function(x: i32): i32 {
  x * 2
};
```

---

## 4. Memory Management & Layout

Memory management follows a hybrid approach heavily optimized for safety, predictable C interop, and multithreading.

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
- The `gc header` (which includes lightweight reference counts and tracing data) and `type id` are accessed via negative offsets from the base pointer.

### 4.3 External / FFI Allocations
Structs and buffers allocated by external sources (e.g., C's `malloc`) reside in a distinct virtual address space. The GC verifies the address range of any pointer. If a pointer falls into the foreign address space, the GC ignores it. Foreign structs lack the `[gc header | type id]` prefixes.

---

## 5. Implementations and Extensions

The `extend` keyword adds methods or interface implementations to types. It can be applied to structs, type aliases, and primitive types.

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

### 5.3 Extending Primitives & Type Aliases
Primitives can be extended just like structs. You can also extend specific union aliases. The methods are only accessible if the compiler knows the variable is definitively of that alias type.

```typescript
pub struct Red;
pub struct Blue;
pub type Theme = Red | Blue;

extend Theme {
  function is_cool(self): bool {
    self is Blue
  }
}
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

Generics operate on functions, structs, and interfaces. Bidirectional type inference eliminates the need to specify types in most scenarios.

### 7.1 Interface Bounds
Generic parameters can be constrained using interfaces via the `+` syntax.

```typescript
function process<T: Named + Printable>(item: T) {
  print(item.get_name() + ": " + item.to_string());
}
```

### 7.2 Overlapping Interface Priority
Extensions mapping interfaces to generic types might overlap (e.g., implementing an interface for `<T>` and specifically for `i32`).
- **Priority Rule**: The more specific implementation always wins.
- If the compiler detects an ambiguous overlap with no clear winner, compilation halts.

---

## 8. Type Logic (`is` and `as`)

### 8.1 Type Checking (`is`)
The `is` keyword checks the current inhabited type of a variable at runtime, evaluating to a `bool`.

### 8.2 Casting and Narrowing (`as`)

**Type Narrowing:**
When combined with union types, `as` forces the compiler to treat a variable as a specific variant. If the variable does not hold that variant at runtime, the program panics.

```typescript
var val: str | i32 = "hello";
if val is i32 {
  var num = val as i32; // Valid Narrowing
}
```

**Primitive Type Casting:**
`as` translates between primitive numeric boundaries (e.g., `i8 as i32`, `f64 as i32`). Unsafe casts (downcasting) evaluate via truncation.

---

## 9. Core Standard Library & State Wrappers

The auto-imported `of:core` prelude provides immediate access to essential types.

### 9.1 State Wrappers (`Option` and `Poll`)
To prevent dangerous resolution collisions (where unioning a generic `T` with `null` fails if `T` itself contains or is `null`), the language utilizes formal state structures.

```typescript
// For iteration and absence
pub struct None;
pub struct Some<T> { pub value: T }
pub type Option<T> = Some<T> | None;

// For state machine logic (Async)
pub struct Pending;
pub struct Ready<T> { pub value: T }
pub type Poll<T> = Ready<T> | Pending;
```

### 9.2 Built-in Interfaces (Traits)
- **`Eq` & `Hash`**: Controls hashing and `==` / `!=` behaviors.
- **`Index<Idx, Output>`** / **`IndexMut<Idx, Output>`**: Overloads array/map bracket notations.
- **`Iterator<T>`**: Enables `for item in collection` usage. Relies on the `Option` wrapper wrapper to separate data from exhaustion.
  ```typescript
  pub interface Iterator<T> {
    function next(self): Option<T>;
  }
  ```
- **`Clone`**: Defines deep copy semantics. By default, crossing isolation boundaries triggers a structural deep copy.
- **`Drop`**: Triggered immediately before the GC deallocates an object.
- **`ReprC`**: Disables `[gc header | type id]` generation, establishing standard C struct memory layout for direct interop.

### 9.3 Collections and Strings
- **`List<T>`**: Dynamic arrays `[...]`.
- **`Map<K, V>`**: Hash maps `{ "key": value }`.
- **`str`**: Immutable string types with native methods (`split`, `trim`, `replace`, etc.).
- Errors are not based on global exceptions, but via generic unions returning data or error variants: `T | ErrorStruct`.

---

## 10. Foreign Function Interface (FFI)

The `extern` keyword maps functionality across C ABI boundaries.

### 10.1 Pointers and Nullability
- **`*T` syntax**: Specifies a raw machine pointer.
- **Nullable pointers (`*T | null`)**: Native tagged unions cannot cross C boundaries. `*T | null` is syntactic sugar resolving to a raw pointer where `null` implies `0x0`.

### 10.2 Extern Structs and Ownership
Extern structs natively inhabit the foreign heap. They have no GC data and must be managed dynamically (e.g., via `malloc/free`).

```typescript
extern struct Buffer {
  data: *u8,
  size: u64,
}
```

### 10.3 Pinning Managed Structs
To pass managed pointers into C functions that retain state, the GC memory must be pinned.
```typescript
function pin<T>(value: T): *T;
function unpin<T>(ptr: *T);
```
Passing an immovable, retaining pointer to C without pinning it is a logic error and causes use-after-free crashes.

---

## 11. Metaprogramming (Procedural Macros)

The language uses Procedural Macros evaluated entirely via AST manipulation (no declarative macros).

- **Syntax**: Macros are invoked using the `@` decorator syntax preceding definitions.
- **Execution Model**: Macros are compiler-plugins executed in a strict Phase 1 compilation step. The macro runner receives serialized AST nodes, performs arbitrary logical transformations, and returns the modified AST node.

```typescript
// Macro Invocation
@json_serializable
pub struct Config {
  pub host: str,
  pub port: i32,
}

@Route("/api/users", method="GET")
function get_users(): str {
  "users"
}
```

Macro definitions leverage the standard compiler API:

```typescript
import { MacroContext, ASTNode } from "of:compiler";

@proc_macro
pub function json_serializable(ctx: MacroContext, node: ASTNode): ASTNode {
  // Returns a modified AST node injecting standard json serialization methods
}
```

---

## 12. Threads, Channels, and Shared State

Concurrency utilizes isolated execution contexts (Workers/Threads) alongside Channels. Shared memory escapes are highly restricted.

### 12.1 Threads and Cross-Boundary Data
Unpinned managed objects cannot cross thread boundaries directly. When spawning a thread, closures capturing variables trigger an automatic deep structural clone to isolate state.

### 12.2 Channels and Move Optimizations
Channels provide message-passing pipelines strictly generating a `Sender<T>` and a `Receiver<T>`.
- **Single Receiver Rules**: `Receiver<T>` is strictly single-owner and cannot be cloned, guaranteeing linear queue processing.
- **Zero-Copy Move**: When calling `tx.send(obj)`, the GC runtime inspection executes. If the object has a reference count of exactly `1` (holding no other references in the sender's thread), the runtime skips memory cloning and transfers the raw pointer to the receiver thread.

```typescript
var (tx, rx) = Channel.new<Person>();
var p = Person { name: "John" };

// Because 'p' is not used after this, its RC == 1. 
// The GC triggers a zero-copy pointer move instead of cloning.
tx.send(p);
```

### 12.3 `Shared<T>` Extraction Rules
To share global state directly, `Shared<T>` uses an internal OS mutex.
- **Detachment Rule**: Holding raw, unlocked pointers to shared GC memory causes immediate data races. Therefore, reading or extracting a managed struct from the lock closure forces a deep clone boundary.

```typescript
var state = Shared.new(Person { name: "Alice", age: 30 });

var local_name = state.lock(function(inner_ref) {
  inner_ref.age = 31; // Safe inside lock boundary
  
  // Returning the string forces an automatic deep clone, 
  // safely detaching it from the Shared state memory.
  return inner_ref.name;
});
```

---

## 13. Async / Await and Streams

Asynchronous programming uses a Dart-inspired syntax wrapping a Rust-like inert memory space state-machine model.

### 13.1 Futures and Explicit Execution
The `async` keyword changes a function's return type to an inert `Future<T>`.
- Futures are State Machines. They do nothing until driven.
- To execute, a Future must be explicitly `await`-ed or passed to an executor (e.g., `spawn()`). If a Future is generated but discarded, the compiler traps it as an error ("forgot to await" protection).

```typescript
async function fetch_data(): str {
  "data"
}

function main() {
  // fetch_data(); // COMPILER ERROR: Future created but unused.
  spawn(fetch_data()); // Correct explicit async loop entry
}
```

### 13.2 State Machine FFI and `Context`
`Future<Output>` maps directly to a low-level C ABI compliant polling interface. This ensures event loops written in C (like `libuv`) can pass waker callbacks natively into the language.

```typescript
pub interface Future<Output> {
  // Uses Poll state wrapper to safely handle nullability overlap
  function poll(self, ctx: *Context): Poll<Output>;
}

extern struct Context {
  waker_data: *u8,
  wake_fn: function(*u8), 
}
```

### 13.3 Streams and Async Iteration
Generators and sequences use `Stream<T>` and the `AsyncIterator` interface.

```typescript
pub interface AsyncIterator<T> {
  // Option handles stream completion safely
  function next_async(self): Future<Option<T>>;
}

async function* ticker(): Stream<i32> {
  yield 1;
  await sleep(100);
  yield 2;
}

// Consumed naturally via `for await` syntax
async function process() {
  for await (var val in ticker()) {
    print(val);
  }
}
```