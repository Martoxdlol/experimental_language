//! Variadic FFI calls via `libffi` (`docs/19` §13).
//!
//! Cranelift has no notion of a variadic call: an `ir::Signature` carries only
//! `{params, returns, call_conv}`, with no fixed/variadic boundary, and its
//! per-target ABI lowering treats every parameter uniformly. That is wrong for
//! the C variadic ABI on the targets we emit:
//!
//! - **aarch64-apple-darwin**: the Apple ARM64 ABI passes *every* variadic
//!   argument on the stack (8-byte slots) while named arguments use x0–x7/v0–v7.
//!   Cranelift would place the variadic arguments in registers.
//! - **x86-64 System V**: `%al` must hold the number of vector registers used by
//!   the call; Cranelift never sets it, so float variadics are miscompiled.
//! - **x86-64 Windows**: float variadics must be duplicated into the matching
//!   general-purpose register.
//!
//! So a `@Variadic extern function` call cannot be lowered as an ordinary
//! Cranelift call. Instead the code generator marshals the (already
//! C-default-promoted) arguments into a flat value buffer plus a parallel array
//! of type tags and routes the call through [`lang_variadic_call`], which drives
//! `libffi`'s `ffi_prep_cif_var` / `ffi_call`. `libffi` implements the correct
//! per-target variadic ABI (Apple stack passing, SysV `%al`, Windows
//! duplication), so JIT and native output behave identically on every target.
//!
//! We bind the small, stable public `libffi` C surface directly (rather than via
//! the `libffi-sys` crate) because this environment has neither `pkg-config`
//! (needed by `libffi-sys`'s `system` feature) nor autotools (needed by its
//! vendored build). The binding is deliberately minimal and resolved lazily with
//! `dlopen`/`dlsym`: Linux distributions often ship the runtime SONAME
//! (`libffi.so.8`) without the unversioned development symlink required by
//! `-lffi`. We never construct an `ffi_type` or read an `ffi_cif` field — we
//! only pass pointers to `libffi`'s exported `ffi_type_*` objects and an opaque,
//! over-aligned, over-sized `ffi_cif` scratch buffer that `ffi_prep_cif_var`
//! fills in. The only target-specific constant is [`FFI_DEFAULT_ABI`], verified
//! against `libffi`'s `ffitarget.h` (and a wrong value is caught at runtime:
//! `ffi_prep_cif_var` returns `FFI_BAD_ABI`).

use std::ffi::{c_void, CStr};
use std::sync::OnceLock;

// -- argument/return type tags -----------------------------------------------
//
// The code generator computes one tag per argument (and one for the return),
// applying the C default argument promotions to variadic-position arguments
// before tagging: `float` → `double`, and any integer narrower than `int` →
// `int`. Each tag selects the `ffi_type` the marshalled slot is interpreted as.
// These constants are the single source of truth, shared with the backend
// (`runtime::variadic::VTAG_*`).

/// `void` — only valid as a return tag (a call that yields no value).
pub const VTAG_VOID: u8 = 0;
pub const VTAG_I8: u8 = 1;
pub const VTAG_U8: u8 = 2;
pub const VTAG_I16: u8 = 3;
pub const VTAG_U16: u8 = 4;
pub const VTAG_I32: u8 = 5;
pub const VTAG_U32: u8 = 6;
pub const VTAG_I64: u8 = 7;
pub const VTAG_U64: u8 = 8;
pub const VTAG_F32: u8 = 9;
pub const VTAG_F64: u8 = 10;
/// A machine pointer (`*T`, `*c_void`, `str`, an extern function pointer, …).
pub const VTAG_PTR: u8 = 11;

/// The width, in bytes, of one marshalled argument slot in the value buffer.
/// Every argument occupies one slot regardless of its type; `libffi` reads only
/// the low `ffi_type->size` bytes (the targets we emit are little-endian, so the
/// significant bytes sit at the slot's base). The return slot is the same width
/// — at least `sizeof(ffi_arg)`, as `ffi_call` requires for narrow integer
/// returns.
pub const VARIADIC_SLOT_BYTES: usize = 8;

// -- libffi binding ----------------------------------------------------------

/// `FFI_DEFAULT_ABI` for the current target, as defined by `libffi`'s
/// `ffitarget.h`. This is the only target-specific magic number; a wrong value
/// makes `ffi_prep_cif_var` return `FFI_BAD_ABI`, which we assert on.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
const FFI_DEFAULT_ABI: u32 = 2; // FFI_UNIX64
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const FFI_DEFAULT_ABI: u32 = 1; // FFI_WIN64
#[cfg(all(target_arch = "aarch64", not(target_os = "windows")))]
const FFI_DEFAULT_ABI: u32 = 1; // FFI_SYSV
#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
const FFI_DEFAULT_ABI: u32 = 2; // FFI_WIN64

