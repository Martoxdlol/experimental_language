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
use compiler::ids::{DefId, LocalId};
use compiler::sema::results::ForIter;
use compiler::sema::{Adjust, Analysis, Builtin, CloneKind, DefKind, NumIntrinsic, ValueRes};
use compiler::span::Span;
use compiler::ty::{FloatTy, IntTy, Ty, TyKind};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, AbiParam, InstBuilder, MemFlags, StackSlotData, StackSlotKind, Type as ClType, Value,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Shared codegen helpers (type lowering, layout, monomorphization, async-body
/// analysis), factored out of the per-function generator below.
mod support;
use support::*;
mod gen_cast;
mod gen_stmt;
mod gen_expr;
mod gen_match;
mod gen_collections;
mod gen_struct;
mod gen_call;

/// Pointer-width integer type on the host (str/reference values are pointers).
/// The JIT only targets the 64-bit host, so this is `I64`.
pub(crate) const PTR: ClType = types::I64;

/// Descriptor `kind` for a plain object (scan its listed pointer offsets).
/// Mirrors `runtime::gc::KIND_PLAIN`.
pub(crate) const GC_KIND_PLAIN: u64 = 0;

/// Monotonic counter for unique anonymous data-object names (string literals).
static DATA_CTR: AtomicU64 = AtomicU64::new(0);

/// Register the runtime's C-ABI entry points so the JIT can resolve calls.
fn register_runtime_symbols(b: &mut JITBuilder) {
    b.symbol("lang_alloc", runtime::lang_alloc as *const u8);
    b.symbol("lang_panic", runtime::lang_panic as *const u8);
    b.symbol("lang_gc_safepoint", runtime::gc::lang_gc_safepoint as *const u8);
    b.symbol("lang_gc_pin", runtime::gc::lang_gc_pin as *const u8);
    b.symbol("lang_gc_unpin", runtime::gc::lang_gc_unpin as *const u8);
    b.symbol("lang_gc_register_drop", runtime::gc::lang_gc_register_drop as *const u8);
    b.symbol("lang_block_on", runtime::async_rt::lang_block_on as *const u8);
    b.symbol("lang_async_yield", runtime::async_rt::lang_async_yield as *const u8);
    b.symbol("lang_async_sleep", runtime::async_rt::lang_async_sleep as *const u8);
    b.symbol("lang_async_spawn", runtime::threads::lang_async_spawn as *const u8);
    b.symbol("lang_async_spawn_future", runtime::threads::lang_async_spawn_future as *const u8);
    b.symbol("lang_thread_spawn", runtime::threads::lang_thread_spawn as *const u8);
    b.symbol("lang_thread_join_future", runtime::threads::lang_thread_join_future as *const u8);
    b.symbol("lang_channel_new", runtime::channels::lang_channel_new as *const u8);
    b.symbol("lang_chan_send", runtime::channels::lang_chan_send as *const u8);
    b.symbol("lang_chan_recv_future", runtime::channels::lang_chan_recv_future as *const u8);
    b.symbol("lang_chan_try_recv", runtime::channels::lang_chan_try_recv as *const u8);
    b.symbol("lang_shared_new", runtime::shared::lang_shared_new as *const u8);
    b.symbol("lang_shared_lock", runtime::shared::lang_shared_lock as *const u8);
    b.symbol("lang_shared_unlock", runtime::shared::lang_shared_unlock as *const u8);
    b.symbol("lang_shared_try_lock", runtime::shared::lang_shared_try_lock as *const u8);
    b.symbol("lang_exit", runtime::lang_exit as *const u8);
    b.symbol("lang_abort", runtime::lang_abort as *const u8);
    b.symbol("lang_list_new", runtime::lang_list_new as *const u8);
    b.symbol("lang_list_push", runtime::lang_list_push as *const u8);
    b.symbol("lang_list_size", runtime::lang_list_size as *const u8);
    b.symbol("lang_list_get", runtime::lang_list_get as *const u8);
    b.symbol("lang_list_set", runtime::lang_list_set as *const u8);
    b.symbol("lang_list_clone", runtime::lang_list_clone as *const u8);
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
    b.symbol("lang_str_from_utf8", runtime::lang_str_from_utf8 as *const u8);
    b.symbol("lang_str_size", runtime::lang_str_size as *const u8);
    b.symbol("lang_str_byte_size", runtime::lang_str_byte_size as *const u8);
    b.symbol("lang_str_eq", runtime::lang_str_eq as *const u8);
    b.symbol("lang_str_cmp", runtime::lang_str_cmp as *const u8);
    b.symbol("lang_str_contains", runtime::lang_str_contains as *const u8);
    b.symbol("lang_str_starts_with", runtime::lang_str_starts_with as *const u8);
    b.symbol("lang_str_ends_with", runtime::lang_str_ends_with as *const u8);
    b.symbol("lang_str_substring", runtime::lang_str_substring as *const u8);
    b.symbol("lang_str_to_upper", runtime::lang_str_to_upper as *const u8);
    b.symbol("lang_str_to_lower", runtime::lang_str_to_lower as *const u8);
    b.symbol("lang_str_trim", runtime::lang_str_trim as *const u8);
    b.symbol("lang_str_concat", runtime::lang_str_concat as *const u8);
    b.symbol("lang_int_to_str", runtime::lang_int_to_str as *const u8);
    b.symbol("lang_uint_to_str", runtime::lang_uint_to_str as *const u8);
    b.symbol("lang_float_to_str", runtime::lang_float_to_str as *const u8);
    b.symbol("lang_bool_to_str", runtime::lang_bool_to_str as *const u8);
    b.symbol("lang_char_to_str", runtime::lang_char_to_str as *const u8);
    b.symbol("lang_print", runtime::lang_print as *const u8);
    b.symbol("lang_println", runtime::lang_println as *const u8);
}

/// A failure to lower a construct that is otherwise well-typed.
#[derive(Clone, Debug)]
pub struct CodegenError {
    pub message: String,
    pub span: Span,
}

impl CodegenError {
    fn new(span: Span, msg: impl Into<String>) -> Self {
        CodegenError { message: msg.into(), span }
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
}

impl Jit {
    /// Raw code pointer for a compiled function by language name.
    pub fn func_ptr(&self, name: &str) -> Option<*const u8> {
        self.funcs.get(name).map(|id| self.module.get_finalized_function(*id))
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
        let Some(ptr) = self.func_ptr("main") else { return false };
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

/// Build a target ISA for `triple`. `pic` selects position-independent code:
/// the JIT loads code at a fixed address (`false`), but object output is linked
/// into a PIE executable, so it must be position-independent (`true`).
fn make_isa(triple: target_lexicon::Triple, pic: bool) -> cranelift_codegen::isa::OwnedTargetIsa {
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").unwrap();
    flags.set("is_pic", if pic { "true" } else { "false" }).unwrap();
    // Frame pointers let the GC walk the stack to find precise roots.
    flags.set("preserve_frame_pointers", "true").unwrap();
    cranelift_codegen::isa::lookup(triple)
        .expect("target ISA")
        .finish(settings::Flags::new(flags))
        .expect("ISA flags")
}

/// Drive the monomorphizing code generator over any `Module` (JIT or object),
/// returning the exported `name → FuncId` map and the captured GC safepoints.
fn run_codegen<M: Module>(
    analysis: &Analysis,
    module: &mut M,
) -> CgResult<(HashMap<String, FuncId>, Vec<Safepoint>, Vec<(i64, FuncId)>)> {
    let mut cg = Codegen {
        analysis,
        module,
        funcs: HashMap::new(),
        by_name: HashMap::new(),
        worklist: Vec::new(),
        closures: Vec::new(),
        async_jobs: Vec::new(),
        safepoints: Vec::new(),
    };
    cg.seed()?;
    cg.run()?;
    let drops = cg.collect_drops();
    Ok((cg.by_name, cg.safepoints, drops))
}

/// Compile every lowerable function in `analysis` and return a runnable [`Jit`].
pub fn compile(analysis: &Analysis) -> CgResult<Jit> {
    let isa = make_isa(target_lexicon::Triple::host(), false);
    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    register_runtime_symbols(&mut builder);
    let mut module = JITModule::new(builder);

    let (by_name, safepoints, drops) = run_codegen(analysis, &mut module)?;

    module.finalize_definitions().expect("finalize");

    // Register each call safepoint's precise stack map with the runtime, now
    // that function base addresses are known. `pc` is the call instruction.
    for (func_id, code_offset, frame_to_fp, offsets) in &safepoints {
        let base = module.get_finalized_function(*func_id) as usize;
        let pc = base + *code_offset as usize;
        unsafe {
            runtime::gc::lang_gc_register_safepoint(pc, *frame_to_fp, offsets.as_ptr(), offsets.len());
        }
    }

    // Register each `Drop` type's finalizer (`docs/16` §8).
    for (type_id, func_id) in &drops {
        let addr = module.get_finalized_function(*func_id);
        let f: runtime::gc::DropFn = unsafe { std::mem::transmute(addr) };
        unsafe { runtime::gc::lang_gc_register_drop(*type_id as u64, f) };
    }

    let main_is_async = main_is_async(analysis);
    let pending_tid = Some(1000 + analysis.program.pending_def.index() as i64);
    Ok(Jit { module, funcs: by_name, pending_tid, main_is_async })
}

/// Whether the user `main` is declared `async function` — its compiled symbol
/// returns a `Future<…>` box and must be driven by the runtime executor.
fn main_is_async(analysis: &Analysis) -> bool {
    analysis
        .program
        .defs
        .iter()
        .enumerate()
        .any(|(idx, d)| {
            d.name == "main"
                && matches!(d.kind, DefKind::Function)
                && analysis
                    .results
                    .async_fns
                    .contains_key(&compiler::ids::DefId(idx as u32))
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
pub fn compile_object(analysis: &Analysis, out: &Path) -> CgResult<()> {
    let mut triple = target_lexicon::Triple::host();
    // A bare `*-apple-darwin` host triple yields a Mach-O object with an
    // "unknown" platform that the linker rejects; promote it to `macosx` with a
    // deployment target so a proper `LC_BUILD_VERSION` is emitted.
    if let target_lexicon::OperatingSystem::Darwin(v) = triple.operating_system {
        let dt = v.unwrap_or(target_lexicon::DeploymentTarget { major: 11, minor: 0, patch: 0 });
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
    let (by_name, safepoints, drops) = run_codegen(analysis, &mut module)?;

    let user_main = *by_name
        .get("main")
        .ok_or_else(|| CodegenError::new(Span::dummy(), "no `main` function to build"))?;

    let main_async = main_is_async(analysis);
    let pending_tid = 1000 + analysis.program.pending_def.index() as i64;
    emit_native_entry(&mut module, user_main, main_async, pending_tid, &safepoints, &drops)?;

    let product = module.finish();
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

        let en_ref = module.declare_func_in_func(en_id, b.func);
        let on = b.ins().iconst(types::I8, 1);
        b.ins().call(en_ref, &[on]);

        let main_ref = module.declare_func_in_func(user_main, b.func);
        if main_is_async {
            // Async `main`: calling the symbol just builds the root future.
            // Hand it to the runtime executor to drive to completion.
            let call = b.ins().call(main_ref, &[]);
            let fut = b.inst_results(call)[0];
            let ptid = b.ins().iconst(types::I64, pending_tid);
            let bo_ref = module.declare_func_in_func(
                block_on_id.expect("block_on declared when main is async"),
                b.func,
            );
            b.ins().call(bo_ref, &[fut, ptid]);
        } else {
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
/// closure's analysis, AST body, and the enclosing instance's substitution.
struct ClosureJob {
    func_id: FuncId,
    info: compiler::sema::results::ClosureInfo,
    body: Expr,
    subst: HashMap<DefId, Ty>,
    span: Span,
}

/// A bare `async { … }` block or `async` closure awaiting `poll`-function
/// generation: its Cranelift id, analysis, AST body, substitution, and the
/// future `Output` type.
struct AsyncJob {
    poll_fid: FuncId,
    info: compiler::sema::results::AsyncInfo,
    body: Expr,
    subst: HashMap<DefId, Ty>,
    span: Span,
    out: Ty,
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
    awaits: HashMap<Span, (i64, cranelift_codegen::ir::Block, cranelift_codegen::ir::Block)>,
    /// Shared block that builds a `Pending` result and returns it.
    pending_block: cranelift_codegen::ir::Block,
}

struct Codegen<'a, M: Module> {
    analysis: &'a Analysis,
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
    /// Captured GC safepoints: `(func, call code offset, frame_to_fp, ref SP
    /// offsets)`, registered with the runtime after linking.
    safepoints: Vec<(FuncId, u32, u32, Vec<u32>)>,
}

impl<'a, M: Module> Codegen<'a, M> {
    /// Declare every non-generic function/method as a `[]`-instance; generic
    /// templates are instantiated on demand from their call sites.
    /// Collect `(type_id, drop FuncId)` for every non-generic type with a `Drop`
    /// impl (`docs/16` §8). `seed` already compiled their `drop` methods as
    /// `[]`-instances. (Generic `Drop` types are a follow-up.)
    fn collect_drops(&self) -> Vec<(i64, FuncId)> {
        let drop_def = self.analysis.program.drop_def;
        if drop_def == DefId(0) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (&(type_def, iface_def), &extend_def) in &self.analysis.results.iface_impls {
            if iface_def != drop_def {
                continue;
            }
            let drop_method = (0..self.analysis.program.defs.len() as u32).map(DefId).find(|&d| {
                let def = self.analysis.program.def(d);
                def.kind == DefKind::ExtendMethod
                    && def.parent == Some(extend_def)
                    && def.name == "drop"
            });
            if let Some(dm) = drop_method {
                if let Some(&fid) = self.funcs.get(&(dm, Vec::new())) {
                    out.push((1000 + type_def.index() as i64, fid));
                }
            }
        }
        out
    }

    fn seed(&mut self) -> CgResult<()> {
        for (i, def) in self.analysis.program.defs.iter().enumerate() {
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
            let Some(ItemKind::Function(f)) = &def.item else { continue };
            if f.body.is_none() {
                continue;
            }
            let did = DefId(i as u32);
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
            match self.async_jobs.pop() {
                Some(job) => self.define_async_job(job)?,
                None => break,
            }
        }
        Ok(())
    }

    fn define_instance(&mut self, inst: Instance) -> CgResult<()> {
        let (def, args) = inst;
        let func_id = self.funcs[&(def, args.clone())];
        let Some(ItemKind::Function(f)) = self.analysis.program.def(def).item.clone() else {
            return Ok(());
        };
        // An async function's body yields the future `Output`, not a `Future`;
        // it lowers to a `Future` state machine (`docs/21`): the function named
        // `func_id` becomes a *constructor* that allocates the machine, and a
        // separate `poll` function runs the body.
        if let Some(&out) = self.analysis.results.async_fns.get(&def) {
            let Some(body) = f.body.clone() else { return Ok(()) };
            return self.define_async_fn(def, args, func_id, &body, out);
        }
        let Some(body) = f.body.clone() else { return Ok(()) };

        let subst = build_subst(self.analysis, def, &args);
        let mut ctx = self.module.make_context();
        ctx.func.signature =
            signature_of(self.module, self.analysis, def, &subst)?.expect("declared sig");
        let mut fctx = FunctionBuilderContext::new();

        let ret_ty = self.analysis.results.fn_return.get(&def).copied()
            .unwrap_or(self.analysis.tcx.null);
        let param_locals =
            self.analysis.results.fn_params.get(&def).cloned().unwrap_or_default();

        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let param_vals: Vec<Value> = b.block_params(entry).to_vec();

            {
                let mut fg = FnGen {
                    cx: CgShared { analysis: self.analysis },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    subst,
                    b: &mut b,
                    vars: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty,
                    async_out: None,
                    async_ctx: None,
                };
                for (i, local) in param_locals.iter().enumerate() {
                    let ty = fg.cx.analysis.results.local_ty(*local).unwrap();
                    let ct = fg.cx_clty(ty).expect("param clty");
                    let var = fg.fresh_var(*local, ct);
                    fg.b.def_var(var, param_vals[i]);
                }
                let val = fg.gen_block(&body)?;
                fg.emit_return(val)?;
            }
            b.seal_all_blocks();
            b.finalize();
        }

        self.module.define_function(func_id, &mut ctx)
            .map_err(|e| CodegenError::new(self.analysis.program.def(def).span,
                format!("define: {e}")))?;

        // Capture this function's GC safepoints: the SP-relative offsets of
        // live references at each call, plus FP→bottom-of-frame, for the
        // runtime's precise root scan.
        if let Some(cc) = ctx.compiled_code() {
            let frame_to_fp =
                cc.buffer.frame_layout().map(|fl| fl.frame_to_fp_offset).unwrap_or(0);
            for (code_offset, _span, map) in cc.buffer.user_stack_maps() {
                let offsets: Vec<u32> = map.entries().map(|(_, off)| off).collect();
                if !offsets.is_empty() {
                    self.safepoints.push((func_id, *code_offset, frame_to_fp, offsets));
                }
            }
        }
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Define a lifted closure function: `(env, params…) -> ret`. Captured
    /// locals are loaded from the environment; parameters come from the block.
    fn define_closure(&mut self, job: ClosureJob) -> CgResult<()> {
        let ClosureJob { func_id, info, body, subst, span } = job;
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
        let mut fctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let block_params: Vec<Value> = b.block_params(entry).to_vec();
            {
                let mut fg = FnGen {
                    cx: CgShared { analysis: self.analysis },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    subst,
                    b: &mut b,
                    vars: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty,
                    async_out: None,
                    async_ctx: None,
                };
                let env = block_params[0];
                // Captures live in the env after the function pointer (offset 8).
                for (k, (local, ty)) in info.captures.iter().enumerate() {
                    let ct = fg.cx_clty(*ty).expect("capture clty");
                    let off = (8 + k * 8) as i32;
                    let loaded = fg.b.ins().load(ct, MemFlags::trusted(), env, off);
                    let var = fg.fresh_var(*local, ct);
                    fg.b.def_var(var, loaded);
                }
                for (i, (local, ty)) in info.params.iter().enumerate() {
                    let ct = fg.cx_clty(*ty).expect("param clty");
                    let var = fg.fresh_var(*local, ct);
                    fg.b.def_var(var, block_params[i + 1]);
                }
                let val = fg.gen_expr(&body)?;
                fg.emit_return(val)?;
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(func_id, &mut ctx)
            .map_err(|e| CodegenError::new(span, format!("define closure: {e}")))?;
        if let Some(cc) = ctx.compiled_code() {
            let frame_to_fp =
                cc.buffer.frame_layout().map(|fl| fl.frame_to_fp_offset).unwrap_or(0);
            for (code_offset, _span, map) in cc.buffer.user_stack_maps() {
                let offsets: Vec<u32> = map.entries().map(|(_, off)| off).collect();
                if !offsets.is_empty() {
                    self.safepoints.push((func_id, *code_offset, frame_to_fp, offsets));
                }
            }
        }
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Capture a just-compiled function's GC safepoints (the SP offsets of live
    /// references at each call) for the runtime's precise root scan.
    fn capture_safepoints(&mut self, func_id: FuncId, ctx: &cranelift_codegen::Context) {
        if let Some(cc) = ctx.compiled_code() {
            let frame_to_fp =
                cc.buffer.frame_layout().map(|fl| fl.frame_to_fp_offset).unwrap_or(0);
            for (code_offset, _span, map) in cc.buffer.user_stack_maps() {
                let offsets: Vec<u32> = map.entries().map(|(_, off)| off).collect();
                if !offsets.is_empty() {
                    self.safepoints.push((func_id, *code_offset, frame_to_fp, offsets));
                }
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
        body: &Block,
        out: Ty,
    ) -> CgResult<()> {
        // A body containing `await` needs the full suspension state machine;
        // `await`-free bodies use the simpler path below.
        if block_has_await(body) {
            return self.define_async_fn_stateful(def, args, ctor_fid, body, out);
        }

        let subst = build_subst(self.analysis, def, &args);
        let param_locals =
            self.analysis.results.fn_params.get(&def).cloned().unwrap_or_default();

        // State struct layout: [state @0][param0 @8][param1 @16]… Managed params
        // are GC-traced. (Body locals live in the poll function's own frame in
        // this no-`await` slice; they move into the struct when `await` lands.)
        let mut param_cltys = Vec::with_capacity(param_locals.len());
        let mut ptr_offsets = Vec::new();
        for (i, local) in param_locals.iter().enumerate() {
            let ty = self.analysis.results.local_ty(*local).unwrap_or(self.analysis.tcx.error);
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
        let poll_fid = self.module
            .declare_function(&poll_name, Linkage::Local, &poll_sig)
            .map_err(|e| CodegenError::new(self.analysis.program.def(def).span,
                format!("declare poll: {e}")))?;

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
                    cx: CgShared { analysis: self.analysis },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: Some(out),
                    async_ctx: None,
                };
                // The state struct holds GC roots and must stay live across the
                // body's allocations.
                fg.mark_root(self_val);
                // Load each argument from the state struct into its local.
                for (i, local) in param_locals.iter().enumerate() {
                    if let Some(ct) = param_cltys[i] {
                        let off = (8 + i * 8) as i32;
                        let loaded = fg.b.ins().load(ct, MemFlags::trusted(), self_val, off);
                        let var = fg.fresh_var(*local, ct);
                        fg.b.def_var(var, loaded);
                    }
                }
                let val = fg.gen_block(body)?;
                fg.emit_return(val)?;
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(poll_fid, &mut ctx)
            .map_err(|e| CodegenError::new(self.analysis.program.def(def).span,
                format!("define poll: {e}")))?;
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
                    cx: CgShared { analysis: self.analysis },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: None,
                    async_ctx: None,
                };
                // Managed arguments must survive the state allocation (a
                // safepoint) before they are stored.
                for (i, v) in pvals.iter().enumerate() {
                    if ptr_offsets.contains(&((8 + i * 8) as u32)) {
                        fg.mark_root(*v);
                    }
                }
                let desc = fg.emit_descriptor(state_size, GC_KIND_PLAIN, &ptr_offsets);
                let state = fg.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
                    .expect("lang_alloc returns a pointer");
                let zero = fg.b.ins().iconst(types::I64, 0);
                fg.b.ins().store(MemFlags::trusted(), zero, state, 0);
                for (i, v) in pvals.iter().enumerate() {
                    fg.b.ins().store(MemFlags::trusted(), *v, state, (8 + i * 8) as i32);
                }
                let fut = fg.emit_future_box(poll_fid, state);
                fg.b.ins().return_(&[fut]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(ctor_fid, &mut cctx)
            .map_err(|e| CodegenError::new(self.analysis.program.def(def).span,
                format!("define async ctor: {e}")))?;
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
        body: &Block,
        entry_set: &HashSet<LocalId>,
        live: &[(LocalId, i32, ClType)],
        err_span: Span,
    ) -> CgResult<()> {
        let mut await_spans = Vec::new();
        scan_stmt_awaits(body, &mut await_spans);

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
            let mut awaits: HashMap<Span, (i64, cranelift_codegen::ir::Block, cranelift_codegen::ir::Block)>
                = HashMap::new();
            for (k, sp) in await_spans.iter().enumerate() {
                let pb = b.create_block();
                let rb = b.create_block();
                awaits.insert(*sp, ((k + 1) as i64, pb, rb));
            }
            {
                let mut fg = FnGen {
                    cx: CgShared { analysis: self.analysis },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: Some(out),
                    async_ctx: None,
                };
                fg.mark_root(self_val);
                for (l, _off, ct) in live {
                    fg.fresh_var(*l, *ct);
                }
                // Entry dispatch: resume at the block matching `state`, else start.
                let state_v = fg.b.ins().load(types::I64, MemFlags::trusted(), self_val, 0);
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
                    self_val, ctx_val, inner_off: ASYNC_INNER_OFF, save_locals,
                    awaits: awaits.clone(), pending_block,
                });
                let val = fg.gen_block(body)?;
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
        self.module.define_function(poll_fid, &mut ctx)
            .map_err(|e| CodegenError::new(err_span, format!("define poll: {e}")))?;
        self.capture_safepoints(poll_fid, &ctx);
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
        body: &Block,
        out: Ty,
    ) -> CgResult<()> {
        let subst = build_subst(self.analysis, def, &args);
        let param_locals =
            self.analysis.results.fn_params.get(&def).cloned().unwrap_or_default();

        // Lay out the state struct and build the poll function.
        let layout = async_state_layout(self.analysis, &subst, &param_locals, body);
        let entry_set: HashSet<LocalId> = param_locals.iter().copied().collect();
        // Parameter values are stored by the constructor into these slots.
        let param_offs: Vec<i32> =
            param_locals.iter().map(|l| layout.slot_off[l]).collect();
        let state_size = layout.state_size;
        let ptr_offsets = layout.ptr_offsets.clone();
        let span = self.analysis.program.def(def).span;

        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.params.push(AbiParam::new(PTR));
        poll_sig.returns.push(AbiParam::new(PTR));
        let poll_name = format!("{}$poll", mangle(self.analysis, def, &args));
        let poll_fid = self.module
            .declare_function(&poll_name, Linkage::Local, &poll_sig)
            .map_err(|e| CodegenError::new(span, format!("declare poll: {e}")))?;
        self.build_stateful_poll(poll_fid, &subst, out, body, &entry_set, &layout.live, span)?;

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
                    cx: CgShared { analysis: self.analysis },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    subst: subst.clone(),
                    b: &mut b,
                    vars: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: None,
                    async_ctx: None,
                };
                // Managed arguments must survive the state allocation.
                for (i, v) in pvals.iter().enumerate() {
                    if ptr_offsets.contains(&(param_offs[i] as u32)) {
                        fg.mark_root(*v);
                    }
                }
                let desc = fg.emit_descriptor(state_size, GC_KIND_PLAIN, &ptr_offsets);
                let state = fg.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
                    .expect("lang_alloc returns a pointer");
                let zero = fg.b.ins().iconst(types::I64, 0);
                fg.b.ins().store(MemFlags::trusted(), zero, state, 0);
                for (i, v) in pvals.iter().enumerate() {
                    fg.b.ins().store(MemFlags::trusted(), *v, state, param_offs[i]);
                }
                let fut = fg.emit_future_box(poll_fid, state);
                fg.b.ins().return_(&[fut]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(ctor_fid, &mut cctx)
            .map_err(|e| CodegenError::new(self.analysis.program.def(def).span,
                format!("define async ctor: {e}")))?;
        self.capture_safepoints(ctor_fid, &cctx);
        self.module.clear_context(&mut cctx);
        Ok(())
    }

    /// Define the `poll` function of a bare `async { … }` block: load its
    /// captured locals from the state struct, run the body, and return the
    /// result wrapped in `Ready<Output> | Pending` (`docs/21`).
    fn define_async_job(&mut self, job: AsyncJob) -> CgResult<()> {
        let AsyncJob { poll_fid, info, body, subst, span, out } = job;
        // A block containing `await` is a suspendable state machine; its
        // captures are the entry locals (pre-stored by `gen_async_block`).
        if let ExprKind::Block(block) = &body.kind {
            if block_has_await(block) {
                let cap_ids: Vec<LocalId> = info.captures.iter().map(|(l, _)| *l).collect();
                let layout = async_state_layout(self.analysis, &subst, &cap_ids, block);
                let entry_set: HashSet<LocalId> = cap_ids.into_iter().collect();
                return self.build_stateful_poll(
                    poll_fid, &subst, out, block, &entry_set, &layout.live, span,
                );
            }
        }
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
                    cx: CgShared { analysis: self.analysis },
                    module: self.module,
                    funcs: &mut self.funcs,
                    worklist: &mut self.worklist,
                    closures: &mut self.closures,
                    async_jobs: &mut self.async_jobs,
                    subst,
                    b: &mut b,
                    vars: HashMap::new(),
                    term: false,
                    loops: Vec::new(),
                    ret_ty: out,
                    async_out: Some(out),
                    async_ctx: None,
                };
                fg.mark_root(self_val);
                // Captures live in the state struct after the state word (@8).
                for (k, (local, ty)) in info.captures.iter().enumerate() {
                    if let Some(ct) = fg.cx_clty(*ty) {
                        let off = (8 + k * 8) as i32;
                        let loaded = fg.b.ins().load(ct, MemFlags::trusted(), self_val, off);
                        let var = fg.fresh_var(*local, ct);
                        fg.b.def_var(var, loaded);
                    }
                }
                let val = fg.gen_expr(&body)?;
                fg.emit_return(val)?;
            }
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(poll_fid, &mut ctx)
            .map_err(|e| CodegenError::new(span, format!("define async block poll: {e}")))?;
        self.capture_safepoints(poll_fid, &ctx);
        self.module.clear_context(&mut ctx);
        Ok(())
    }
}

struct CgShared<'a> {
    analysis: &'a Analysis,
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
    /// This instance's generic-parameter substitution.
    subst: HashMap<DefId, Ty>,
    b: &'b mut FunctionBuilder<'f>,
    /// Language local → Cranelift variable.
    vars: HashMap<LocalId, Variable>,
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
        type_id(self.cx.analysis, resolve_shallow(self.cx.analysis, ty, &self.subst))
    }

    pub(crate) fn fresh_var(&mut self, local: LocalId, ct: ClType) -> Variable {
        let var = self.b.declare_var(ct);
        // Managed-pointer locals are GC roots: Cranelift records them in the
        // precise stack map at each safepoint (call).
        if let Some(ty) = self.cx.analysis.results.local_ty(local) {
            let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
            if is_managed_ptr(self.cx.analysis, resolved) {
                self.b.declare_var_needs_stack_map(var);
            }
        }
        self.vars.insert(local, var);
        var
    }

    /// Declare `v` a GC root: Cranelift records it in the precise stack map at
    /// every safepoint where it is live. Use for managed-pointer temporaries
    /// that outlive a later allocation but are not themselves `gen_expr`
    /// results (which `gen_expr` already marks).
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
        // In an async `poll` body, a return value is the future's `Output`; wrap
        // it in `Ready<Output>` and box into the `Ready<Output> | Pending` union
        // the `poll` ABI returns (`docs/21` §1).
        if let Some(out) = self.async_out {
            let boxed = self.box_ready(val, out);
            self.b.ins().return_(&[boxed]);
            self.term = true;
            return Ok(());
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
