//! Cranelift code generation and JIT execution.
//!
//! This is the first backend slice: it lowers the imperative core the type
//! checker understands — primitive-typed functions, locals, operators, blocks,
//! `if`/`else`, `return`, and direct calls — into Cranelift IR and JIT-compiles
//! it. Each language function becomes one Cranelift function; locals become
//! Cranelift variables; the checker's recorded types pick integer widths and
//! signedness.
//!
//! Forms the checker accepts but this slice cannot yet lower (unions, structs,
//! strings, closures, …) produce a [`CodegenError`] rather than wrong code, so
//! the supported surface is always explicit. The collector grows alongside the
//! checker.
//!
//! GC integration and precise stack maps are wired in (`docs/16`). Panic
//! sources from `docs/14` are honored: divide-by-zero, integer overflow on
//! `+`/`-`/`*` (debug semantics — the only profile so far; release wrapping
//! awaits build profiles), and shifts past the bit width all call `lang_panic`.
//! Both the JIT (`compile`) and native object output (`compile_object`) share
//! one `Module`-generic backend.

use compiler::ast::*;
use compiler::hir::{self, Hir};
use compiler::ids::{DefId, LocalId};
use compiler::sema::results::ForIter;
use compiler::sema::{Adjust, Analysis, Builtin, CloneKind, DefKind, NumIntrinsic};
use compiler::span::Span;
use compiler::ty::{FloatTy, IntTy, Ty, TyKind};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlags, StackSlotData, StackSlotKind, Type as ClType, Value, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Shared codegen helpers (type lowering, layout, monomorphization, async-body
/// analysis), factored out of the per-function generator below.
mod support;
use support::*;
mod dwarf;
mod gen_call;
mod gen_cast;
mod gen_collections;
mod gen_expr;
mod gen_hir;
mod gen_struct;
pub mod macro_host;

/// Pointer-width integer type on the host (str/reference values are pointers).
/// The JIT only targets the 64-bit host, so this is `I64`.
pub(crate) const PTR: ClType = types::I64;

/// Descriptor `kind` for a plain object (scan its listed pointer offsets).
/// Mirrors `runtime::gc::KIND_PLAIN`.
pub(crate) const GC_KIND_PLAIN: u64 = 0;

/// Descriptor `kind` for a `@RefCounted` object (`docs/16` §8.1): a hidden
/// atomic strong-count word at offset 0; traced like a plain object.
/// Mirrors `runtime::gc::KIND_REFCOUNTED`.
pub(crate) const GC_KIND_REFCOUNTED: u64 = 4;

/// Monotonic counter for unique anonymous data-object names (string literals).
static DATA_CTR: AtomicU64 = AtomicU64::new(0);

/// Register the runtime's C-ABI entry points so the JIT can resolve calls.
fn register_runtime_symbols(b: &mut JITBuilder) {
    b.symbol("lang_alloc", runtime::lang_alloc as *const u8);
    b.symbol("lang_panic", runtime::lang_panic as *const u8);
    b.symbol(
        "lang_gc_safepoint",
        runtime::gc::lang_gc_safepoint as *const u8,
    );
    b.symbol("lang_gc_pin", runtime::gc::lang_gc_pin as *const u8);
    b.symbol("lang_gc_unpin", runtime::gc::lang_gc_unpin as *const u8);
    b.symbol(
        "lang_gc_register_drop",
        runtime::gc::lang_gc_register_drop as *const u8,
    );
    b.symbol(
        "lang_block_on",
        runtime::async_rt::lang_block_on as *const u8,
    );
    b.symbol(
        "lang_async_yield",
        runtime::async_rt::lang_async_yield as *const u8,
    );
    b.symbol(
        "lang_async_sleep",
        runtime::async_rt::lang_async_sleep as *const u8,
    );
    b.symbol(
        "lang_async_timeout",
        runtime::async_rt::lang_async_timeout as *const u8,
    );
    b.symbol(
        "lang_async_spawn",
        runtime::threads::lang_async_spawn as *const u8,
    );
    b.symbol(
        "lang_async_spawn_future",
        runtime::threads::lang_async_spawn_future as *const u8,
    );
    b.symbol(
        "lang_future_cancel",
        runtime::threads::lang_future_cancel as *const u8,
    );
    b.symbol(
        "lang_thread_spawn",
        runtime::threads::lang_thread_spawn as *const u8,
    );
    b.symbol(
        "lang_thread_spawn_async",
        runtime::threads::lang_thread_spawn_async as *const u8,
    );
    b.symbol(
        "lang_task_spawn",
        runtime::threads::lang_task_spawn as *const u8,
    );
    b.symbol(
        "lang_task_spawn_async",
        runtime::threads::lang_task_spawn_async as *const u8,
    );
    b.symbol(
        "lang_thread_join_future",
        runtime::threads::lang_thread_join_future as *const u8,
    );
    b.symbol(
        "lang_task_join_future",
        runtime::threads::lang_task_join_future as *const u8,
    );
    b.symbol(
        "lang_task_cancel",
        runtime::threads::lang_task_cancel as *const u8,
    );
    b.symbol(
        "lang_thread_detach",
        runtime::threads::lang_thread_detach as *const u8,
    );
    b.symbol(
        "lang_channel_new",
        runtime::channels::lang_channel_new as *const u8,
    );
    b.symbol(
        "lang_chan_send",
        runtime::channels::lang_chan_send as *const u8,
    );
    b.symbol(
        "lang_chan_recv_future",
        runtime::channels::lang_chan_recv_future as *const u8,
    );
    b.symbol(
        "lang_chan_try_recv",
        runtime::channels::lang_chan_try_recv as *const u8,
    );
    b.symbol(
        "lang_chan_recv_blocking",
        runtime::channels::lang_chan_recv_blocking as *const u8,
    );
    b.symbol(
        "lang_chan_sender_acquire",
        runtime::channels::lang_chan_sender_acquire as *const u8,
    );
    b.symbol(
        "lang_chan_sender_release",
        runtime::channels::lang_chan_sender_release as *const u8,
    );
    b.symbol(
        "lang_chan_receiver_acquire",
        runtime::channels::lang_chan_receiver_acquire as *const u8,
    );
    b.symbol(
        "lang_chan_receiver_release",
        runtime::channels::lang_chan_receiver_release as *const u8,
    );
    b.symbol("lang_rc_retain", runtime::gc::lang_rc_retain as *const u8);
    b.symbol("lang_rc_release", runtime::gc::lang_rc_release as *const u8);
    b.symbol(
        "lang_shared_new",
        runtime::shared::lang_shared_new as *const u8,
    );
    b.symbol(
        "lang_shared_lock_future",
        runtime::shared::lang_shared_lock_future as *const u8,
    );
    b.symbol(
        "lang_shared_try_acquire",
        runtime::shared::lang_shared_try_acquire as *const u8,
    );
    b.symbol(
        "lang_shared_read",
        runtime::shared::lang_shared_read as *const u8,
    );
    b.symbol(
        "lang_shared_release",
        runtime::shared::lang_shared_release as *const u8,
    );
    b.symbol(
        "lang_shared_release_all",
        runtime::shared::lang_shared_release_all as *const u8,
    );
    b.symbol("lang_exit", runtime::lang_exit as *const u8);
    b.symbol("lang_abort", runtime::lang_abort as *const u8);
    b.symbol(
        "lang_foreign_alloc",
        runtime::foreign::lang_foreign_alloc as *const u8,
    );
    b.symbol(
        "lang_foreign_alloc_zeroed",
        runtime::foreign::lang_foreign_alloc_zeroed as *const u8,
    );
    b.symbol(
        "lang_foreign_free",
        runtime::foreign::lang_foreign_free as *const u8,
    );
    b.symbol(
        "lang_foreign_realloc",
        runtime::foreign::lang_foreign_realloc as *const u8,
    );
    b.symbol(
        "lang_cstring_from_str",
        runtime::foreign::lang_cstring_from_str as *const u8,
    );
    b.symbol(
        "lang_cstr_to_str",
        runtime::foreign::lang_cstr_to_str as *const u8,
    );
    b.symbol(
        "lang_cstr_len",
        runtime::foreign::lang_cstr_len as *const u8,
    );
    b.symbol(
        "lang_buffer_read",
        runtime::foreign::lang_buffer_read as *const u8,
    );
    b.symbol(
        "lang_buffer_write",
        runtime::foreign::lang_buffer_write as *const u8,
    );
    b.symbol(
        "lang_foreign_outstanding",
        runtime::foreign::lang_foreign_outstanding as *const u8,
    );
    b.symbol("lang_list_new", runtime::lang_list_new as *const u8);
    b.symbol("lang_list_push", runtime::lang_list_push as *const u8);
    b.symbol("lang_list_size", runtime::lang_list_size as *const u8);
    b.symbol("lang_list_get", runtime::lang_list_get as *const u8);
    b.symbol("lang_list_set", runtime::lang_list_set as *const u8);
    b.symbol("lang_list_clone", runtime::lang_list_clone as *const u8);
    b.symbol("lang_list_clear", runtime::lang_list_clear as *const u8);
    b.symbol("lang_list_pop", runtime::lang_list_pop as *const u8);
    b.symbol("lang_list_insert", runtime::lang_list_insert as *const u8);
    b.symbol("lang_list_remove", runtime::lang_list_remove as *const u8);
    b.symbol(
        "lang_list_truncate",
        runtime::lang_list_truncate as *const u8,
    );
    b.symbol("lang_list_slice", runtime::lang_list_slice as *const u8);
    b.symbol("lang_map_new", runtime::lang_map_new as *const u8);
    b.symbol("lang_map_set", runtime::lang_map_set as *const u8);
    b.symbol("lang_map_get", runtime::lang_map_get as *const u8);
    b.symbol("lang_map_index", runtime::lang_map_index as *const u8);
    b.symbol("lang_map_contains", runtime::lang_map_contains as *const u8);
    b.symbol("lang_map_remove", runtime::lang_map_remove as *const u8);
    b.symbol("lang_map_size", runtime::lang_map_size as *const u8);
    b.symbol("lang_map_clear", runtime::lang_map_clear as *const u8);
    b.symbol("lang_map_entries", runtime::lang_map_entries as *const u8);
    b.symbol("lang_map_extend", runtime::lang_map_extend as *const u8);
    b.symbol("lang_map_clone", runtime::lang_map_clone as *const u8);
    b.symbol(
        "lang_str_from_utf8",
        runtime::lang_str_from_utf8 as *const u8,
    );
    b.symbol("lang_str_size", runtime::lang_str_size as *const u8);
    b.symbol(
        "lang_str_byte_size",
        runtime::lang_str_byte_size as *const u8,
    );
    b.symbol("lang_str_eq", runtime::lang_str_eq as *const u8);
    b.symbol("lang_str_cmp", runtime::lang_str_cmp as *const u8);
    b.symbol("lang_str_contains", runtime::lang_str_contains as *const u8);
    b.symbol(
        "lang_str_starts_with",
        runtime::lang_str_starts_with as *const u8,
    );
    b.symbol(
        "lang_str_ends_with",
        runtime::lang_str_ends_with as *const u8,
    );
    b.symbol(
        "lang_str_substring",
        runtime::lang_str_substring as *const u8,
    );
    b.symbol("lang_str_to_upper", runtime::lang_str_to_upper as *const u8);
    b.symbol("lang_str_to_lower", runtime::lang_str_to_lower as *const u8);
    b.symbol("lang_str_trim", runtime::lang_str_trim as *const u8);
    b.symbol("lang_str_repeat", runtime::lang_str_repeat as *const u8);
    b.symbol("lang_str_replace", runtime::lang_str_replace as *const u8);
    b.symbol("lang_str_index_of", runtime::lang_str_index_of as *const u8);
    b.symbol("lang_str_split", runtime::lang_str_split as *const u8);
    b.symbol("lang_str_char_at", runtime::lang_str_char_at as *const u8);
    b.symbol("lang_str_to_chars", runtime::lang_str_to_chars as *const u8);
    b.symbol("lang_str_to_bytes", runtime::lang_str_to_bytes as *const u8);
    b.symbol("lang_str_concat", runtime::lang_str_concat as *const u8);
    b.symbol("lang_hash_i64", runtime::hash::lang_hash_i64 as *const u8);
    b.symbol("lang_hash_str", runtime::hash::lang_hash_str as *const u8);
    b.symbol("lang_hash_f64", runtime::hash::lang_hash_f64 as *const u8);
    b.symbol("lang_eq_i64", runtime::hash::lang_eq_i64 as *const u8);
    b.symbol("lang_eq_str", runtime::hash::lang_eq_str as *const u8);
    b.symbol("lang_int_to_str", runtime::lang_int_to_str as *const u8);
    b.symbol("lang_uint_to_str", runtime::lang_uint_to_str as *const u8);
    b.symbol("lang_float_to_str", runtime::lang_float_to_str as *const u8);
    b.symbol("lang_bool_to_str", runtime::lang_bool_to_str as *const u8);
    b.symbol("lang_char_to_str", runtime::lang_char_to_str as *const u8);
    b.symbol("lang_print", runtime::lang_print as *const u8);
    b.symbol("lang_println", runtime::lang_println as *const u8);
    b.symbol("lang_eprint", runtime::lang_eprint as *const u8);
    b.symbol("lang_eprintln", runtime::lang_eprintln as *const u8);
    b.symbol(
        "lang_io_stdin_read",
        runtime::lang_io_stdin_read as *const u8,
    );
    b.symbol(
        "lang_io_stdin_read_to_end",
        runtime::lang_io_stdin_read_to_end as *const u8,
    );
    b.symbol(
        "lang_io_stdout_write",
        runtime::lang_io_stdout_write as *const u8,
    );
    b.symbol(
        "lang_io_stderr_write",
        runtime::lang_io_stderr_write as *const u8,
    );
    b.symbol(
        "lang_io_stdout_flush",
        runtime::lang_io_stdout_flush as *const u8,
    );
    b.symbol(
        "lang_io_stderr_flush",
        runtime::lang_io_stderr_flush as *const u8,
    );
    b.symbol(
        "lang_io_stdin_read_async",
        runtime::async_rt::lang_io_stdin_read_async as *const u8,
    );
    b.symbol(
        "lang_io_stdin_read_to_end_async",
        runtime::async_rt::lang_io_stdin_read_to_end_async as *const u8,
    );
    b.symbol(
        "lang_io_stdout_write_async",
        runtime::async_rt::lang_io_stdout_write_async as *const u8,
    );
    b.symbol(
        "lang_io_stderr_write_async",
        runtime::async_rt::lang_io_stderr_write_async as *const u8,
    );
    b.symbol(
        "lang_io_stdout_flush_async",
        runtime::async_rt::lang_io_stdout_flush_async as *const u8,
    );
    b.symbol(
        "lang_io_stderr_flush_async",
        runtime::async_rt::lang_io_stderr_flush_async as *const u8,
    );
    b.symbol(
        "lang_fs_read_text",
        runtime::fs::lang_fs_read_text as *const u8,
    );
    b.symbol(
        "lang_fs_write_text",
        runtime::fs::lang_fs_write_text as *const u8,
    );
    b.symbol(
        "lang_fs_append_text",
        runtime::fs::lang_fs_append_text as *const u8,
    );
    b.symbol(
        "lang_fs_read_bytes",
        runtime::fs::lang_fs_read_bytes as *const u8,
    );
    b.symbol(
        "lang_fs_write_bytes",
        runtime::fs::lang_fs_write_bytes as *const u8,
    );
    b.symbol(
        "lang_fs_file_open",
        runtime::fs::lang_fs_file_open as *const u8,
    );
    b.symbol(
        "lang_fs_file_close",
        runtime::fs::lang_fs_file_close as *const u8,
    );
    b.symbol(
        "lang_fs_file_read",
        runtime::fs::lang_fs_file_read as *const u8,
    );
    b.symbol(
        "lang_fs_file_read_to_end",
        runtime::fs::lang_fs_file_read_to_end as *const u8,
    );
    b.symbol(
        "lang_fs_file_write",
        runtime::fs::lang_fs_file_write as *const u8,
    );
    b.symbol(
        "lang_fs_file_flush",
        runtime::fs::lang_fs_file_flush as *const u8,
    );
    b.symbol(
        "lang_fs_file_seek",
        runtime::fs::lang_fs_file_seek as *const u8,
    );
    b.symbol("lang_fs_exists", runtime::fs::lang_fs_exists as *const u8);
    b.symbol("lang_fs_is_file", runtime::fs::lang_fs_is_file as *const u8);
    b.symbol("lang_fs_is_dir", runtime::fs::lang_fs_is_dir as *const u8);
    b.symbol("lang_fs_kind", runtime::fs::lang_fs_kind as *const u8);
    b.symbol("lang_fs_len", runtime::fs::lang_fs_len as *const u8);
    b.symbol(
        "lang_fs_read_only",
        runtime::fs::lang_fs_read_only as *const u8,
    );
    b.symbol(
        "lang_fs_executable",
        runtime::fs::lang_fs_executable as *const u8,
    );
    b.symbol("lang_fs_remove", runtime::fs::lang_fs_remove as *const u8);
    b.symbol("lang_fs_rename", runtime::fs::lang_fs_rename as *const u8);
    b.symbol(
        "lang_fs_create_dir",
        runtime::fs::lang_fs_create_dir as *const u8,
    );
    b.symbol(
        "lang_fs_create_dir_all",
        runtime::fs::lang_fs_create_dir_all as *const u8,
    );
    b.symbol(
        "lang_fs_canonicalize",
        runtime::fs::lang_fs_canonicalize as *const u8,
    );
    b.symbol(
        "lang_fs_native_separator",
        runtime::fs::lang_fs_native_separator as *const u8,
    );
    b.symbol(
        "lang_fs_read_dir",
        runtime::fs::lang_fs_read_dir as *const u8,
    );
    b.symbol(
        "lang_process_args",
        runtime::process::lang_process_args as *const u8,
    );
    b.symbol(
        "lang_process_env",
        runtime::process::lang_process_env as *const u8,
    );
    b.symbol(
        "lang_process_env_all",
        runtime::process::lang_process_env_all as *const u8,
    );
    b.symbol(
        "lang_process_set_env",
        runtime::process::lang_process_set_env as *const u8,
    );
    b.symbol(
        "lang_process_status",
        runtime::process::lang_process_status as *const u8,
    );
    b.symbol(
        "lang_process_output",
        runtime::process::lang_process_output as *const u8,
    );
    b.symbol(
        "lang_rand_os_u32",
        runtime::rand::lang_rand_os_u32 as *const u8,
    );
    b.symbol(
        "lang_time_monotonic_nanos",
        runtime::time::lang_time_monotonic_nanos as *const u8,
    );
    b.symbol(
        "lang_time_system_nanos",
        runtime::time::lang_time_system_nanos as *const u8,
    );
    b.symbol(
        "lang_time_sleep_nanos",
        runtime::time::lang_time_sleep_nanos as *const u8,
    );
    // Variadic `extern function` calls (`docs/19` §13) route through `libffi`.
    b.symbol(
        "lang_variadic_call",
        runtime::variadic::lang_variadic_call as *const u8,
    );
    // Procedural-macro host functions (`docs/22`): the prelude's
    // `extend ASTNode/MacroContext` methods are seeded into every JIT, so their
    // `__ast_*`/`__mctx_*` externs must always resolve (dead code in a normal
    // program; live when a macro runs).
    macro_host::register_symbols(b);
}

/// A failure to lower a construct that is otherwise well-typed.
#[derive(Clone, Debug)]
pub struct CodegenError {
    pub message: String,
    pub span: Span,
}

impl CodegenError {
    fn new(span: Span, msg: impl Into<String>) -> Self {
        CodegenError {
            message: msg.into(),
            span,
        }
    }
}

pub(crate) type CgResult<T> = Result<T, CodegenError>;

/// A JIT-compiled program. Owns the executable memory; function pointers are
/// valid for as long as it lives.
pub struct Jit {
    module: JITModule,
    /// Language function name → its Cranelift id.
    funcs: HashMap<String, FuncId>,
    /// The `Pending` type id (`docs/21`), if the program reached the async
    /// runtime — recorded so a top-level driver can call `lang_block_on` on an
    /// `async main`'s root future without crossing back into the analysis.
    pending_tid: Option<i64>,
    /// Whether the user `main` is an `async function` (`docs/21` §6 — async
    /// `main`): the compiled symbol returns a `Future<…>` box instead of
    /// running the body. A native or JIT driver consumes that future via the
    /// runtime executor.
    main_is_async: bool,
    /// Per-function source-line provenance `(func, code byte offset, source byte
    /// offset)` — the debug-line data captured from `set_srcloc` (basis for
    /// native DWARF; exposed for tests/tooling).
    line_info: Vec<(FuncId, u32, u32)>,
}

impl Jit {
    /// The number of captured source-line mappings (debug-info provenance).
    pub fn source_line_entries(&self) -> usize {
        self.line_info.len()
    }

    /// Raw code pointer for a compiled function by language name.
    pub fn func_ptr(&self, name: &str) -> Option<*const u8> {
        self.funcs
            .get(name)
            .map(|id| self.module.get_finalized_function(*id))
    }

    /// Run the program's `main` — calling it directly if sync, or driving its
    /// root future via the runtime executor if it is an `async function`
    /// (`docs/21` §6). The user does not (cannot) call the executor entry
    /// `lang_block_on` themselves.
    ///
    /// # Safety
    /// `main` must exist with the standard zero-arg signature; for async main
    /// it must return a `Future<…>` box (constructor ABI).
    pub unsafe fn run_main(&self) -> bool {
        let Some(ptr) = self.func_ptr("main") else {
            return false;
        };
        if self.main_is_async {
            let pending_tid = self.pending_tid.unwrap_or(0);
            let ctor: extern "C" fn() -> *mut u8 = unsafe { std::mem::transmute(ptr) };
            let fut = ctor();
            // The future graph isn't reachable from any scanned stack until the
            // executor reads it; pin it across the cross-thread handoff inside
            // the runtime drives the future to completion.
            unsafe { runtime::async_rt::lang_block_on(fut, pending_tid) };
        } else {
            let main: extern "C" fn() = unsafe { std::mem::transmute(ptr) };
            main();
        }
        true
    }

    /// Run a zero-argument, no-result function by its symbol — used by
    /// `otter_fusion test` to invoke a `test "name" { … }` body. Returns false if
    /// the symbol is missing. A failing test panics (process exit 101); the
    /// runner runs each test in a subprocess and reads the exit code.
    ///
    /// # Safety
    /// The named function must exist and take no arguments / return nothing.
    pub unsafe fn run_void(&self, name: &str) -> bool {
        let Some(ptr) = self.func_ptr(name) else {
            return false;
        };
        let f: extern "C" fn() = unsafe { std::mem::transmute(ptr) };
        f();
        true
    }

    /// Call a zero-argument function returning `i64` (test/`main` convenience).
    ///
    /// # Safety
    /// The named function must exist, take no arguments, and return an `i64`.
    pub unsafe fn call_i64(&self, name: &str) -> Option<i64> {
        let ptr = self.func_ptr(name)?;
        let f: extern "C" fn() -> i64 = unsafe { std::mem::transmute(ptr) };
        Some(f())
    }
}

/// Enable or disable the tracing garbage collector. `lang run` enables it for
/// (single-threaded) programs; the in-process test harness leaves it off.
pub fn set_gc_enabled(on: bool) {
    runtime::gc::lang_gc_set_enabled(on);
}

/// Build profile (`docs/14` §5). In **debug** (the default) overflowing `+`/`-`/
/// `*` and signed `INT_MIN / -1` panic; in **release** they wrap (two's
/// complement). Shifts past the bit width and divide-by-zero panic in *both*
/// profiles. Set before `compile`/`compile_object`.
static RELEASE_PROFILE: AtomicBool = AtomicBool::new(false);

/// Select the release build profile (wrapping arithmetic) for subsequent
/// compilation. Defaults to debug (checked arithmetic).
pub fn set_release_profile(on: bool) {
    RELEASE_PROFILE.store(on, Ordering::Relaxed);
}

fn is_release() -> bool {
    RELEASE_PROFILE.load(Ordering::Relaxed)
}

/// A captured GC safepoint: `(function, call code offset, frame_to_fp, live
/// reference SP offsets)`. The collector needs the absolute pc (function base
/// address + code offset), which is only known after JIT finalization or, for
/// object output, at program load time.
type Safepoint = (FuncId, u32, u32, Vec<u32>);

fn collect_direct_call_counts(hir: &Hir) -> HashMap<DefId, usize> {
    let mut out = HashMap::new();
    for body in hir.bodies.values() {
        collect_direct_calls_block(&body.block, &mut out);
    }
    out
}

fn collect_direct_calls_block(block: &hir::Block, out: &mut HashMap<DefId, usize>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            hir::StmtKind::Let { init, .. } => collect_direct_calls_expr(init, out),
            hir::StmtKind::Assign { target, value } => {
                collect_direct_calls_expr(target, out);
                collect_direct_calls_expr(value, out);
            }
            hir::StmtKind::Expr(e) => collect_direct_calls_expr(e, out),
            hir::StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &block.trailing {
        collect_direct_calls_expr(t, out);
    }
}

fn collect_direct_calls_expr(expr: &hir::Expr, out: &mut HashMap<DefId, usize>) {
    use hir::ExprKind as K;
    match &expr.kind {
        K::Call {
            kind: hir::CallKind::Direct { def, .. },
            args,
            ..
        } => {
            *out.entry(*def).or_insert(0) += 1;
            for arg in args {
                collect_direct_calls_expr(arg, out);
            }
        }
        K::Call { kind, args, .. } => {
            if let hir::CallKind::Closure { callee } = kind {
                collect_direct_calls_expr(callee, out);
            }
            for arg in args {
                collect_direct_calls_expr(arg, out);
            }
        }
        K::Unary { operand, .. }
        | K::Cast { expr: operand, .. }
        | K::Field {
            receiver: operand, ..
        }
        | K::TupleIndex {
            receiver: operand, ..
        }
        | K::Try { expr: operand, .. }
        | K::Await { expr: operand, .. }
        | K::Spawn { expr: operand, .. }
        | K::Ref(operand)
        | K::Deref(operand)
        | K::Adjust { expr: operand, .. }
        | K::Return(Some(operand))
        | K::Break(Some(operand)) => collect_direct_calls_expr(operand, out),
        K::Binary { left, right, .. } => {
            collect_direct_calls_expr(left, out);
            collect_direct_calls_expr(right, out);
        }
        K::Tuple(xs) | K::List(xs) => {
            for x in xs {
                collect_direct_calls_expr(x, out);
            }
        }
        K::Map(items) => {
            for item in items {
                match item {
                    hir::MapEntry::Kv { key, value } => {
                        collect_direct_calls_expr(key, out);
                        collect_direct_calls_expr(value, out);
                    }
                    hir::MapEntry::Spread(x) => collect_direct_calls_expr(x, out),
                }
            }
        }
        K::Struct { fields, spread, .. } => {
            for field in fields {
                collect_direct_calls_expr(&field.value, out);
            }
            if let Some(x) = spread {
                collect_direct_calls_expr(x, out);
            }
        }
        K::Str(parts) => {
            for part in parts {
                if let hir::StrPart::Interp { expr, .. } = part {
                    collect_direct_calls_expr(expr, out);
                }
            }
        }
        K::Intrinsic { args, .. } => {
            for arg in args {
                collect_direct_calls_expr(arg, out);
            }
        }
        K::Index { receiver, index } => {
            collect_direct_calls_expr(receiver, out);
            collect_direct_calls_expr(index, out);
        }
        K::If {
            cond,
            then_block,
            else_branch,
        } => {
            collect_direct_calls_expr(cond, out);
            collect_direct_calls_block(then_block, out);
            if let Some(x) = else_branch {
                collect_direct_calls_expr(x, out);
            }
        }
        K::Match { scrutinee, arms } => {
            collect_direct_calls_expr(scrutinee, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_direct_calls_expr(guard, out);
                }
                collect_direct_calls_expr(&arm.body, out);
            }
        }
        K::Block(b) | K::Loop(b) => collect_direct_calls_block(b, out),
        K::While { cond, body } => {
            collect_direct_calls_expr(cond, out);
            collect_direct_calls_block(body, out);
        }
        K::For { iter, body, .. } => {
            collect_direct_calls_expr(iter, out);
            collect_direct_calls_block(body, out);
        }
        K::Closure { body, .. } => collect_direct_calls_expr(body, out),
        K::AsyncBlock { body, .. } => collect_direct_calls_block(body, out),
        _ => {}
    }
}

