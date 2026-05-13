# Language Specification

This is the full specification, split across topical chapters. Chapters are intended to be read in order, but each is reasonably self-contained.

## Index

1. [Lexical structure](./01-lexical.md) — comments (incl. `///` doc), identifiers, literals (`""` str, `''` char), literal suffixes, string interpolation, keywords.
2. [Primitive types](./02-types.md) — integers, floats, `bool`, `char`, `str`, `null`.
3. [Discriminated unions](./03-unions.md) — pipe syntax, flattening, recursion, ordering.
4. [Structs](./04-structs.md) — regular, tuple, unit; construction syntax; field shorthand; spread.
5. [Tuples](./05-tuples.md) — structural typing; positional access; destructuring.
6. [Variables](./06-variables.md) — `var`, mutability, module-level, scoping, shadowing.
7. [Expressions](./07-expressions.md) — blocks, `if`, `match`.
8. [Control flow](./08-control-flow.md) — `for`, `while`, `loop`, `break`, `continue`.
9. [Functions and closures](./09-functions.md) — `function`, lambdas, `|x|`, `it`, function types.
10. [Interfaces and extensions](./10-interfaces.md) — `interface`, `extend`, `Self`, defaults, orphan rule.
11. [Generics](./11-generics.md) — monomorphization, dynamic dispatch, overlap rules.
12. [Type logic](./12-type-logic.md) — `is`, `as`, flow typing, narrowing.
13. [Error handling](./13-error-handling.md) — `T | E`, `?`, `Try` interface.
14. [Panics](./14-panics.md) — `panic`, panic sources, integer overflow.
15. [Operators](./15-operators.md) — operator overloading; built-in interfaces; `ToStr` for string interpolation.
16. [Memory model](./16-memory.md) — GC, reference counting, layout, drop semantics.
17. [Modules and imports](./17-modules.md) — project layout, `mod` / `pub mod` tree, `import` paths (`core:` / `std:` / `pkg:` / project / relative), package-escape rule, visibility chain, `pub import` re-exports.
18. [Standard library](./18-stdlib.md) — `core:prelude` (`str`, `List`, `Map`, `Item`, `Iterator`, `Buffer`) + index of `std:*` modules.
19. [Foreign Function Interface](./19-ffi.md) — `extern`, pointers, pinning.
20. [Concurrency](./20-concurrency.md) — `std:thread`, `std:sync`: threads, channels, `Shared`.
21. [Async](./21-async.md) — `Future`, `await`, `spawn` (`std:async`), `AsyncIterator`, async closures.
22. [Macros](./22-macros.md) — procedural macros: `@UpperCamelCase`, decorator / expression / block forms, hygiene, sandbox, phases, `@Derive`.

## Design at a glance

- **Type system**: nominal structs, structural tuples, discriminated unions everywhere.
- **Memory**: reference-counted managed heap with a cycle collector; deterministic-ish drop; manual pinning for FFI.
- **Mutation**: every binding is mutable; access restrictions come from module visibility.
- **Errors**: unions plus `?`; no exceptions; panics terminate the thread.
- **Concurrency**: isolated threads, MPSC/MPMC channels with zero-copy on RC=1, `Shared<T>` mutex.
- **Async**: state-machine futures, explicit executor, no built-in generator syntax.
- **FFI**: C ABI via `extern`; managed heap and foreign heap are disjoint regions.
- **Generics**: monomorphized by default; dynamic dispatch only when an interface type appears as a value type.

## Conventions in this document

- Code samples are in the language unless explicitly marked otherwise.
- `// ...` and `/* ... */` are comments.
- Type identifiers (`Person`, `i32`, `Color`) appear in identifier syntax.
- Where a section says "panics", it means the panic semantics described in [14-panics.md](./14-panics.md).
