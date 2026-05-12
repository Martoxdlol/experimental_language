# 13. Error Handling

There are no exceptions and no `try`/`catch`. Errors are values, expressed as variants in a union. Propagation is done with the postfix `?` operator. Hard, unrecoverable failures use panics (see [14-panics.md](./14-panics.md)).

## 13.1 Errors as union variants

A fallible function returns a union of success and failure types:

```
struct Error {
  pub message: str,
  pub code: i32,
}

function divide(a: f64, b: f64): f64 | Error {
  if b == 0.0 {
    Error { message: "Division by zero", code: 1 }
  } else {
    a / b
  }
}
```

The caller inspects the union with `is`/`as`, `match`, or `?`:

```
var r = divide(10.0, 2.0)
match r {
  f64 v   => print("ok: " + (v as str)),
  Error e => print("err: " + e.message),
}
```

## 13.2 The `?` operator

`?` is a postfix operator for early-return propagation. Given:

```
function caller(): R {
  var x = expr?
  ...
}
```

`expr?` is roughly:

```
{
  var __tmp = expr
  if __tmp is <FailureSet> { return __tmp }
  __tmp                            // narrowed to <SuccessSet>
}
```

The compiler partitions the type of `expr` into two sets, **success** and **failure**, based on the enclosing function's return type `R`:

- A variant `V` of `expr`'s type goes into **failure** iff `V` is assignable to `R` (i.e. `V` is a variant of `R`, after union normalization).
- All other variants of `expr`'s type form **success**.

`expr?` then evaluates to a value whose static type is the success set. If at runtime `expr` is in the failure set, the function returns that value.

### Examples

```
function read_user(id: i64): User | NotFound | DbError { ... }

function summary(id: i64): str | NotFound | DbError {
  var user = read_user(id)?   // ? handles NotFound and DbError; user: User here
  user.name
}
```

`read_user`'s type is `User | NotFound | DbError`. `summary` returns `str | NotFound | DbError`. The partition:

- `NotFound` and `DbError` are assignable to `R = str | NotFound | DbError` → failure set.
- `User` is not assignable to `R` → success set.

So `user` has the narrowed type `User`. If `read_user(id)` returned `NotFound`, `summary` returns `NotFound` and never evaluates `.name`.

### When partitioning fails

If the success set is empty (every variant of `expr` is also a variant of `R`), `?` is meaningless — the expression always returns; the compiler reports an error and asks you to use a direct match.

If the failure set is empty (no variant of `expr` matches `R`), `?` is meaningless — there's nothing to propagate; the compiler reports an error.

If `expr`'s type has variants that are neither in `R` nor strictly successful (i.e. `expr` produces an error type that the caller does not list), `?` cannot type-check: the failure variant would have nowhere to go. The compiler reports the variant that fails to propagate and asks you to either widen `R` or handle the variant explicitly.

### Worked example: chained `?`

```
function load_config(): Config | IoError | ParseError { ... }
function validate(c: Config): Config | ValidationError { ... }

function start(): str | IoError | ParseError | ValidationError {
  var c = load_config()?    // strips IoError | ParseError
  var v = validate(c)?      // strips ValidationError
  "ok: " + v.name
}
```

`start` returns the full set of possible failures, and each `?` propagates whichever subset its argument can produce.

## 13.3 The `Try` interface (custom propagation)

The default `?` works on any union by partitioning against the enclosing return type. For types that are *not* unions but still want to participate in `?` (e.g. a wrapper struct), implement `Try`:

```
pub interface Try<Output, Residual> {
  function branch(self): Output | Residual
}
```

A type `T: Try<O, R>` can be used with `?` in any function whose return type accepts `R`:

```
v?     // for v: T where T: Try<O, R>
// desugars to:
{
  var __tmp = v.branch()
  if __tmp is R { return __tmp }   // possibly with FromResidual conversion (see below)
  __tmp as O
}
```

For unions, the compiler provides a built-in `Try` impl that does the partitioning described in 13.2. User code does not implement `Try` for unions.

### Residual conversion (`FromResidual`)

Sometimes a function returns an error type that is **almost** what a `?` would propagate, but not exactly. For instance, a wrapper:

```
struct AppError { pub source: IoError | ParseError, pub context: str }
```

Calling `load_config()?` from a function returning `Config | AppError` should convert `IoError | ParseError` → `AppError`. This is the role of `FromResidual`:

```
pub interface FromResidual<R> {
  function from_residual(r: R): Self
}
```

If `expr?`'s residual variant doesn't directly fit `R` but `R` has an implementation `FromResidual<ResidualVariant>`, the compiler inserts the conversion at the `return`. (Concretely: `return AppError.from_residual(io_error)` instead of `return io_error`.)

If multiple paths are possible (direct propagation and `FromResidual` conversion), direct propagation wins. If only the conversion fits, it is used. If neither, the compiler errors.

This makes `?` extensible without growing the union mechanism.

## 13.4 No exceptions, no unwinding, no `try`/`catch`

The language does not have:

- Exception types or `throw` statements.
- `try`/`catch`/`finally` constructs.
- Stack-unwinding errors that bubble through frames silently.

Every error must be either:

- Returned as a value (handled or propagated with `?`).
- Triggered as a panic (terminates the thread).

This is intentional: every fallible call site is visible at the type level. `?` makes propagation concise without hiding control flow — you can grep for `?` to see every place that may return early.

## 13.5 Panics are not catchable

`panic(...)` and panic-on-failure operations (`as`, integer division by zero, etc.) terminate the current thread. They are not catchable. Use them only for programming bugs and truly unrecoverable conditions; use `T | Error` and `?` for expected error paths.

See [14-panics.md](./14-panics.md).

## 13.6 Style recommendations

- Define a single error type per module boundary (e.g. `AppError`) and convert specific errors into it using `FromResidual`. This keeps callers' return types short.
- Use `?` for "if this fails I want to return now" and `match` for "I want to decide based on which failure".
- Don't use union types of more than ~5 error variants in a public API; collapse them with `FromResidual`.

## 13.7 Examples

```
struct IoError    { pub kind: str }
struct ParseError { pub at:   i64 }

function read_file(path: str): str | IoError { ... }
function parse(s: str): Config | ParseError { ... }

// Propagating both
function load(path: str): Config | IoError | ParseError {
  var s = read_file(path)?       // strips IoError on failure
  parse(s)?                      // strips ParseError on failure
}

// Wrapping into a single error type
struct AppError {
  pub message: str,
}

extend AppError: FromResidual<IoError> {
  function from_residual(e: IoError): AppError {
    AppError { message: "io: " + e.kind }
  }
}
extend AppError: FromResidual<ParseError> {
  function from_residual(e: ParseError): AppError {
    AppError { message: "parse at " + (e.at as str) }
  }
}

function load_wrapped(path: str): Config | AppError {
  var s = read_file(path)?       // IoError -> AppError via FromResidual
  parse(s)?                      // ParseError -> AppError via FromResidual
}
```