/// Build a target ISA for `triple`. `pic` selects position-independent code:
/// the JIT loads code at a fixed address (`false`), but object output is linked
/// into a PIE executable, so it must be position-independent (`true`).
fn make_isa(triple: target_lexicon::Triple, pic: bool) -> cranelift_codegen::isa::OwnedTargetIsa {
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").unwrap();
    // Debug builds keep Cranelift IR close to the source for observability.
    // Release builds ask Cranelift to run its normal speed-oriented pipeline:
    // instruction selection/combining, local simplification, CFG cleanup, and
    // target-specific late optimizations. This is backend policy, not language
    // semantics; overflow behavior is still governed separately by
    // `RELEASE_PROFILE`.
    flags
        .set("opt_level", if is_release() { "speed" } else { "none" })
        .unwrap();
    flags
        .set("is_pic", if pic { "true" } else { "false" })
        .unwrap();
    // Frame pointers let the GC walk the stack to find precise roots.
    flags.set("preserve_frame_pointers", "true").unwrap();
    cranelift_codegen::isa::lookup(triple)
        .expect("target ISA")
        .finish(settings::Flags::new(flags))
        .expect("ISA flags")
}

/// Drive codegen from a caller-selected seed set. Calls, vtables, finalizers,
/// closures, async poll/drop functions, and generic instances are still declared
/// lazily as reachable code demands them; the filter only controls the initial
/// roots. This is the backend's coarse dead-code elimination boundary.
fn run_codegen_with_filter<M: Module>(
    analysis: &Analysis,
    hir: &Hir,
    module: &mut M,
    include_seed: impl Fn(DefId) -> bool,
) -> CgResult<(
    HashMap<String, FuncId>,
    Vec<Safepoint>,
    Vec<(i64, FuncId)>,
    Vec<(FuncId, u32, u32)>,
    HashMap<FuncId, u32>,
)> {
    // Pre-compute the set of locals captured by some closure. A LocalId in
    // this set is cell-backed wherever it is bound (`docs/09` §7).
    // Locals captured by some closure / `async` block — collected by walking the
    // HIR (was the `closures` / `async_blocks` side tables).
    let captured_locals: HashSet<LocalId> = hir.captured_locals();
    let direct_call_counts = collect_direct_call_counts(hir);
    let mut cg = Codegen {
        analysis,
        hir,
        module,
        funcs: HashMap::new(),
        by_name: HashMap::new(),
        worklist: Vec::new(),
        closures: Vec::new(),
        async_jobs: Vec::new(),
        clone_thunks: Vec::new(),
        safepoints: Vec::new(),
        captured_locals,
        direct_call_counts,
        clif: None,
        line_info: Vec::new(),
        func_len: HashMap::new(),
    };
    cg.seed_with(include_seed)?;
    cg.run()?;
    let drops = cg.collect_drops();
    Ok((cg.by_name, cg.safepoints, drops, cg.line_info, cg.func_len))
}

