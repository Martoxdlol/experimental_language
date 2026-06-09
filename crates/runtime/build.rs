//! Compile the worker-panic-isolation `setjmp`/`longjmp` shim
//! (`src/panic_boundary.c`) and link it into the runtime. See that file for
//! why a C shim is required (Cranelift frames have no unwind tables, so Rust's
//! unwinder cannot cross them; `longjmp` restores the saved context directly).
//!
//! The compiled objects are bundled into both the `rlib` (used by the in-process
//! JIT) and the `staticlib` (`libruntime.a`, linked by native `otter_fusion
//! build` output), so `otter_pb_*` resolves in both run modes.

fn main() {
    println!("cargo:rerun-if-changed=src/panic_boundary.c");
    let mut build = cc::Build::new();
    build.file("src/panic_boundary.c");
    // The native `otter_fusion build` path emits its object with a macOS 11.0
    // `LC_BUILD_VERSION` (see the CLI linker step); pin the C object to match so
    // the system linker does not warn about a version mismatch.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build.flag("-mmacosx-version-min=11.0");
    }
    build.compile("otter_panic_boundary");

    // Variadic `extern function` calls (`docs/19` §13) go through `libffi`, but
    // the runtime resolves that library lazily with `dlopen`/`dlsym`. Linux
    // systems often install only a versioned runtime SONAME such as
    // `libffi.so.8`, without the unversioned development symlink that `-lffi`
    // needs. Avoiding a build-time link edge keeps the workspace runnable on a
    // stock Linux runtime image while preserving the same generated-code ABI.
}