/// `FFI_OK` — the success status returned by `ffi_prep_cif_var`.
const FFI_OK: i32 = 0;

type FfiPrepCifVar = unsafe extern "C" fn(
    cif: *mut c_void,
    abi: u32,
    nfixed: u32,
    ntotal: u32,
    rtype: *const c_void,
    atypes: *mut *const c_void,
) -> i32;

type FfiCall = unsafe extern "C" fn(
    cif: *mut c_void,
    func: *const c_void,
    rvalue: *mut c_void,
    avalue: *mut *const c_void,
);

struct LibFfi {
    _handle: *mut c_void,
    ffi_prep_cif_var: FfiPrepCifVar,
    ffi_call: FfiCall,
    ffi_type_void: *const c_void,
    ffi_type_uint8: *const c_void,
    ffi_type_sint8: *const c_void,
    ffi_type_uint16: *const c_void,
    ffi_type_sint16: *const c_void,
    ffi_type_uint32: *const c_void,
    ffi_type_sint32: *const c_void,
    ffi_type_uint64: *const c_void,
    ffi_type_sint64: *const c_void,
    ffi_type_float: *const c_void,
    ffi_type_double: *const c_void,
    ffi_type_pointer: *const c_void,
}

// The handle is intentionally leaked for process lifetime; libffi's data-symbol
// addresses must remain valid for every later variadic call.
unsafe impl Send for LibFfi {}
unsafe impl Sync for LibFfi {}

static LIBFFI: OnceLock<LibFfi> = OnceLock::new();

#[cfg(target_os = "linux")]
const LIBFFI_CANDIDATES: &[&[u8]] = &[
    b"libffi.so.8\0",
    b"libffi.so.7\0",
    b"libffi.so.6\0",
    b"libffi.so\0",
];

#[cfg(target_os = "macos")]
const LIBFFI_CANDIDATES: &[&[u8]] = &[
    b"libffi.dylib\0",
    b"/usr/lib/libffi.dylib\0",
    b"/opt/homebrew/opt/libffi/lib/libffi.dylib\0",
    b"/usr/local/opt/libffi/lib/libffi.dylib\0",
];

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const LIBFFI_CANDIDATES: &[&[u8]] = &[b"libffi.so\0", b"libffi.so.8\0"];

fn libffi() -> &'static LibFfi {
    LIBFFI.get_or_init(load_libffi)
}