/// Compile `analysis` and return the generated Cranelift IR of every function
/// as deterministic text (`--emit=clif`). Functions are emitted in code-
/// generation order (entry points first, then their callees / lifted closures /
/// async `poll`s as the worklist drains). The module is never finalized — only
/// the IR text is collected, so this is side-effect-free.
pub fn compile_clif(analysis: &Analysis) -> CgResult<String> {
    compile_clif_with_filter(analysis, |_| true)
}

/// Compile only definitions selected as emit roots and return their generated
/// Cranelift IR plus any callees they pull in. This keeps debug `emit clif`
/// focused on user source while preserving lazy codegen for imported stdlib
/// functions that the user code actually calls.
pub fn compile_clif_for_files(analysis: &Analysis, file_count: usize) -> CgResult<String> {
    compile_clif_with_filter(analysis, |def| {
        analysis.program.def(def).span.file.0 < file_count as u32
    })
}

fn compile_clif_with_filter(
    analysis: &Analysis,
    include_seed: impl Fn(DefId) -> bool,
) -> CgResult<String> {
    let hir = &analysis.hir;
    let isa = make_isa(target_lexicon::Triple::host(), false);
    let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    let mut module = JITModule::new(builder);

    // Locals captured by some closure / `async` block — collected by walking the
    // HIR (was the `closures` / `async_blocks` side tables).
    let captured_locals: HashSet<LocalId> = hir.captured_locals();
    let direct_call_counts = collect_direct_call_counts(hir);
    let mut cg = Codegen {
        analysis,
        hir,
        module: &mut module,
        funcs: HashMap::new(),
        by_name: HashMap::new(),
        worklist: Vec::new(),
        closures: Vec::new(),
        async_jobs: Vec::new(),
        clone_thunks: Vec::new(),
        safepoints: Vec::new(),
        captured_locals,
        direct_call_counts,
        clif: Some(Vec::new()),
        line_info: Vec::new(),
        func_len: HashMap::new(),
    };
    cg.seed_with(&include_seed)?;
    cg.run()?;
    Ok(cg.clif.take().unwrap_or_default().join("\n"))
}

/// Compile every lowerable function in `analysis` and return a runnable [`Jit`].
/// `dlopen` each library named by `@Link(lib = "…")` (`docs/19` §13) so its
/// symbols become visible to the JIT's `dlsym(RTLD_DEFAULT)` lookup. (Native
/// builds instead pass `-l<lib>` to the linker — see the CLI.)
fn dlopen_link_libs(hir: &Hir) {
    if hir.link_libs.is_empty() {
        return;
    }
    // RTLD_NOW (2) | RTLD_GLOBAL (8) — resolve now, export symbols process-wide.
    const FLAGS: i32 = 2 | 8;
    unsafe extern "C" {
        fn dlopen(
            filename: *const std::os::raw::c_char,
            flag: std::os::raw::c_int,
        ) -> *mut std::os::raw::c_void;
    }
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    for lib in &hir.link_libs {
        let name = format!("lib{lib}.{ext}\0");
        // SAFETY: a NUL-terminated path; a failed open just leaves the symbol
        // unresolved (the call will error later) — best-effort, like the linker.
        unsafe {
            dlopen(name.as_ptr() as *const std::os::raw::c_char, FLAGS);
        }
    }
}

/// JIT-compile and return a runnable [`Jit`], walking the AST per function.
pub fn compile(analysis: &Analysis) -> CgResult<Jit> {
    compile_jit(analysis, &[])
}

/// As [`compile`], but additionally registers `extra` `(symbol, address)`
/// pairs into the JIT's symbol table before lowering. This is the hook the
/// procedural-macro engine (`crates/macros`) uses to resolve the prelude's
/// `extern function __ast_* / __mctx_*` declarations to its host functions, so
/// a macro JIT can call back into the compiler's AST arena (`docs/22`).
///
/// # Safety
/// Each address must point to a live `extern "C"` function whose signature
/// matches the corresponding extern declaration; it must outlive the returned
/// [`Jit`].
pub fn compile_with_symbols(analysis: &Analysis, extra: &[(&str, *const u8)]) -> CgResult<Jit> {
    compile_jit(analysis, extra)
}

/// JIT-compile from only the program's `main` root. Reachable callees are still
/// discovered through monomorphization while the worklist drains. This is the
/// path ordinary `run` uses so imported stdlib and unused helpers do not become
/// executable code just because they exist in the analyzed program.
pub fn compile_entry(analysis: &Analysis) -> CgResult<Jit> {
    compile_jit_for_names(analysis, &["main"])
}

/// JIT-compile from a small set of exported source symbols. Used by isolated
/// test/bench children, where compiling every sibling body only adds work.
pub fn compile_jit_for_names(analysis: &Analysis, names: &[&str]) -> CgResult<Jit> {
    compile_jit_with_filter(analysis, &[], |def| {
        names
            .iter()
            .any(|name| analysis.program.def(def).name == *name)
    })
}

/// Alias for [`compile`] retained by the code-generation test suite. Code
/// generation always lowers from the typed HIR ([`gen_hir`]); the AST is no
/// longer walked, so this is identical to [`compile`].
pub fn compile_hir(analysis: &Analysis) -> CgResult<Jit> {
    compile(analysis)
}

/// The number of lowerable function bodies (every body is HIR-lowered). Retained
/// by tests that assert the HIR code path is exercised.
pub fn hir_eligible_fns(analysis: &Analysis) -> usize {
    analysis.hir.bodies.len()
}

fn compile_jit(analysis: &Analysis, extra_symbols: &[(&str, *const u8)]) -> CgResult<Jit> {
    compile_jit_with_filter(analysis, extra_symbols, |_| true)
}

fn compile_jit_with_filter(
    analysis: &Analysis,
    extra_symbols: &[(&str, *const u8)],
    include_seed: impl Fn(DefId) -> bool,
) -> CgResult<Jit> {
    let hir = &analysis.hir;
    dlopen_link_libs(hir);
    let isa = make_isa(target_lexicon::Triple::host(), false);
    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    register_runtime_symbols(&mut builder);
    for (name, addr) in extra_symbols {
        builder.symbol(*name, *addr);
    }
    let mut module = JITModule::new(builder);

    let (by_name, safepoints, drops, line_info, _func_len) =
        run_codegen_with_filter(analysis, hir, &mut module, include_seed)?;

    module.finalize_definitions().expect("finalize");

    // Register each call safepoint's precise stack map with the runtime, now
    // that function base addresses are known. `pc` is the call instruction.
    for (func_id, code_offset, frame_to_fp, offsets) in &safepoints {
        let base = module.get_finalized_function(*func_id) as usize;
        let pc = base + *code_offset as usize;
        unsafe {
            runtime::gc::lang_gc_register_safepoint(
                pc,
                *frame_to_fp,
                offsets.as_ptr(),
                offsets.len(),
            );
        }
    }

    // Register each `Drop` type's finalizer (`docs/16` §8).
    for (type_id, func_id) in &drops {
        let addr = module.get_finalized_function(*func_id);
        let f: runtime::gc::DropFn = unsafe { std::mem::transmute(addr) };
        unsafe { runtime::gc::lang_gc_register_drop(*type_id as u64, f) };
    }

    let main_is_async = main_is_async(analysis, hir);
    let pending_tid = Some(1000 + analysis.program.pending_def.index() as i64);
    Ok(Jit {
        module,
        funcs: by_name,
        pending_tid,
        main_is_async,
        line_info,
    })
}

/// Whether the user `main` is declared `async function` — its compiled symbol
/// returns a `Future<…>` box and must be driven by the runtime executor.
fn main_is_async(analysis: &Analysis, hir: &Hir) -> bool {
    analysis.program.defs.iter().enumerate().any(|(idx, d)| {
        d.name == "main"
            && matches!(d.kind, DefKind::Function)
            && hir
                .fn_sigs
                .get(&compiler::ids::DefId(idx as u32))
                .and_then(|s| s.async_output)
                .is_some()
    })
}

/// Compile `analysis` to a native relocatable object file at `out`, suitable
/// for linking against `libruntime.a` into a standalone executable.
///
/// Unlike the JIT, function load addresses are unknown at compile time, so GC
/// safepoints cannot be pre-registered. Instead the emitted C entry point
/// (`main`) registers each safepoint at startup — it takes each function's
/// runtime address (`func_addr`), adds the recorded code offset to form the
/// precise pc, and calls `lang_gc_register_safepoint` — then enables the
/// collector and calls the program's `main`.
pub fn compile_object(analysis: &Analysis, out: &Path, src: &str, src_name: &str) -> CgResult<()> {
    let mut triple = target_lexicon::Triple::host();
    // A bare `*-apple-darwin` host triple yields a Mach-O object with an
    // "unknown" platform that the linker rejects; promote it to `macosx` with a
    // deployment target so a proper `LC_BUILD_VERSION` is emitted.
    if let target_lexicon::OperatingSystem::Darwin(v) = triple.operating_system {
        let dt = v.unwrap_or(target_lexicon::DeploymentTarget {
            major: 11,
            minor: 0,
            patch: 0,
        });
        triple.operating_system = target_lexicon::OperatingSystem::MacOSX(Some(dt));
    }
    let isa = make_isa(triple, true);
    let builder = ObjectBuilder::new(
        isa,
        "lang_program",
        cranelift_module::default_libcall_names(),
    )
    .expect("object builder");
    let mut module = ObjectModule::new(builder);

    // Symbol names are written verbatim; the `object` crate applies the
    // platform mangling (the leading `_` on Mach-O), so the bare `lang_*`
    // runtime names match `libruntime.a`'s exported symbols after linking.
    let hir = &analysis.hir;
    let (by_name, safepoints, drops, line_info, func_len) =
        run_codegen_with_filter(analysis, hir, &mut module, |def| {
            analysis.program.def(def).name == "main"
        })?;

    let user_main = *by_name
        .get("main")
        .ok_or_else(|| CodegenError::new(Span::dummy(), "no `main` function to build"))?;

    let main_async = main_is_async(analysis, hir);
    let pending_tid = 1000 + analysis.program.pending_def.index() as i64;
    emit_native_entry(
        &mut module,
        user_main,
        main_async,
        pending_tid,
        &safepoints,
        &drops,
    )?;

    // Attach DWARF `.debug_line`/`.debug_info` (source-level debug info) to the
    // object: a `gimli` line program over the captured per-function source-line
    // ranges, with function start addresses as `Address::Symbol` relocations.
    // Debug sections are non-allocated, so this never affects the loaded program.
    //
    // ELF places `.debug_*` sections directly; Mach-O uses `__debug_*` in the
    // `__DWARF` segment (handled in `emit_dwarf` by object format).
    let mut product = module.finish();
    if let Err(e) = dwarf::emit_dwarf(&mut product, &line_info, &func_len, src, src_name) {
        // Debug info is best-effort: a failure must not block the build.
        eprintln!("warning: could not emit DWARF debug info: {e}");
    }
    let bytes = product.emit().expect("emit object");
    std::fs::write(out, bytes)
        .map_err(|e| CodegenError::new(Span::dummy(), format!("write object: {e}")))?;
    Ok(())
}

/// Emit the program's C entry point (`main`) into `module`: register every GC
/// safepoint with the runtime (computing each precise pc from the function's
/// runtime address plus its code offset), enable the collector, then call the
/// language `main`.
fn emit_native_entry<M: Module>(
    module: &mut M,
    user_main: FuncId,
    main_is_async: bool,
    pending_tid: i64,
    safepoints: &[Safepoint],
    drops: &[(i64, FuncId)],
) -> CgResult<()> {
    // Emit each safepoint's SP-offset array as a read-only data object and
    // collect (data id, frame_to_fp, len, func id, code offset) for the entry.
    struct SpData {
        offsets: DataId,
        n: usize,
        frame_to_fp: u32,
        func: FuncId,
        code_offset: u32,
    }
    let mut sp_data = Vec::new();
    for (i, (func, code_offset, frame_to_fp, offsets)) in safepoints.iter().enumerate() {
        let mut bytes = Vec::with_capacity(offsets.len() * 4);
        for off in offsets {
            bytes.extend_from_slice(&off.to_le_bytes());
        }
        let name = format!("lang_gc_sp_offsets_{i}");
        let data_id = module
            .declare_data(&name, Linkage::Local, false, false)
            .map_err(|e| CodegenError::new(Span::dummy(), format!("declare sp data: {e}")))?;
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        desc.set_align(4); // the array is read back as `*const u32`
        module
            .define_data(data_id, &desc)
            .map_err(|e| CodegenError::new(Span::dummy(), format!("define sp data: {e}")))?;
        sp_data.push(SpData {
            offsets: data_id,
            n: offsets.len(),
            frame_to_fp: *frame_to_fp,
            func: *func,
            code_offset: *code_offset,
        });
    }

    // `lang_gc_register_safepoint(pc: usize, frame_to_fp: u32, offsets: *const u32, n: usize)`
    let mut reg_sig = module.make_signature();
    reg_sig.params.push(AbiParam::new(PTR)); // pc
    reg_sig.params.push(AbiParam::new(types::I32)); // frame_to_fp
    reg_sig.params.push(AbiParam::new(PTR)); // offsets
    reg_sig.params.push(AbiParam::new(PTR)); // n
    let reg_id = module
        .declare_function("lang_gc_register_safepoint", Linkage::Import, &reg_sig)
        .expect("declare register_safepoint");

    // `lang_gc_register_drop(type_id: u64, f: fn(*mut u8))` — register finalizers.
    let mut drop_sig = module.make_signature();
    drop_sig.params.push(AbiParam::new(types::I64)); // type_id
    drop_sig.params.push(AbiParam::new(PTR)); // drop fn ptr
    let drop_reg_id = module
        .declare_function("lang_gc_register_drop", Linkage::Import, &drop_sig)
        .expect("declare register_drop");

    // `lang_gc_set_enabled(on: bool)` — `bool` is a byte in the C ABI.
    let mut en_sig = module.make_signature();
    en_sig.params.push(AbiParam::new(types::I8));
    let en_id = module
        .declare_function("lang_gc_set_enabled", Linkage::Import, &en_sig)
        .expect("declare set_enabled");

    // The executor entry — used internally by the program entry when `main`
    // is an `async function` (`docs/21`): drive the root future to completion.
    let block_on_id = if main_is_async {
        let mut bo_sig = module.make_signature();
        bo_sig.params.push(AbiParam::new(PTR)); // fut
        bo_sig.params.push(AbiParam::new(types::I64)); // pending_tid
        bo_sig.returns.push(AbiParam::new(types::I64));
        Some(
            module
                .declare_function("lang_block_on", Linkage::Import, &bo_sig)
                .expect("declare lang_block_on"),
        )
    } else {
        None
    };

    // The C entry: `int main(void)`. The `object` crate mangles this to `_main`
    // on Mach-O, which is the program entry the C runtime startup calls.
    let mut main_sig = module.make_signature();
    main_sig.returns.push(AbiParam::new(types::I32));
    let main_id = module
        .declare_function("main", Linkage::Export, &main_sig)
        .expect("declare entry main");

    let mut ctx = module.make_context();
    ctx.func.signature = main_sig;
    let mut fctx = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let entry = b.create_block();
        b.switch_to_block(entry);
        b.seal_block(entry);

        let reg_ref = module.declare_func_in_func(reg_id, b.func);
        for sp in &sp_data {
            let fref = module.declare_func_in_func(sp.func, b.func);
            let faddr = b.ins().func_addr(PTR, fref);
            let pc = b.ins().iadd_imm(faddr, sp.code_offset as i64);
            let frame_to_fp = b.ins().iconst(types::I32, sp.frame_to_fp as i64);
            let gv = module.declare_data_in_func(sp.offsets, b.func);
            let optr = b.ins().global_value(PTR, gv);
            let n = b.ins().iconst(PTR, sp.n as i64);
            b.ins().call(reg_ref, &[pc, frame_to_fp, optr, n]);
        }

        // Register each `Drop` type's finalizer before enabling the collector.
        let drop_reg_ref = module.declare_func_in_func(drop_reg_id, b.func);
        for (type_id, func) in drops {
            let fref = module.declare_func_in_func(*func, b.func);
            let faddr = b.ins().func_addr(PTR, fref);
            let tid = b.ins().iconst(types::I64, *type_id);
            b.ins().call(drop_reg_ref, &[tid, faddr]);
        }

        let main_ref = module.declare_func_in_func(user_main, b.func);
        if main_is_async {
            // Async `main`: calling the symbol just builds the root future.
            // Do that before enabling GC: until `lang_block_on` starts, the
            // returned future is only a native-entry temporary and is not yet
            // pinned or reachable from a generated stack map. No user body has
            // run at this point; `lang_block_on` pins the future before polling.
            let call = b.ins().call(main_ref, &[]);
            let fut = b.inst_results(call)[0];
            let en_ref = module.declare_func_in_func(en_id, b.func);
            let on = b.ins().iconst(types::I8, 1);
            b.ins().call(en_ref, &[on]);
            let ptid = b.ins().iconst(types::I64, pending_tid);
            let bo_ref = module.declare_func_in_func(
                block_on_id.expect("block_on declared when main is async"),
                b.func,
            );
            b.ins().call(bo_ref, &[fut, ptid]);
        } else {
            let en_ref = module.declare_func_in_func(en_id, b.func);
            let on = b.ins().iconst(types::I8, 1);
            b.ins().call(en_ref, &[on]);
            b.ins().call(main_ref, &[]);
        }

        let zero = b.ins().iconst(types::I32, 0);
        b.ins().return_(&[zero]);
        b.finalize();
    }
    module
        .define_function(main_id, &mut ctx)
        .map_err(|e| CodegenError::new(Span::dummy(), format!("define entry main: {e}")))?;
    module.clear_context(&mut ctx);
    Ok(())
}

/// A concrete instantiation: a function/method def plus its generic arguments
/// (empty for non-generic). Monomorphization compiles one Cranelift function
/// per distinct instance.
pub(crate) type Instance = (DefId, Vec<Ty>);

/// A lifted closure function awaiting code generation: its Cranelift id, the
/// closure's analysis, typed HIR body, and the enclosing instance's
/// substitution.
struct ClosureJob {
    func_id: FuncId,
    info: compiler::sema::results::ClosureInfo,
    body: compiler::hir::Expr,
    subst: HashMap<DefId, Ty>,
    span: Span,
    /// Whether captures are by value (a `Thread.spawn` snapshot; `docs/20` §6)
    /// rather than by reference (an ordinary closure; `docs/09` §7). Decides how
    /// the lifted body binds each capture: a direct value from the env slot, or
    /// a shared cell pointer.
    by_value: bool,
}

/// A bare `async { … }` block or `async` closure awaiting `poll`-function
/// generation: its Cranelift id, analysis, typed HIR body, substitution, and
/// the future `Output` type.
struct AsyncJob {
    poll_fid: FuncId,
    drop_fid: FuncId,
    info: compiler::sema::results::AsyncInfo,
    body: compiler::hir::Expr,
    subst: HashMap<DefId, Ty>,
    span: Span,
    out: Ty,
    /// Channel endpoints this future **owns** (`docs/20` §1/§2): a `Thread.spawn`
    /// async worker captures its `Sender`/`Receiver` snapshot, so the endpoint
    /// must outlive the closure that merely *built* the future and be released
    /// when the future *completes*, not when that closure returns. Each entry is
    /// `(captured local, endpoint type, is_sender)`; `define_closure` populates it
    /// (and suppresses the building closure's own release) for an async worker.
    /// The poll releases each on its single completion path. Empty for an
    /// ordinary `async { … }` block, which borrows its captures and never owns
    /// them.
    owned_endpoints: Vec<(LocalId, Ty, bool)>,
    /// Captures that a spawned async worker future must snapshot as values even
    /// if the generic closure analysis marked the local cell-backed for nested
    /// captures. Used for channel endpoints transferred into the future.
    value_capture_locals: HashSet<LocalId>,
}

/// A clone-out thunk awaiting code generation (`docs/20` §4). A
/// `extern "C" fn(R) -> R` that deep-clones its argument (via `gen_clone_value`),
/// invoked by the `Shared` lock future to detach the body's returned value from
/// the cell *while the lock is still held*. One is generated per distinct `R`
/// type returned from a `lock`/`try_lock` body whose `R` is a managed pointer.
struct CloneThunkJob {
    func_id: FuncId,
    r_ty: Ty,
    subst: HashMap<DefId, Ty>,
    span: Span,
}

/// Per-async-`poll`-function state-machine context (`docs/21`): where the state
/// struct lives, the slots that hold each saved local, and the suspend/resume
/// blocks for each `await` site.
struct AsyncCtx {
    /// The state struct pointer (the `poll` function's `self`).
    self_val: Value,
    /// The `Context` pointer passed to `poll` (forwarded to inner `poll`s).
    ctx_val: Value,
    /// Byte offset of the suspended-inner-future slot within the state struct.
    inner_off: i32,
    /// Every local with a runtime value, and its state-struct slot offset —
    /// saved at each suspend point and restored on resume.
    save_locals: Vec<(LocalId, i32)>,
    /// `await` keyword span → (state discriminant, poll block, resume block).
    awaits: HashMap<
        Span,
        (
            i64,
            cranelift_codegen::ir::Block,
            cranelift_codegen::ir::Block,
        ),
    >,
    /// Shared block that builds a `Pending` result and returns it.
    pending_block: cranelift_codegen::ir::Block,
    /// Sync `for` loop `iter.span` → `(primary, secondary, index)` state-struct
    /// slots, for loops whose body awaits (so their iteration state survives a
    /// suspend). Empty when no such loop exists.
    for_slots: HashMap<Span, (i32, i32, i32)>,
}