#[cfg(unix)]
fn load_libffi() -> LibFfi {
    let mut failures = Vec::new();
    for candidate in LIBFFI_CANDIDATES {
        // SAFETY: `candidate` is a static NUL-terminated byte string.
        unsafe {
            let _ = libc::dlerror();
            let handle = libc::dlopen(candidate.as_ptr().cast(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if !handle.is_null() {
                return load_libffi_symbols(handle);
            }
            let err = dlerror_string().unwrap_or_else(|| "unknown loader error".to_string());
            failures.push(format!("{} ({err})", c_name(candidate)));
        }
    }
    panic!(
        "lang_variadic_call: could not load libffi; tried {}",
        failures.join(", ")
    );
}

#[cfg(not(unix))]
fn load_libffi() -> LibFfi {
    panic!("lang_variadic_call: dynamic libffi loading is not implemented on this platform");
}

#[cfg(unix)]
unsafe fn load_libffi_symbols(handle: *mut c_void) -> LibFfi {
    LibFfi {
        _handle: handle,
        // SAFETY: `dlsym` returned callable addresses for libffi's stable C API.
        ffi_prep_cif_var: unsafe {
            std::mem::transmute::<*mut c_void, FfiPrepCifVar>(lookup_raw(
                handle,
                b"ffi_prep_cif_var\0",
            ))
        },
        ffi_call: unsafe {
            std::mem::transmute::<*mut c_void, FfiCall>(lookup_raw(handle, b"ffi_call\0"))
        },
        ffi_type_void: unsafe { lookup_raw(handle, b"ffi_type_void\0").cast_const() },
        ffi_type_uint8: unsafe { lookup_raw(handle, b"ffi_type_uint8\0").cast_const() },
        ffi_type_sint8: unsafe { lookup_raw(handle, b"ffi_type_sint8\0").cast_const() },
        ffi_type_uint16: unsafe { lookup_raw(handle, b"ffi_type_uint16\0").cast_const() },
        ffi_type_sint16: unsafe { lookup_raw(handle, b"ffi_type_sint16\0").cast_const() },
        ffi_type_uint32: unsafe { lookup_raw(handle, b"ffi_type_uint32\0").cast_const() },
        ffi_type_sint32: unsafe { lookup_raw(handle, b"ffi_type_sint32\0").cast_const() },
        ffi_type_uint64: unsafe { lookup_raw(handle, b"ffi_type_uint64\0").cast_const() },
        ffi_type_sint64: unsafe { lookup_raw(handle, b"ffi_type_sint64\0").cast_const() },
        ffi_type_float: unsafe { lookup_raw(handle, b"ffi_type_float\0").cast_const() },
        ffi_type_double: unsafe { lookup_raw(handle, b"ffi_type_double\0").cast_const() },
        ffi_type_pointer: unsafe { lookup_raw(handle, b"ffi_type_pointer\0").cast_const() },
    }
}

#[cfg(unix)]
unsafe fn lookup_raw(handle: *mut c_void, symbol: &'static [u8]) -> *mut c_void {
    // SAFETY: `symbol` is a static NUL-terminated byte string and `handle` is a
    // successful `dlopen` result kept alive by `LibFfi`.
    let ptr = unsafe { libc::dlsym(handle, symbol.as_ptr().cast()) };
    if ptr.is_null() {
        let err = dlerror_string().unwrap_or_else(|| "unknown symbol lookup error".to_string());
        panic!(
            "lang_variadic_call: libffi missing symbol {} ({err})",
            c_name(symbol)
        );
    }
    ptr
}

#[cfg(unix)]
fn dlerror_string() -> Option<String> {
    // SAFETY: `dlerror` returns either null or a process-owned NUL-terminated
    // diagnostic string valid until the next dynamic-loader call.
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn c_name(bytes: &'static [u8]) -> &'static str {
    std::str::from_utf8(&bytes[..bytes.len().saturating_sub(1)]).unwrap_or("<invalid>")
}

/// Map an argument/return tag to the address of `libffi`'s corresponding
/// `ffi_type` object.
fn ffi_type_for(tag: u8) -> *const c_void {
    let ffi = libffi();
    match tag {
        VTAG_VOID => ffi.ffi_type_void,
        VTAG_I8 => ffi.ffi_type_sint8,
        VTAG_U8 => ffi.ffi_type_uint8,
        VTAG_I16 => ffi.ffi_type_sint16,
        VTAG_U16 => ffi.ffi_type_uint16,
        VTAG_I32 => ffi.ffi_type_sint32,
        VTAG_U32 => ffi.ffi_type_uint32,
        VTAG_I64 => ffi.ffi_type_sint64,
        VTAG_U64 => ffi.ffi_type_uint64,
        VTAG_F32 => ffi.ffi_type_float,
        VTAG_F64 => ffi.ffi_type_double,
        VTAG_PTR => ffi.ffi_type_pointer,
        other => panic!("lang_variadic_call: invalid type tag {other}"),
    }
}

/// An opaque, over-aligned, over-sized scratch buffer for one `ffi_cif`.
///
/// We never inspect the `ffi_cif`'s fields — `ffi_prep_cif_var` writes it and
/// `ffi_call` reads it — so we only need space and alignment. A real `ffi_cif`
/// is a small fixed struct (well under 64 bytes on every target `libffi`
/// supports); 256 bytes with 16-byte alignment is a safe, generous bound.
#[repr(C, align(16))]
struct FfiCif([u8; 256]);

/// Perform a C variadic call through `libffi` (`docs/19` §13).
///
/// The code generator has already evaluated and C-default-promoted every
/// argument, packing each into an 8-byte slot of `values` (little-endian, low
/// bytes significant) with a parallel `tags` entry, and reserved an 8-byte
/// `ret_slot`. This builds the matching `ffi_cif` and invokes `fn_ptr`.
///
/// # Safety
/// - `fn_ptr` must point to a C function matching the described signature.
/// - `tags` must point to `n_total` readable bytes; `values` to
///   `n_total * 8` readable bytes.
/// - `ret_slot` must point to at least 8 writable bytes.
/// - `0 < n_fixed <= n_total` (the checker guarantees this).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_variadic_call(
    fn_ptr: *const c_void,
    n_fixed: u32,
    n_total: u32,
    tags: *const u8,
    values: *const u8,
    ret_tag: u8,
    ret_slot: *mut u8,
) {
    let ffi = libffi();
    let n = n_total as usize;
    let mut atypes: Vec<*const c_void> = Vec::with_capacity(n);
    let mut avalues: Vec<*const c_void> = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: `tags`/`values` are valid for `n_total` entries (caller
        // contract); each value slot is `VARIADIC_SLOT_BYTES` wide.
        let tag = unsafe { *tags.add(i) };
        atypes.push(ffi_type_for(tag));
        avalues.push(unsafe { values.add(i * VARIADIC_SLOT_BYTES) }.cast());
    }
    let rtype = ffi_type_for(ret_tag);
    let mut cif = FfiCif([0u8; 256]);
    let cif_ptr = (&raw mut cif).cast::<c_void>();
    // SAFETY: `cif` is a valid, sufficiently large/aligned scratch buffer;
    // `atypes`/`rtype` are valid `ffi_type` pointers; counts come from the
    // caller (the checker enforces `0 < n_fixed <= n_total`).
    let status = unsafe {
        (ffi.ffi_prep_cif_var)(
            cif_ptr,
            FFI_DEFAULT_ABI,
            n_fixed,
            n_total,
            rtype,
            atypes.as_mut_ptr(),
        )
    };
    assert_eq!(status, FFI_OK, "ffi_prep_cif_var failed (status {status})");
    // SAFETY: the cif is prepared; `fn_ptr` matches it; `ret_slot` has >= 8
    // bytes; `avalues` point into the live `values` buffer.
    unsafe {
        (ffi.ffi_call)(
            cif_ptr,
            fn_ptr,
            ret_slot.cast::<c_void>(),
            avalues.as_mut_ptr(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        // `int snprintf(char *str, size_t size, const char *format, ...)` — a
        // real variadic libc function, used to exercise the `libffi` binding,
        // tag mapping, and slot marshalling independently of the code generator.
        fn snprintf(buf: *mut u8, size: usize, fmt: *const u8, ...) -> i32;
    }

    /// Drive `lang_variadic_call` against `snprintf` exactly as the backend
    /// would: 3 fixed args (`buf`, `size`, `fmt`) followed by a promoted `int`
    /// and `double`, each packed into an 8-byte little-endian slot.
    #[test]
    fn variadic_call_snprintf_int_and_double() {
        let mut out = [0u8; 64];
        let fmt = b"%d %.1f\0";
        // One 8-byte slot per argument; integers/pointers in the low bytes, the
        // double as its bit pattern.
        let values: [u64; 5] = [
            out.as_mut_ptr() as u64, // buf      (pointer)
            out.len() as u64,        // size     (u64)
            fmt.as_ptr() as u64,     // fmt      (pointer)
            42u64,                   // 42       (variadic int)
            3.5f64.to_bits(),        // 3.5      (variadic double)
        ];
        let tags = [VTAG_PTR, VTAG_U64, VTAG_PTR, VTAG_I32, VTAG_F64];
        let mut ret = [0u8; VARIADIC_SLOT_BYTES];

        // SAFETY: the slots, tags, and counts describe a valid `snprintf` call.
        unsafe {
            lang_variadic_call(
                snprintf as *const c_void,
                3,
                5,
                tags.as_ptr(),
                values.as_ptr().cast::<u8>(),
                VTAG_I32,
                ret.as_mut_ptr(),
            );
        }

        // snprintf returns the would-be length (excluding the NUL): "42 3.5" = 6.
        let written = i32::from_le_bytes(ret[..4].try_into().unwrap());
        assert_eq!(written, 6);
        let nul = out.iter().position(|&b| b == 0).unwrap();
        assert_eq!(std::str::from_utf8(&out[..nul]).unwrap(), "42 3.5");
    }

    /// A negative `int` variadic argument round-trips with the correct sign
    /// (the backend sign-extends into the slot; `libffi` reads `sint32`).
    #[test]
    fn variadic_call_snprintf_negative_int() {
        let mut out = [0u8; 32];
        let fmt = b"%d\0";
        let values: [u64; 4] = [
            out.as_mut_ptr() as u64,
            out.len() as u64,
            fmt.as_ptr() as u64,
            (-12345i32 as i64) as u64, // sign-extended, as the backend emits
        ];
        let tags = [VTAG_PTR, VTAG_U64, VTAG_PTR, VTAG_I32];
        let mut ret = [0u8; VARIADIC_SLOT_BYTES];
        unsafe {
            lang_variadic_call(
                snprintf as *const c_void,
                3,
                4,
                tags.as_ptr(),
                values.as_ptr().cast::<u8>(),
                VTAG_I32,
                ret.as_mut_ptr(),
            );
        }
        let nul = out.iter().position(|&b| b == 0).unwrap();
        assert_eq!(std::str::from_utf8(&out[..nul]).unwrap(), "-12345");
    }
}