struct Codegen<'a, M: Module> {
    analysis: &'a Analysis,
    /// The typed, resolved, desugared HIR the checker produced — the sole source
    /// of function bodies and the program facts code generation consumes.
    hir: &'a Hir,
    module: &'a mut M,
    /// Compiled instances and their Cranelift ids.
    funcs: HashMap<Instance, FuncId>,
    /// Language function name → id, for external lookup (`main`, tests).
    by_name: HashMap<String, FuncId>,
    /// Instances declared but not yet defined.
    worklist: Vec<Instance>,
    /// Lifted closure functions declared but not yet defined.
    closures: Vec<ClosureJob>,
    /// Async block/closure `poll` functions declared but not yet defined.
    async_jobs: Vec<AsyncJob>,
    /// `Shared` lock-body clone-out thunks declared but not yet defined.
    clone_thunks: Vec<CloneThunkJob>,
    /// Captured GC safepoints: `(func, call code offset, frame_to_fp, ref SP
    /// offsets)`, registered with the runtime after linking.
    safepoints: Vec<(FuncId, u32, u32, Vec<u32>)>,
    /// LocalIds captured by some closure anywhere in the program. Captured
    /// locals are stored in heap-allocated **cells** (`docs/09` §7: "every
    /// captured variable is captured by reference"): outer accesses and the
    /// closure body share the same cell, so primitive mutations propagate.
    /// LocalIds are program-wide unique, so a global set is enough — a binding
    /// in any function consults this set at its declaration site.
    captured_locals: HashSet<LocalId>,
    /// Whole-HIR direct call counts by callee definition. The HIR inliner uses
    /// this as a conservative call-graph signal: expression-bodied helpers called
    /// once are preferred candidates, with a tiny-size fallback.
    direct_call_counts: HashMap<DefId, usize>,
    /// When `Some`, every function's generated Cranelift IR text is appended
    /// here (debug observability: `--emit=clif`). `None` on normal builds.
    clif: Option<Vec<String>>,
    /// Per-function source-line provenance for debug info: `(func, code byte
    /// offset, source byte offset)` from each compiled function's `MachSrcLoc`
    /// ranges (set via `set_srcloc`). The basis for DWARF `.debug_line`.
    line_info: Vec<(FuncId, u32, u32)>,
    /// Each compiled function's total code length (for DWARF `end_sequence`).
    func_len: HashMap<FuncId, u32>,
}

impl<'a, M: Module> Codegen<'a, M> {
    /// Collect `(type_id, drop FuncId)` for every type with a `Drop` impl
    /// (`docs/16` §8), to register as GC finalizers.
    ///
    /// * **Non-generic** `Drop` types use the stable per-`def` id
    ///   `1000 + type_def.index()`; `seed` already compiled their `drop` methods
    ///   as `[]`-instances, so we look them up directly.
    /// * **Generic** `Drop` types (`extend<T> S<T>: Drop`) need one finalizer
    ///   *per monomorphization* — every `S<int>` shares a `def` but compiles its
    ///   own `drop` body. Each such instance was instantiated and keyed by its
    ///   `FuncId` at the allocation site (see `drop_type_id`), so we scan the
    ///   instance table for every compiled `drop`-method instance of a generic
    ///   `Drop` impl and register it under the matching per-instance id.
    fn collect_drops(&self) -> Vec<(i64, FuncId)> {
        let drop_def = self.analysis.program.drop_def;
        if drop_def == DefId(0) {
            return Vec::new();
        }
        let prog = &self.analysis.program;
        let mut out = Vec::new();
        // `drop` methods of generic `Drop` extends — registered per instance.
        let mut generic_drop_methods: HashSet<DefId> = HashSet::new();
        for (&(type_def, iface_def), &extend_def) in &self.hir.iface_impls {
            if iface_def != drop_def {
                continue;
            }
            let drop_method = (0..prog.defs.len() as u32).map(DefId).find(|&d| {
                let def = prog.def(d);
                def.kind == DefKind::ExtendMethod
                    && def.parent == Some(extend_def)
                    && def.name == "drop"
            });
            let Some(dm) = drop_method else { continue };
            if prog.def(extend_def).generics.is_empty() {
                // Non-generic: the single `[]`-instance keyed by the type's def.
                if let Some(&fid) = self.funcs.get(&(dm, Vec::new())) {
                    out.push((1000 + type_def.index() as i64, fid));
                }
            } else {
                generic_drop_methods.insert(dm);
            }
        }
        // Every compiled instance of a generic `Drop` method, keyed by `FuncId`.
        for (&(def, ref args), &fid) in &self.funcs {
            if !args.is_empty() && generic_drop_methods.contains(&def) {
                out.push((GENERIC_DROP_TID_BASE + fid.as_u32() as i64, fid));
            }
        }
        // Deterministic order (the maps iterated above are unordered); ids are
        // unique, so a sort by id is a stable total order for reproducible output.
        out.sort_by_key(|&(tid, _)| tid);
        out
    }

    fn seed_with(&mut self, include_def: impl Fn(DefId) -> bool) -> CgResult<()> {
        for (i, def) in self.analysis.program.defs.iter().enumerate() {
            let did = DefId(i as u32);
            if !include_def(did) {
                continue;
            }
            // `test "name" { … }` bodies are zero-arg unit functions compiled like
            // any non-generic function so `otter_fusion test` can call them.
            if def.kind == DefKind::Test {
                if let Some(fid) = declare_instance(
                    self.module,
                    &mut self.funcs,
                    &mut self.worklist,
                    self.analysis,
                    did,
                    Vec::new(),
                )? {
                    self.by_name.entry(def.name.clone()).or_insert(fid);
                }
                continue;
            }
            if !matches!(def.kind, DefKind::Function | DefKind::ExtendMethod)
                || !def.generics.is_empty()
            {
                continue;
            }
            // A method's own generics may be empty while its enclosing `extend`
            // is generic (`extend<T> S<T> { function m(self) {…} }`); its body
            // still references the extend's `T`, so it must be monomorphized per
            // instantiation from call sites, never seeded with an empty subst.
            if def.kind == DefKind::ExtendMethod {
                if let Some(parent) = def.parent {
                    if !self.analysis.program.def(parent).generics.is_empty() {
                        continue;
                    }
                }
            }
            // The `core:compiler` macro-surface methods (`docs/22`) call host
            // externs that only exist inside the macro JIT; compile them lazily
            // (when a macro calls them) rather than seeding them into every
            // program — otherwise native object output gets unresolved symbols.
            if self
                .analysis
                .program
                .is_macro_surface_method(DefId(i as u32))
            {
                continue;
            }
            let Some(ItemKind::Function(f)) = &def.item else {
                continue;
            };
            if f.body.is_none() {
                continue;
            }
            if let Some(fid) = declare_instance(
                self.module,
                &mut self.funcs,
                &mut self.worklist,
                self.analysis,
                did,
                Vec::new(),
            )? {
                self.by_name.entry(def.name.clone()).or_insert(fid);
            }
        }
        Ok(())
    }

    /// Define every instance, discovering new ones (via generic calls) as it
    /// goes, until the worklist is empty.
    fn run(&mut self) -> CgResult<()> {
        loop {
            while let Some(inst) = self.worklist.pop() {
                self.define_instance(inst)?;
            }
            if let Some(job) = self.closures.pop() {
                self.define_closure(job)?;
                continue;
            }
            if let Some(job) = self.clone_thunks.pop() {
                self.define_clone_thunk(job)?;
                continue;
            }
            match self.async_jobs.pop() {
                Some(job) => self.define_async_job(job)?,
                None => break,
            }
        }
        Ok(())
    }

    /// Append a function's generated Cranelift IR to the `--emit=clif` buffer
    /// (only when collecting). `label` is a readable header (the source symbol).
    fn record_clif(&mut self, label: &str, ctx: &cranelift_codegen::Context) {
        if let Some(buf) = &mut self.clif {
            buf.push(format!("; {label}\n{}", ctx.func.display()));
        }
    }

    fn define_instance(&mut self, inst: Instance) -> CgResult<()> {
        let (def, args) = inst;
        let func_id = self.funcs[&(def, args.clone())];
        // A free/extend function (with a body) or a `test` body (always present);
        // anything else (extern, struct, …) has no instance to define.
        let item = self.analysis.program.def(def).item.clone();
        let body_present = match &item {
            Some(ItemKind::Function(f)) => f.body.is_some(),
            Some(ItemKind::Test(_)) => true,
            _ => return Ok(()),
        };
        // An async function's body yields the future `Output`, not a `Future`;
        // it lowers to a `Future` state machine (`docs/21`): the function named
        // `func_id` becomes a *constructor* that allocates the machine, and a
        // separate `poll` function runs the body. (Tests are never async.)
        if let Some(out) = self.hir.fn_sigs.get(&def).and_then(|s| s.async_output) {
            if !body_present {
                return Ok(());
            }
            let hir = self.hir;
            let hb = hir.bodies.get(&def).ok_or_else(|| {
                CodegenError::new(
                    self.analysis.program.def(def).span,
                    "async function has no HIR body",
                )
            })?;
            return self.define_async_fn(def, args, func_id, BodyView(&hb.block), out);
        }
        if !body_present {
            return Ok(());
        }

        let subst = build_subst(self.analysis, def, &args);
        let mut ctx = self.module.make_context();
        ctx.func.signature =
            signature_of(self.module, self.analysis, def, &subst)?.expect("declared sig");
        let mut fctx = FunctionBuilderContext::new();

        let fsig = self.hir.fn_sigs.get(&def);
        let ret_ty = fsig.map(|s| s.ret).unwrap_or(self.analysis.tcx.null);
        let param_locals: Vec<LocalId> = fsig
            .map(|s| s.params.iter().map(|(l, _)| *l).collect())
            .unwrap_or_default();

        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let param_vals: Vec<Value> = b.block_params(entry).to_vec();

            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst,
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty,
                    async_out: None,
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                for (i, local) in param_locals.iter().enumerate() {
                    let ty = fg.cx.analysis.hir.local_ty(*local).unwrap();
                    let ct = fg.cx_clty(ty).expect("param clty");
                    fg.bind_local(*local, ct, param_vals[i]);
                }
                // Codegen walks the typed, desugared HIR body (`gen_hir`); the
                // params bound above and `emit_return` below are shared.
                let hir = fg.cx.hir;
                let hb = hir.bodies.get(&def).ok_or_else(|| {
                    CodegenError::new(
                        fg.cx.analysis.program.def(def).span,
                        "function has no HIR body",
                    )
                })?;
                fg.prepare_stack_struct_locals(&hb.block);
                let val = fg.h_block(&hb.block)?;
                let val = fg.rc_return_value(hb.block.trailing.as_deref(), val);
                fg.endpoint_return_value(hb.block.trailing.as_deref(), val)?;
                fg.emit_return(val)?;
            }
            b.seal_all_blocks();
            b.finalize();
        }

        let clif_label = self.analysis.program.def(def).name.clone();
        self.record_clif(&clif_label, &ctx);
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                CodegenError::new(self.analysis.program.def(def).span, format!("define: {e}"))
            })?;

        // Capture this function's GC safepoints (precise root scan) and its
        // source-line provenance (debug info).
        self.capture_safepoints(func_id, &ctx);
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Define a lifted closure function: `(env, params…) -> ret`. Captured
    /// locals are loaded from the environment; parameters come from the block.
    fn define_closure(&mut self, job: ClosureJob) -> CgResult<()> {
        let ClosureJob {
            func_id,
            info,
            body,
            subst,
            span,
            by_value,
        } = job;
        let mut ctx = self.module.make_context();
        // Signature: env pointer, then each (substituted) parameter.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        for (_, ty) in &info.params {
            let ct = clty_subst(self.analysis, *ty, &subst)
                .ok_or_else(|| CodegenError::new(span, "closure parameter is zero-sized"))?;
            sig.params.push(AbiParam::new(ct));
        }
        let ret_ty = info.ret;
        if let Some(rc) = clty_subst(self.analysis, ret_ty, &subst) {
            sig.returns.push(AbiParam::new(rc));
        }
        ctx.func.signature = sig;
        // An async worker's body is a single `async { … }` block (`docs/20` §1):
        // captured channel endpoints are owned by the *future* it builds, not by
        // this builder closure. Collect them here so their release can be
        // transferred to the future after the body is generated.
        let is_async_worker =
            by_value && matches!(&body.kind, compiler::hir::ExprKind::AsyncBlock { .. });
        let mut worker_endpoints: Vec<(LocalId, Ty, bool)> = Vec::new();
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let block_params: Vec<Value> = b.block_params(entry).to_vec();
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst,
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty,
                    async_out: None,
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                let env = block_params[0];
                // Captures live in the env after the function pointer (offset 8).
                // By reference (`docs/09` §7): the slot holds a *cell pointer*;
                // body reads/writes route through the cell, shared with the outer
                // scope. By value (`Thread.spawn`, `docs/20` §6): the slot holds
                // the captured *value* — bind it as a fresh local (a per-worker
                // copy), so the worker never shares mutable state with the spawner.
                for (k, (local, ty)) in info.captures.iter().enumerate() {
                    let ct = fg.cx_clty(*ty).expect("capture clty");
                    let off = (8 + k * 8) as i32;
                    if by_value {
                        let val = fg.b.ins().load(ct, MemFlags::trusted(), env, off);
                        fg.bind_local(*local, ct, val);
                        // A `Thread.spawn` worker that captured a channel endpoint
                        // OWNS that endpoint for the worker's lifetime; releasing
                        // it when the worker returns is the deterministic
                        // last-sender drop that closes the channel (`docs/20` §2,
                        // `docs/16` §8). Record the chan id to release on exit —
                        // unless the future owns it (async worker, transferred
                        // below).
                        if let Some(is_sender) = fg.channel_endpoint_kind(*ty) {
                            if is_async_worker {
                                worker_endpoints.push((*local, *ty, is_sender));
                            } else {
                                let chan = fg.emit_channel_id(val, *ty, span)?;
                                fg.endpoint_releases.push((chan, is_sender));
                            }
                        }
                    } else {
                        let cell_ptr = fg.b.ins().load(PTR, MemFlags::trusted(), env, off);
                        fg.bind_local_cell(*local, ct, cell_ptr);
                    }
                }
                for (i, (local, ty)) in info.params.iter().enumerate() {
                    let ct = fg.cx_clty(*ty).expect("param clty");
                    fg.bind_local(*local, ct, block_params[i + 1]);
                }
                if is_async_worker && !worker_endpoints.is_empty() {
                    fg.value_capture_locals = worker_endpoints
                        .iter()
                        .map(|(local, _, _)| *local)
                        .collect();
                }
                let val = fg.h_expr(&body)?;
                // A closure returning a borrowed `@RefCounted` value must hand the
                // caller an owned `+1` that survives `emit_return`'s release of the
                // closure's owned locals (`docs/16` §8.1) — same `+1`-return
                // convention as a top-level function.
                let ret_expr = match &body.kind {
                    compiler::hir::ExprKind::Block(blk) => blk.trailing.as_deref(),
                    _ => Some(&body),
                };
                let val = fg.rc_return_value(ret_expr, val);
                fg.endpoint_return_value(ret_expr, val)?;
                fg.emit_return(val)?;
            }
            // Transfer endpoint ownership to the async worker's future (the
            // `AsyncJob` just pushed while generating the body): it releases them
            // on completion instead of this builder closure (`docs/20` §1/§2).
            if is_async_worker && !worker_endpoints.is_empty() {
                if let Some(job) = self.async_jobs.last_mut() {
                    job.owned_endpoints = std::mem::take(&mut worker_endpoints);
                }
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.record_clif("<closure>", &ctx);
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| CodegenError::new(span, format!("define closure: {e}")))?;
        self.capture_safepoints(func_id, &ctx);
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Define a `Shared` lock-body clone-out thunk: `extern "C" fn(R) -> R` that
    /// deep-clones its argument via [`FnGen::gen_clone_value`] (`docs/20` §4). The
    /// lock future calls it to detach the body's returned value from the cell
    /// while the lock is still held. Only generated for a managed (pointer) `R`.
    fn define_clone_thunk(&mut self, job: CloneThunkJob) -> CgResult<()> {
        let CloneThunkJob {
            func_id,
            r_ty,
            subst,
            span,
        } = job;
        let mut ctx = self.module.make_context();
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        sig.returns.push(AbiParam::new(PTR));
        ctx.func.signature = sig;
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let arg = b.block_params(entry)[0];
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst,
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: r_ty,
                    async_out: None,
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                fg.mark_root(arg);
                let cloned = fg.gen_clone_value(arg, r_ty, span)?;
                fg.b.ins().return_(&[cloned]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.record_clif("<clone thunk>", &ctx);
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| CodegenError::new(span, format!("define clone thunk: {e}")))?;
        self.capture_safepoints(func_id, &ctx);
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Capture a just-compiled function's GC safepoints (the SP offsets of live
    /// references at each call) for the runtime's precise root scan.
    fn capture_safepoints(&mut self, func_id: FuncId, ctx: &cranelift_codegen::Context) {
        if let Some(cc) = ctx.compiled_code() {
            let frame_to_fp = cc
                .buffer
                .frame_layout()
                .map(|fl| fl.frame_to_fp_offset)
                .unwrap_or(0);
            for (code_offset, _span, map) in cc.buffer.user_stack_maps() {
                let offsets: Vec<u32> = map.entries().map(|(_, off)| off).collect();
                if !offsets.is_empty() {
                    self.safepoints
                        .push((func_id, *code_offset, frame_to_fp, offsets));
                }
            }
            // Source-line provenance for debug info: each `MachSrcLoc` maps a
            // code byte range to the source byte offset we set via `set_srcloc`.
            let mut any = false;
            for ml in cc.buffer.get_srclocs_sorted() {
                let src = ml.loc.bits();
                if src != 0 {
                    self.line_info.push((func_id, ml.start, src));
                    any = true;
                }
            }
            if any {
                self.func_len.insert(func_id, cc.code_info().total_size);
            }
        }
    }

    /// Lower an async function to a `Future` state machine (`docs/21` §3): a
    /// `poll(self, ctx) -> Ready<Out> | Pending` function runs the body, and the
    /// function's public symbol (`ctor_fid`) becomes a *constructor* that
    /// allocates the state struct (storing the arguments), wraps it in a
    /// `Future<Out>` interface-object box, and returns it. (This slice supports
    /// bodies with no `await`; `await` lowering is layered on next.)
    fn define_async_fn(
        &mut self,
        def: DefId,
        args: Vec<Ty>,
        ctor_fid: FuncId,
        body: BodyView,
        out: Ty,
    ) -> CgResult<()> {
        // A body containing `await` needs the full suspension state machine;
        // `await`-free bodies use the simpler path below.
        if body.has_await() {
            return self.define_async_fn_stateful(def, args, ctor_fid, body, out);
        }

        let subst = build_subst(self.analysis, def, &args);
        let param_locals: Vec<LocalId> = self
            .hir
            .fn_sigs
            .get(&def)
            .map(|s| s.params.iter().map(|(l, _)| *l).collect())
            .unwrap_or_default();

        // State struct layout: [state @0][param0 @8][param1 @16]… Managed params
        // are GC-traced. (Body locals live in the poll function's own frame in
        // this no-`await` slice; they move into the struct when `await` lands.)
        let mut param_cltys = Vec::with_capacity(param_locals.len());
        let mut ptr_offsets = Vec::new();
        for (i, local) in param_locals.iter().enumerate() {
            let ty = self
                .analysis
                .hir
                .local_ty(*local)
                .unwrap_or(self.analysis.tcx.error);
            let resolved = resolve_shallow(self.analysis, ty, &subst);
            let ct = clty_of(self.analysis, resolved);
            if is_managed_ptr(self.analysis, resolved) {
                ptr_offsets.push((8 + i * 8) as u32);
            }
            param_cltys.push(ct);
        }
        let state_size = (8 + param_locals.len() * 8) as u32;

        // Declare the poll function: (self: ptr, ctx: ptr) -> ptr.
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.returns.push(AbiParam::new(PTR));
        let poll_name = format!("{}$poll", mangle(self.analysis, def, &args));
        let poll_fid = self
            .module
            .declare_function(&poll_name, Linkage::Local, &poll_sig)
            .map_err(|e| {
                CodegenError::new(
                    self.analysis.program.def(def).span,
                    format!("declare poll: {e}"),
                )
            })?;

        // -- poll function body --------------------------------------------
        let mut ctx = self.module.make_context();
        ctx.func.signature = poll_sig;
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let self_val = b.block_params(entry)[0];
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: Some(out),
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                // The state struct holds GC roots and must stay live across the
                // body's allocations.
                fg.mark_root(self_val);
                // Load each argument from the state struct into its local.
                for (i, local) in param_locals.iter().enumerate() {
                    if let Some(ct) = param_cltys[i] {
                        let off = (8 + i * 8) as i32;
                        let loaded = fg.b.ins().load(ct, MemFlags::trusted(), self_val, off);
                        fg.bind_local(*local, ct, loaded);
                    }
                }
                let val = fg.gen_body_view(&body)?;
                let val = fg.rc_return_value(body.0.trailing.as_deref(), val);
                fg.endpoint_return_value(body.0.trailing.as_deref(), val)?;
                fg.emit_return(val)?;
            }
            b.seal_all_blocks();
            b.finalize();
        }
        let clif_label = format!("{}$poll", self.analysis.program.def(def).name);
        self.record_clif(&clif_label, &ctx);
        self.module
            .define_function(poll_fid, &mut ctx)
            .map_err(|e| {
                CodegenError::new(
                    self.analysis.program.def(def).span,
                    format!("define poll: {e}"),
                )
            })?;
        self.capture_safepoints(poll_fid, &ctx);
        self.module.clear_context(&mut ctx);

        // -- constructor body ----------------------------------------------
        let mut cctx = self.module.make_context();
        cctx.func.signature =
            signature_of(self.module, self.analysis, def, &subst)?.expect("ctor sig");
        let mut cfctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut cfctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let pvals: Vec<Value> = b.block_params(entry).to_vec();
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: None,
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                // Managed arguments must survive the state allocation (a
                // safepoint) before they are stored.
                for (i, v) in pvals.iter().enumerate() {
                    if ptr_offsets.contains(&((8 + i * 8) as u32)) {
                        fg.mark_root(*v);
                    }
                }
                let desc = fg.emit_descriptor(state_size, GC_KIND_PLAIN, &ptr_offsets);
                let state = fg
                    .call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
                    .expect("lang_alloc returns a pointer");
                let zero = fg.b.ins().iconst(types::I64, 0);
                fg.b.ins().store(MemFlags::trusted(), zero, state, 0);
                for (i, v) in pvals.iter().enumerate() {
                    fg.b.ins()
                        .store(MemFlags::trusted(), *v, state, (8 + i * 8) as i32);
                }
                let fut = fg.emit_future_box(poll_fid, None, state);
                fg.b.ins().return_(&[fut]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        let clif_label = format!("{}$ctor", self.analysis.program.def(def).name);
        self.record_clif(&clif_label, &cctx);
        self.module
            .define_function(ctor_fid, &mut cctx)
            .map_err(|e| {
                CodegenError::new(
                    self.analysis.program.def(def).span,
                    format!("define async ctor: {e}"),
                )
            })?;
        self.capture_safepoints(ctor_fid, &cctx);
        self.module.clear_context(&mut cctx);
        Ok(())
    }

    /// Build the body of an async `poll` function (the suspendable state
    /// machine, shared by async functions and `async { … }` blocks). `poll_fid`
    /// is already declared as `(self, ctx) -> ptr`. `entry_set` names the locals
    /// the constructor pre-stored (parameters / captures), loaded at the start;
    /// all other live locals are zeroed. `live` is the save/restore set with
    /// state-struct offsets. Dispatches on the saved state word to resume at the
    /// right `await`.
    fn build_stateful_poll(
        &mut self,
        poll_fid: FuncId,
        subst: &HashMap<DefId, Ty>,
        out: Ty,
        body: BodyView,
        entry_set: &HashSet<LocalId>,
        live: &[(LocalId, i32, ClType)],
        for_slots: &HashMap<Span, (i32, i32, i32)>,
        owned_endpoints: &[(LocalId, Ty, bool)],
        value_capture_locals: &HashSet<LocalId>,
        err_span: Span,
    ) -> CgResult<()> {
        let mut await_spans = Vec::new();
        body.scan_awaits(&mut await_spans);

        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.returns.push(AbiParam::new(PTR));
        let mut ctx = self.module.make_context();
        ctx.func.signature = poll_sig;
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let self_val = b.block_params(entry)[0];
            let ctx_val = b.block_params(entry)[1];
            let body_entry = b.create_block();
            let pending_block = b.create_block();
            let mut awaits: HashMap<
                Span,
                (
                    i64,
                    cranelift_codegen::ir::Block,
                    cranelift_codegen::ir::Block,
                ),
            > = HashMap::new();
            for (k, sp) in await_spans.iter().enumerate() {
                let pb = b.create_block();
                let rb = b.create_block();
                awaits.insert(*sp, ((k + 1) as i64, pb, rb));
            }
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: Some(out),
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: value_capture_locals.clone(),
                    rc_owned: Vec::new(),
                };
                fg.mark_root(self_val);
                for (l, _off, ct) in live {
                    fg.fresh_var(*l, *ct);
                }
                // Entry dispatch: resume at the block matching `state`, else start.
                let state_v =
                    fg.b.ins()
                        .load(types::I64, MemFlags::trusted(), self_val, 0);
                for (state_n, _pb, rb) in awaits.values() {
                    let nv = fg.b.ins().iconst(types::I64, *state_n);
                    let c = fg.b.ins().icmp(IntCC::Equal, state_v, nv);
                    let next = fg.b.create_block();
                    fg.b.ins().brif(c, *rb, &[], next, &[]);
                    fg.switch(next);
                }
                fg.b.ins().jump(body_entry, &[]);

                // Start: load entry locals from their slots, zero the rest.
                fg.switch(body_entry);
                for (l, off, ct) in live {
                    let var = *fg.vars.get(l).expect("declared local var");
                    if entry_set.contains(l) {
                        let v = fg.b.ins().load(*ct, MemFlags::trusted(), self_val, *off);
                        fg.b.def_var(var, v);
                    } else {
                        let z = fg.zero_val(*ct);
                        fg.b.def_var(var, z);
                    }
                }
                let save_locals: Vec<(LocalId, i32)> =
                    live.iter().map(|(l, off, _)| (*l, *off)).collect();
                fg.async_ctx = Some(AsyncCtx {
                    self_val,
                    ctx_val,
                    inner_off: ASYNC_INNER_OFF,
                    save_locals,
                    awaits: awaits.clone(),
                    pending_block,
                    for_slots: for_slots.clone(),
                });
                let val = fg.gen_body_view(&body)?;
                let val = fg.rc_return_value(body.0.trailing.as_deref(), val);
                fg.endpoint_return_value(body.0.trailing.as_deref(), val)?;
                // Release each owned channel endpoint on the completion path (the
                // building closure transferred ownership instead of releasing it,
                // `docs/20` §1/§2). Read the endpoint through its capture local
                // *here*, at the completion block, so the chan id dominates the
                // `emit_return` that follows (a value computed in `body_entry`
                // would not dominate completion reached via a resume block).
                for (local, ty, is_sender) in owned_endpoints {
                    let ep = fg
                        .read_local(*local)
                        .ok_or_else(|| CodegenError::new(err_span, "owned endpoint has no slot"))?;
                    let chan = fg.emit_channel_id(ep, *ty, err_span)?;
                    fg.endpoint_releases.push((chan, *is_sender));
                }
                fg.emit_return(val)?;

                // Resume blocks: reload every local, jump to the await's poll.
                for (_state_n, pb, rb) in awaits.values() {
                    fg.switch(*rb);
                    for (l, off, ct) in live {
                        let var = *fg.vars.get(l).expect("declared local var");
                        let v = fg.b.ins().load(*ct, MemFlags::trusted(), self_val, *off);
                        fg.b.def_var(var, v);
                    }
                    fg.b.ins().jump(*pb, &[]);
                }

                // Shared Pending return.
                fg.switch(pending_block);
                let p = fg.box_pending();
                fg.b.ins().return_(&[p]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.record_clif("<async poll>", &ctx);
        self.module
            .define_function(poll_fid, &mut ctx)
            .map_err(|e| CodegenError::new(err_span, format!("define poll: {e}")))?;
        self.capture_safepoints(poll_fid, &ctx);
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Define the generated cleanup hook stored in a generated `Future` box.
    /// Normal completion releases owned channel endpoints on the poll function's
    /// return path; cancellation reaches this hook instead, using the same state
    /// slots to release those endpoints promptly.
    fn define_async_drop(
        &mut self,
        drop_fid: FuncId,
        subst: &HashMap<DefId, Ty>,
        info: &compiler::sema::results::AsyncInfo,
        block_view: Option<BodyView<'_>>,
        owned_endpoints: &[(LocalId, Ty, bool)],
        value_capture_locals: &HashSet<LocalId>,
        span: Span,
    ) -> CgResult<()> {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let state = b.block_params(entry)[0];
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: self.analysis.tcx.null,
                    async_out: None,
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                let mut offsets = HashMap::new();
                if let Some(bv) = block_view {
                    if bv.has_await() {
                        let cap_ids: Vec<LocalId> = info.captures.iter().map(|(l, _)| *l).collect();
                        let layout = async_state_layout(
                            self.analysis,
                            subst,
                            &cap_ids,
                            bv,
                            &self.captured_locals,
                            value_capture_locals,
                        );
                        for local in cap_ids {
                            offsets.insert(local, layout.slot_off[&local]);
                        }
                    }
                }
                if offsets.is_empty() {
                    for (k, (local, _ty)) in info.captures.iter().enumerate() {
                        offsets.insert(*local, (8 + k * 8) as i32);
                    }
                }
                for (local, ty) in &info.captures {
                    let Some(off) = offsets.get(local).copied() else {
                        continue;
                    };
                    if let Some(ct) = fg.cx_clty(*ty) {
                        if fg.cx.captured_locals.contains(local)
                            && !value_capture_locals.contains(local)
                        {
                            let cell_ptr = fg.b.ins().load(PTR, MemFlags::trusted(), state, off);
                            fg.bind_local_cell(*local, ct, cell_ptr);
                        } else {
                            let loaded = fg.b.ins().load(ct, MemFlags::trusted(), state, off);
                            fg.bind_local(*local, ct, loaded);
                        }
                    }
                }
                for (local, ty, is_sender) in owned_endpoints {
                    let ep = fg.read_local(*local).ok_or_else(|| {
                        CodegenError::new(span, "owned endpoint has no state slot")
                    })?;
                    let chan = fg.emit_channel_id(ep, *ty, span)?;
                    let name = if *is_sender {
                        "lang_chan_sender_release"
                    } else {
                        "lang_chan_receiver_release"
                    };
                    fg.call_intrinsic(name, &[types::I64], None, &[chan]);
                }
            }
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
        }
        self.module
            .define_function(drop_fid, &mut ctx)
            .map_err(|e| CodegenError::new(span, format!("define async drop: {e}")))?;
        self.capture_safepoints(drop_fid, &ctx);
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Lower an async function whose body contains `await` to a full `Future`
    /// state machine (`docs/21` §3–4): a `poll` function (built by
    /// [`Self::build_stateful_poll`]) plus a constructor (`ctor_fid`) that
    /// allocates the state struct with the arguments stored and returns the
    /// `Future` box.
    fn define_async_fn_stateful(
        &mut self,
        def: DefId,
        args: Vec<Ty>,
        ctor_fid: FuncId,
        body: BodyView,
        out: Ty,
    ) -> CgResult<()> {
        let subst = build_subst(self.analysis, def, &args);
        let param_locals: Vec<LocalId> = self
            .hir
            .fn_sigs
            .get(&def)
            .map(|s| s.params.iter().map(|(l, _)| *l).collect())
            .unwrap_or_default();

        // Lay out the state struct and build the poll function.
        let layout = async_state_layout(
            self.analysis,
            &subst,
            &param_locals,
            body,
            &self.captured_locals,
            &HashSet::new(),
        );
        let entry_set: HashSet<LocalId> = param_locals.iter().copied().collect();
        // Parameter values are stored by the constructor into these slots.
        let param_offs: Vec<i32> = param_locals.iter().map(|l| layout.slot_off[l]).collect();
        let state_size = layout.state_size;
        let ptr_offsets = layout.ptr_offsets.clone();
        let span = self.analysis.program.def(def).span;

        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.returns.push(AbiParam::new(PTR));
        let poll_name = format!("{}$poll", mangle(self.analysis, def, &args));
        let poll_fid = self
            .module
            .declare_function(&poll_name, Linkage::Local, &poll_sig)
            .map_err(|e| CodegenError::new(span, format!("declare poll: {e}")))?;
        // An async function owns its endpoint *parameters* through the ordinary
        // param/return refcount discipline, not the by-value-capture path — so it
        // transfers no owned endpoints here.
        self.build_stateful_poll(
            poll_fid,
            &subst,
            out,
            body,
            &entry_set,
            &layout.live,
            &layout.for_slots,
            &[],
            &HashSet::new(),
            span,
        )?;

        // -- constructor body ----------------------------------------------
        let mut cctx = self.module.make_context();
        cctx.func.signature =
            signature_of(self.module, self.analysis, def, &subst)?.expect("ctor sig");
        let mut cfctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut cfctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let pvals: Vec<Value> = b.block_params(entry).to_vec();
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: None,
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                // Managed arguments must survive the state allocation.
                for (i, v) in pvals.iter().enumerate() {
                    if ptr_offsets.contains(&(param_offs[i] as u32)) {
                        fg.mark_root(*v);
                    }
                }
                let desc = fg.emit_descriptor(state_size, GC_KIND_PLAIN, &ptr_offsets);
                let state = fg
                    .call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
                    .expect("lang_alloc returns a pointer");
                let zero = fg.b.ins().iconst(types::I64, 0);
                fg.b.ins().store(MemFlags::trusted(), zero, state, 0);
                for (i, v) in pvals.iter().enumerate() {
                    fg.b.ins()
                        .store(MemFlags::trusted(), *v, state, param_offs[i]);
                }
                let fut = fg.emit_future_box(poll_fid, None, state);
                fg.b.ins().return_(&[fut]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        let clif_label = format!("{}$ctor", self.analysis.program.def(def).name);
        self.record_clif(&clif_label, &cctx);
        self.module
            .define_function(ctor_fid, &mut cctx)
            .map_err(|e| {
                CodegenError::new(
                    self.analysis.program.def(def).span,
                    format!("define async ctor: {e}"),
                )
            })?;
        self.capture_safepoints(ctor_fid, &cctx);
        self.module.clear_context(&mut cctx);
        Ok(())
    }

    /// Define the `poll` function of a bare `async { … }` block: load its
    /// captured locals from the state struct, run the body, and return the
    /// result wrapped in `Ready<Output> | Pending` (`docs/21`).
    fn define_async_job(&mut self, job: AsyncJob) -> CgResult<()> {
        let AsyncJob {
            poll_fid,
            drop_fid,
            info,
            body,
            subst,
            span,
            out,
            owned_endpoints,
            value_capture_locals,
        } = job;
        // A view of the wrapped HIR block.
        let block_view = match &body.kind {
            compiler::hir::ExprKind::Block(b) => Some(BodyView(b)),
            _ => None,
        };
        // A block containing `await` is a suspendable state machine; its
        // captures are the entry locals (pre-stored by `gen_async_block`).
        if let Some(bv) = block_view {
            if bv.has_await() {
                let cap_ids: Vec<LocalId> = info.captures.iter().map(|(l, _)| *l).collect();
                let layout = async_state_layout(
                    self.analysis,
                    &subst,
                    &cap_ids,
                    bv,
                    &self.captured_locals,
                    &value_capture_locals,
                );
                let entry_set: HashSet<LocalId> = cap_ids.into_iter().collect();
                self.define_async_drop(
                    drop_fid,
                    &subst,
                    &info,
                    block_view,
                    &owned_endpoints,
                    &value_capture_locals,
                    span,
                )?;
                return self.build_stateful_poll(
                    poll_fid,
                    &subst,
                    out,
                    bv,
                    &entry_set,
                    &layout.live,
                    &layout.for_slots,
                    &owned_endpoints,
                    &value_capture_locals,
                    span,
                );
            }
        }
        self.define_async_drop(
            drop_fid,
            &subst,
            &info,
            block_view,
            &owned_endpoints,
            &value_capture_locals,
            span,
        )?;
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        sig.params.push(AbiParam::new(PTR));
        sig.returns.push(AbiParam::new(PTR));
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let self_val = b.block_params(entry)[0];
            {
                let mut fg = FnGen {
                    cx: CgShared {
                        analysis: self.analysis,
                        hir: self.hir,
                        captured_locals: &self.captured_locals,
                        direct_call_counts: &self.direct_call_counts,
                    },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    clone_thunks: &mut self.clone_thunks,
                    subst,
                    b: &mut b,
                    vars: HashMap::new(),
                    iface_local_concretes: HashMap::new(),
                    stack_struct_locals: HashSet::new(),
                    cell_content: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: Some(out),
                    async_ctx: None,
                    endpoint_releases: Vec::new(),
                    endpoint_owned: Vec::new(),
                    value_capture_locals: HashSet::new(),
                    rc_owned: Vec::new(),
                };
                fg.mark_root(self_val);
                // Captures live in the state struct after the state word (@8).
                // Each slot holds a cell pointer (`docs/09` §7 / closure env
                // layout): the outer scope's gen_async_block stored the cell
                // pointer from each captured local's variable.
                for (k, (local, ty)) in info.captures.iter().enumerate() {
                    if let Some(ct) = fg.cx_clty(*ty) {
                        let off = (8 + k * 8) as i32;
                        if fg.cx.captured_locals.contains(local) {
                            let cell_ptr = fg.b.ins().load(PTR, MemFlags::trusted(), self_val, off);
                            fg.bind_local_cell(*local, ct, cell_ptr);
                        } else {
                            let loaded = fg.b.ins().load(ct, MemFlags::trusted(), self_val, off);
                            fg.bind_local(*local, ct, loaded);
                        }
                    }
                }
                // Release each owned channel endpoint when the future completes
                // (the building closure transferred ownership instead of releasing
                // it); `emit_return` emits the releases on the single completion
                // path (`docs/20` §1/§2).
                for (local, ty, is_sender) in &owned_endpoints {
                    let ep = fg
                        .read_local(*local)
                        .ok_or_else(|| CodegenError::new(span, "owned endpoint has no slot"))?;
                    let chan = fg.emit_channel_id(ep, *ty, span)?;
                    fg.endpoint_releases.push((chan, *is_sender));
                }
                let val = fg.h_expr(&body)?;
                let ret_expr = match &body.kind {
                    compiler::hir::ExprKind::Block(blk) => blk.trailing.as_deref(),
                    _ => Some(&body),
                };
                let val = fg.rc_return_value(ret_expr, val);
                fg.endpoint_return_value(ret_expr, val)?;
                fg.emit_return(val)?;
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.record_clif("<async block poll>", &ctx);
        self.module
            .define_function(poll_fid, &mut ctx)
            .map_err(|e| CodegenError::new(span, format!("define async block poll: {e}")))?;
        self.capture_safepoints(poll_fid, &ctx);
        self.module.clear_context(&mut ctx);
        Ok(())
    }
}

struct CgShared<'a> {
    analysis: &'a Analysis,
    /// The typed HIR (see [`Codegen::hir`]) — reachable from every per-function
    /// generator method as codegen migrates its reads off `CheckResults`.
    hir: &'a Hir,
    /// Reference into [`Codegen::captured_locals`] — the set of LocalIds the
    /// closure analysis identified as captured anywhere in the program. Cell-
    /// backed binding/access for these locals (`docs/09` §7) is gated on
    /// membership here. `'a` ties the borrow to the outer codegen.
    captured_locals: &'a HashSet<LocalId>,
    /// Whole-HIR direct call counts for conservative inlining heuristics.
    direct_call_counts: &'a HashMap<DefId, usize>,
}

/// Per-function code generator (for one monomorphized instance).
struct FnGen<'a, 'b, 'f, M: Module> {
    cx: CgShared<'a>,
    module: &'a mut M,
    /// Shared instance table — new instances are declared on demand at calls.
    funcs: &'a mut HashMap<Instance, FuncId>,
    worklist: &'a mut Vec<Instance>,
    /// Lifted closure functions queued for code generation.
    closures: &'a mut Vec<ClosureJob>,
    /// Async block/closure `poll` functions queued for code generation.
    async_jobs: &'a mut Vec<AsyncJob>,
    /// `Shared` lock-body clone-out thunks queued for code generation.
    clone_thunks: &'a mut Vec<CloneThunkJob>,
    /// This instance's generic-parameter substitution.
    subst: HashMap<DefId, Ty>,
    b: &'b mut FunctionBuilder<'f>,
    /// Language local → Cranelift variable. For *cell-backed* locals (those
    /// captured by some closure, `docs/09` §7), the variable holds the cell's
    /// pointer; reads/writes go through `lang_alloc`-allocated cells so the
    /// outer scope and the closure share state. See [`cell_content`].
    vars: HashMap<LocalId, Variable>,
    /// Conservative straight-line devirtualization facts: an interface-typed
    /// local currently holds a value produced from this concrete type. Cleared
    /// or restored around control-flow joins; used only to replace vtable calls
    /// with direct impl calls when the concrete receiver is provable.
    iface_local_concretes: HashMap<LocalId, Ty>,
    /// Locals whose binding-site record struct literal is proven not to escape
    /// this function and whose layout needs no heap tracing/finalization. These
    /// can use a zeroed stack slot while preserving the usual field-block
    /// pointer representation inside this frame.
    stack_struct_locals: HashSet<LocalId>,
    /// LocalId → its Cranelift content type, set only for cell-backed locals.
    /// Membership in this map is what `read_local`/`write_local` consult to
    /// decide between a direct `use_var` / `def_var` and a load/store through
    /// the cell pointer held in `vars`.
    cell_content: HashMap<LocalId, ClType>,
    /// Whether the current block has been terminated (a return/jump/branch was
    /// emitted), so later instructions in the same block are suppressed.
    /// Cranelift's own `is_filled` is private, so we track it ourselves.
    term: bool,
    /// Stack of enclosing loops, for `break`/`continue` lowering.
    loops: Vec<LoopCg>,
    ret_ty: Ty,
    /// When this body is an async state machine's `poll` function, the future's
    /// `Output` type. Returns (trailing or explicit `return`) are then wrapped
    /// in a `Ready<Output>` and boxed into the `Ready<Output> | Pending` union
    /// the `poll` ABI returns (`docs/21`).
    async_out: Option<Ty>,
    /// When this is an async `poll` body whose source contains `await`, the
    /// state-machine context driving suspension/resumption.
    async_ctx: Option<AsyncCtx>,
    /// Channel endpoints this body deterministically releases on exit
    /// (`docs/16` §8 / `docs/20` §2): `(chan_id, is_sender)`. Populated for a
    /// `Thread.spawn` worker body that captured `Sender`/`Receiver` handles —
    /// releasing them when the worker returns is what closes the channel on the
    /// last-sender drop. `emit_return` drains this on every return path.
    endpoint_releases: Vec<(Value, bool)>,
    /// Channel endpoint locals owned by this frame. Ordinary function/closure
    /// endpoint locals release on return; a returned endpoint local is retained
    /// first so ownership transfers to the caller instead of closing in the
    /// callee.
    endpoint_owned: Vec<(LocalId, Ty, bool)>,
    /// Locals that nested async blocks should capture as values, not cells.
    value_capture_locals: HashSet<LocalId>,
    /// Owned `@RefCounted` locals (`docs/16` §8.1), in binding order: non-captured,
    /// `let`-bound locals whose type is a `@RefCounted` struct. Each holds one
    /// strong reference (`+1`); `emit_return` releases them on every return path
    /// (reverse order), and re-binding/assigning one releases its prior value
    /// first. The last release of an object runs its `Drop` synchronously and
    /// frees it — deterministic finalization without waiting for a collection.
    rc_owned: Vec<LocalId>,
}

/// Cranelift blocks for an enclosing loop.
struct LoopCg {
    /// Where `continue` jumps (loop header / body start).
    continue_block: cranelift_codegen::ir::Block,
    /// Where `break` jumps (the loop's exit).
    break_block: cranelift_codegen::ir::Block,
    /// Whether `break_block` takes a value argument (a value-producing `loop`).
    has_value: bool,
}

impl<'a, 'b, 'f, M: Module> FnGen<'a, 'b, 'f, M> {
    pub(crate) fn cx_clty(&self, ty: Ty) -> Option<ClType> {
        clty_subst(self.cx.analysis, ty, &self.subst)
    }

    /// Runtime type id of `ty` under this instance's substitution.
    pub(crate) fn type_id_of(&self, ty: Ty) -> i64 {
        type_id(
            self.cx.analysis,
            resolve_shallow(self.cx.analysis, ty, &self.subst),
        )
    }

    pub(crate) fn fresh_var(&mut self, local: LocalId, ct: ClType) -> Variable {
        if self.cx.captured_locals.contains(&local) && !self.value_capture_locals.contains(&local) {
            // Cell-backed: the Cranelift variable holds the cell *pointer*;
            // reads/writes route through `read_local`/`write_local`. The cell
            // ptr is itself a managed-heap root.
            let var = self.b.declare_var(PTR);
            self.b.declare_var_needs_stack_map(var);
            self.vars.insert(local, var);
            self.cell_content.insert(local, ct);
            return var;
        }
        let var = self.b.declare_var(ct);
        // Managed-pointer locals are GC roots: Cranelift records them in the
        // precise stack map at each safepoint (call).
        if let Some(ty) = self.cx.analysis.hir.local_ty(local) {
            let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
            if is_managed_ptr(self.cx.analysis, resolved) {
                self.b.declare_var_needs_stack_map(var);
            }
        }
        self.vars.insert(local, var);
        var
    }

    /// Bind `local` to its initial value `init`. For a plain local this is
    /// `fresh_var` + `def_var`; for a *captured* local (`docs/09` §7) the
    /// initial value is stored into a fresh managed cell and the variable
    /// becomes that cell's pointer. The outer scope and any closure body
    /// share the cell through `read_local`/`write_local`.
    pub(crate) fn bind_local(&mut self, local: LocalId, ct: ClType, init: Value) {
        let var = self.fresh_var(local, ct);
        let init_is_managed = self
            .cx
            .analysis
            .hir
            .local_ty(local)
            .map(|ty| {
                let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
                is_managed_ptr(self.cx.analysis, resolved)
            })
            .unwrap_or(false);
        if init_is_managed {
            self.mark_root(init);
        }
        if self.cell_content.contains_key(&local) {
            // If `init` is a managed pointer, it must be a stack-map root
            // across the cell allocation — otherwise a GC stress collect
            // between `lang_alloc` and the store would free the pointee
            // (this bit the closure-tagger GC-stress test).
            let cell = self.alloc_local_cell(local);
            self.b.ins().store(MemFlags::trusted(), init, cell, 0);
            self.b.def_var(var, cell);
        } else {
            self.b.def_var(var, init);
        }
    }

    /// Bind a captured local whose cell pointer is already known — used when
    /// entering a closure body, where each capture's cell pointer is loaded
    /// from the env slot. The Cranelift variable holds that cell ptr; reads
    /// and writes inside the body route through the cell, sharing state with
    /// the outer scope.
    pub(crate) fn bind_local_cell(&mut self, local: LocalId, ct: ClType, cell_ptr: Value) {
        let var = self.fresh_var(local, ct);
        // `fresh_var` puts this in `cell_content` (the local is by definition
        // captured here), so read/write paths route through the cell.
        self.b.def_var(var, cell_ptr);
    }

    /// Allocate a fresh cell for `local`. The cell is an 8-byte managed object;
    /// when the content is a managed pointer the descriptor records
    /// `ptr_offsets = [0]` so the collector traces it.
    fn alloc_local_cell(&mut self, local: LocalId) -> Value {
        let mut ptr_offsets: Vec<u32> = Vec::new();
        if let Some(ty) = self.cx.analysis.hir.local_ty(local) {
            let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
            if is_managed_ptr(self.cx.analysis, resolved) {
                ptr_offsets.push(0);
            }
        }
        let desc = self.emit_descriptor(8, GC_KIND_PLAIN, &ptr_offsets);
        self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer")
    }

    /// Read the value of `local`. Cell-backed locals load through their cell
    /// pointer; plain locals use Cranelift's `use_var`.
    pub(crate) fn read_local(&mut self, local: LocalId) -> Option<Value> {
        let var = *self.vars.get(&local)?;
        if let Some(&ct) = self.cell_content.get(&local) {
            let cell = self.b.use_var(var);
            return Some(self.b.ins().load(ct, MemFlags::trusted(), cell, 0));
        }
        Some(self.b.use_var(var))
    }

    /// Write `v` to `local`. Cell-backed locals store through the cell pointer;
    /// plain locals use Cranelift's `def_var`. Returns an error only if the
    /// local is unbound.
    pub(crate) fn write_local(&mut self, local: LocalId, v: Value, span: Span) -> CgResult<()> {
        let var = *self
            .vars
            .get(&local)
            .ok_or_else(|| CodegenError::new(span, "write to unbound local"))?;
        if self.cell_content.contains_key(&local) {
            let cell = self.b.use_var(var);
            self.b.ins().store(MemFlags::trusted(), v, cell, 0);
            return Ok(());
        }
        self.b.def_var(var, v);
        Ok(())
    }

    /// Declare `v` a GC root: Cranelift records it in the precise stack map at
    /// every safepoint where it is live. Use for managed-pointer temporaries
    /// that outlive a later allocation but are not themselves `gen_expr`
    /// results (which `gen_expr` already marks).
    /// Generate a function/async body by walking its typed HIR block.
    pub(crate) fn gen_body_view(&mut self, body: &BodyView) -> CgResult<Option<Value>> {
        self.prepare_stack_struct_locals(body.0);
        self.h_block(body.0)
    }

    pub(crate) fn mark_root(&mut self, v: Value) -> Value {
        self.b.declare_value_needs_stack_map(v);
        v
    }

    /// Switch to `block`, resetting the termination flag for the new block.
    pub(crate) fn switch(&mut self, block: cranelift_codegen::ir::Block) {
        self.b.switch_to_block(block);
        self.term = false;
    }

    pub(crate) fn emit_return(&mut self, val: Option<Value>) -> CgResult<()> {
        if self.term {
            return Ok(());
        }
        // Deterministic `@RefCounted` release (`docs/16` §8.1): every owned
        // refcounted local drops its strong reference on this return path. The
        // return value itself is unaffected — a borrowed return was retained at
        // the `return` site (a `+1` handed to the caller), and an owned return is
        // a temporary not in this list. Locals not bound on the running path read
        // back null and release is a no-op, so this is safe across conditionals.
        if !self.rc_owned.is_empty() {
            self.rc_release_owned_locals();
        }
        // Deterministic channel-endpoint release (`docs/16` §8 / `docs/20` §2):
        // a worker body that captured `Sender`/`Receiver` handles releases them
        // here, on every return path, so the channel closes the instant the
        // last sender is dropped. Emitted before the terminator; the chan-id
        // values were computed at capture-bind time and dominate every return.
        if !self.endpoint_releases.is_empty() {
            let releases = self.endpoint_releases.clone();
            for (chan, is_sender) in releases {
                let name = if is_sender {
                    "lang_chan_sender_release"
                } else {
                    "lang_chan_receiver_release"
                };
                self.call_intrinsic(name, &[cranelift_codegen::ir::types::I64], None, &[chan]);
            }
        }
        // Ordinary endpoint locals own runtime endpoint references just like
        // worker-captured endpoints above, except their channel id must be read
        // from the local at the actual return path.
        if !self.endpoint_owned.is_empty() {
            self.endpoint_release_owned_locals()?;
        }
        // In an async `poll` body, a return value is the future's `Output`; wrap
        // it in `Ready<Output>` and box into the `Ready<Output> | Pending` union
        // the `poll` ABI returns (`docs/21` §1).
        if let Some(out) = self.async_out {
            let boxed = self.box_ready(val, out);
            self.b.ins().return_(&[boxed]);
            self.term = true;
            return Ok(());
        }
        // A by-value `extern struct` return (e.g. `CStr.from_ptr`): the value is
        // a pointer to a stack/inline field block that dies with this frame.
        // Copy the block to a managed heap block and return *that* stable
        // pointer — the same escape handling `box_value` applies for unions
        // (`docs/19` §3).
        if let Some(v) = val {
            let rty = resolve_shallow(self.cx.analysis, self.ret_ty, &self.subst);
            if is_extern_struct_ty(self.cx.analysis, rty) {
                let heap = self.heap_copy_extern(v, rty);
                self.b.ins().return_(&[heap]);
                self.term = true;
                return Ok(());
            }
        }
        match (self.cx_clty(self.ret_ty), val) {
            (Some(_), Some(v)) => {
                self.b.ins().return_(&[v]);
            }
            (Some(_), None) => {
                // Non-void return type but no value: a diverging body.
                let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
                self.b.ins().trap(tc);
            }
            (None, _) => {
                self.b.ins().return_(&[]);
            }
        }
        self.term = true;
        Ok(())
    }
}

/// Map a language type to a Cranelift value type, or `None` for zero-sized
/// (`null`/`never`) or not-yet-lowerable aggregate types.
#[cfg(test)]
mod tests;
