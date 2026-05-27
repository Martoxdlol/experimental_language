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

use compiler::sema::StructFields;
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

/// Pointer-width integer type on the host (str/reference values are pointers).
/// The JIT only targets the 64-bit host, so this is `I64`.
const PTR: ClType = types::I64;

/// Descriptor `kind` for a plain object (scan its listed pointer offsets).
/// Mirrors `runtime::gc::KIND_PLAIN`.
const GC_KIND_PLAIN: u64 = 0;

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
    b.symbol("lang_thread_spawn", runtime::threads::lang_thread_spawn as *const u8);
    b.symbol("lang_thread_join", runtime::threads::lang_thread_join as *const u8);
    b.symbol("lang_thread_panicked", runtime::threads::lang_thread_panicked as *const u8);
    b.symbol("lang_thread_message", runtime::threads::lang_thread_message as *const u8);
    b.symbol("lang_channel_new", runtime::channels::lang_channel_new as *const u8);
    b.symbol("lang_chan_send", runtime::channels::lang_chan_send as *const u8);
    b.symbol("lang_chan_recv", runtime::channels::lang_chan_recv as *const u8);
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

type CgResult<T> = Result<T, CodegenError>;

/// A JIT-compiled program. Owns the executable memory; function pointers are
/// valid for as long as it lives.
pub struct Jit {
    module: JITModule,
    /// Language function name → its Cranelift id.
    funcs: HashMap<String, FuncId>,
}

impl Jit {
    /// Raw code pointer for a compiled function by language name.
    pub fn func_ptr(&self, name: &str) -> Option<*const u8> {
        self.funcs.get(name).map(|id| self.module.get_finalized_function(*id))
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

    Ok(Jit { module, funcs: by_name })
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

    emit_native_entry(&mut module, user_main, &safepoints, &drops)?;

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
        b.ins().call(main_ref, &[]);

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
type Instance = (DefId, Vec<Ty>);

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

/// The generic-parameter → argument substitution for an instance.
// -- async body analysis (state-machine lowering, `docs/21`) ----------------

/// Whether `block` contains an `await` in its own async scope (NOT descending
/// into nested closures / `async { … }` blocks, which have their own `poll`).
fn block_has_await(block: &Block) -> bool {
    block.stmts.iter().any(stmt_has_await)
        || block.trailing.as_deref().is_some_and(expr_has_await)
}

fn stmt_has_await(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Var(v) => expr_has_await(&v.init),
        StmtKind::Assign { target, value } => expr_has_await(target) || expr_has_await(value),
        StmtKind::Expr(e) => expr_has_await(e),
        StmtKind::Item(_) => false,
    }
}

fn expr_has_await(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Await { .. } => true,
        // Nested async scopes have their own poll function — do not descend.
        ExprKind::Closure { .. } | ExprKind::AnonFn(_) | ExprKind::AsyncBlock(_) => false,
        ExprKind::Paren(x) | ExprKind::Unary { operand: x, .. }
        | ExprKind::Cast { expr: x, .. } | ExprKind::Field { receiver: x, .. }
        | ExprKind::TupleIndex { receiver: x, .. } | ExprKind::Try { expr: x, .. }
        | ExprKind::Ref { expr: x, .. } | ExprKind::Deref { expr: x, .. } => expr_has_await(x),
        ExprKind::Binary { left, right, .. } => expr_has_await(left) || expr_has_await(right),
        ExprKind::Tuple(xs) | ExprKind::List(xs) => xs.iter().any(expr_has_await),
        ExprKind::Call { callee, args, trailing_closure, .. } => {
            expr_has_await(callee)
                || args.iter().any(expr_has_await)
                || trailing_closure.as_deref().is_some_and(expr_has_await)
        }
        ExprKind::Index { receiver, index } => expr_has_await(receiver) || expr_has_await(index),
        ExprKind::StructLit { fields, spread, .. } => {
            fields.iter().any(|f| f.value.as_ref().is_some_and(expr_has_await))
                || spread.as_deref().is_some_and(expr_has_await)
        }
        ExprKind::MapLit(items) => items.iter().any(|it| match it {
            MapItem::Entry { key, value, .. } => expr_has_await(key) || expr_has_await(value),
            MapItem::Spread(e) => expr_has_await(e),
        }),
        ExprKind::If { cond, then_block, else_branch } => {
            expr_has_await(cond) || block_has_await(then_block)
                || match else_branch {
                    Some(ElseBranch::If(e)) => expr_has_await(e),
                    Some(ElseBranch::Block(b)) => block_has_await(b),
                    None => false,
                }
        }
        ExprKind::Match { scrutinee, arms } => {
            expr_has_await(scrutinee)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(expr_has_await) || expr_has_await(&a.body)
                })
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => block_has_await(b),
        ExprKind::While { cond, body } => expr_has_await(cond) || block_has_await(body),
        ExprKind::For { in_async, iter, body, .. } => {
            *in_async || expr_has_await(iter) || block_has_await(body)
        }
        ExprKind::Return(v) | ExprKind::Break(v) => v.as_deref().is_some_and(expr_has_await),
        _ => false,
    }
}

/// Record the local a binding-site `span` resolves to (deduped, in order).
fn push_local(a: &Analysis, span: Span, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    if let Some(ValueRes::Local(id)) = a.results.resolution(span) {
        if seen.insert(id) {
            out.push(id);
        }
    }
}

/// Enumerate every local *binding* introduced in `block` (so an async state
/// struct can reserve a slot for each), NOT descending into nested closures /
/// `async { … }` blocks (their locals live in their own frames).
fn collect_block_locals(a: &Analysis, block: &Block, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    for s in &block.stmts {
        collect_stmt_locals(a, s, out, seen);
    }
    if let Some(t) = &block.trailing {
        collect_expr_locals(a, t, out, seen);
    }
}

fn collect_stmt_locals(a: &Analysis, s: &Stmt, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    match &s.kind {
        StmtKind::Var(v) => {
            collect_pat_locals(a, &v.pattern, out, seen);
            collect_expr_locals(a, &v.init, out, seen);
        }
        StmtKind::Assign { target, value } => {
            collect_expr_locals(a, target, out, seen);
            collect_expr_locals(a, value, out, seen);
        }
        StmtKind::Expr(e) => collect_expr_locals(a, e, out, seen),
        StmtKind::Item(_) => {}
    }
}

fn collect_pat_locals(a: &Analysis, p: &Pattern, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    match &p.kind {
        PatternKind::Binding(name) => push_local(a, name.span, out, seen),
        PatternKind::TypeBinding { binding: Some(name), .. } => push_local(a, name.span, out, seen),
        PatternKind::TupleStruct { fields, rest, .. } => {
            for f in fields {
                collect_pat_locals(a, f, out, seen);
            }
            if let Some(r) = rest {
                if let Some(n) = &r.name {
                    push_local(a, n.span, out, seen);
                }
            }
        }
        PatternKind::RecordStruct { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(sub) => collect_pat_locals(a, sub, out, seen),
                    None => push_local(a, f.name.span, out, seen), // shorthand binds the field
                }
            }
        }
        PatternKind::Tuple { elems, rest } | PatternKind::List { elems, rest } => {
            for e in elems {
                collect_pat_locals(a, e, out, seen);
            }
            if let Some((_, r)) = rest {
                if let Some(n) = &r.name {
                    push_local(a, n.span, out, seen);
                }
            }
        }
        PatternKind::Or(ps) => {
            for sub in ps {
                collect_pat_locals(a, sub, out, seen);
            }
        }
        _ => {}
    }
}

fn collect_expr_locals(a: &Analysis, e: &Expr, out: &mut Vec<LocalId>, seen: &mut HashSet<LocalId>) {
    match &e.kind {
        // Nested async/closure scopes own their locals.
        ExprKind::Closure { .. } | ExprKind::AnonFn(_) | ExprKind::AsyncBlock(_) => {}
        ExprKind::Paren(x) | ExprKind::Unary { operand: x, .. }
        | ExprKind::Cast { expr: x, .. } | ExprKind::Field { receiver: x, .. }
        | ExprKind::TupleIndex { receiver: x, .. } | ExprKind::Try { expr: x, .. }
        | ExprKind::Ref { expr: x, .. } | ExprKind::Deref { expr: x, .. }
        | ExprKind::Await { expr: x, .. } => collect_expr_locals(a, x, out, seen),
        ExprKind::Binary { left, right, .. } => {
            collect_expr_locals(a, left, out, seen);
            collect_expr_locals(a, right, out, seen);
        }
        ExprKind::Tuple(xs) | ExprKind::List(xs) => {
            for x in xs {
                collect_expr_locals(a, x, out, seen);
            }
        }
        ExprKind::Call { callee, args, trailing_closure, .. } => {
            collect_expr_locals(a, callee, out, seen);
            for x in args {
                collect_expr_locals(a, x, out, seen);
            }
            if let Some(tc) = trailing_closure {
                collect_expr_locals(a, tc, out, seen);
            }
        }
        ExprKind::Index { receiver, index } => {
            collect_expr_locals(a, receiver, out, seen);
            collect_expr_locals(a, index, out, seen);
        }
        ExprKind::StructLit { fields, spread, .. } => {
            for f in fields {
                if let Some(v) = &f.value {
                    collect_expr_locals(a, v, out, seen);
                }
            }
            if let Some(s) = spread {
                collect_expr_locals(a, s, out, seen);
            }
        }
        ExprKind::MapLit(items) => {
            for it in items {
                match it {
                    MapItem::Entry { key, value, .. } => {
                        collect_expr_locals(a, key, out, seen);
                        collect_expr_locals(a, value, out, seen);
                    }
                    MapItem::Spread(x) => collect_expr_locals(a, x, out, seen),
                }
            }
        }
        ExprKind::If { cond, then_block, else_branch } => {
            collect_expr_locals(a, cond, out, seen);
            collect_block_locals(a, then_block, out, seen);
            match else_branch {
                Some(ElseBranch::If(x)) => collect_expr_locals(a, x, out, seen),
                Some(ElseBranch::Block(b)) => collect_block_locals(a, b, out, seen),
                None => {}
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_expr_locals(a, scrutinee, out, seen);
            for arm in arms {
                collect_pat_locals(a, &arm.pattern, out, seen);
                if let Some(g) = &arm.guard {
                    collect_expr_locals(a, g, out, seen);
                }
                collect_expr_locals(a, &arm.body, out, seen);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => collect_block_locals(a, b, out, seen),
        ExprKind::While { cond, body } => {
            collect_expr_locals(a, cond, out, seen);
            collect_block_locals(a, body, out, seen);
        }
        ExprKind::For { pattern, iter, body, .. } => {
            collect_pat_locals(a, pattern, out, seen);
            collect_expr_locals(a, iter, out, seen);
            collect_block_locals(a, body, out, seen);
        }
        ExprKind::Return(v) | ExprKind::Break(v) => {
            if let Some(x) = v {
                collect_expr_locals(a, x, out, seen);
            }
        }
        _ => {}
    }
}

/// Collect the spans of `await`s that appear in a *statement-level* position
/// (the whole RHS of a `var`/assignment, a bare expression statement, a block's
/// trailing expression, or `return`) — the positions where no sibling
/// sub-expression temporary is live across the suspension point, so saving and
/// restoring named locals alone is correct. Recurses through control-flow
/// bodies. Awaits elsewhere are not collected (and are rejected at codegen)
/// until ANF hoisting lands.
fn scan_stmt_awaits(block: &Block, out: &mut Vec<Span>) {
    for s in &block.stmts {
        match &s.kind {
            StmtKind::Var(v) => scan_value_await(&v.init, out),
            StmtKind::Assign { value, .. } => scan_value_await(value, out),
            StmtKind::Expr(e) => scan_value_await(e, out),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(t) = &block.trailing {
        scan_value_await(t, out);
    }
}

fn scan_value_await(e: &Expr, out: &mut Vec<Span>) {
    match &e.kind {
        ExprKind::Await { kw_span, .. } => out.push(*kw_span),
        ExprKind::Paren(x) | ExprKind::Return(Some(x)) | ExprKind::Break(Some(x)) => {
            scan_value_await(x, out)
        }
        ExprKind::Block(b) | ExprKind::Loop(b) => scan_stmt_awaits(b, out),
        ExprKind::While { body, .. } => scan_stmt_awaits(body, out),
        ExprKind::For { in_async, iter, body, .. } => {
            // `for await` introduces one suspend site (the `next_async()` await),
            // keyed by the iterable span.
            if *in_async {
                out.push(iter.span);
            }
            scan_stmt_awaits(body, out);
        }
        ExprKind::If { then_block, else_branch, .. } => {
            scan_stmt_awaits(then_block, out);
            match else_branch {
                Some(ElseBranch::If(x)) => scan_value_await(x, out),
                Some(ElseBranch::Block(b)) => scan_stmt_awaits(b, out),
                None => {}
            }
        }
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                scan_value_await(&arm.body, out);
            }
        }
        _ => {}
    }
}

/// The state-struct layout for an async body that suspends: `[state @0][inner
/// future @8][local_i @16 + i*8]` over every body local (the `entry` locals —
/// parameters or captures — come first, since the constructor pre-stores them).
struct AsyncLayout {
    /// Local → byte offset within the state struct.
    slot_off: HashMap<LocalId, i32>,
    /// Locals that carry a runtime value (with offset + Cranelift type) — the
    /// ones the state machine saves and restores.
    live: Vec<(LocalId, i32, ClType)>,
    /// Managed-pointer field offsets for the GC descriptor (includes `inner`).
    ptr_offsets: Vec<u32>,
    /// Total state-struct size in bytes.
    state_size: u32,
}

/// Offset of the suspended-inner-future slot in every async state struct.
const ASYNC_INNER_OFF: i32 = 8;

fn async_state_layout(
    analysis: &Analysis,
    subst: &HashMap<DefId, Ty>,
    entry: &[LocalId],
    body: &Block,
) -> AsyncLayout {
    let mut all_locals = entry.to_vec();
    let mut seen: HashSet<LocalId> = all_locals.iter().copied().collect();
    collect_block_locals(analysis, body, &mut all_locals, &mut seen);
    let mut slot_off = HashMap::new();
    let mut ptr_offsets = vec![ASYNC_INNER_OFF as u32]; // the inner-future slot is managed
    let mut live = Vec::new();
    for (i, l) in all_locals.iter().enumerate() {
        let off = (16 + i * 8) as i32;
        slot_off.insert(*l, off);
        let ty = analysis.results.local_ty(*l).unwrap_or(analysis.tcx.error);
        let resolved = resolve_shallow(analysis, ty, subst);
        if let Some(ct) = clty_of(analysis, resolved) {
            live.push((*l, off, ct));
            if is_managed_ptr(analysis, resolved) {
                ptr_offsets.push(off as u32);
            }
        }
    }
    let state_size = (16 + all_locals.len() * 8) as u32;
    AsyncLayout { slot_off, live, ptr_offsets, state_size }
}

fn build_subst(analysis: &Analysis, def: DefId, args: &[Ty]) -> HashMap<DefId, Ty> {
    let prog = &analysis.program;
    // For an `extend`/interface method, the enclosing block's generics (e.g. the
    // `T` of `extend<T> Pair<T>`) come first; the method's own generics follow.
    let mut params: Vec<DefId> = Vec::new();
    if let Some(parent) = prog.def(def).parent {
        if matches!(prog.def(parent).kind, DefKind::Extend) {
            params.extend(prog.def(parent).generics.iter().copied());
        }
    }
    params.extend(prog.def(def).generics.iter().copied());
    params.into_iter().zip(args.iter().copied()).collect()
}

/// A unique Cranelift symbol for an instance: name, def id, and arg type ids.
fn mangle(analysis: &Analysis, def: DefId, args: &[Ty]) -> String {
    let mut s = format!("{}${}", analysis.program.def(def).name, def.index());
    for a in args {
        s.push('_');
        s.push_str(&type_id(analysis, *a).to_string());
    }
    s
}

/// Shallow-substitute a top-level `Param` (sufficient for clty/type_id/layout).
fn resolve_shallow(analysis: &Analysis, ty: Ty, subst: &HashMap<DefId, Ty>) -> Ty {
    if subst.is_empty() {
        return ty;
    }
    match analysis.tcx.kind(ty) {
        TyKind::Param(d) => subst.get(d).copied().unwrap_or(ty),
        _ => ty,
    }
}

fn clty_subst(analysis: &Analysis, ty: Ty, subst: &HashMap<DefId, Ty>) -> Option<ClType> {
    clty_of(analysis, resolve_shallow(analysis, ty, subst))
}

/// Build a function/method instance's Cranelift signature under `subst`, or
/// `None` if a parameter type is not lowerable.
fn signature_of(
    module: &mut impl Module,
    analysis: &Analysis,
    def: DefId,
    subst: &HashMap<DefId, Ty>,
) -> CgResult<Option<cranelift_codegen::ir::Signature>> {
    let results = &analysis.results;
    let ret = results.fn_return.get(&def).copied().unwrap_or(analysis.tcx.null);
    let params = results.fn_params.get(&def).cloned().unwrap_or_default();
    let mut sig = module.make_signature();
    for p in &params {
        let ty = results.local_ty(*p).unwrap_or(analysis.tcx.error);
        match clty_subst(analysis, ty, subst) {
            Some(ct) => sig.params.push(AbiParam::new(ct)),
            None => return Ok(None),
        }
    }
    if let Some(ct) = clty_subst(analysis, ret, subst) {
        sig.returns.push(AbiParam::new(ct));
    }
    Ok(Some(sig))
}

/// Declare an instance (idempotently), queuing it for definition. Returns
/// `None` if its signature is not lowerable.
fn declare_instance(
    module: &mut impl Module,
    funcs: &mut HashMap<Instance, FuncId>,
    worklist: &mut Vec<Instance>,
    analysis: &Analysis,
    def: DefId,
    args: Vec<Ty>,
) -> CgResult<Option<FuncId>> {
    let inst = (def, args);
    if let Some(&f) = funcs.get(&inst) {
        return Ok(Some(f));
    }
    let subst = build_subst(analysis, def, &inst.1);
    let Some(sig) = signature_of(module, analysis, def, &subst)? else {
        return Ok(None);
    };
    let name = mangle(analysis, def, &inst.1);
    let fid = module
        .declare_function(&name, Linkage::Export, &sig)
        .map_err(|e| CodegenError::new(analysis.program.def(def).span, format!("declare: {e}")))?;
    funcs.insert(inst.clone(), fid);
    worklist.push(inst);
    Ok(Some(fid))
}

/// Shared, immutable codegen context handed to the per-function generator.
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
    fn cx_clty(&self, ty: Ty) -> Option<ClType> {
        clty_subst(self.cx.analysis, ty, &self.subst)
    }

    /// Runtime type id of `ty` under this instance's substitution.
    fn type_id_of(&self, ty: Ty) -> i64 {
        type_id(self.cx.analysis, resolve_shallow(self.cx.analysis, ty, &self.subst))
    }

    fn fresh_var(&mut self, local: LocalId, ct: ClType) -> Variable {
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
    fn mark_root(&mut self, v: Value) -> Value {
        self.b.declare_value_needs_stack_map(v);
        v
    }

    /// Switch to `block`, resetting the termination flag for the new block.
    fn switch(&mut self, block: cranelift_codegen::ir::Block) {
        self.b.switch_to_block(block);
        self.term = false;
    }

    fn emit_return(&mut self, val: Option<Value>) -> CgResult<()> {
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

    // -- statements & blocks -------------------------------------------------

    fn gen_block(&mut self, block: &Block) -> CgResult<Option<Value>> {
        for stmt in &block.stmts {
            self.gen_stmt(stmt)?;
            if self.term {
                // Unreachable code after a diverging statement; stop.
                return Ok(None);
            }
        }
        match &block.trailing {
            Some(e) => self.gen_expr(e),
            None => Ok(None),
        }
    }

    fn gen_stmt(&mut self, stmt: &Stmt) -> CgResult<()> {
        match &stmt.kind {
            StmtKind::Var(local) => {
                let init_ty = self.cx.analysis.results.expr_ty(local.init.span)
                    .unwrap_or(self.cx.analysis.tcx.error);
                let val = self.gen_expr(&local.init)?;
                self.bind_pattern(&local.pattern, val, init_ty)?;
                Ok(())
            }
            StmtKind::Assign { target, value } => {
                let v = self.gen_expr(value)?;
                self.gen_assign(target, v)?;
                Ok(())
            }
            StmtKind::Expr(e) => {
                self.gen_expr(e)?;
                Ok(())
            }
            StmtKind::Item(_) => Ok(()),
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, val: Option<Value>, ty: Ty) -> CgResult<()> {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                if let Some(v) = val {
                    let ct = self.b.func.dfg.value_type(v);
                    let local = match self.cx.analysis.results.resolution(name.span) {
                        Some(ValueRes::Local(id)) => id,
                        _ => return Err(CodegenError::new(name.span, "unresolved binding")),
                    };
                    let var = self.fresh_var(local, ct);
                    self.b.def_var(var, v);
                }
                Ok(())
            }
            PatternKind::Wildcard => Ok(()),
            // Irrefutable tuple/struct destructuring: load each element from the
            // aggregate pointer and bind the sub-pattern.
            PatternKind::Tuple { elems, rest: None } => {
                let ptr = val.ok_or_else(|| {
                    CodegenError::new(pattern.span, "destructured value has no pointer")
                })?;
                let layout = self.layout_for_ty(ty).ok_or_else(|| {
                    CodegenError::new(pattern.span, "tuple pattern on non-aggregate")
                })?;
                let elem_tys = match self.cx.analysis.tcx.kind(ty).clone() {
                    TyKind::Tuple(ts) => ts,
                    _ => return Err(CodegenError::new(pattern.span, "tuple pattern on non-tuple")),
                };
                for (i, sub) in elems.iter().enumerate() {
                    let elem_val = match layout.cltys.get(i) {
                        Some(Some(ct)) => Some(self.b.ins().load(
                            *ct,
                            MemFlags::trusted(),
                            ptr,
                            layout.offsets[i] as i32,
                        )),
                        _ => None,
                    };
                    self.bind_pattern(sub, elem_val, elem_tys[i])?;
                }
                Ok(())
            }
            _ => Err(CodegenError::new(pattern.span, "pattern not yet lowerable")),
        }
    }

    fn gen_assign(&mut self, target: &Expr, val: Option<Value>) -> CgResult<()> {
        match &target.kind {
            ExprKind::Ident(_) => {
                let local = self.resolve_local(target.span)?;
                let var = self.vars.get(&local).copied().ok_or_else(|| {
                    CodegenError::new(target.span, "assignment to unbound local")
                })?;
                if let Some(v) = val {
                    self.b.def_var(var, v);
                }
                Ok(())
            }
            ExprKind::Underscore => Ok(()),
            ExprKind::Field { receiver, name } => {
                self.gen_field_store(receiver, &name.name, val)
            }
            ExprKind::TupleIndex { receiver, index, .. } => {
                self.gen_field_store(receiver, &index.to_string(), val)
            }
            ExprKind::Index { receiver, index } => self.gen_index_store(receiver, index, val),
            _ => Err(CodegenError::new(target.span, "assignment target not yet lowerable")),
        }
    }

    // -- expressions ---------------------------------------------------------

    /// Generate an expression, then apply any implicit coercion the checker
    /// recorded at its span (currently: widening into a union/`dynamic` box).
    fn gen_expr(&mut self, expr: &Expr) -> CgResult<Option<Value>> {
        let v = self.gen_expr_raw(expr)?;
        let v = self.apply_adjustment(expr.span, v)?;
        // A managed-ref result is a potential GC root if it stays live across a
        // later call (e.g. an outer struct pointer held while a field
        // expression allocates). Mark it so Cranelift records it in stack maps.
        if let Some(val) = v {
            if self.result_is_managed_ref(expr.span) {
                self.b.declare_value_needs_stack_map(val);
            }
        }
        Ok(v)
    }

    /// Whether the (post-adjustment) value produced at `span` is a managed
    /// pointer the GC must treat as a root.
    fn result_is_managed_ref(&self, span: Span) -> bool {
        let a = self.cx.analysis;
        match a.results.adjustment(span) {
            Some(Adjust::Widen(_)) => true, // a `{type_id,data}` box is managed
            Some(Adjust::WidenDyn(_)) => true, // a `{vtable,data}` box is managed
            Some(Adjust::Unbox(t)) => is_managed_ptr(a, resolve_shallow(a, t, &self.subst)),
            None => {
                let ty = a.results.expr_ty(span).unwrap_or(a.tcx.error);
                is_managed_ptr(a, resolve_shallow(a, ty, &self.subst))
            }
        }
    }

    /// Apply any coercion the checker recorded at `span` to a freshly produced
    /// value: box on widen, unbox on narrow.
    fn apply_adjustment(&mut self, span: Span, v: Option<Value>) -> CgResult<Option<Value>> {
        match self.cx.analysis.results.adjustment(span) {
            Some(Adjust::Widen(_)) => {
                // `expr_ty(span)` is the pre-widening ("raw") type, which equals
                // the target union only when the value is already boxed.
                let from = self.cx.analysis.results.expr_ty(span)
                    .unwrap_or(self.cx.analysis.tcx.error);
                Ok(Some(self.apply_widen(v, from)))
            }
            Some(Adjust::Unbox(target)) => {
                let ptr = v.expect("unbox target is a boxed pointer");
                match clty_of(self.cx.analysis, target) {
                    Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))),
                    None => Ok(None), // narrowed to `null`
                }
            }
            Some(Adjust::WidenDyn(iface)) => {
                let from = self.cx.analysis.results.expr_ty(span)
                    .unwrap_or(self.cx.analysis.tcx.error);
                Ok(Some(self.gen_widen_dyn(v, from, iface, span)?))
            }
            None => Ok(v),
        }
    }

    /// Wrap a concrete value into an interface object: allocate a managed
    /// `{vtable, data}` box and point its vtable at the (concrete-type,
    /// interface) method table.
    fn gen_widen_dyn(&mut self, v: Option<Value>, from: Ty, iface: Ty, span: Span)
        -> CgResult<Value>
    {
        let data = v.ok_or_else(|| CodegenError::new(span, "interface value has no data"))?;
        self.mark_root(data);
        let vtable = self.emit_vtable(from, iface, span)?;
        // box: [vtable: *const (unmanaged)][data: *managed][type_id: i64]
        // The type id supports `is`/`as` downcasts back to the concrete type.
        let desc = self.emit_descriptor(24, GC_KIND_PLAIN, &[8]);
        let ptr = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        self.b.ins().store(MemFlags::trusted(), vtable, ptr, 0);
        self.b.ins().store(MemFlags::trusted(), data, ptr, 8);
        let tid = self.type_id_of(from);
        let tid_v = self.b.ins().iconst(types::I64, tid);
        self.b.ins().store(MemFlags::trusted(), tid_v, ptr, 16);
        Ok(ptr)
    }

    /// Build (or reference) the vtable for `(concrete type, interface)`: a data
    /// object of one function pointer per interface method, in declaration
    /// order, each pointing at the concrete type's monomorphized impl. Returns
    /// the vtable's address.
    fn emit_vtable(&mut self, concrete: Ty, iface: Ty, span: Span) -> CgResult<Value> {
        let analysis = self.cx.analysis;
        let concrete = resolve_shallow(analysis, concrete, &self.subst);
        let TyKind::Named { def: cdef, args: cargs } = analysis.tcx.kind(concrete).clone() else {
            return Err(CodegenError::new(span, "interface data is not a nominal type"));
        };
        let TyKind::Named { def: idef, .. } = analysis.tcx.kind(iface).clone() else {
            return Err(CodegenError::new(span, "interface target is not an interface"));
        };
        let ext = analysis.results.iface_impls.get(&(cdef, idef)).copied()
            .ok_or_else(|| CodegenError::new(span, "no impl of interface for this type"))?;
        // Interface methods, in declaration order.
        let methods: Vec<DefId> = (0..analysis.program.defs.len() as u32)
            .map(DefId)
            .filter(|&d| {
                let def = analysis.program.def(d);
                def.kind == DefKind::InterfaceMethod && def.parent == Some(idef)
            })
            .collect();
        let ext_generic = !analysis.program.def(ext).generics.is_empty();
        // Resolve each interface method to the concrete impl's FuncId.
        let mut func_ids = Vec::with_capacity(methods.len());
        for m in &methods {
            let mname = analysis.program.def(*m).name.clone();
            let impl_def = (0..analysis.program.defs.len() as u32).map(DefId).find(|&d| {
                let def = analysis.program.def(d);
                def.kind == DefKind::ExtendMethod && def.parent == Some(ext) && def.name == mname
            }).ok_or_else(|| CodegenError::new(span, "interface method has no impl"))?;
            let targs = if ext_generic { cargs.clone() } else { Vec::new() };
            let fid = declare_instance(
                self.module, self.funcs, self.worklist, analysis, impl_def, targs,
            )?.ok_or_else(|| CodegenError::new(span, "impl method is not lowerable"))?;
            func_ids.push(fid);
        }
        // Emit the vtable data object: one pointer slot per method.
        let name = format!("vtable.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self.module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare vtable data");
        let mut desc = DataDescription::new();
        desc.define(vec![0u8; func_ids.len() * 8].into_boxed_slice());
        for (slot, fid) in func_ids.iter().enumerate() {
            let fref = self.module.declare_func_in_data(*fid, &mut desc);
            desc.write_function_addr((slot * 8) as u32, fref);
        }
        self.module.define_data(data_id, &desc).expect("define vtable");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        Ok(self.b.ins().global_value(PTR, gv))
    }

    /// Box `v` into a union/`dynamic` value, unless it is already boxed.
    fn apply_widen(&mut self, v: Option<Value>, from: Ty) -> Value {
        if matches!(self.cx.analysis.tcx.kind(from), TyKind::Union(_) | TyKind::Dynamic) {
            // Already a `{type_id, data}` box — widening is a no-op.
            return v.expect("boxed union value is a pointer");
        }
        self.box_value(v, from)
    }

    /// Allocate a `{type_id: i64, data: i64}` box for a union/dynamic value.
    /// The payload (offset 8) is a managed pointer iff the boxed type is one.
    fn box_value(&mut self, v: Option<Value>, from: Ty) -> Value {
        let resolved = resolve_shallow(self.cx.analysis, from, &self.subst);
        let managed = is_managed_ptr(self.cx.analysis, resolved);
        // If the payload is itself a managed pointer, it must survive the box
        // allocation below (which is a GC safepoint) even though it is not yet
        // stored anywhere — root it so a collection cannot free it.
        if managed {
            if let Some(val) = v {
                self.mark_root(val);
            }
        }
        let ptr_offsets: &[u32] = if managed { &[8] } else { &[] };
        let desc = self.emit_descriptor(16, GC_KIND_PLAIN, ptr_offsets);
        let ptr = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let id = { let id = self.type_id_of(from); self.b.ins().iconst(types::I64, id) };
        self.b.ins().store(MemFlags::trusted(), id, ptr, 0);
        if let Some(v) = v {
            self.b.ins().store(MemFlags::trusted(), v, ptr, 8);
        }
        ptr
    }

    /// Box `val` into a `Ready<out> | Pending` union (the `poll` result): build a
    /// `Ready<out>` whose single `value` field holds `val` *widened to an 8-byte
    /// slot* (so the runtime executor and `await` can read it as one machine
    /// word regardless of `out`'s width), then a `{type_id, payload}` union box
    /// tagged with `Ready<out>`'s type id (`docs/21` §1).
    fn box_ready(&mut self, val: Option<Value>, out_ty: Ty) -> Value {
        let out_resolved = resolve_shallow(self.cx.analysis, out_ty, &self.subst);
        let out_managed = is_managed_ptr(self.cx.analysis, out_resolved);
        // Widen the result to a single i64 slot.
        let widened = match val {
            Some(v) => {
                let c = self.b.func.dfg.value_type(v);
                if c == types::I64 {
                    v
                } else if c.is_int() {
                    self.b.ins().uextend(types::I64, v)
                } else if c == types::F64 {
                    self.b.ins().bitcast(types::I64, MemFlags::new(), v)
                } else {
                    // f32: reinterpret to i32, then zero-extend into the slot.
                    let i = self.b.ins().bitcast(types::I32, MemFlags::new(), v);
                    self.b.ins().uextend(types::I64, i)
                }
            }
            None => self.b.ins().iconst(types::I64, 0),
        };
        // Root a managed payload across the `Ready` allocation (a safepoint).
        if out_managed {
            self.mark_root(widened);
        }
        let ready_def = self.cx.analysis.program.ready_def;
        let ptr_offsets: &[u32] = if out_managed { &[0] } else { &[] };
        let rdesc = self.emit_descriptor(8, GC_KIND_PLAIN, ptr_offsets);
        let ready = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[rdesc])
            .expect("lang_alloc returns a pointer");
        self.b.ins().store(MemFlags::trusted(), widened, ready, 0);
        self.mark_root(ready);
        let desc = self.emit_descriptor(16, GC_KIND_PLAIN, &[8]);
        let bx = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let tid = 1000 + ready_def.index() as i64;
        let tid_v = self.b.ins().iconst(types::I64, tid);
        self.b.ins().store(MemFlags::trusted(), tid_v, bx, 0);
        self.b.ins().store(MemFlags::trusted(), ready, bx, 8);
        bx
    }

    /// Box a `Pending` value into a `Ready<out> | Pending` union (the `poll`
    /// result for a not-yet-complete future). `Pending` is a unit struct, so the
    /// payload is null; only the tag matters (`docs/21` §1). Used by the `await`
    /// suspension path (in progress).
    #[allow(dead_code)]
    fn box_pending(&mut self) -> Value {
        let pending_def = self.cx.analysis.program.pending_def;
        let desc = self.emit_descriptor(16, GC_KIND_PLAIN, &[]);
        let bx = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let tid = 1000 + pending_def.index() as i64;
        let tid_v = self.b.ins().iconst(types::I64, tid);
        self.b.ins().store(MemFlags::trusted(), tid_v, bx, 0);
        let zero = self.b.ins().iconst(PTR, 0);
        self.b.ins().store(MemFlags::trusted(), zero, bx, 8);
        bx
    }

    /// Build a one-slot vtable data object for a generated `Future`: slot 0 is
    /// the `poll` function pointer (the `Future` interface has only `poll`).
    /// Returns the vtable's address.
    fn emit_future_vtable(&mut self, poll_fid: FuncId) -> Value {
        let name = format!("future_vtable.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self.module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare future vtable");
        let mut desc = DataDescription::new();
        desc.define(vec![0u8; 8].into_boxed_slice());
        desc.set_align(8); // holds a function pointer
        let fref = self.module.declare_func_in_data(poll_fid, &mut desc);
        desc.write_function_addr(0, fref);
        self.module.define_data(data_id, &desc).expect("define future vtable");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        self.b.ins().global_value(PTR, gv)
    }

    /// Allocate and initialise a `Future<Out>` interface-object box for a state
    /// machine: `[vtable @0][data = state struct @8][type_id @16]`. The data
    /// pointer is GC-traced (offset 8). `type_id` is 0 (downcasts on generated
    /// futures are a follow-up).
    fn emit_future_box(&mut self, poll_fid: FuncId, state: Value) -> Value {
        self.mark_root(state);
        let vtable = self.emit_future_vtable(poll_fid);
        let desc = self.emit_descriptor(24, GC_KIND_PLAIN, &[8]);
        let bx = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        self.b.ins().store(MemFlags::trusted(), vtable, bx, 0);
        self.b.ins().store(MemFlags::trusted(), state, bx, 8);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().store(MemFlags::trusted(), zero, bx, 16);
        bx
    }

    /// A zero value of Cranelift type `ct` (for initialising state-machine
    /// local slots that have not yet been assigned).
    fn zero_val(&mut self, ct: ClType) -> Value {
        if ct == types::F64 {
            self.b.ins().f64const(0.0)
        } else if ct == types::F32 {
            self.b.ins().f32const(0.0)
        } else {
            self.b.ins().iconst(ct, 0)
        }
    }

    /// Lower `await fut` inside an async `poll` body (`docs/21` §4): save all
    /// live locals and the inner future into the state struct, set the resume
    /// state, then poll the inner future. On `Pending`, return `Pending` from
    /// this `poll` (the executor re-enters at the resume block). On `Ready`,
    /// continue with the unwrapped value.
    fn gen_await(&mut self, inner: &Expr, kw_span: Span) -> CgResult<Option<Value>> {
        // Evaluate the inner future, then suspend on it at this await's site.
        let fut = self.gen_expr(inner)?.ok_or_else(|| {
            CodegenError::new(inner.span, "awaited expression has no value")
        })?;
        let out = self.cx.analysis.results.awaits.get(&kw_span).copied()
            .unwrap_or(self.cx.analysis.tcx.error);
        self.emit_await_suspend(fut, kw_span, out)
    }

    /// Suspend on `fut` at the `await` site keyed by `await_span` (shared by
    /// `await` expressions and `for await` loops): save every live local + the
    /// inner future, return `Pending` if the inner poll is not ready, otherwise
    /// continue with the unwrapped value narrowed to `out`. The `await_span`
    /// must be a registered suspend site (statement-level / `for await`).
    fn emit_await_suspend(&mut self, fut: Value, await_span: Span, out: Ty)
        -> CgResult<Option<Value>>
    {
        let (state_n, poll_block, inner_off, self_val, ctx_val, pending_block, saves) = {
            let actx = self.async_ctx.as_ref().ok_or_else(|| {
                CodegenError::new(await_span, "`await` outside an async body")
            })?;
            let &(state_n, poll_block, _resume) = actx.awaits.get(&await_span).ok_or_else(|| {
                CodegenError::new(
                    await_span,
                    "`await` in this position is not yet supported — use it as a \
                     statement (`var x = await e;` or `await e;`), a trailing \
                     expression, or a `return` operand",
                )
            })?;
            (state_n, poll_block, actx.inner_off, actx.self_val, actx.ctx_val,
             actx.pending_block, actx.save_locals.clone())
        };
        // Suspend: persist every live local + the inner future + resume state.
        for (local, off) in &saves {
            if let Some(&var) = self.vars.get(local) {
                let v = self.b.use_var(var);
                self.b.ins().store(MemFlags::trusted(), v, self_val, *off);
            }
        }
        self.b.ins().store(MemFlags::trusted(), fut, self_val, inner_off);
        let st = self.b.ins().iconst(types::I64, state_n);
        self.b.ins().store(MemFlags::trusted(), st, self_val, 0);
        self.b.ins().jump(poll_block, &[]);
        self.switch(poll_block);

        // Poll the inner future through its vtable (slot 0 = `poll`), forwarding
        // our `Context`.
        let innerv = self.b.ins().load(PTR, MemFlags::trusted(), self_val, inner_off);
        let r = self.emit_vtable_call(0, innerv, &[ctx_val], Some(PTR))?
            .ok_or_else(|| CodegenError::new(await_span, "poll returned no value"))?;
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), r, 0);
        let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
        let ptid = self.b.ins().iconst(types::I64, pending_tid);
        let is_pending = self.b.ins().icmp(IntCC::Equal, tag, ptid);
        let got = self.b.create_block();
        self.b.ins().brif(is_pending, pending_block, &[], got, &[]);
        self.switch(got);
        // Ready<Out>: payload @8 is the `Ready` struct; its widened `value` (@0)
        // is the result. Narrow it back to the await's output type.
        let ready = self.b.ins().load(PTR, MemFlags::trusted(), r, 8);
        let valw = self.b.ins().load(types::I64, MemFlags::trusted(), ready, 0);
        self.i64_to_elem(valw, out, await_span)
    }

    /// Lower `block_on(fut)` (`docs/21` §6): drive the future to completion via
    /// the runtime executor and narrow its widened `Out` result.
    fn gen_block_on(&mut self, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let out = self.cx.analysis.results.block_ons.get(&span).copied()
            .unwrap_or(self.cx.analysis.tcx.error);
        let fut = self.gen_expr(&args[0])?.ok_or_else(|| {
            CodegenError::new(args[0].span, "block_on argument has no value")
        })?;
        let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
        let ptid = self.b.ins().iconst(types::I64, pending_tid);
        let raw = self.call_intrinsic(
            "lang_block_on", &[PTR, types::I64], Some(types::I64), &[fut, ptid],
        ).expect("lang_block_on returns a value");
        self.i64_to_elem(raw, out, span)
    }

    fn gen_expr_raw(&mut self, expr: &Expr) -> CgResult<Option<Value>> {
        let ty = self.cx.analysis.results.expr_ty(expr.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        match &expr.kind {
            ExprKind::Int(_) => Ok(Some(self.gen_int_lit(expr, ty)?)),
            ExprKind::Float(lit) => Ok(Some(self.gen_float_lit(lit, ty, expr.span)?)),
            ExprKind::Bool(v) => Ok(Some(self.b.ins().iconst(types::I8, i64::from(*v)))),
            ExprKind::Char(c) => {
                let scalar = parse_char(&c.raw)
                    .ok_or_else(|| CodegenError::new(expr.span, "bad char literal"))?;
                Ok(Some(self.b.ins().iconst(types::I32, i64::from(scalar))))
            }
            ExprKind::Null => Ok(None),
            ExprKind::Str(s) => Ok(Some(self.gen_str_literal(s)?)),
            ExprKind::Cast { op, expr: inner, .. } => {
                let from = self.cx.analysis.results.expr_ty(inner.span)
                    .unwrap_or(self.cx.analysis.tcx.error);
                // The lowered target type (for `is`, whose own type is `bool`).
                let target = self.cx.analysis.results.cast_targets
                    .get(&expr.span)
                    .copied()
                    .unwrap_or(ty);
                match op {
                    CastOp::Is => self.gen_is(inner, from, target),
                    CastOp::As => self.gen_cast(inner, from, target),
                }
            }
            ExprKind::Ident(_) => {
                match self.cx.analysis.results.resolution(expr.span) {
                    Some(ValueRes::Local(local)) => {
                        let var = self.vars.get(&local).copied().ok_or_else(|| {
                            CodegenError::new(expr.span, "use of unbound local")
                        })?;
                        Ok(Some(self.b.use_var(var)))
                    }
                    // A unit struct used as a value carries no data — only its
                    // type matters (e.g. once boxed into a union). Represent it
                    // as a null pointer placeholder.
                    Some(ValueRes::StructCtor(_)) => Ok(Some(self.b.ins().iconst(PTR, 0))),
                    _ => Err(CodegenError::new(expr.span, "value reference not yet lowerable")),
                }
            }
            ExprKind::Paren(inner) => self.gen_expr(inner),
            ExprKind::Unary { op, operand, .. } => self.gen_unary(*op, operand, ty),
            ExprKind::Binary { op, left, right, op_span } => {
                self.gen_binary(*op, left, right, *op_span)
            }
            ExprKind::Block(b) => self.gen_block(b),
            ExprKind::If { cond, then_block, else_branch } => {
                self.gen_if(cond, then_block, else_branch.as_ref(), ty)
            }
            ExprKind::Return(value) => {
                let v = match value {
                    Some(e) => self.gen_expr(e)?,
                    None => None,
                };
                self.emit_return(v)?;
                Ok(None)
            }
            ExprKind::Call { callee, args, trailing_closure, .. } => {
                // A trailing closure is the call's final argument.
                if let Some(tc) = trailing_closure {
                    let mut all = args.clone();
                    all.push((**tc).clone());
                    self.gen_call(callee, &all, expr.span)
                } else {
                    self.gen_call(callee, args, expr.span)
                }
            }
            ExprKind::StructLit { path, fields, spread } => {
                let (def, sargs) = match self.cx.analysis.tcx.kind(ty) {
                    TyKind::Named { def, args } => (*def, args.clone()),
                    _ => return Err(CodegenError::new(expr.span, "struct literal has non-struct type")),
                };
                let _ = path;
                Ok(Some(self.gen_struct_lit(def, &sargs, fields, spread.as_deref(), expr.span)?))
            }
            ExprKind::SelfExpr => self.gen_local_use(expr.span),
            ExprKind::List(elems) => {
                let elem = self.list_elem_of(ty).ok_or_else(|| {
                    CodegenError::new(expr.span, "list literal has non-list type")
                })?;
                let list = self.gen_list_new(elem);
                for el in elems {
                    let v = self.gen_expr(el)?;
                    let raw = self.elem_to_i64(v, elem, el.span)?;
                    self.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[list, raw]);
                }
                Ok(Some(list))
            }
            ExprKind::MapLit(items) => self.gen_map_lit(items, ty, expr.span),
            ExprKind::Closure { is_async, body, .. } => {
                if *is_async {
                    return Err(CodegenError::new(
                        expr.span,
                        "`async` closure code generation is not yet implemented",
                    ));
                }
                self.gen_closure(body, expr.span)
            }
            ExprKind::AsyncBlock(block) => self.gen_async_block(block, expr.span),
            ExprKind::Await { expr: inner, kw_span } => self.gen_await(inner, *kw_span),
            ExprKind::Index { receiver, index } => self.gen_index_load(receiver, index),
            ExprKind::Tuple(elems) => {
                let elem_tys = match self.cx.analysis.tcx.kind(ty).clone() {
                    TyKind::Tuple(ts) => ts,
                    _ => return Err(CodegenError::new(expr.span, "tuple has non-tuple type")),
                };
                let layout = tuple_layout(self.cx.analysis, &elem_tys);
                let ptr = self.alloc_struct(&layout);
                for (i, e) in elems.iter().enumerate() {
                    let v = self.gen_expr(e)?;
                    if let (Some(v), Some(Some(_))) = (v, layout.cltys.get(i)) {
                        self.b.ins().store(MemFlags::trusted(), v, ptr, layout.offsets[i] as i32);
                    }
                }
                Ok(Some(ptr))
            }
            ExprKind::Field { receiver, name } => {
                // A numeric-namespace constant (`i32.MAX`, `f64.NAN`, …).
                if let Some(intr) = self.cx.analysis.results.num_intrinsics.get(&expr.span).copied() {
                    return self.gen_num_intrinsic(intr, &[]);
                }
                self.gen_field_load(receiver, &name.name)
            }
            ExprKind::TupleIndex { receiver, index, .. } => {
                self.gen_field_load(receiver, &index.to_string())
            }
            ExprKind::Match { scrutinee, arms } => self.gen_match(scrutinee, arms, ty),
            ExprKind::Try { expr: inner, q_span } => self.gen_try(inner, *q_span, ty),
            ExprKind::While { cond, body } => self.gen_while(cond, body),
            ExprKind::For { pattern, in_async, iter, body } => {
                if *in_async {
                    self.gen_for_await(pattern, iter, body)
                } else {
                    self.gen_for(pattern, iter, body)
                }
            }
            ExprKind::Loop(body) => self.gen_loop(body, ty),
            ExprKind::Break(value) => self.gen_break(value.as_deref(), expr.span),
            ExprKind::Continue => self.gen_continue(expr.span),
            _ => Err(CodegenError::new(expr.span, "expression not yet lowerable")),
        }
    }

    fn gen_int_lit(&mut self, expr: &Expr, ty: Ty) -> CgResult<Value> {
        let ExprKind::Int(lit) = &expr.kind else { unreachable!() };
        let digits: String = lit.raw.chars().filter(|c| *c != '_').collect();
        let radix = match lit.base {
            compiler::token::IntBase::Dec => 10,
            compiler::token::IntBase::Hex => 16,
            compiler::token::IntBase::Oct => 8,
            compiler::token::IntBase::Bin => 2,
        };
        let value = u64::from_str_radix(&digits, radix)
            .map_err(|_| CodegenError::new(expr.span, "integer literal out of range"))?;
        let ct = self.cx_clty(ty).unwrap_or(types::I64);
        Ok(self.b.ins().iconst(ct, value as i64))
    }

    fn gen_float_lit(&mut self, lit: &FloatLit, ty: Ty, span: Span) -> CgResult<Value> {
        let raw: String = lit.raw.chars().filter(|c| *c != '_').collect();
        let v: f64 = raw.parse()
            .map_err(|_| CodegenError::new(span, "float literal parse error"))?;
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Float(FloatTy::F32) => Ok(self.b.ins().f32const(v as f32)),
            _ => Ok(self.b.ins().f64const(v)),
        }
    }

    fn gen_unary(&mut self, op: UnaryOp, operand: &Expr, ty: Ty) -> CgResult<Option<Value>> {
        let v = self.gen_expr(operand)?.ok_or_else(|| {
            CodegenError::new(operand.span, "operand has no value")
        })?;
        let is_float = matches!(self.cx.analysis.tcx.kind(ty), TyKind::Float(_));
        let is_bool = matches!(self.cx.analysis.tcx.kind(ty), TyKind::Bool);
        let out = match op {
            UnaryOp::Neg if is_float => self.b.ins().fneg(v),
            UnaryOp::Neg => self.b.ins().ineg(v),
            // `!` on a `bool` is *logical* negation (0↔1), not bitwise — a bool
            // is an `i8` holding 0/1, so `bnot` would give 0xFF/0xFE (both
            // truthy). Integer `!`/`~` is bitwise complement (`docs/15`).
            UnaryOp::Not if is_bool => {
                let one = self.b.ins().iconst(types::I8, 1);
                self.b.ins().bxor(v, one)
            }
            UnaryOp::Not | UnaryOp::BitNot => self.b.ins().bnot(v),
        };
        Ok(Some(out))
    }

    fn gen_binary(&mut self, op: BinaryOp, left: &Expr, right: &Expr, op_span: Span)
        -> CgResult<Option<Value>>
    {
        use BinaryOp::*;
        if matches!(op, And | Or) {
            return self.gen_logical(op, left, right);
        }
        // Overloaded operator → call the resolved `extend` method.
        if let Some(&mdef) = self.cx.analysis.results.operator_methods.get(&op_span) {
            let l = self.gen_expr(left)?.ok_or_else(|| {
                CodegenError::new(left.span, "operand has no value")
            })?;
            let r = self.gen_expr(right)?.ok_or_else(|| {
                CodegenError::new(right.span, "operand has no value")
            })?;
            let result = self.emit_call(mdef, Vec::new(), &[l, r], op_span)?;
            // `a != b` negates the `eq` result.
            if matches!(op, Ne) {
                let v = result.ok_or_else(|| CodegenError::new(op_span, "`eq` returned no value"))?;
                let zero = self.b.ins().iconst(types::I8, 0);
                return Ok(Some(self.b.ins().icmp(IntCC::Equal, v, zero)));
            }
            return Ok(result);
        }
        let lty = self.cx.analysis.results.expr_ty(left.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let l = self.gen_expr(left)?.ok_or_else(|| {
            CodegenError::new(left.span, "operand has no value")
        })?;
        let r = self.gen_expr(right)?.ok_or_else(|| {
            CodegenError::new(right.span, "operand has no value")
        })?;
        // `str + str` → runtime concatenation.
        if matches!(op, Add) && matches!(self.cx.analysis.tcx.kind(lty), TyKind::Str) {
            let s = self.call_intrinsic("lang_str_concat", &[PTR, PTR], Some(PTR), &[l, r]);
            return Ok(s);
        }
        // `str` comparisons are by content (byte-wise / lexicographic), not by
        // pointer identity (`docs/02` §7).
        if matches!(self.cx.analysis.tcx.kind(lty), TyKind::Str)
            && matches!(op, Eq | Ne | Lt | Le | Gt | Ge)
        {
            return Ok(Some(self.gen_str_compare(op, l, r)));
        }
        let (is_float, signed) = match self.cx.analysis.tcx.kind(lty) {
            TyKind::Float(_) => (true, true),
            TyKind::Int(it) => (false, it.is_signed()),
            _ => (false, true),
        };
        // Integer division/modulo by zero always panics (`docs/14`, `docs/02`).
        if matches!(op, Div | Rem) && !is_float {
            self.guard_nonzero(r);
            // Signed `INT_MIN / -1` (and `% -1`) overflows. In debug this panics
            // (Cranelift would otherwise trap raw); in release it wraps, handled
            // inside the signed div/rem arms below (`docs/14` §2/§5).
            if signed && !is_release() {
                self.guard_div_overflow(l, r);
            }
        }
        let out = match op {
            Add if is_float => self.b.ins().fadd(l, r),
            Add => self.checked_arith(Add, signed, l, r),
            Sub if is_float => self.b.ins().fsub(l, r),
            Sub => self.checked_arith(Sub, signed, l, r),
            Mul if is_float => self.b.ins().fmul(l, r),
            Mul => self.checked_arith(Mul, signed, l, r),
            Div if is_float => self.b.ins().fdiv(l, r),
            Div if signed => self.gen_signed_div(l, r),
            Div => self.b.ins().udiv(l, r),
            Rem if signed => self.gen_signed_rem(l, r),
            Rem => self.b.ins().urem(l, r),
            BitAnd => self.b.ins().band(l, r),
            BitOr => self.b.ins().bor(l, r),
            BitXor => self.b.ins().bxor(l, r),
            Shl => { self.guard_shift(l, r); self.b.ins().ishl(l, r) }
            Shr if signed => { self.guard_shift(l, r); self.b.ins().sshr(l, r) }
            Shr => { self.guard_shift(l, r); self.b.ins().ushr(l, r) }
            Eq | Ne | Lt | Le | Gt | Ge => {
                return Ok(Some(self.gen_compare(op, is_float, signed, l, r)));
            }
            And | Or => unreachable!(),
        };
        Ok(Some(out))
    }

    /// Emit a guarded panic: when `cond` (an `I8` boolean) is true, call
    /// `lang_panic(msg)` and trap; otherwise fall through to the continuation.
    fn guard_panic(&mut self, cond: Value, msg: &str) {
        let panic_bb = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, panic_bb, &[], cont, &[]);
        self.switch(panic_bb);
        let m = self.const_str(msg);
        self.call_intrinsic("lang_panic", &[PTR], None, &[m]);
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;
        self.switch(cont);
    }

    /// Convert a float to an integer, panicking on NaN or out-of-range inputs
    /// (`docs/14` §2/§6). Valid inputs satisfy `lo <= v < hi`, where the bounds
    /// are the smallest/largest representable magnitudes for the target width;
    /// NaN fails both comparisons and therefore panics.
    fn gen_float_to_int(&mut self, v: Value, ff: FloatTy, b: IntTy) -> Value {
        let w = b.bits().unwrap_or(64) as i32;
        let signed = b.is_signed();
        let (lo, hi): (f64, f64) = if signed {
            (-(2f64.powi(w - 1)), 2f64.powi(w - 1))
        } else {
            (0.0, 2f64.powi(w))
        };
        let (lo_v, hi_v) = match ff {
            FloatTy::F32 => (self.b.ins().f32const(lo as f32), self.b.ins().f32const(hi as f32)),
            FloatTy::F64 => (self.b.ins().f64const(lo), self.b.ins().f64const(hi)),
        };
        let ge_lo = self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, v, lo_v);
        let lt_hi = self.b.ins().fcmp(FloatCC::LessThan, v, hi_v);
        let in_range = self.b.ins().band(ge_lo, lt_hi);
        let one = self.b.ins().iconst(types::I8, 1);
        let oor = self.b.ins().bxor(in_range, one);
        self.guard_panic(oor, "cast from float to integer is out of range or NaN");
        let it = int_clty(b);
        if signed { self.b.ins().fcvt_to_sint(it, v) } else { self.b.ins().fcvt_to_uint(it, v) }
    }

    /// Panic if `cp` (an `I32` code point) is not a valid Unicode scalar value:
    /// it must be `<= 0x10FFFF` and outside the surrogate range
    /// `0xD800..=0xDFFF` (`docs/14` §2).
    fn guard_valid_char(&mut self, cp: Value) {
        let max = self.b.ins().iconst(types::I32, 0x10_FFFF);
        let too_big = self.b.ins().icmp(IntCC::UnsignedGreaterThan, cp, max);
        self.guard_panic(too_big, "cast to char is out of the Unicode range");
        let lo = self.b.ins().iconst(types::I32, 0xD800);
        let hi = self.b.ins().iconst(types::I32, 0xDFFF);
        let ge = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, cp, lo);
        let le = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, cp, hi);
        let is_surrogate = self.b.ins().band(ge, le);
        self.guard_panic(is_surrogate, "cast to char is a surrogate code point");
    }

    /// Panic if `divisor` is zero (integer `/`/`%` are always-panic per spec).
    fn guard_nonzero(&mut self, divisor: Value) {
        let ity = self.b.func.dfg.value_type(divisor);
        let zero = self.b.ins().iconst(ity, 0);
        let is_zero = self.b.ins().icmp(IntCC::Equal, divisor, zero);
        self.guard_panic(is_zero, "divide by zero");
    }

    /// Integer add/sub/mul. In **debug** an overflow panics; in **release** it
    /// wraps (two's complement / modular), the fast path (`docs/14` §2/§5).
    fn checked_arith(&mut self, op: BinaryOp, signed: bool, l: Value, r: Value) -> Value {
        use BinaryOp::*;
        if is_release() {
            return match op {
                Add => self.b.ins().iadd(l, r),
                Sub => self.b.ins().isub(l, r),
                Mul => self.b.ins().imul(l, r),
                _ => unreachable!("checked_arith only handles +/-/*"),
            };
        }
        let (res, ovf) = match (op, signed) {
            (Add, true) => self.b.ins().sadd_overflow(l, r),
            (Add, false) => self.b.ins().uadd_overflow(l, r),
            (Sub, true) => self.b.ins().ssub_overflow(l, r),
            (Sub, false) => self.b.ins().usub_overflow(l, r),
            (Mul, true) => self.b.ins().smul_overflow(l, r),
            (Mul, false) => self.b.ins().umul_overflow(l, r),
            _ => unreachable!("checked_arith only handles +/-/*"),
        };
        let what = match op {
            Add => "add",
            Sub => "subtract",
            Mul => "multiply",
            _ => unreachable!(),
        };
        self.guard_panic(ovf, &format!("attempt to {what} with overflow"));
        res
    }

    /// Panic on signed division overflow: `INT_MIN / -1` (the one case where a
    /// signed `/` or `%` overflows the result type, `docs/14` §2).
    fn guard_div_overflow(&mut self, l: Value, r: Value) {
        let ity = self.b.func.dfg.value_type(l);
        let bits = ity.bits();
        let min = if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) };
        let min_v = self.b.ins().iconst(ity, min);
        let neg1 = self.b.ins().iconst(ity, -1);
        let l_is_min = self.b.ins().icmp(IntCC::Equal, l, min_v);
        let r_is_neg1 = self.b.ins().icmp(IntCC::Equal, r, neg1);
        let both = self.b.ins().band(l_is_min, r_is_neg1);
        self.guard_panic(both, "attempt to divide with overflow");
    }

    /// Compute `(is_overflow, safe_divisor)` for signed `INT_MIN / -1`: the
    /// overflow flag, plus the divisor replaced by `1` in that case so the
    /// hardware `sdiv`/`srem` does not trap (used only in release, where the
    /// overflowing case wraps rather than panics).
    fn div_overflow_select(&mut self, l: Value, r: Value) -> (Value, Value) {
        let ity = self.b.func.dfg.value_type(l);
        let bits = ity.bits();
        let min = if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) };
        let min_v = self.b.ins().iconst(ity, min);
        let neg1 = self.b.ins().iconst(ity, -1);
        let l_is_min = self.b.ins().icmp(IntCC::Equal, l, min_v);
        let r_is_neg1 = self.b.ins().icmp(IntCC::Equal, r, neg1);
        let ovf = self.b.ins().band(l_is_min, r_is_neg1);
        let one = self.b.ins().iconst(ity, 1);
        let safe_r = self.b.ins().select(ovf, one, r);
        (ovf, safe_r)
    }

    /// Signed division. Debug callers have already guarded `INT_MIN / -1`; in
    /// release that case wraps to `INT_MIN` (`docs/14` §5).
    fn gen_signed_div(&mut self, l: Value, r: Value) -> Value {
        if !is_release() {
            return self.b.ins().sdiv(l, r);
        }
        let (ovf, safe_r) = self.div_overflow_select(l, r);
        let q = self.b.ins().sdiv(l, safe_r);
        // `INT_MIN / -1` wraps to `INT_MIN`, which is `l` in the overflow case.
        self.b.ins().select(ovf, l, q)
    }

    /// Signed remainder. In release `INT_MIN % -1` wraps to `0` (`docs/14` §5).
    fn gen_signed_rem(&mut self, l: Value, r: Value) -> Value {
        if !is_release() {
            return self.b.ins().srem(l, r);
        }
        let (ovf, safe_r) = self.div_overflow_select(l, r);
        let ity = self.b.func.dfg.value_type(l);
        let rem = self.b.ins().srem(l, safe_r);
        let zero = self.b.ins().iconst(ity, 0);
        self.b.ins().select(ovf, zero, rem)
    }

    /// A shift (`<<`/`>>`) panics — in debug *and* release — when the shift
    /// amount is `>=` the operand bit width (`docs/14` §2).
    fn guard_shift(&mut self, value: Value, amount: Value) {
        let width = self.b.func.dfg.value_type(value).bits() as i64;
        let amt_ty = self.b.func.dfg.value_type(amount);
        let width_v = self.b.ins().iconst(amt_ty, width);
        // The shift amount is unsigned (a bit position); compare unsigned.
        let too_big = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, amount, width_v);
        self.guard_panic(too_big, "shift amount >= bit width");
    }

    /// Lower a `str` comparison via the runtime (content equality / ordering).
    fn gen_str_compare(&mut self, op: BinaryOp, l: Value, r: Value) -> Value {
        use BinaryOp::*;
        match op {
            Eq => self.call_intrinsic("lang_str_eq", &[PTR, PTR], Some(types::I8), &[l, r])
                .expect("str_eq"),
            Ne => {
                let eq = self.call_intrinsic("lang_str_eq", &[PTR, PTR], Some(types::I8), &[l, r])
                    .expect("str_eq");
                let zero = self.b.ins().iconst(types::I8, 0);
                self.b.ins().icmp(IntCC::Equal, eq, zero)
            }
            _ => {
                let cmp = self.call_intrinsic("lang_str_cmp", &[PTR, PTR], Some(types::I64), &[l, r])
                    .expect("str_cmp");
                let zero = self.b.ins().iconst(types::I64, 0);
                let cc = match op {
                    Lt => IntCC::SignedLessThan,
                    Le => IntCC::SignedLessThanOrEqual,
                    Gt => IntCC::SignedGreaterThan,
                    Ge => IntCC::SignedGreaterThanOrEqual,
                    _ => unreachable!(),
                };
                self.b.ins().icmp(cc, cmp, zero)
            }
        }
    }

    fn gen_compare(&mut self, op: BinaryOp, is_float: bool, signed: bool, l: Value, r: Value)
        -> Value
    {
        use BinaryOp::*;
        if is_float {
            let cc = match op {
                Eq => FloatCC::Equal,
                Ne => FloatCC::NotEqual,
                Lt => FloatCC::LessThan,
                Le => FloatCC::LessThanOrEqual,
                Gt => FloatCC::GreaterThan,
                Ge => FloatCC::GreaterThanOrEqual,
                _ => unreachable!(),
            };
            self.b.ins().fcmp(cc, l, r)
        } else {
            let cc = match (op, signed) {
                (Eq, _) => IntCC::Equal,
                (Ne, _) => IntCC::NotEqual,
                (Lt, true) => IntCC::SignedLessThan,
                (Lt, false) => IntCC::UnsignedLessThan,
                (Le, true) => IntCC::SignedLessThanOrEqual,
                (Le, false) => IntCC::UnsignedLessThanOrEqual,
                (Gt, true) => IntCC::SignedGreaterThan,
                (Gt, false) => IntCC::UnsignedGreaterThan,
                (Ge, true) => IntCC::SignedGreaterThanOrEqual,
                (Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                _ => unreachable!(),
            };
            self.b.ins().icmp(cc, l, r)
        }
    }

    fn gen_logical(&mut self, op: BinaryOp, left: &Expr, right: &Expr) -> CgResult<Option<Value>> {
        // Short-circuit via blocks; result is i8 bool in a merge block param.
        let l = self.gen_expr(left)?.ok_or_else(|| {
            CodegenError::new(left.span, "operand has no value")
        })?;
        let rhs_block = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I8);

        match op {
            BinaryOp::And => {
                // if l { eval rhs } else { false }
                self.b.ins().brif(l, rhs_block, &[], merge, &[l.into()]);
            }
            BinaryOp::Or => {
                // if l { true } else { eval rhs }
                self.b.ins().brif(l, merge, &[l.into()], rhs_block, &[]);
            }
            _ => unreachable!(),
        }
        self.term = true;
        self.switch(rhs_block);
        let r = self.gen_expr(right)?.ok_or_else(|| {
            CodegenError::new(right.span, "operand has no value")
        })?;
        if !self.term {
            self.b.ins().jump(merge, &[r.into()]);
            self.term = true;
        }
        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    fn gen_if(
        &mut self,
        cond: &Expr,
        then_block: &Block,
        else_branch: Option<&ElseBranch>,
        result_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let c = self.gen_expr(cond)?.ok_or_else(|| {
            CodegenError::new(cond.span, "condition has no value")
        })?;
        let then_bb = self.b.create_block();
        let else_bb = self.b.create_block();
        let merge = self.b.create_block();

        let result_ct = self.cx_clty(result_ty);
        if let Some(ct) = result_ct {
            self.b.append_block_param(merge, ct);
        }

        self.b.ins().brif(c, then_bb, &[], else_bb, &[]);
        self.term = true;

        // then
        self.switch(then_bb);
        let then_val = self.gen_block(then_block)?;
        self.jump_to_merge(merge, then_val, result_ct)?;

        // else
        self.switch(else_bb);
        let else_val = match else_branch {
            None => None,
            Some(ElseBranch::Block(b)) => self.gen_block(b)?,
            Some(ElseBranch::If(e)) => self.gen_expr(e)?,
        };
        self.jump_to_merge(merge, else_val, result_ct)?;

        self.switch(merge);
        Ok(result_ct.map(|_| self.b.block_params(merge)[0]))
    }

    fn jump_to_merge(&mut self, merge: cranelift_codegen::ir::Block, val: Option<Value>,
        result_ct: Option<ClType>) -> CgResult<()>
    {
        if self.term {
            return Ok(()); // branch diverged (e.g. `return`)
        }
        match (result_ct, val) {
            (Some(_), Some(v)) => self.b.ins().jump(merge, &[v.into()]),
            (Some(ct), None) => {
                // Branch produced no value but a value is expected: only valid
                // if this path is unreachable; supply a placeholder.
                let zero = self.b.ins().iconst(if ct.is_int() { ct } else { types::I64 }, 0);
                self.b.ins().jump(merge, &[zero.into()])
            }
            (None, _) => self.b.ins().jump(merge, &[]),
        };
        self.term = true;
        Ok(())
    }

    // -- `?` propagation -----------------------------------------------------

    /// `expr?`: if the union box holds a failure variant (one also in the
    /// function's return type), return it; otherwise continue with the success
    /// value (`success_ty` is the checker-computed result type).
    fn gen_try(&mut self, inner: &Expr, q_span: Span, success_ty: Ty) -> CgResult<Option<Value>> {
        let et = self.cx.analysis.results.expr_ty(inner.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let ptr = self.gen_expr(inner)?.ok_or_else(|| {
            CodegenError::new(inner.span, "`?` operand has no value")
        })?;
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 0);

        // Failure variants: those of `et` that are also in the return type.
        let r = self.ret_ty;
        let r_variants = self.cx.analysis.tcx.variants(r);
        let failures: Vec<Ty> = self
            .cx
            .analysis
            .tcx
            .variants(et)
            .into_iter()
            .filter(|v| r_variants.contains(v))
            .collect();

        for fv in failures {
            let fid = { let id = self.type_id_of(fv); self.b.ins().iconst(types::I64, id) };
            let is_fail = self.b.ins().icmp(IntCC::Equal, tag, fid);
            let ret_block = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(is_fail, ret_block, &[], next, &[]);
            self.term = true;

            self.switch(ret_block);
            // Return the box as the function's return type. When R is a union
            // the box passes through; otherwise unbox to R's single variant.
            let ret_val = if matches!(self.cx.analysis.tcx.kind(r), TyKind::Union(_) | TyKind::Dynamic) {
                Some(ptr)
            } else {
                clty_of(self.cx.analysis, r)
                    .map(|ct| self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))
            };
            self.emit_return(ret_val)?;

            self.switch(next);
        }

        // Residual conversions (`docs/13` §4): a failure variant `E` not in R
        // is propagated by converting it via `Target.from_residual(e)` and
        // returning that (boxed through R).
        let conversions = self
            .cx
            .analysis
            .results
            .residual_conversions
            .get(&q_span)
            .cloned()
            .unwrap_or_default();
        for (residual, method, target) in conversions {
            let rid = { let id = self.type_id_of(residual); self.b.ins().iconst(types::I64, id) };
            let is_fail = self.b.ins().icmp(IntCC::Equal, tag, rid);
            let ret_block = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(is_fail, ret_block, &[], next, &[]);
            self.term = true;

            self.switch(ret_block);
            // Unbox the residual payload, convert it, then box the result.
            let payload = match clty_of(self.cx.analysis, residual) {
                Some(ct) => self.b.ins().load(ct, MemFlags::trusted(), ptr, 8),
                None => self.b.ins().iconst(PTR, 0),
            };
            let converted = self
                .emit_call(method, Vec::new(), &[payload], inner.span)?
                .ok_or_else(|| CodegenError::new(inner.span, "`from_residual` returned no value"))?;
            // The converted value has type `target`; box it through R (a union)
            // or return it directly when R is that single type.
            let ret_val = if matches!(self.cx.analysis.tcx.kind(r), TyKind::Union(_) | TyKind::Dynamic) {
                Some(self.box_value(Some(converted), target))
            } else {
                Some(converted)
            };
            self.emit_return(ret_val)?;

            self.switch(next);
        }

        // Success path: narrow the box to the success type.
        if matches!(self.cx.analysis.tcx.kind(success_ty), TyKind::Union(_) | TyKind::Dynamic) {
            Ok(Some(ptr))
        } else {
            match clty_of(self.cx.analysis, success_ty) {
                Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))),
                None => Ok(None),
            }
        }
    }

    // -- match ---------------------------------------------------------------

    fn gen_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        result_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let sty = self.cx.analysis.results.expr_ty(scrutinee.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let scrut = self.gen_expr(scrutinee)?;
        let is_union = matches!(self.cx.analysis.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic);
        let tag = if is_union {
            scrut.map(|p| self.b.ins().load(types::I64, MemFlags::trusted(), p, 0))
        } else {
            None
        };

        let result_ct = self.cx_clty(result_ty);
        let merge = self.b.create_block();
        if let Some(ct) = result_ct {
            self.b.append_block_param(merge, ct);
        }

        for arm in arms {
            let matched = self.pattern_matches(&arm.pattern, sty, scrut, tag)?;
            let cand = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(matched, cand, &[], next, &[]);
            self.term = true;

            self.switch(cand);
            self.bind_match_pattern(&arm.pattern, sty, scrut, tag)?;
            // A guard, if present, must pass for the arm to fire.
            let proceed = match &arm.guard {
                Some(g) => self.gen_expr(g)?.ok_or_else(|| {
                    CodegenError::new(g.span, "guard has no value")
                })?,
                None => self.b.ins().iconst(types::I8, 1),
            };
            let body_block = self.b.create_block();
            self.b.ins().brif(proceed, body_block, &[], next, &[]);
            self.term = true;

            self.switch(body_block);
            let body_val = self.gen_expr(&arm.body)?;
            self.jump_to_merge(merge, body_val, result_ct)?;

            self.switch(next);
        }
        // Exhaustiveness is checked statically; reaching here is impossible.
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;

        self.switch(merge);
        Ok(result_ct.map(|_| self.b.block_params(merge)[0]))
    }

    /// Whether `pattern` structurally matches the scrutinee, as an i8 boolean
    /// (without binding). Tuple patterns are irrefutable here; their bindings
    /// are extracted in [`Self::bind_match_pattern`].
    fn pattern_matches(
        &mut self,
        pattern: &Pattern,
        sty: Ty,
        scrut: Option<Value>,
        tag: Option<Value>,
    ) -> CgResult<Value> {
        let one = self.b.ins().iconst(types::I8, 1);
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) | PatternKind::Tuple { .. } => {
                Ok(one)
            }
            PatternKind::TypeBinding { .. } | PatternKind::UnitPath(_) => {
                let t = self.cx.analysis.results.pattern_types.get(&pattern.span).copied()
                    .unwrap_or(self.cx.analysis.tcx.error);
                match tag {
                    Some(tag) => Ok(self.tag_in_target(tag, t)),
                    // Concrete scrutinee: statically known.
                    None => {
                        let yes = sty == t;
                        Ok(self.b.ins().iconst(types::I8, i64::from(yes)))
                    }
                }
            }
            PatternKind::Literal(e) => {
                // `null` literal against a union: compare the tag.
                if let ExprKind::Null = &e.kind {
                    return match tag {
                        Some(tag) => {
                            let nid = self.b.ins().iconst(
                                types::I64,
                                type_id(self.cx.analysis, self.cx.analysis.tcx.null),
                            );
                            Ok(self.b.ins().icmp(IntCC::Equal, tag, nid))
                        }
                        None => Ok(one),
                    };
                }
                let lit = self.gen_expr(e)?.ok_or_else(|| {
                    CodegenError::new(e.span, "literal pattern has no value")
                })?;
                let scrut = scrut.ok_or_else(|| {
                    CodegenError::new(pattern.span, "scrutinee has no value")
                })?;
                // Compare against the (concrete-typed) scrutinee value.
                match self.cx.analysis.tcx.kind(sty) {
                    TyKind::Float(_) => Ok(self.b.ins().fcmp(FloatCC::Equal, scrut, lit)),
                    _ => Ok(self.b.ins().icmp(IntCC::Equal, scrut, lit)),
                }
            }
            _ => Err(CodegenError::new(pattern.span, "pattern not yet lowerable in match")),
        }
    }

    /// Bind the names introduced by a matched pattern, extracting payloads.
    fn bind_match_pattern(
        &mut self,
        pattern: &Pattern,
        sty: Ty,
        scrut: Option<Value>,
        _tag: Option<Value>,
    ) -> CgResult<()> {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::UnitPath(_) => Ok(()),
            PatternKind::Binding(name) => {
                if let (Some(v), Some(ct)) = (scrut, self.cx_clty(sty)) {
                    let local = self.resolve_local(name.span)?;
                    let var = self.fresh_var(local, ct);
                    self.b.def_var(var, v);
                    let _ = ct;
                }
                Ok(())
            }
            PatternKind::TypeBinding { binding: Some(b), .. } => {
                let t = self.cx.analysis.results.pattern_types.get(&pattern.span).copied()
                    .unwrap_or(self.cx.analysis.tcx.error);
                // Extract the payload: unbox from a union, else use as-is.
                let val = match (scrut, self.cx_clty(t)) {
                    (Some(p), Some(ct)) if matches!(
                        self.cx.analysis.tcx.kind(sty), TyKind::Union(_) | TyKind::Dynamic
                    ) => Some(self.b.ins().load(ct, MemFlags::trusted(), p, 8)),
                    (s, Some(_)) => s,
                    _ => None,
                };
                if let (Some(v), Some(ct)) = (val, self.cx_clty(t)) {
                    let local = self.resolve_local(b.span)?;
                    let var = self.fresh_var(local, ct);
                    self.b.def_var(var, v);
                }
                Ok(())
            }
            PatternKind::TypeBinding { binding: None, .. } => Ok(()),
            PatternKind::Tuple { elems, rest: None } => {
                let layout = self.layout_for_ty(sty).ok_or_else(|| {
                    CodegenError::new(pattern.span, "tuple pattern on non-aggregate")
                })?;
                let elem_tys = match self.cx.analysis.tcx.kind(sty).clone() {
                    TyKind::Tuple(ts) => ts,
                    _ => return Err(CodegenError::new(pattern.span, "tuple pattern on non-tuple")),
                };
                let ptr = scrut.ok_or_else(|| {
                    CodegenError::new(pattern.span, "tuple scrutinee has no value")
                })?;
                for (i, sub) in elems.iter().enumerate() {
                    let elem_val = match layout.cltys.get(i) {
                        Some(Some(ct)) => Some(self.b.ins().load(
                            *ct, MemFlags::trusted(), ptr, layout.offsets[i] as i32,
                        )),
                        _ => None,
                    };
                    self.bind_match_pattern(sub, elem_tys[i], elem_val, None)?;
                }
                Ok(())
            }
            _ => Err(CodegenError::new(pattern.span, "pattern not yet lowerable in match")),
        }
    }

    /// Lower `for pat in iter { body }`. A `List` iterates by index (fast path);
    /// any other type recorded by the checker drives the `Iterator` protocol.
    fn gen_for(&mut self, pattern: &Pattern, iter: &Expr, body: &Block) -> CgResult<Option<Value>> {
        if let Some(info) = self.cx.analysis.results.for_iters.get(&iter.span).cloned() {
            return self.gen_for_iterator(pattern, iter, body, info);
        }
        if let Some((kt, vt, entry_ty)) = self.cx.analysis.results.for_maps.get(&iter.span).copied() {
            return self.gen_for_map(pattern, iter, body, kt, vt, entry_ty);
        }
        let ity = self.cx.analysis.results.expr_ty(iter.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let elem = self.list_elem_of(ity).ok_or_else(|| {
            CodegenError::new(iter.span, "`for` currently iterates `List<T>` only")
        })?;
        let list = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "iterable has no value")
        })?;

        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let latch = self.b.create_block();
        let exit = self.b.create_block();

        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(header);
        self.emit_safepoint();
        let i = self.b.use_var(iv);
        let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;

        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, i2])
            .expect("get");
        let elem_val = self.i64_to_elem(raw, elem, iter.span)?;
        self.bind_pattern(pattern, elem_val, elem)?;
        self.loops.push(LoopCg { continue_block: latch, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(latch, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(latch);
        let i3 = self.b.use_var(iv);
        let one = self.b.ins().iconst(types::I64, 1);
        let inc = self.b.ins().iadd(i3, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(exit);
        Ok(None)
    }

    /// Lower `for pat in iter { body }` via the `Iterator` protocol: evaluate the
    /// iterator once, then loop calling `next()`, breaking on `Done` and binding
    /// the unwrapped `Item<U>.value` each step.
    fn gen_for_iterator(
        &mut self,
        pattern: &Pattern,
        iter: &Expr,
        body: &Block,
        info: ForIter,
    ) -> CgResult<Option<Value>> {
        let iter_val = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "iterator has no value")
        })?;
        // The iterator object is mutated by `next` and lives across the loop.
        self.mark_root(iter_val);

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();

        self.b.ins().jump(header, &[]);
        self.term = true;

        // header: u = iter.next(); branch on the `Done` tag.
        self.switch(header);
        self.emit_safepoint();
        // Dispatch `next`: a concrete/generic `extend` method is a direct call;
        // an interface method (bounded type param or interface object) resolves
        // to the concrete impl, or goes through the vtable for an object.
        let u = if self.cx.analysis.program.def(info.next).kind == DefKind::InterfaceMethod {
            let recv = resolve_shallow(self.cx.analysis, info.iter_ty, &self.subst);
            if self.is_interface_ty(recv) {
                let slot = self.vtable_slot(info.next)
                    .ok_or_else(|| CodegenError::new(iter.span, "iterator method not in interface"))?;
                self.emit_vtable_call(slot, iter_val, &[], Some(PTR))?
            } else {
                let (target, targs) = self.resolve_iface_method(info.next, recv)
                    .ok_or_else(|| CodegenError::new(iter.span, "cannot resolve iterator `next`"))?;
                self.emit_call(target, targs, &[iter_val], iter.span)?
            }
        } else {
            // Resolve the method's type args through this instance's subst.
            let next_targs: Vec<Ty> = info.next_targs.iter()
                .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                .collect();
            self.emit_call(info.next, next_targs, &[iter_val], iter.span)?
        }
        .ok_or_else(|| CodegenError::new(iter.span, "`next` returned no value"))?;
        self.mark_root(u);
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), u, 0);
        let done_id = self.type_id_of(info.done_ty);
        let done_c = self.b.ins().iconst(types::I64, done_id);
        let is_done = self.b.ins().icmp(IntCC::Equal, tag, done_c);
        self.b.ins().brif(is_done, exit, &[], body_bb, &[]);
        self.term = true;

        // body: unwrap the `Item<U>` payload and bind the loop variable.
        self.switch(body_bb);
        let item_ptr = self.b.ins().load(PTR, MemFlags::trusted(), u, 8);
        let layout = self.layout_for_ty(info.item_ty).ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no layout")
        })?;
        let idx = layout.index_of("value").ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no `value` field")
        })?;
        let off = layout.offsets[idx] as i32;
        let value = match layout.cltys[idx] {
            Some(ct) => {
                let v = self.b.ins().load(ct, MemFlags::trusted(), item_ptr, off);
                let resolved = resolve_shallow(self.cx.analysis, info.elem, &self.subst);
                if is_managed_ptr(self.cx.analysis, resolved) {
                    self.mark_root(v);
                }
                Some(v)
            }
            None => None,
        };
        self.bind_pattern(pattern, value, info.elem)?;
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(None)
    }

    /// Lower `for await x in stream { body }` (`docs/21` §10): each iteration
    /// `await`s `stream.next_async()` (a suspend site), breaks on `Done`, and
    /// binds the unwrapped `Item<T>.value`. Only valid inside an async `poll`
    /// body. The stream must be a simple variable so re-loading it each
    /// iteration (across suspends) is correct.
    fn gen_for_await(&mut self, pattern: &Pattern, iter: &Expr, body: &Block)
        -> CgResult<Option<Value>>
    {
        let info = self.cx.analysis.results.for_async_iters.get(&iter.span).cloned()
            .ok_or_else(|| CodegenError::new(iter.span, "for-await stream was not analysed"))?;
        if !matches!(&iter.kind, ExprKind::Ident(_) | ExprKind::SelfExpr) {
            return Err(CodegenError::new(iter.span,
                "`for await` currently requires the stream to be a variable — \
                 bind it with `var s = …;` first"));
        }

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;

        // header: fut = stream.next_async(); await it (suspends until ready).
        self.switch(header);
        let iter_val = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "stream has no value")
        })?;
        let next_targs: Vec<Ty> = info.next_targs.iter()
            .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
            .collect();
        let fut = self.emit_call(info.next_async, next_targs, &[iter_val], iter.span)?
            .ok_or_else(|| CodegenError::new(iter.span, "`next_async` returned no value"))?;
        let u = self.emit_await_suspend(fut, iter.span, info.union_ty)?
            .ok_or_else(|| CodegenError::new(iter.span, "awaited `next_async` has no value"))?;
        self.mark_root(u);
        let tag = self.b.ins().load(types::I64, MemFlags::trusted(), u, 0);
        let done_id = self.type_id_of(info.done_ty);
        let done_c = self.b.ins().iconst(types::I64, done_id);
        let is_done = self.b.ins().icmp(IntCC::Equal, tag, done_c);
        self.b.ins().brif(is_done, exit, &[], body_bb, &[]);
        self.term = true;

        // body: unwrap `Item<T>.value` and bind the loop variable.
        self.switch(body_bb);
        let item_ptr = self.b.ins().load(PTR, MemFlags::trusted(), u, 8);
        let layout = self.layout_for_ty(info.item_ty).ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no layout")
        })?;
        let idx = layout.index_of("value").ok_or_else(|| {
            CodegenError::new(iter.span, "`Item<T>` has no `value` field")
        })?;
        let off = layout.offsets[idx] as i32;
        let value = match layout.cltys[idx] {
            Some(ct) => {
                let v = self.b.ins().load(ct, MemFlags::trusted(), item_ptr, off);
                let resolved = resolve_shallow(self.cx.analysis, info.elem, &self.subst);
                if is_managed_ptr(self.cx.analysis, resolved) {
                    self.mark_root(v);
                }
                Some(v)
            }
            None => None,
        };
        self.bind_pattern(pattern, value, info.elem)?;
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(None)
    }

    /// Lower `for entry in map { body }`: snapshot the keys, then for each key
    /// build an `Entry<K, V>` (key + looked-up value) and bind the loop variable.
    fn gen_for_map(
        &mut self,
        pattern: &Pattern,
        iter: &Expr,
        body: &Block,
        kt: Ty,
        vt: Ty,
        entry_ty: Ty,
    ) -> CgResult<Option<Value>> {
        let map = self.gen_expr(iter)?.ok_or_else(|| {
            CodegenError::new(iter.span, "map has no value")
        })?;
        self.mark_root(map);
        // A snapshot list of the keys (rooted across the loop).
        let one = self.b.ins().iconst(types::I64, 1);
        let keys = self.call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, one])
            .expect("map_entries returns a list");
        self.mark_root(keys);
        let layout = self.struct_layout(
            match self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, entry_ty, &self.subst)).clone() {
                TyKind::Named { def, .. } => def,
                _ => return Err(CodegenError::new(iter.span, "Entry has no layout")),
            },
            &[kt, vt],
        );

        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);

        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let latch = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(header);
        self.emit_safepoint();
        let i = self.b.use_var(iv);
        let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[keys])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;

        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let key_raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[keys, i2])
            .expect("get key");
        let val_raw = self.call_intrinsic("lang_map_index", &[PTR, types::I64], Some(types::I64), &[map, key_raw])
            .expect("get value");
        // Build the Entry { key, value } struct.
        let entry = self.alloc_struct(&layout);
        if let Some(ko) = layout.index_of("key") {
            if let Some(kv) = self.i64_to_elem(key_raw, kt, iter.span)? {
                self.b.ins().store(MemFlags::trusted(), kv, entry, layout.offsets[ko] as i32);
            }
        }
        if let Some(vo) = layout.index_of("value") {
            if let Some(vv) = self.i64_to_elem(val_raw, vt, iter.span)? {
                self.b.ins().store(MemFlags::trusted(), vv, entry, layout.offsets[vo] as i32);
            }
        }
        self.bind_pattern(pattern, Some(entry), entry_ty)?;
        self.loops.push(LoopCg { continue_block: latch, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(latch, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(latch);
        let i3 = self.b.use_var(iv);
        let inc = self.b.ins().iadd(i3, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(exit);
        Ok(None)
    }

    fn gen_while(&mut self, cond: &Expr, body: &Block) -> CgResult<Option<Value>> {
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();

        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(header);
        self.emit_safepoint();
        let c = self.gen_expr(cond)?.ok_or_else(|| {
            CodegenError::new(cond.span, "loop condition has no value")
        })?;
        self.b.ins().brif(c, body_bb, &[], exit, &[]);
        self.term = true;

        self.switch(body_bb);
        self.loops.push(LoopCg { continue_block: header, break_block: exit, has_value: false });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(header, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(None)
    }

    fn gen_loop(&mut self, body: &Block, result_ty: Ty) -> CgResult<Option<Value>> {
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        let result_ct = self.cx_clty(result_ty);
        if let Some(ct) = result_ct {
            self.b.append_block_param(exit, ct);
        }

        self.b.ins().jump(body_bb, &[]);
        self.term = true;

        self.switch(body_bb);
        self.emit_safepoint();
        self.loops.push(LoopCg {
            continue_block: body_bb,
            break_block: exit,
            has_value: result_ct.is_some(),
        });
        self.gen_block(body)?;
        if !self.term {
            self.b.ins().jump(body_bb, &[]);
            self.term = true;
        }
        self.loops.pop();

        self.switch(exit);
        Ok(result_ct.map(|_| self.b.block_params(exit)[0]))
    }

    fn gen_break(&mut self, value: Option<&Expr>, span: Span) -> CgResult<Option<Value>> {
        let (break_block, has_value) = match self.loops.last() {
            Some(f) => (f.break_block, f.has_value),
            None => return Err(CodegenError::new(span, "`break` outside a loop")),
        };
        if has_value {
            let v = match value {
                Some(e) => self.gen_expr(e)?,
                None => None,
            };
            match v {
                Some(v) => self.b.ins().jump(break_block, &[v.into()]),
                None => {
                    let zero = self.b.ins().iconst(types::I64, 0);
                    self.b.ins().jump(break_block, &[zero.into()])
                }
            };
        } else {
            if let Some(e) = value {
                self.gen_expr(e)?; // evaluate for effect, discard
            }
            self.b.ins().jump(break_block, &[]);
        }
        self.term = true;
        Ok(None)
    }

    fn gen_continue(&mut self, span: Span) -> CgResult<Option<Value>> {
        let cont = match self.loops.last() {
            Some(f) => f.continue_block,
            None => return Err(CodegenError::new(span, "`continue` outside a loop")),
        };
        self.b.ins().jump(cont, &[]);
        self.term = true;
        Ok(None)
    }

    // -- builtin List<T> -----------------------------------------------------

    /// If `ty` (resolved) is `List<E>`, return `E` (resolved).
    fn list_elem_of(&self, ty: Ty) -> Option<Ty> {
        let ty = resolve_shallow(self.cx.analysis, ty, &self.subst);
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Named { def, args }
                if *def == self.cx.analysis.program.list_def && args.len() == 1 =>
            {
                Some(resolve_shallow(self.cx.analysis, args[0], &self.subst))
            }
            _ => None,
        }
    }

    /// Create an empty list, telling the runtime whether elements are managed
    /// pointers (so the collector traces them).
    fn gen_list_new(&mut self, elem: Ty) -> Value {
        let resolved = resolve_shallow(self.cx.analysis, elem, &self.subst);
        let is_ptr = i64::from(is_managed_ptr(self.cx.analysis, resolved));
        let flag = self.b.ins().iconst(types::I64, is_ptr);
        self.call_intrinsic("lang_list_new", &[types::I64], Some(PTR), &[flag])
            .expect("list_new returns a pointer")
    }

    /// Widen an element to the list's 8-byte slot (`i64`).
    fn elem_to_i64(&mut self, v: Option<Value>, elem: Ty, span: Span) -> CgResult<Value> {
        let v = v.ok_or_else(|| CodegenError::new(span, "list element has no value"))?;
        match self.cx_clty(elem) {
            Some(c) if c == types::I64 => Ok(v),
            Some(c) if c.is_int() => Ok(self.b.ins().uextend(types::I64, v)),
            _ => Err(CodegenError::new(span, "this element type is not yet storable in a List")),
        }
    }

    /// Narrow an 8-byte slot back to the element type.
    fn i64_to_elem(&mut self, v: Value, elem: Ty, span: Span) -> CgResult<Option<Value>> {
        match self.cx_clty(elem) {
            Some(c) if c == types::I64 => Ok(Some(v)),
            Some(c) if c.is_int() => Ok(Some(self.b.ins().ireduce(c, v))),
            None => Ok(None),
            _ => Err(CodegenError::new(span, "this element type is not yet readable from a List")),
        }
    }

    fn gen_index_load(&mut self, receiver: &Expr, index: &Expr) -> CgResult<Option<Value>> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        // `map[key]` — panics on a missing key.
        if let Some((kt, vt)) = self.map_kv_of(rty) {
            let map = self.gen_expr(receiver)?.ok_or_else(|| {
                CodegenError::new(receiver.span, "map has no value")
            })?;
            let kv = self.gen_expr(index)?;
            let key = self.elem_to_i64(kv, kt, index.span)?;
            let raw = self.call_intrinsic("lang_map_index", &[PTR, types::I64], Some(types::I64), &[map, key])
                .expect("map_index returns a value");
            return self.i64_to_elem(raw, vt, receiver.span);
        }
        let elem = self.list_elem_of(rty).ok_or_else(|| {
            CodegenError::new(receiver.span, "indexing is only supported on `List` and `Map`")
        })?;
        let list = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "list has no value")
        })?;
        let idx = self.gen_expr(index)?.ok_or_else(|| {
            CodegenError::new(index.span, "index has no value")
        })?;
        let raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, idx])
            .expect("list_get returns a value");
        self.i64_to_elem(raw, elem, receiver.span)
    }

    fn gen_index_store(&mut self, receiver: &Expr, index: &Expr, val: Option<Value>) -> CgResult<()> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        // `map[key] = v` — insert or replace.
        if let Some((kt, vt)) = self.map_kv_of(rty) {
            let map = self.gen_expr(receiver)?.ok_or_else(|| {
                CodegenError::new(receiver.span, "map has no value")
            })?;
            let kv = self.gen_expr(index)?;
            let key = self.elem_to_i64(kv, kt, index.span)?;
            let raw = self.elem_to_i64(val, vt, receiver.span)?;
            self.call_intrinsic("lang_map_set", &[PTR, types::I64, types::I64], None, &[map, key, raw]);
            return Ok(());
        }
        let elem = self.list_elem_of(rty).ok_or_else(|| {
            CodegenError::new(receiver.span, "indexed assignment is only supported on `List` and `Map`")
        })?;
        let list = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "list has no value")
        })?;
        let idx = self.gen_expr(index)?.ok_or_else(|| {
            CodegenError::new(index.span, "index has no value")
        })?;
        let raw = self.elem_to_i64(val, elem, receiver.span)?;
        self.call_intrinsic("lang_list_set", &[PTR, types::I64, types::I64], None, &[list, idx, raw]);
        Ok(())
    }

    /// Lower a builtin `List<E>` method call.
    fn gen_list_method(
        &mut self,
        receiver: &Expr,
        elem: Ty,
        name: &str,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let list = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "list has no value")
        })?;
        match name {
            "push" => {
                let v = self.gen_expr(&args[0])?;
                let raw = self.elem_to_i64(v, elem, args[0].span)?;
                self.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[list, raw]);
                Ok(None)
            }
            "size" => Ok(self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])),
            "is_empty" => {
                let n = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::Equal, n, zero)))
            }
            "set" => {
                let idx = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "index has no value")
                })?;
                let v = self.gen_expr(&args[1])?;
                let raw = self.elem_to_i64(v, elem, args[1].span)?;
                self.call_intrinsic("lang_list_set", &[PTR, types::I64, types::I64], None, &[list, idx, raw]);
                Ok(None)
            }
            // `get(i): E | null` — bounds-checked; result is a boxed union.
            "get" => {
                let idx = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "index has no value")
                })?;
                let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                let ge0 = self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, idx, zero);
                let lt = self.b.ins().icmp(IntCC::SignedLessThan, idx, size);
                let in_range = self.b.ins().band(ge0, lt);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(in_range, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, idx])
                    .expect("get");
                let ev = self.i64_to_elem(raw, elem, receiver.span)?;
                let boxed = self.box_value(ev, elem);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            "map" => self.gen_list_map(list, elem, &args[0]),
            "filter" => self.gen_list_filter(list, elem, &args[0]),
            "each" => self.gen_list_each(list, elem, &args[0]),
            "fold" => self.gen_list_fold(list, elem, &args[0], &args[1]),
            other => Err(CodegenError::new(
                receiver.span,
                format!("`List` method `{other}` is not yet lowerable"),
            )),
        }
    }

    /// The closure-argument's return type (the `R` of its `(…) => R`).
    fn closure_ret(&self, arg: &Expr) -> Ty {
        let fty = resolve_shallow(
            self.cx.analysis,
            self.cx.analysis.results.expr_ty(arg.span).unwrap_or(self.cx.analysis.tcx.error),
            &self.subst,
        );
        match self.cx.analysis.tcx.kind(fty) {
            TyKind::Func { ret, .. } => *ret,
            _ => self.cx.analysis.tcx.error,
        }
    }

    /// `xs.map(f)` — a new list of `f` applied to each element.
    fn gen_list_map(&mut self, list: Value, elem: Ty, fexpr: &Expr) -> CgResult<Option<Value>> {
        self.mark_root(list);
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        let u = self.closure_ret(fexpr);
        let result = self.gen_list_new(u);
        self.mark_root(result);
        let u_clty = self.cx_clty(u);
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            let out = this.emit_closure_call(f, &[ev], u_clty);
            let raw = this.elem_to_i64(out, u, fexpr.span)?;
            this.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[result, raw]);
            Ok(())
        })?;
        Ok(Some(result))
    }

    /// `xs.filter(pred)` — a new list of the elements for which `pred` is true.
    fn gen_list_filter(&mut self, list: Value, elem: Ty, fexpr: &Expr) -> CgResult<Option<Value>> {
        self.mark_root(list);
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        let result = self.gen_list_new(elem);
        self.mark_root(result);
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            let keep = this.emit_closure_call(f, &[ev], Some(types::I8))
                .expect("predicate returns bool");
            let then_bb = this.b.create_block();
            let cont = this.b.create_block();
            this.b.ins().brif(keep, then_bb, &[], cont, &[]);
            this.term = true;
            this.switch(then_bb);
            let raw = this.elem_to_i64(Some(ev), elem, fexpr.span)?;
            this.call_intrinsic("lang_list_push", &[PTR, types::I64], None, &[result, raw]);
            this.b.ins().jump(cont, &[]);
            this.term = true;
            this.switch(cont);
            Ok(())
        })?;
        Ok(Some(result))
    }

    /// `xs.each(f)` — call `f` on each element for its side effects.
    fn gen_list_each(&mut self, list: Value, elem: Ty, fexpr: &Expr) -> CgResult<Option<Value>> {
        self.mark_root(list);
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            this.emit_closure_call(f, &[ev], None);
            Ok(())
        })?;
        Ok(None)
    }

    /// `xs.fold(init, f)` — left fold, threading the accumulator.
    fn gen_list_fold(&mut self, list: Value, elem: Ty, init: &Expr, fexpr: &Expr)
        -> CgResult<Option<Value>>
    {
        self.mark_root(list);
        let acc_ty = self.closure_ret(fexpr);
        let acc_clty = self.cx_clty(acc_ty);
        let init_v = self.gen_expr(init)?;
        let f = self.gen_expr(fexpr)?.ok_or_else(|| {
            CodegenError::new(fexpr.span, "closure has no value")
        })?;
        self.mark_root(f);
        // The accumulator threads through the loop as a block parameter.
        let acc_var = self.b.declare_var(acc_clty.unwrap_or(types::I64));
        if is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, acc_ty, &self.subst)) {
            self.b.declare_var_needs_stack_map(acc_var);
        }
        if let Some(v) = init_v {
            self.b.def_var(acc_var, v);
        }
        self.list_for_each(list, elem, fexpr.span, |this, ev| {
            let acc = this.b.use_var(acc_var);
            let out = this.emit_closure_call(f, &[acc, ev], acc_clty)
                .ok_or_else(|| CodegenError::new(fexpr.span, "fold closure has no result"))?;
            this.b.def_var(acc_var, out);
            Ok(())
        })?;
        Ok(init_v.map(|_| self.b.use_var(acc_var)))
    }

    /// Run `body` for each element of `list` (narrowed to `elem`), as an index
    /// loop. Used by the higher-order `List` methods. `span` is for diagnostics.
    fn list_for_each<F>(&mut self, list: Value, elem: Ty, span: Span, mut body: F) -> CgResult<()>
    where
        F: FnMut(&mut Self, Value) -> CgResult<()>,
    {
        let iv = self.b.declare_var(types::I64);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.def_var(iv, zero);
        let header = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(header);
        self.emit_safepoint();
        let i = self.b.use_var(iv);
        let size = self.call_intrinsic("lang_list_size", &[PTR], Some(types::I64), &[list])
            .expect("size");
        let cond = self.b.ins().icmp(IntCC::SignedLessThan, i, size);
        self.b.ins().brif(cond, body_bb, &[], exit, &[]);
        self.term = true;

        self.switch(body_bb);
        let i2 = self.b.use_var(iv);
        let raw = self.call_intrinsic("lang_list_get", &[PTR, types::I64], Some(types::I64), &[list, i2])
            .expect("get");
        let ev = self.i64_to_elem(raw, elem, span)?
            .ok_or_else(|| CodegenError::new(span, "list element is zero-sized"))?;
        body(self, ev)?;
        let i3 = self.b.use_var(iv);
        let one = self.b.ins().iconst(types::I64, 1);
        let inc = self.b.ins().iadd(i3, one);
        self.b.def_var(iv, inc);
        self.b.ins().jump(header, &[]);
        self.term = true;

        self.switch(exit);
        Ok(())
    }

    // -- builtin Map<K, V> ---------------------------------------------------

    /// If `ty` (resolved) is `Map<K, V>`, return `(K, V)` (both resolved).
    fn map_kv_of(&self, ty: Ty) -> Option<(Ty, Ty)> {
        let ty = resolve_shallow(self.cx.analysis, ty, &self.subst);
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Named { def, args }
                if *def == self.cx.analysis.program.map_def && args.len() == 2 =>
            {
                Some((
                    resolve_shallow(self.cx.analysis, args[0], &self.subst),
                    resolve_shallow(self.cx.analysis, args[1], &self.subst),
                ))
            }
            _ => None,
        }
    }

    /// Create an empty map, telling the runtime whether keys/values are managed
    /// pointers (so the collector traces them).
    fn gen_map_new(&mut self, kt: Ty, vt: Ty) -> Value {
        let kp = i64::from(is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, kt, &self.subst)));
        let vp = i64::from(is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, vt, &self.subst)));
        let kpv = self.b.ins().iconst(types::I64, kp);
        let vpv = self.b.ins().iconst(types::I64, vp);
        self.call_intrinsic("lang_map_new", &[types::I64, types::I64], Some(PTR), &[kpv, vpv])
            .expect("map_new returns a pointer")
    }

    /// Lower a builtin `Map<K, V>` method call.
    fn gen_map_method(
        &mut self,
        receiver: &Expr,
        kt: Ty,
        vt: Ty,
        name: &str,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let map = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "map has no value")
        })?;
        match name {
            "set" => {
                let kv = self.gen_expr(&args[0])?;
                let key = self.elem_to_i64(kv, kt, args[0].span)?;
                let vv = self.gen_expr(&args[1])?;
                let val = self.elem_to_i64(vv, vt, args[1].span)?;
                self.call_intrinsic("lang_map_set", &[PTR, types::I64, types::I64], None, &[map, key, val]);
                Ok(None)
            }
            "size" => Ok(self.call_intrinsic("lang_map_size", &[PTR], Some(types::I64), &[map])),
            "is_empty" => {
                let n = self.call_intrinsic("lang_map_size", &[PTR], Some(types::I64), &[map])
                    .expect("size");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::Equal, n, zero)))
            }
            "clear" => {
                self.call_intrinsic("lang_map_clear", &[PTR], None, &[map]);
                Ok(None)
            }
            "contains" => {
                let kv = self.gen_expr(&args[0])?;
                let key = self.elem_to_i64(kv, kt, args[0].span)?;
                let c = self.call_intrinsic("lang_map_contains", &[PTR, types::I64], Some(types::I64), &[map, key])
                    .expect("contains");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::NotEqual, c, zero)))
            }
            // `get(k): V | null` / `remove(k): V | null` — boxed-union result.
            "get" | "remove" => {
                let removing = name == "remove";
                let kv = self.gen_expr(&args[0])?;
                let key = self.elem_to_i64(kv, kt, args[0].span)?;
                let present = self.call_intrinsic("lang_map_contains", &[PTR, types::I64], Some(types::I64), &[map, key])
                    .expect("contains");
                let zero = self.b.ins().iconst(types::I64, 0);
                let found = self.b.ins().icmp(IntCC::NotEqual, present, zero);

                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(found, then_bb, &[], else_bb, &[]);
                self.term = true;

                self.switch(then_bb);
                let raw = self.call_intrinsic("lang_map_get", &[PTR, types::I64], Some(types::I64), &[map, key])
                    .expect("get");
                let ev = self.i64_to_elem(raw, vt, receiver.span)?;
                let boxed = self.box_value(ev, vt);
                if removing {
                    self.call_intrinsic("lang_map_remove", &[PTR, types::I64], None, &[map, key]);
                }
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;

                self.switch(else_bb);
                let null_box = self.box_value(None, self.cx.analysis.tcx.null);
                self.b.ins().jump(merge, &[null_box.into()]);
                self.term = true;

                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            "keys" | "values" => {
                let want_keys = self.b.ins().iconst(types::I64, i64::from(name == "keys"));
                Ok(self.call_intrinsic("lang_map_entries", &[PTR, types::I64], Some(PTR), &[map, want_keys]))
            }
            other => Err(CodegenError::new(
                receiver.span,
                format!("`Map` method `{other}` is not yet lowerable"),
            )),
        }
    }

    /// Lower a map literal `{ k: v, ..base }` to an allocation plus inserts.
    fn gen_map_lit(&mut self, items: &[MapItem], ty: Ty, span: Span) -> CgResult<Option<Value>> {
        let (kt, vt) = self.map_kv_of(ty).ok_or_else(|| {
            CodegenError::new(span, "map literal has non-map type")
        })?;
        let map = self.gen_map_new(kt, vt);
        for item in items {
            match item {
                MapItem::Entry { key, value, .. } => {
                    let kv = self.gen_expr(key)?;
                    let k = self.elem_to_i64(kv, kt, key.span)?;
                    let vv = self.gen_expr(value)?;
                    let v = self.elem_to_i64(vv, vt, value.span)?;
                    self.call_intrinsic("lang_map_set", &[PTR, types::I64, types::I64], None, &[map, k, v]);
                }
                MapItem::Spread(base) => {
                    let src = self.gen_expr(base)?.ok_or_else(|| {
                        CodegenError::new(base.span, "map spread source has no value")
                    })?;
                    self.call_intrinsic("lang_map_extend", &[PTR, PTR], None, &[map, src]);
                }
            }
        }
        Ok(Some(map))
    }

    /// Lower a builtin `str` method call.
    fn gen_str_method(
        &mut self,
        receiver: &Expr,
        name: &str,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let s = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "str receiver has no value")
        })?;
        let arg_str = |this: &mut Self, i: usize| -> CgResult<Value> {
            this.gen_expr(&args[i])?
                .ok_or_else(|| CodegenError::new(args[i].span, "argument has no value"))
        };
        match name {
            "size" => Ok(self.call_intrinsic("lang_str_size", &[PTR], Some(types::I64), &[s])),
            "byte_size" => {
                Ok(self.call_intrinsic("lang_str_byte_size", &[PTR], Some(types::I64), &[s]))
            }
            "is_empty" => {
                let n = self.call_intrinsic("lang_str_byte_size", &[PTR], Some(types::I64), &[s])
                    .expect("byte_size");
                let zero = self.b.ins().iconst(types::I64, 0);
                Ok(Some(self.b.ins().icmp(IntCC::Equal, n, zero)))
            }
            "contains" | "starts_with" | "ends_with" => {
                let arg = arg_str(self, 0)?;
                let func = match name {
                    "contains" => "lang_str_contains",
                    "starts_with" => "lang_str_starts_with",
                    _ => "lang_str_ends_with",
                };
                Ok(self.call_intrinsic(func, &[PTR, PTR], Some(types::I8), &[s, arg]))
            }
            "substring" => {
                let a = arg_str(self, 0)?;
                let b = arg_str(self, 1)?;
                Ok(self.call_intrinsic(
                    "lang_str_substring",
                    &[PTR, types::I64, types::I64],
                    Some(PTR),
                    &[s, a, b],
                ))
            }
            "to_upper" | "to_lower" | "trim" => {
                let func = match name {
                    "to_upper" => "lang_str_to_upper",
                    "to_lower" => "lang_str_to_lower",
                    _ => "lang_str_trim",
                };
                Ok(self.call_intrinsic(func, &[PTR], Some(PTR), &[s]))
            }
            other => Err(CodegenError::new(
                receiver.span,
                format!("`str` method `{other}` is not yet lowerable"),
            )),
        }
    }

    // -- structs -------------------------------------------------------------

    /// Emit a managed-type descriptor blob (`docs/16` §3 — `gc` module) and
    /// return its address: `[size:u64][kind:u64][n_ptrs:u64][off:u32 …]`.
    fn emit_descriptor(&mut self, size: u32, kind: u64, ptr_offsets: &[u32]) -> Value {
        self.emit_descriptor_with(size, kind, 0, ptr_offsets)
    }

    /// Emit a type descriptor blob `[size][kind][type_id][n_ptrs][offsets…]`.
    /// `type_id` is `0` unless the type has a registered `Drop` finalizer
    /// (`docs/16` §8); the collector reads it to find the drop function.
    fn emit_descriptor_with(&mut self, size: u32, kind: u64, type_id: i64, ptr_offsets: &[u32]) -> Value {
        let mut bytes = Vec::with_capacity(32 + ptr_offsets.len() * 4);
        bytes.extend_from_slice(&(size as u64).to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&(type_id as u64).to_le_bytes());
        bytes.extend_from_slice(&(ptr_offsets.len() as u64).to_le_bytes());
        for o in ptr_offsets {
            bytes.extend_from_slice(&o.to_le_bytes());
        }
        let name = format!("desc.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare descriptor");
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &desc).expect("define descriptor");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        self.b.ins().global_value(PTR, gv)
    }

    /// Allocate a managed object for `layout`, returning the field-block ptr.
    fn alloc_struct(&mut self, layout: &Layout) -> Value {
        let desc = self.emit_descriptor(layout.size, GC_KIND_PLAIN, &layout.ptr_offsets);
        self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer")
    }

    /// Allocate a managed object of nominal type `ty`. If `ty` has a `Drop` impl
    /// (`docs/16` §8), the descriptor carries its type id so the collector can
    /// find the finalizer; otherwise this is identical to [`alloc_struct`].
    fn alloc_struct_typed(&mut self, layout: &Layout, ty: Ty) -> Value {
        let tid = if self.ty_has_drop(ty) { self.type_id_of(ty) } else { 0 };
        let desc = self.emit_descriptor_with(layout.size, GC_KIND_PLAIN, tid, &layout.ptr_offsets);
        self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer")
    }

    /// Whether `ty` (after substitution) has a `Drop` implementation.
    fn ty_has_drop(&self, ty: Ty) -> bool {
        let drop_def = self.cx.analysis.program.drop_def;
        if drop_def == DefId(0) {
            return false;
        }
        let resolved = resolve_shallow(self.cx.analysis, ty, &self.subst);
        if let TyKind::Named { def, .. } = self.cx.analysis.tcx.kind(resolved) {
            return self.cx.analysis.results.iface_impls.contains_key(&(*def, drop_def));
        }
        false
    }

    fn gen_struct_lit(
        &mut self,
        def: DefId,
        args: &[Ty],
        fields: &[FieldInit],
        spread: Option<&Expr>,
        span: Span,
    ) -> CgResult<Value> {
        let layout = self.struct_layout(def, args);
        let sty = self.cx.analysis.results.expr_ty(span).unwrap_or(self.cx.analysis.tcx.error);
        let ptr = self.alloc_struct_typed(&layout, sty);

        // A spread base fills every field first; explicit fields override.
        if let Some(base) = spread {
            let base_ptr = self.gen_expr(base)?.ok_or_else(|| {
                CodegenError::new(base.span, "spread base has no value")
            })?;
            for i in 0..layout.offsets.len() {
                if let Some(ct) = layout.cltys[i] {
                    let off = layout.offsets[i] as i32;
                    let v = self.b.ins().load(ct, MemFlags::trusted(), base_ptr, off);
                    self.b.ins().store(MemFlags::trusted(), v, ptr, off);
                }
            }
        }

        for fi in fields {
            let Some(idx) = layout.index_of(&fi.name.name) else {
                return Err(CodegenError::new(fi.span, "unknown field in struct literal"));
            };
            let off = layout.offsets[idx] as i32;
            let val = match &fi.value {
                Some(e) => self.gen_expr(e)?,
                None => self.gen_local_use(fi.name.span)?, // field-init shorthand
            };
            if let (Some(v), Some(_)) = (val, layout.cltys[idx]) {
                self.b.ins().store(MemFlags::trusted(), v, ptr, off);
            }
        }
        let _ = span;
        Ok(ptr)
    }

    /// Construct a tuple struct from positional arguments.
    fn gen_tuple_ctor(&mut self, def: DefId, args: &[Expr]) -> CgResult<Option<Value>> {
        let layout = self.struct_layout(def, &[]);
        let ptr = self.alloc_struct(&layout);
        for (i, a) in args.iter().enumerate() {
            let off = *layout.offsets.get(i).unwrap_or(&0) as i32;
            let v = self.gen_expr(a)?;
            if let (Some(v), Some(Some(_))) = (v, layout.cltys.get(i)) {
                self.b.ins().store(MemFlags::trusted(), v, ptr, off);
            }
        }
        Ok(Some(ptr))
    }

    /// The field-block layout of a struct named-type, with its generic
    /// arguments (resolved through this instance's substitution).
    fn struct_layout(&self, def: DefId, args: &[Ty]) -> Layout {
        let rargs: Vec<Ty> = args
            .iter()
            .map(|a| resolve_shallow(self.cx.analysis, *a, &self.subst))
            .collect();
        compute_layout(self.cx.analysis, def, &rargs)
    }

    /// The field-block layout of a struct or tuple-typed value.
    fn layout_for_ty(&self, ty: Ty) -> Option<Layout> {
        match self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, ty, &self.subst)).clone() {
            TyKind::Named { def, args } => Some(self.struct_layout(def, &args)),
            TyKind::Tuple(elems) => {
                let re: Vec<Ty> = elems
                    .iter()
                    .map(|e| resolve_shallow(self.cx.analysis, *e, &self.subst))
                    .collect();
                Some(tuple_layout(self.cx.analysis, &re))
            }
            _ => None,
        }
    }

    /// Read a field (record name or tuple/struct position) from a pointer.
    fn gen_field_load(&mut self, receiver: &Expr, field: &str) -> CgResult<Option<Value>> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let Some(layout) = self.layout_for_ty(rty) else {
            return Err(CodegenError::new(receiver.span, "field access on non-aggregate"));
        };
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "receiver has no value")
        })?;
        let Some(idx) = layout.index_of(field) else {
            return Err(CodegenError::new(receiver.span, "unknown field"));
        };
        let off = layout.offsets[idx] as i32;
        match layout.cltys[idx] {
            Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, off))),
            None => Ok(None),
        }
    }

    /// Store `val` into a field/tuple-position target.
    fn gen_field_store(&mut self, receiver: &Expr, field: &str, val: Option<Value>)
        -> CgResult<()>
    {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let Some(layout) = self.layout_for_ty(rty) else {
            return Err(CodegenError::new(receiver.span, "field assignment on non-aggregate"));
        };
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "receiver has no value")
        })?;
        let Some(idx) = layout.index_of(field) else {
            return Err(CodegenError::new(receiver.span, "unknown field"));
        };
        if let (Some(v), Some(_)) = (val, layout.cltys[idx]) {
            self.b.ins().store(MemFlags::trusted(), v, ptr, layout.offsets[idx] as i32);
        }
        Ok(())
    }

    /// Load the local resolved at `span` (used for field-init shorthand).
    fn gen_local_use(&mut self, span: Span) -> CgResult<Option<Value>> {
        let local = self.resolve_local(span)?;
        let var = self.vars.get(&local).copied().ok_or_else(|| {
            CodegenError::new(span, "use of unbound local")
        })?;
        Ok(Some(self.b.use_var(var)))
    }

    fn gen_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        // `Thread.spawn { … }` and `JoinHandle.join()` (`docs/20`): recognised by
        // their checker-recorded span tables (no normal resolution).
        if self.cx.analysis.results.thread_spawns.contains_key(&span) {
            return self.gen_thread_spawn(args, span);
        }
        if self.cx.analysis.results.channel_news.contains(&span) {
            return self.gen_channel_new(span);
        }
        if self.cx.analysis.results.shared_news.contains(&span) {
            return self.gen_shared_new(args, span);
        }
        if self.cx.analysis.results.block_ons.contains_key(&span) {
            return self.gen_block_on(args, span);
        }
        if self.cx.analysis.results.async_spawns.contains_key(&span) {
            return self.gen_async_spawn(args, span);
        }
        if self.cx.analysis.results.yield_nows.contains(&span) {
            let prog = &self.cx.analysis.program;
            let ready_tid = 1000 + prog.ready_def.index() as i64;
            let pending_tid = 1000 + prog.pending_def.index() as i64;
            let rt = self.b.ins().iconst(types::I64, ready_tid);
            let pt = self.b.ins().iconst(types::I64, pending_tid);
            return Ok(self.call_intrinsic(
                "lang_async_yield", &[types::I64, types::I64], Some(PTR), &[rt, pt],
            ));
        }
        if self.cx.analysis.results.async_sleeps.contains(&span) {
            let ms = self.gen_expr(&args[0])?.ok_or_else(|| {
                CodegenError::new(args[0].span, "sleep argument has no value")
            })?;
            let prog = &self.cx.analysis.program;
            let ready_tid = 1000 + prog.ready_def.index() as i64;
            let pending_tid = 1000 + prog.pending_def.index() as i64;
            let rt = self.b.ins().iconst(types::I64, ready_tid);
            let pt = self.b.ins().iconst(types::I64, pending_tid);
            return Ok(self.call_intrinsic(
                "lang_async_sleep", &[types::I64, types::I64, types::I64], Some(PTR), &[ms, rt, pt],
            ));
        }
        // `fut.cancel()` (`docs/21` §8): evaluate the receiver for effect; a
        // compute-only future has nothing to release.
        if let ExprKind::Field { receiver, .. } = &callee.kind {
            if self.cx.analysis.results.future_cancels.contains(&callee.span) {
                self.gen_expr(receiver)?;
                return Ok(None);
            }
        }
        if let Some(intr) = self.cx.analysis.results.num_intrinsics.get(&span).copied() {
            return self.gen_num_intrinsic(intr, args);
        }
        if self.cx.analysis.results.thread_joins.contains_key(&span) {
            if let ExprKind::Field { receiver, .. } = &callee.kind {
                return self.gen_thread_join(receiver, span);
            }
        }
        // Empty-collection constructors `Map<K,V>()` / `List<T>()` (and `.new`
        // forms): the checker recorded the type to allocate, keyed by call span.
        if let Some(ty) = self.cx.analysis.results.builtin_ctors.get(&span).copied() {
            if let Some((kt, vt)) = self.map_kv_of(ty) {
                return Ok(Some(self.gen_map_new(kt, vt)));
            }
            if let Some(elem) = self.list_elem_of(ty) {
                return Ok(Some(self.gen_list_new(elem)));
            }
            return Err(CodegenError::new(span, "unknown builtin constructor"));
        }
        // Builtin `.clone()` for primitives, `str`, and immutable-element
        // collections (`docs/15` §8). User/derived clones resolve as methods.
        if let ExprKind::Field { receiver, .. } = &callee.kind {
            if let Some(kind) = self.cx.analysis.results.clone_kinds.get(&callee.span).copied() {
                return self.gen_builtin_clone(receiver, kind);
            }
        }
        // Builtin `List<E>`/`Map<K,V>`/`str` methods. The checker records no
        // resolution for these — so a *resolved* call (e.g. a `T: Clone` bound's
        // `clone`, monomorphized to a `List` receiver) must skip this and go
        // through the method-dispatch path below instead.
        if let ExprKind::Field { receiver, name } = &callee.kind {
            if self.cx.analysis.results.resolution(callee.span).is_none() {
            let rty = self.cx.analysis.results.expr_ty(receiver.span)
                .unwrap_or(self.cx.analysis.tcx.error);
            if let Some(elem) = self.list_elem_of(rty) {
                return self.gen_list_method(receiver, elem, &name.name, args);
            }
            if let Some((kt, vt)) = self.map_kv_of(rty) {
                return self.gen_map_method(receiver, kt, vt, &name.name, args);
            }
            if matches!(self.cx.analysis.tcx.kind(rty), TyKind::Str) {
                return self.gen_str_method(receiver, &name.name, args);
            }
            // Builtin `Sender<T>`/`Receiver<T>` methods (`docs/20` §2).
            if let TyKind::Named { def, args: targs } = self.cx.analysis.tcx.kind(rty).clone() {
                if def == self.cx.analysis.program.sender_def && self.cx.analysis.program.sender_def != DefId(0) {
                    let elem = targs.first().copied().unwrap_or(self.cx.analysis.tcx.error);
                    return self.gen_channel_send(receiver, elem, args);
                }
                if def == self.cx.analysis.program.receiver_def && self.cx.analysis.program.receiver_def != DefId(0) {
                    let elem = targs.first().copied().unwrap_or(self.cx.analysis.tcx.error);
                    return self.gen_channel_recv(receiver, elem, &name.name, span);
                }
                if def == self.cx.analysis.program.shared_def && self.cx.analysis.program.shared_def != DefId(0) {
                    let elem = targs.first().copied().unwrap_or(self.cx.analysis.tcx.error);
                    return self.gen_shared_lock(receiver, elem, &name.name, args, span);
                }
            }
            }
        }
        // Calling a closure *value* — a local/global of `Func` type, or any
        // other `Func`-typed expression that is not a named function/method.
        let is_value_callee = matches!(
            self.cx.analysis.results.resolution(callee.span),
            Some(ValueRes::Local(_)) | Some(ValueRes::Global(_)) | None
        );
        if is_value_callee {
            let callee_ty = resolve_shallow(
                self.cx.analysis,
                self.cx.analysis.results.expr_ty(callee.span).unwrap_or(self.cx.analysis.tcx.error),
                &self.subst,
            );
            if let TyKind::Func { ret, is_extern: false, .. } = self.cx.analysis.tcx.kind(callee_ty).clone() {
                return self.gen_closure_call(callee, ret, args);
            }
        }
        let def = match self.cx.analysis.results.resolution(callee.span) {
            Some(ValueRes::Function(d)) => d,
            Some(ValueRes::Builtin(b)) => return self.gen_builtin_call(b, args),
            Some(ValueRes::StructCtor(d)) => return self.gen_tuple_ctor(d, args),
            Some(ValueRes::Method(d)) => {
                if self.cx.analysis.results.static_calls.contains(&callee.span) {
                    return self.gen_static_call(d, callee, args, span);
                }
                return self.gen_method_call(d, callee, args, span);
            }
            _ => return Err(CodegenError::new(callee.span, "call target not lowerable")),
        };
        // An `extern function` is a direct C-ABI call by its real symbol name —
        // no monomorphization, no body (`docs/19`).
        if self.cx.analysis.program.def(def).kind == DefKind::ExternFunction {
            return self.gen_extern_call(def, args, span);
        }
        // The instance's generic arguments, resolved through this instance's
        // own substitution (for nested generic calls).
        let targs = self.instance_args(callee.span);
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        self.emit_call(def, targs, &arg_vals, span)
    }

    /// Lower a numeric-namespace intrinsic (`docs/18` §10, `docs/14` §5):
    /// constants, float predicates, and the integer overflow-arithmetic families.
    fn gen_num_intrinsic(&mut self, intr: NumIntrinsic, args: &[Expr]) -> CgResult<Option<Value>> {
        match intr {
            NumIntrinsic::IntBound { ty, max } => {
                let it = self.int_ty_of(ty);
                let (lo, hi) = int_min_max(it);
                let ct = int_clty(it);
                Ok(Some(self.b.ins().iconst(ct, if max { hi } else { lo })))
            }
            NumIntrinsic::FloatConst { ty, kind } => {
                let f = match kind {
                    0 => f64::INFINITY,
                    1 => f64::NEG_INFINITY,
                    _ => f64::NAN,
                };
                Ok(Some(match self.cx.analysis.tcx.kind(ty) {
                    TyKind::Float(FloatTy::F32) => self.b.ins().f32const(f as f32),
                    _ => self.b.ins().f64const(f),
                }))
            }
            NumIntrinsic::FloatPred { ty: _, kind } => {
                let v = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "float predicate arg has no value")
                })?;
                let r = match kind {
                    // is_nan: v != v
                    0 => self.b.ins().fcmp(FloatCC::NotEqual, v, v),
                    // is_infinite: v == +inf || v == -inf
                    1 => {
                        let pinf = self.fconst_like(v, f64::INFINITY);
                        let ninf = self.fconst_like(v, f64::NEG_INFINITY);
                        let a = self.b.ins().fcmp(FloatCC::Equal, v, pinf);
                        let b = self.b.ins().fcmp(FloatCC::Equal, v, ninf);
                        self.b.ins().bor(a, b)
                    }
                    // is_finite: a finite value satisfies v - v == 0 (NaN/±inf give NaN).
                    _ => {
                        let diff = self.b.ins().fsub(v, v);
                        let zero = self.fconst_like(v, 0.0);
                        self.b.ins().fcmp(FloatCC::Equal, diff, zero)
                    }
                };
                Ok(Some(r))
            }
            NumIntrinsic::IntArith { ty, family, op } => {
                self.gen_int_arith(ty, family, op, args)
            }
        }
    }

    /// A float constant of the same Cranelift type as `like`.
    fn fconst_like(&mut self, like: Value, v: f64) -> Value {
        match self.b.func.dfg.value_type(like) {
            types::F32 => self.b.ins().f32const(v as f32),
            _ => self.b.ins().f64const(v),
        }
    }

    /// The `IntTy` behind a primitive integer `Ty` (after substitution).
    fn int_ty_of(&self, ty: Ty) -> IntTy {
        match self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, ty, &self.subst)) {
            TyKind::Int(it) => *it,
            _ => IntTy::I64,
        }
    }

    /// Lower a `{wrapping,saturating,checked,overflowing}_{add,sub,mul}` call.
    fn gen_int_arith(&mut self, ty: Ty, family: u8, op: u8, args: &[Expr]) -> CgResult<Option<Value>> {
        let it = self.int_ty_of(ty);
        let signed = it.is_signed();
        let a = self.gen_expr(&args[0])?.ok_or_else(|| CodegenError::new(args[0].span, "arg"))?;
        let b = self.gen_expr(&args[1])?.ok_or_else(|| CodegenError::new(args[1].span, "arg"))?;
        let (res, ovf) = match (op, signed) {
            (0, true) => self.b.ins().sadd_overflow(a, b),
            (0, false) => self.b.ins().uadd_overflow(a, b),
            (1, true) => self.b.ins().ssub_overflow(a, b),
            (1, false) => self.b.ins().usub_overflow(a, b),
            (2, true) => self.b.ins().smul_overflow(a, b),
            _ => self.b.ins().umul_overflow(a, b),
        };
        let ct = int_clty(it);
        match family {
            0 => Ok(Some(res)), // wrapping: the two's-complement result
            1 => {
                // saturating: on overflow clamp to MIN/MAX by the result's sign.
                let (lo, hi) = int_min_max(it);
                let min = self.b.ins().iconst(ct, lo);
                let max = self.b.ins().iconst(ct, hi);
                let zero = self.b.ins().iconst(ct, 0);
                let clamp = if !signed {
                    // unsigned: add/mul overflow → MAX; sub underflow → MIN(0).
                    if op == 1 { min } else { max }
                } else {
                    // signed: pick by the sign the true result would have had.
                    let to_max = match op {
                        0 => self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, zero), // a+b overflow: same sign as a
                        1 => self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, zero), // a-b overflow: sign of a
                        _ => {
                            // mul: result positive iff operands have equal sign.
                            let an = self.b.ins().icmp(IntCC::SignedLessThan, a, zero);
                            let bn = self.b.ins().icmp(IntCC::SignedLessThan, b, zero);
                            let diff = self.b.ins().bxor(an, bn);
                            let one = self.b.ins().iconst(types::I8, 1);
                            self.b.ins().bxor(diff, one) // positive (→MAX) when signs equal
                        }
                    };
                    self.b.ins().select(to_max, max, min)
                };
                Ok(Some(self.b.ins().select(ovf, clamp, res)))
            }
            2 => {
                // checked: `T | null` — null on overflow, else the value boxed.
                let union_ty = self.cx.analysis.tcx.error; // not needed; box by elem
                let _ = union_ty;
                let one = self.b.ins().iconst(types::I8, 1);
                let no_ovf = self.b.ins().bxor(ovf, one);
                // Build the union: value box when no overflow, else null box.
                let some_bb = self.b.create_block();
                let none_bb = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, PTR);
                self.b.ins().brif(no_ovf, some_bb, &[], none_bb, &[]);
                self.term = true;
                self.switch(some_bb);
                let boxed = self.box_value(Some(res), ty);
                self.b.ins().jump(merge, &[boxed.into()]);
                self.term = true;
                self.switch(none_bb);
                let null_ty = self.cx.analysis.tcx.null;
                let nb = self.box_value(None, null_ty);
                self.b.ins().jump(merge, &[nb.into()]);
                self.term = true;
                self.switch(merge);
                Ok(Some(self.b.block_params(merge)[0]))
            }
            _ => {
                // overflowing: `(T, bool)` tuple.
                let elems = vec![ty, self.cx.analysis.tcx.bool];
                let layout = tuple_layout(self.cx.analysis, &elems);
                let ptr = self.alloc_struct(&layout);
                self.b.ins().store(MemFlags::trusted(), res, ptr, layout.offsets[0] as i32);
                self.b.ins().store(MemFlags::trusted(), ovf, ptr, layout.offsets[1] as i32);
                Ok(Some(ptr))
            }
        }
    }

    /// Lower a builtin `.clone()` (`docs/15` §8). Immutable values clone to
    /// themselves (sharing is sound); collections of immutable elements copy
    /// their backing storage into a fresh managed object.
    fn gen_builtin_clone(
        &mut self,
        receiver: &Expr,
        kind: CloneKind,
    ) -> CgResult<Option<Value>> {
        
        let v = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "clone receiver has no value")
        })?;
        Ok(match kind {
            CloneKind::Identity => Some(v),
            CloneKind::List => self.call_intrinsic("lang_list_clone", &[PTR], Some(PTR), &[v]),
            CloneKind::Map => self.call_intrinsic("lang_map_clone", &[PTR], Some(PTR), &[v]),
        })
    }

    /// The intrinsic [`CloneKind`] for a builtin receiver type, or `None` for a
    /// user type (which clones through its own `Clone` impl). Mirrors the
    /// checker's `check_builtin_clone` so monomorphized `T: Clone` dispatch
    /// agrees with direct `.clone()` calls.
    fn builtin_clone_kind(&self, ty: Ty) -> Option<CloneKind> {
        if matches!(
            self.cx.analysis.tcx.kind(ty),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Null
        ) {
            return Some(CloneKind::Identity);
        }
        if self.list_elem_of(ty).is_some() {
            return Some(CloneKind::List);
        }
        if self.map_kv_of(ty).is_some() {
            return Some(CloneKind::Map);
        }
        None
    }

    /// Lower `Thread.spawn { … }` (`docs/20` §1): evaluate the closure to its
    /// heap environment, spawn an OS thread to run it, and wrap the returned
    /// worker id in a `JoinHandle<R>`.
    fn gen_thread_spawn(&mut self, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let clo = args.first().ok_or_else(|| {
            CodegenError::new(span, "`Thread.spawn` closure argument missing")
        })?;
        let env = self.gen_expr(clo)?.ok_or_else(|| {
            CodegenError::new(clo.span, "spawn closure has no value")
        })?;
        let id = self
            .call_intrinsic("lang_thread_spawn", &[PTR], Some(types::I64), &[env])
            .expect("lang_thread_spawn returns an id");
        let r = self.cx.analysis.results.thread_spawns.get(&span).copied()
            .unwrap_or(self.cx.analysis.tcx.error);
        let jh_def = self.cx.analysis.program.join_handle_def;
        let layout = self.struct_layout(jh_def, &[r]);
        let ptr = self.alloc_struct(&layout);
        let off = layout.offsets[layout.index_of("id").unwrap_or(0)] as i32;
        self.b.ins().store(MemFlags::trusted(), id, ptr, off);
        // Pin the handle as a global root for its lifetime: it may be held on a
        // thread whose stack a collector cannot perfectly reconstruct, and it is
        // tiny, so pinning until `join` is both simple and robust (`docs/20`).
        self.call_intrinsic("lang_gc_pin", &[PTR], None, &[ptr]);
        Ok(Some(ptr))
    }

    /// Lower `spawn(fut)` (`docs/21` §6): hand the future to a worker thread
    /// that drives it to completion, returning a `JoinHandle<Out>` (the same
    /// handle type `Thread.spawn` produces, so `.join()` works unchanged).
    fn gen_async_spawn(&mut self, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let fut = self.gen_expr(&args[0])?.ok_or_else(|| {
            CodegenError::new(args[0].span, "spawn argument has no value")
        })?;
        let pending_tid = 1000 + self.cx.analysis.program.pending_def.index() as i64;
        let ptid = self.b.ins().iconst(types::I64, pending_tid);
        let id = self
            .call_intrinsic("lang_async_spawn", &[PTR, types::I64], Some(types::I64), &[fut, ptid])
            .expect("lang_async_spawn returns an id");
        let r = self.cx.analysis.results.async_spawns.get(&span).copied()
            .unwrap_or(self.cx.analysis.tcx.error);
        let jh_def = self.cx.analysis.program.join_handle_def;
        let layout = self.struct_layout(jh_def, &[r]);
        let ptr = self.alloc_struct(&layout);
        let off = layout.offsets[layout.index_of("id").unwrap_or(0)] as i32;
        self.b.ins().store(MemFlags::trusted(), id, ptr, off);
        self.call_intrinsic("lang_gc_pin", &[PTR], None, &[ptr]);
        Ok(Some(ptr))
    }

    /// Lower `JoinHandle<R>.join()` (`docs/20` §1): block on the worker, then
    /// build `Joined<R> { value } | Panicked { message }` from the result.
    fn gen_thread_join(&mut self, receiver: &Expr, span: Span) -> CgResult<Option<Value>> {
        let r = self.cx.analysis.results.thread_joins.get(&span).copied()
            .unwrap_or(self.cx.analysis.tcx.error);
        let jh = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "join receiver has no value")
        })?;
        let jh_def = self.cx.analysis.program.join_handle_def;
        let jh_layout = self.struct_layout(jh_def, &[r]);
        let id_off = jh_layout.offsets[jh_layout.index_of("id").unwrap_or(0)] as i32;
        let id = self.b.ins().load(types::I64, MemFlags::trusted(), jh, id_off);
        // The handle is consumed by `join`; unpin it from the global roots.
        self.call_intrinsic("lang_gc_unpin", &[PTR], None, &[jh]);
        let result = self
            .call_intrinsic("lang_thread_join", &[types::I64], Some(types::I64), &[id])
            .expect("join result");
        let panicked = self
            .call_intrinsic("lang_thread_panicked", &[types::I64], Some(types::I64), &[id])
            .expect("panicked flag");

        // The `Joined<R> | Panicked` union and its (checker-interned) variants.
        let union_ty = self.cx.analysis.results.expr_ty(span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let joined_def = self.cx.analysis.program.joined_def;
        let panicked_def = self.cx.analysis.program.panicked_def;
        let variants = self.cx.analysis.tcx.variants(union_ty);
        let find = |want: DefId| {
            variants.iter().copied().find(|t| {
                matches!(self.cx.analysis.tcx.kind(*t), TyKind::Named { def, .. } if *def == want)
            }).unwrap_or(self.cx.analysis.tcx.error)
        };
        let joined_ty = find(joined_def);
        let panicked_ty = find(panicked_def);

        let joined_bb = self.b.create_block();
        let panic_bb = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, PTR);
        let zero = self.b.ins().iconst(types::I64, 0);
        let is_panic = self.b.ins().icmp(IntCC::NotEqual, panicked, zero);
        self.b.ins().brif(is_panic, panic_bb, &[], joined_bb, &[]);
        self.term = true;

        // joined: `Joined<R> { value }` (narrow the word result to `R`), boxed.
        self.switch(joined_bb);
        let val = self.i64_to_elem(result, r, span)?;
        let r_res = resolve_shallow(self.cx.analysis, r, &self.subst);
        if is_managed_ptr(self.cx.analysis, r_res) {
            if let Some(v) = val {
                self.mark_root(v);
            }
        }
        let jlayout = self.struct_layout(joined_def, &[r]);
        let jptr = self.alloc_struct(&jlayout);
        if let Some(v) = val {
            let off = jlayout.offsets[jlayout.index_of("value").unwrap_or(0)] as i32;
            self.b.ins().store(MemFlags::trusted(), v, jptr, off);
        }
        let boxed_j = self.box_value(Some(jptr), joined_ty);
        self.b.ins().jump(merge, &[boxed_j.into()]);
        self.term = true;

        // panicked: `Panicked { message }`, boxed.
        self.switch(panic_bb);
        let msg = self
            .call_intrinsic("lang_thread_message", &[types::I64], Some(PTR), &[id])
            .expect("panic message");
        self.mark_root(msg);
        let plyt = self.struct_layout(panicked_def, &[]);
        let pptr = self.alloc_struct(&plyt);
        let moff = plyt.offsets[plyt.index_of("message").unwrap_or(0)] as i32;
        self.b.ins().store(MemFlags::trusted(), msg, pptr, moff);
        let boxed_p = self.box_value(Some(pptr), panicked_ty);
        self.b.ins().jump(merge, &[boxed_p.into()]);
        self.term = true;

        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    /// Lower `channel<T>()` (`docs/20` §2): allocate a runtime channel and build
    /// the `(Sender<T>, Receiver<T>)` tuple, both carrying the channel id.
    fn gen_channel_new(&mut self, span: Span) -> CgResult<Option<Value>> {
        let id = self.call_intrinsic("lang_channel_new", &[], Some(types::I64), &[])
            .expect("channel id");
        let result_ty = self.cx.analysis.results.expr_ty(span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let elem_tys = match self.cx.analysis.tcx.kind(result_ty).clone() {
            TyKind::Tuple(ts) => ts,
            _ => return Err(CodegenError::new(span, "`channel` result is not a tuple")),
        };
        let sender = self.build_channel_end(elem_tys[0], id, span)?;
        self.mark_root(sender);
        let receiver = self.build_channel_end(elem_tys[1], id, span)?;
        self.mark_root(receiver);
        let layout = tuple_layout(self.cx.analysis, &elem_tys);
        let tup = self.alloc_struct(&layout);
        self.b.ins().store(MemFlags::trusted(), sender, tup, layout.offsets[0] as i32);
        self.b.ins().store(MemFlags::trusted(), receiver, tup, layout.offsets[1] as i32);
        Ok(Some(tup))
    }

    /// Allocate a `Sender<T>`/`Receiver<T>` struct holding the channel `id`.
    fn build_channel_end(&mut self, end_ty: Ty, id: Value, span: Span) -> CgResult<Value> {
        let resolved = resolve_shallow(self.cx.analysis, end_ty, &self.subst);
        let (def, args) = match self.cx.analysis.tcx.kind(resolved).clone() {
            TyKind::Named { def, args } => (def, args),
            _ => return Err(CodegenError::new(span, "channel end is not a struct")),
        };
        let layout = self.struct_layout(def, &args);
        let p = self.alloc_struct(&layout);
        let off = layout.offsets[layout.index_of("chan").unwrap_or(0)] as i32;
        self.b.ins().store(MemFlags::trusted(), id, p, off);
        Ok(p)
    }

    /// Lower `Sender<T>.send(value)` (`docs/20` §2): enqueue onto the channel.
    fn gen_channel_send(&mut self, receiver: &Expr, elem: Ty, args: &[Expr]) -> CgResult<Option<Value>> {
        let chan = self.gen_channel_id(receiver)?;
        let v = self.gen_expr(&args[0])?;
        let raw = self.elem_to_i64(v, elem, args[0].span)?;
        self.call_intrinsic("lang_chan_send", &[types::I64, types::I64], None, &[chan, raw]);
        Ok(None)
    }

    /// Lower `Receiver<T>.recv()` / `.try_recv()` (`docs/20` §2).
    fn gen_channel_recv(&mut self, receiver: &Expr, elem: Ty, method: &str, span: Span)
        -> CgResult<Option<Value>>
    {
        let chan = self.gen_channel_id(receiver)?;
        if method == "recv" {
            let raw = self.call_intrinsic("lang_chan_recv", &[types::I64], Some(types::I64), &[chan])
                .expect("recv value");
            return self.i64_to_elem(raw, elem, span);
        }
        // try_recv: returns `T | null` — null when the queue is empty.
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot, 8, 3,
        ));
        let has_ptr = self.b.ins().stack_addr(PTR, slot, 0);
        let raw = self.call_intrinsic(
            "lang_chan_try_recv", &[types::I64, PTR], Some(types::I64), &[chan, has_ptr],
        ).expect("try_recv value");
        let has = self.b.ins().load(types::I64, MemFlags::trusted(), has_ptr, 0);
        let zero = self.b.ins().iconst(types::I64, 0);
        let got = self.b.ins().icmp(IntCC::NotEqual, has, zero);
        // Build the `T | null` union: a value box when present, else a null ptr.
        let some_bb = self.b.create_block();
        let none_bb = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, PTR);
        self.b.ins().brif(got, some_bb, &[], none_bb, &[]);
        self.term = true;
        self.switch(some_bb);
        let val = self.i64_to_elem(raw, elem, span)?;
        if is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, elem, &self.subst)) {
            if let Some(v) = val { self.mark_root(v); }
        }
        let boxed = self.box_value(val, elem);
        self.b.ins().jump(merge, &[boxed.into()]);
        self.term = true;
        self.switch(none_bb);
        // The empty case is `null` *boxed into the union* (a box tagged with the
        // null type id), not a raw null pointer — so `match`/`is` dispatch works.
        let null_ty = self.cx.analysis.tcx.null;
        let null_box = self.box_value(None, null_ty);
        self.b.ins().jump(merge, &[null_box.into()]);
        self.term = true;
        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    /// Read the channel id field from a `Sender`/`Receiver` receiver value.
    fn gen_channel_id(&mut self, receiver: &Expr) -> CgResult<Value> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let layout = self.layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "channel end is not a struct"))?;
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "channel receiver has no value")
        })?;
        let off = layout.offsets[layout.index_of("chan").unwrap_or(0)] as i32;
        Ok(self.b.ins().load(types::I64, MemFlags::trusted(), ptr, off))
    }

    /// Lower `Shared.new(value)` (`docs/20` §4): create a runtime mutex cell and
    /// wrap its id in a `Shared<T>` handle.
    fn gen_shared_new(&mut self, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let elem = self.cx.analysis.results.expr_ty(args[0].span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let v = self.gen_expr(&args[0])?;
        let raw = self.elem_to_i64(v, elem, args[0].span)?;
        let id = self.call_intrinsic("lang_shared_new", &[types::I64], Some(types::I64), &[raw])
            .expect("shared id");
        let result_ty = self.cx.analysis.results.expr_ty(span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let shared = self.build_channel_end(result_ty, id, span)?; // {id} struct, same shape
        Ok(Some(shared))
    }

    /// Lower `Shared<T>.lock(body)` / `.try_lock(body)` (`docs/20` §4): acquire
    /// the lock, run the closure with the protected value, release.
    fn gen_shared_lock(&mut self, receiver: &Expr, elem: Ty, method: &str, args: &[Expr], span: Span)
        -> CgResult<Option<Value>>
    {
        let try_lock = method == "try_lock";
        let id = self.gen_shared_id(receiver)?;
        // The closure that runs under the lock, and its result clty.
        let r_ty = self.cx.analysis.results.closures.get(&args[0].span).map(|c| c.ret)
            .unwrap_or(self.cx.analysis.tcx.error);
        let r_clty = self.cx_clty(r_ty);

        if !try_lock {
            let raw = self.call_intrinsic("lang_shared_lock", &[types::I64], Some(types::I64), &[id])
                .expect("lock value");
            let inner = self.i64_to_elem(raw, elem, span)?;
            let env = self.gen_expr(&args[0])?.ok_or_else(|| {
                CodegenError::new(args[0].span, "lock body has no value")
            })?;
            let call_args: Vec<Value> = inner.into_iter().collect();
            let r = self.emit_closure_call(env, &call_args, r_clty);
            self.call_intrinsic("lang_shared_unlock", &[types::I64], None, &[id]);
            return Ok(r);
        }

        // try_lock → `R | LockBusy`.
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let got_ptr = self.b.ins().stack_addr(PTR, slot, 0);
        let raw = self.call_intrinsic(
            "lang_shared_try_lock", &[types::I64, PTR], Some(types::I64), &[id, got_ptr],
        ).expect("try_lock value");
        let got = self.b.ins().load(types::I64, MemFlags::trusted(), got_ptr, 0);
        let zero = self.b.ins().iconst(types::I64, 0);
        let acquired = self.b.ins().icmp(IntCC::NotEqual, got, zero);

        let union_ty = self.cx.analysis.results.expr_ty(span).unwrap_or(self.cx.analysis.tcx.error);
        let busy_def = self.cx.analysis.program.lock_busy_def;
        let busy_ty = self.cx.analysis.tcx.variants(union_ty).into_iter()
            .find(|t| matches!(self.cx.analysis.tcx.kind(*t), TyKind::Named { def, .. } if *def == busy_def))
            .unwrap_or(self.cx.analysis.tcx.error);

        let ok_bb = self.b.create_block();
        let busy_bb = self.b.create_block();
        let merge = self.b.create_block();
        self.b.append_block_param(merge, PTR);
        self.b.ins().brif(acquired, ok_bb, &[], busy_bb, &[]);
        self.term = true;

        self.switch(ok_bb);
        let inner = self.i64_to_elem(raw, elem, span)?;
        let env = self.gen_expr(&args[0])?.ok_or_else(|| {
            CodegenError::new(args[0].span, "try_lock body has no value")
        })?;
        let call_args: Vec<Value> = inner.into_iter().collect();
        let r = self.emit_closure_call(env, &call_args, r_clty);
        self.call_intrinsic("lang_shared_unlock", &[types::I64], None, &[id]);
        if is_managed_ptr(self.cx.analysis, resolve_shallow(self.cx.analysis, r_ty, &self.subst)) {
            if let Some(v) = r { self.mark_root(v); }
        }
        let boxed_ok = self.box_value(r, r_ty);
        self.b.ins().jump(merge, &[boxed_ok.into()]);
        self.term = true;

        self.switch(busy_bb);
        let busy_box = self.box_value(None, busy_ty);
        self.b.ins().jump(merge, &[busy_box.into()]);
        self.term = true;

        self.switch(merge);
        Ok(Some(self.b.block_params(merge)[0]))
    }

    /// Read the channel/mutex id field from a `Shared` receiver value.
    fn gen_shared_id(&mut self, receiver: &Expr) -> CgResult<Value> {
        let rty = self.cx.analysis.results.expr_ty(receiver.span)
            .unwrap_or(self.cx.analysis.tcx.error);
        let layout = self.layout_for_ty(rty)
            .ok_or_else(|| CodegenError::new(receiver.span, "`Shared` is not a struct"))?;
        let ptr = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "`Shared` receiver has no value")
        })?;
        let off = layout.offsets[layout.index_of("id").unwrap_or(0)] as i32;
        Ok(self.b.ins().load(types::I64, MemFlags::trusted(), ptr, off))
    }

    /// Lower a call to an `extern function`: declare it as a C-ABI import by its
    /// real symbol name (the `object` crate applies platform mangling for native
    /// output; the JIT resolves it via `dlsym`) and call it directly (`docs/19`).
    fn gen_extern_call(&mut self, def: DefId, args: &[Expr], span: Span) -> CgResult<Option<Value>> {
        let (ptys, rty) = self
            .cx
            .analysis
            .results
            .extern_sigs
            .get(&def)
            .cloned()
            .ok_or_else(|| CodegenError::new(span, "extern signature not recorded"))?;
        let mut sig = self.module.make_signature();
        for pt in &ptys {
            let ct = clty_of(self.cx.analysis, *pt)
                .ok_or_else(|| CodegenError::new(span, "extern parameter is zero-sized"))?;
            sig.params.push(AbiParam::new(ct));
        }
        let ret_clty = clty_of(self.cx.analysis, rty);
        if let Some(rc) = ret_clty {
            sig.returns.push(AbiParam::new(rc));
        }
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self
                .gen_expr(a)?
                .ok_or_else(|| CodegenError::new(a.span, "extern argument has no value"))?;
            arg_vals.push(v);
        }
        let name = self.cx.analysis.program.def(def).name.clone();
        let id = self
            .module
            .declare_function(&name, Linkage::Import, &sig)
            .map_err(|e| CodegenError::new(span, format!("declare extern `{name}`: {e}")))?;
        let fref = self.module.declare_func_in_func(id, self.b.func);
        let inst = self.b.ins().call(fref, &arg_vals);
        Ok(self.b.inst_results(inst).first().copied())
    }

    /// The generic arguments recorded for the call at `callee_span`, resolved
    /// through the current instance's substitution.
    fn instance_args(&self, callee_span: Span) -> Vec<Ty> {
        match self.cx.analysis.results.type_args(callee_span) {
            Some(ts) => ts
                .iter()
                .map(|t| resolve_shallow(self.cx.analysis, *t, &self.subst))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Lower a static method call `Type.method(args)` / `T.method(args)`
    /// (`docs/09` §6, `docs/10`): no receiver is passed. For an interface static
    /// method reached through a bound, resolve it to the concrete impl using the
    /// (substituted) receiver type the checker recorded.
    fn gen_static_call(
        &mut self,
        def: DefId,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let (target, targs) = if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod {
            let recv = self.cx.analysis.results.static_recv.get(&callee.span).copied()
                .unwrap_or(self.cx.analysis.tcx.error);
            let recv = resolve_shallow(self.cx.analysis, recv, &self.subst);
            self.resolve_iface_method(def, recv).ok_or_else(|| {
                CodegenError::new(span, "cannot resolve static interface method to a concrete impl")
            })?
        } else {
            (def, self.instance_args(callee.span))
        };
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        self.emit_call(target, targs, &arg_vals, span)
    }

    /// Lower `recv.method(args)`: the receiver becomes the leading `self` arg.
    fn gen_method_call(
        &mut self,
        def: DefId,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let ExprKind::Field { receiver, .. } = &callee.kind else {
            return Err(CodegenError::new(span, "malformed method call"));
        };
        let recv_ty = resolve_shallow(
            self.cx.analysis,
            self.cx.analysis.results.expr_ty(receiver.span).unwrap_or(self.cx.analysis.tcx.error),
            &self.subst,
        );
        // A method on an interface object dispatches dynamically via its vtable.
        if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod
            && matches!(self.cx.analysis.tcx.kind(recv_ty),
                TyKind::Named { def: d, .. } if self.cx.analysis.program.def(*d).kind == DefKind::Interface)
        {
            return self.gen_dyn_method_call(def, receiver, args, span);
        }
        // `Clone.clone` reached through a `T: Clone` bound: if the monomorphized
        // receiver is a builtin-cloneable type (primitive/`str`/immutable
        // collection), emit the intrinsic clone rather than seeking an `extend`.
        if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod
            && self.cx.analysis.program.def(def).parent == Some(self.cx.analysis.program.clone_def)
        {
            if let Some(kind) = self.builtin_clone_kind(recv_ty) {
                return self.gen_builtin_clone(receiver, kind);
            }
        }
        // An interface method on a generic type parameter is resolved to the
        // concrete `extend` impl of whatever the parameter was monomorphized to.
        let (target, targs) = if self.cx.analysis.program.def(def).kind == DefKind::InterfaceMethod {
            self.resolve_iface_method(def, recv_ty).ok_or_else(|| {
                CodegenError::new(span, "cannot resolve interface method to a concrete impl")
            })?
        } else {
            // A generic `extend`'s method takes the extend's type arguments,
            // recorded by the checker at the call site.
            (def, self.instance_args(callee.span))
        };
        let self_val = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "method receiver has no value")
        })?;
        let mut arg_vals = vec![self_val];
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        self.emit_call(target, targs, &arg_vals, span)
    }

    /// Lower a closure expression to a heap environment `[fn_ptr, captures…]`
    /// and queue its lifted function for compilation. The environment pointer
    /// is the closure value.
    fn gen_closure(&mut self, body: &Expr, span: Span) -> CgResult<Option<Value>> {
        let info = self.cx.analysis.results.closures.get(&span).cloned()
            .ok_or_else(|| CodegenError::new(span, "closure was not analysed"))?;

        // Declare the lifted function: (env, params…) -> ret.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        for (_, ty) in &info.params {
            let ct = self.cx_clty(*ty)
                .ok_or_else(|| CodegenError::new(span, "closure parameter is zero-sized"))?;
            sig.params.push(AbiParam::new(ct));
        }
        if let Some(rc) = self.cx_clty(info.ret) {
            sig.returns.push(AbiParam::new(rc));
        }
        let name = format!("closure.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let func_id = self.module.declare_function(&name, Linkage::Local, &sig)
            .expect("declare closure");

        // Environment layout: [fn_ptr][cap0][cap1]… ; managed captures are GC
        // roots traced via the descriptor.
        let n = info.captures.len();
        let size = (8 + n * 8) as u32;
        let mut ptr_offsets = Vec::new();
        for (k, (_, ty)) in info.captures.iter().enumerate() {
            let resolved = resolve_shallow(self.cx.analysis, *ty, &self.subst);
            if is_managed_ptr(self.cx.analysis, resolved) {
                ptr_offsets.push((8 + k * 8) as u32);
            }
        }
        let desc = self.emit_descriptor(size, GC_KIND_PLAIN, &ptr_offsets);
        let env = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        // Store the function pointer at offset 0.
        let fref = self.module.declare_func_in_func(func_id, self.b.func);
        let faddr = self.b.ins().func_addr(PTR, fref);
        self.b.ins().store(MemFlags::trusted(), faddr, env, 0);
        // Capture each enclosing local by value.
        for (k, (local, _)) in info.captures.iter().enumerate() {
            let var = *self.vars.get(local)
                .ok_or_else(|| CodegenError::new(span, "captured local has no slot"))?;
            let v = self.b.use_var(var);
            self.b.ins().store(MemFlags::trusted(), v, env, (8 + k * 8) as i32);
        }

        self.closures.push(ClosureJob {
            func_id,
            info,
            body: body.clone(),
            subst: self.subst.clone(),
            span,
        });
        Ok(Some(env))
    }

    /// Lower a bare `async { … }` block (`docs/21` §6) to a `Future` state
    /// machine: allocate a state struct holding the captured locals, wrap it in
    /// a `Future<Output>` box, and queue the block's body as the `poll` function.
    fn gen_async_block(&mut self, block: &Block, span: Span) -> CgResult<Option<Value>> {
        let info = self.cx.analysis.results.async_blocks.get(&span).cloned()
            .ok_or_else(|| CodegenError::new(span, "async block was not analysed"))?;
        if !info.params.is_empty() {
            return Err(CodegenError::new(span, "async closure lowering is not yet implemented"));
        }

        // Declare the poll function: (self: ptr, ctx: ptr) -> ptr.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR));
        sig.params.push(AbiParam::new(PTR));
        sig.returns.push(AbiParam::new(PTR));
        let name = format!("asyncblk.{}$poll", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let poll_fid = self.module.declare_function(&name, Linkage::Local, &sig)
            .expect("declare async block poll");

        // A block containing `await` needs the full state-machine layout (room
        // for every body local + the inner future); an await-free block only
        // needs to store the captures. The constructor here and the `poll`
        // function (in `define_async_job`) compute the same layout.
        let (size, ptr_offsets, cap_offs): (u32, Vec<u32>, Vec<i32>) = if block_has_await(block) {
            let cap_ids: Vec<LocalId> = info.captures.iter().map(|(l, _)| *l).collect();
            let layout = async_state_layout(self.cx.analysis, &self.subst, &cap_ids, block);
            let cap_offs = cap_ids.iter().map(|l| layout.slot_off[l]).collect();
            (layout.state_size, layout.ptr_offsets, cap_offs)
        } else {
            // [state @0][cap0 @8][cap1 @16]…
            let n = info.captures.len();
            let mut ptr_offsets = Vec::new();
            let mut cap_offs = Vec::new();
            for (k, (_, ty)) in info.captures.iter().enumerate() {
                let off = (8 + k * 8) as i32;
                cap_offs.push(off);
                let resolved = resolve_shallow(self.cx.analysis, *ty, &self.subst);
                if is_managed_ptr(self.cx.analysis, resolved) {
                    ptr_offsets.push(off as u32);
                }
            }
            ((8 + n * 8) as u32, ptr_offsets, cap_offs)
        };
        let desc = self.emit_descriptor(size, GC_KIND_PLAIN, &ptr_offsets);
        let state = self.call_intrinsic("lang_alloc", &[PTR], Some(PTR), &[desc])
            .expect("lang_alloc returns a pointer");
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().store(MemFlags::trusted(), zero, state, 0);
        for (k, (local, _)) in info.captures.iter().enumerate() {
            let var = *self.vars.get(local)
                .ok_or_else(|| CodegenError::new(span, "captured local has no slot"))?;
            let v = self.b.use_var(var);
            self.b.ins().store(MemFlags::trusted(), v, state, cap_offs[k]);
        }
        let out = info.output;
        let fut = self.emit_future_box(poll_fid, state);
        // The block body becomes the poll function body.
        let body = Expr { kind: ExprKind::Block(block.clone()), span };
        self.async_jobs.push(AsyncJob {
            poll_fid,
            info,
            body,
            subst: self.subst.clone(),
            span,
            out,
        });
        Ok(Some(fut))
    }

    /// Call a closure value: load its function pointer and call indirectly,
    /// passing the environment as the leading argument.
    fn gen_closure_call(
        &mut self,
        callee: &Expr,
        ret: Ty,
        args: &[Expr],
    ) -> CgResult<Option<Value>> {
        let env = self.gen_expr(callee)?.ok_or_else(|| {
            CodegenError::new(callee.span, "closure value has no value")
        })?;
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            // Implicit widenings recorded by the checker are applied by gen_expr.
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        let ret_clty = self.cx_clty(ret);
        Ok(self.emit_closure_call(env, &arg_vals, ret_clty))
    }

    /// Call a closure `env` value with already-evaluated arguments: load its
    /// function pointer (offset 0) and call indirectly, passing the env first.
    fn emit_closure_call(&mut self, env: Value, args: &[Value], ret_clty: Option<ClType>) -> Option<Value> {
        self.mark_root(env);
        let fnptr = self.b.ins().load(PTR, MemFlags::trusted(), env, 0);
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR)); // env
        let mut arg_vals = vec![env];
        for &v in args {
            sig.params.push(AbiParam::new(self.b.func.dfg.value_type(v)));
            arg_vals.push(v);
        }
        if let Some(rc) = ret_clty {
            sig.returns.push(AbiParam::new(rc));
        }
        let sigref = self.b.import_signature(sig);
        let call = self.b.ins().call_indirect(sigref, fnptr, &arg_vals);
        self.b.inst_results(call).first().copied()
    }

    /// Dispatch `obj.method(args)` through `obj`'s vtable: load the function
    /// pointer at the method's slot and call it indirectly with the data pointer
    /// as `self`.
    fn gen_dyn_method_call(
        &mut self,
        iface_method: DefId,
        receiver: &Expr,
        args: &[Expr],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let iface = self.cx.analysis.program.def(iface_method).parent
            .ok_or_else(|| CodegenError::new(span, "interface method has no interface"))?;
        let prog = &self.cx.analysis.program;
        let slot = (0..prog.defs.len() as u32)
            .map(DefId)
            .filter(|&d| {
                let de = prog.def(d);
                de.kind == DefKind::InterfaceMethod && de.parent == Some(iface)
            })
            .position(|d| d == iface_method)
            .ok_or_else(|| CodegenError::new(span, "method not found in interface"))?;

        let obj = self.gen_expr(receiver)?.ok_or_else(|| {
            CodegenError::new(receiver.span, "interface receiver has no value")
        })?;
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self.gen_expr(a)?.ok_or_else(|| {
                CodegenError::new(a.span, "argument has no value")
            })?;
            arg_vals.push(v);
        }
        let ret_ty = resolve_shallow(
            self.cx.analysis,
            self.cx.analysis.results.expr_ty(span).unwrap_or(self.cx.analysis.tcx.error),
            &self.subst,
        );
        let ret_clty = clty_of(self.cx.analysis, ret_ty);
        self.emit_vtable_call(slot, obj, &arg_vals, ret_clty)
    }

    /// Index of an interface method within its interface (its vtable slot).
    fn vtable_slot(&self, iface_method: DefId) -> Option<usize> {
        let prog = &self.cx.analysis.program;
        let iface = prog.def(iface_method).parent?;
        (0..prog.defs.len() as u32)
            .map(DefId)
            .filter(|&d| {
                let de = prog.def(d);
                de.kind == DefKind::InterfaceMethod && de.parent == Some(iface)
            })
            .position(|d| d == iface_method)
    }

    /// Emit an indirect call through an interface object's vtable: `obj` is the
    /// `{vtable, data}` box, `slot` the method index, `args` the (already
    /// evaluated) non-self arguments.
    fn emit_vtable_call(
        &mut self,
        slot: usize,
        obj: Value,
        args: &[Value],
        ret_clty: Option<ClType>,
    ) -> CgResult<Option<Value>> {
        self.mark_root(obj);
        let vtable = self.b.ins().load(PTR, MemFlags::trusted(), obj, 0);
        let fnptr = self.b.ins().load(PTR, MemFlags::trusted(), vtable, (slot * 8) as i32);
        let data = self.b.ins().load(PTR, MemFlags::trusted(), obj, 8);

        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(PTR)); // self (data pointer)
        let mut arg_vals = vec![data];
        for &v in args {
            sig.params.push(AbiParam::new(self.b.func.dfg.value_type(v)));
            arg_vals.push(v);
        }
        if let Some(r) = ret_clty {
            sig.returns.push(AbiParam::new(r));
        }
        let sigref = self.b.import_signature(sig);
        let call = self.b.ins().call_indirect(sigref, fnptr, &arg_vals);
        Ok(self.b.inst_results(call).first().copied())
    }

    /// Resolve an interface method to the concrete `extend` method for the
    /// receiver's (monomorphized) type, plus the extend's type arguments.
    fn resolve_iface_method(&self, iface_method: DefId, recv: Ty) -> Option<(DefId, Vec<Ty>)> {
        let prog = &self.cx.analysis.program;
        let iface = prog.def(iface_method).parent?;
        let mname = prog.def(iface_method).name.clone();
        let recv = resolve_shallow(self.cx.analysis, recv, &self.subst);
        let TyKind::Named { def: cdef, args } = self.cx.analysis.tcx.kind(recv).clone() else {
            return None;
        };
        let ext = self.cx.analysis.results.iface_impls.get(&(cdef, iface)).copied()?;
        let method = (0..prog.defs.len() as u32).map(DefId).find(|&d| {
            let def = prog.def(d);
            def.kind == DefKind::ExtendMethod && def.parent == Some(ext) && def.name == mname
        })?;
        // A generic `extend Name<P0, …>` takes the receiver's type arguments in
        // order (the common form); a concrete `extend` takes none.
        let targs = if prog.def(ext).generics.is_empty() { Vec::new() } else { args };
        Some((method, targs))
    }

    /// Emit a direct call to a compiled instance, declaring it on demand.
    fn emit_call(
        &mut self,
        def: DefId,
        type_args: Vec<Ty>,
        arg_vals: &[Value],
        span: Span,
    ) -> CgResult<Option<Value>> {
        let func_id = match self.funcs.get(&(def, type_args.clone())).copied() {
            Some(f) => f,
            None => declare_instance(
                self.module,
                self.funcs,
                self.worklist,
                self.cx.analysis,
                def,
                type_args,
            )?
            .ok_or_else(|| CodegenError::new(span, "callee is not lowerable"))?,
        };
        let func_ref = self.module.declare_func_in_func(func_id, self.b.func);
        let inst = self.b.ins().call(func_ref, arg_vals);
        Ok(self.b.inst_results(inst).first().copied())
    }

    /// Lower a call to a builtin (`print`/`println`): one `str` argument.
    fn gen_builtin_call(&mut self, b: Builtin, args: &[Expr]) -> CgResult<Option<Value>> {
        match b {
            Builtin::Print | Builtin::Println => {
                let arg = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "builtin argument has no value")
                })?;
                let name = if matches!(b, Builtin::Print) { "lang_print" } else { "lang_println" };
                self.call_intrinsic(name, &[PTR], None, &[arg]);
                Ok(None)
            }
            // Diverging builtins (`never`): call the runtime, then terminate the
            // block with a trap so any code after the call is correctly dead.
            Builtin::Panic => {
                let msg = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "panic message has no value")
                })?;
                self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
                self.emit_unreachable();
                Ok(None)
            }
            // The attached value is evaluated (its side effects run, it is boxed
            // into `dynamic`) but the language never inspects it; the thread
            // terminates with a generic message.
            Builtin::PanicWith => {
                let _ = self.gen_expr(&args[0])?;
                let msg = self.const_str("explicit panic (panic_with)");
                self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
                self.emit_unreachable();
                Ok(None)
            }
            Builtin::Exit => {
                let code = self.gen_expr(&args[0])?.ok_or_else(|| {
                    CodegenError::new(args[0].span, "exit code has no value")
                })?;
                self.call_intrinsic("lang_exit", &[types::I32], None, &[code]);
                self.emit_unreachable();
                Ok(None)
            }
            Builtin::Abort => {
                self.call_intrinsic("lang_abort", &[], None, &[]);
                self.emit_unreachable();
                Ok(None)
            }
        }
    }

    /// Emit a cooperative GC safepoint poll (`docs/20`). Placed at loop headers
    /// so a compute-bound thread reaches a safepoint promptly when another
    /// thread requests a stop-the-world collection. Cheap on the common path
    /// (a flag load + branch inside the runtime).
    fn emit_safepoint(&mut self) {
        self.call_intrinsic("lang_gc_safepoint", &[], None, &[]);
    }

    /// Terminate the current block after a `never`-returning call: emit a trap
    /// (the runtime call does not return) and mark the block terminated.
    fn emit_unreachable(&mut self) {
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;
    }

    // -- runtime intrinsics, strings, casts ---------------------------------

    /// Declare (idempotently) and call a runtime intrinsic by symbol name.
    fn call_intrinsic(
        &mut self,
        name: &str,
        params: &[ClType],
        ret: Option<ClType>,
        args: &[Value],
    ) -> Option<Value> {
        let mut sig = self.module.make_signature();
        for p in params {
            sig.params.push(AbiParam::new(*p));
        }
        if let Some(r) = ret {
            sig.returns.push(AbiParam::new(r));
        }
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .expect("declare intrinsic");
        let fref = self.module.declare_func_in_func(id, self.b.func);
        let inst = self.b.ins().call(fref, args);
        self.b.inst_results(inst).first().copied()
    }

    /// Lower a string literal to a `str` pointer. Interpolation is not yet
    /// lowerable; text parts have their escapes processed.
    fn gen_str_literal(&mut self, s: &StringLit) -> CgResult<Value> {
        // Interpolation desugars to a chain of `+` (concat) over each part's
        // `to_str` (`docs/01` §8). Each part becomes one `str` value.
        let mut parts: Vec<Value> = Vec::new();
        for part in &s.parts {
            match part {
                StringPart::Text { text, .. } => {
                    let mut bytes = Vec::new();
                    unescape_into(text, &mut bytes);
                    parts.push(self.emit_str_bytes(bytes));
                }
                StringPart::Ident(id) => {
                    let ty = self.cx.analysis.results.expr_ty(id.span)
                        .unwrap_or(self.cx.analysis.tcx.error);
                    let raw = self.gen_local_use(id.span)?;
                    // Apply narrowing/widening recorded for this use.
                    let v = self.apply_adjustment(id.span, raw)?;
                    parts.push(self.stringify(v, ty, id.span)?);
                }
                StringPart::Expr(e) => {
                    let ty = self.cx.analysis.results.expr_ty(e.span)
                        .unwrap_or(self.cx.analysis.tcx.error);
                    let v = self.gen_expr(e)?;
                    parts.push(self.stringify(v, ty, e.span)?);
                }
            }
        }
        if parts.is_empty() {
            return Ok(self.const_str(""));
        }
        // Each part is a managed `str` held live across the remaining parts'
        // allocations and the concat chain; root them all so a collection
        // mid-build cannot free a part that has not yet been concatenated.
        for &p in &parts {
            self.mark_root(p);
        }
        let mut acc = parts[0];
        for &p in &parts[1..] {
            acc = self
                .call_intrinsic("lang_str_concat", &[PTR, PTR], Some(PTR), &[acc, p])
                .expect("concat returns a value");
            self.mark_root(acc);
        }
        Ok(acc)
    }

    /// Convert an interpolated value to a `str`.
    fn stringify(&mut self, v: Option<Value>, ty: Ty, span: Span) -> CgResult<Value> {
        // A user type with a `to_str(self): str` method (e.g. `@Derive(ToStr)`):
        // call it with the value as the receiver.
        if let Some(&mdef) = self.cx.analysis.results.stringify_methods.get(&span) {
            let recv = v.ok_or_else(|| CodegenError::new(span, "interpolated value has no payload"))?;
            let targs = self.instance_args(span);
            return self
                .emit_call(mdef, targs, &[recv], span)?
                .ok_or_else(|| CodegenError::new(span, "`to_str` returned no value"));
        }
        match self.cx.analysis.tcx.kind(ty) {
            TyKind::Str => v.ok_or_else(|| CodegenError::new(span, "str has no value")),
            TyKind::Null => Ok(self.const_str("null")),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char => {
                let v = v.ok_or_else(|| CodegenError::new(span, "value has no payload"))?;
                self.cast_to_str(v, ty, span)
            }
            _ => Err(CodegenError::new(span, "type is not stringifiable")),
        }
    }

    /// Build a `str` value from raw UTF-8 bytes via a read-only data object.
    fn emit_str_bytes(&mut self, bytes: Vec<u8>) -> Value {
        let len = bytes.len();
        let name = format!("str.{}", DATA_CTR.fetch_add(1, Ordering::Relaxed));
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .expect("declare data");
        let mut desc = DataDescription::new();
        desc.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &desc).expect("define data");
        let gv = self.module.declare_data_in_func(data_id, self.b.func);
        let addr = self.b.ins().global_value(PTR, gv);
        let len_val = self.b.ins().iconst(PTR, len as i64);
        self.call_intrinsic("lang_str_from_utf8", &[PTR, PTR], Some(PTR), &[addr, len_val])
            .expect("from_utf8 returns a value")
    }

    /// A `str` value for a compile-time-known message (e.g. a panic reason).
    fn const_str(&mut self, text: &str) -> Value {
        self.emit_str_bytes(text.as_bytes().to_vec())
    }

    fn gen_cast(&mut self, inner: &Expr, from: Ty, to: Ty) -> CgResult<Option<Value>> {
        // Narrowing a union/`dynamic`: the operand is a box; check its type id.
        if matches!(self.cx.analysis.tcx.kind(from), TyKind::Union(_) | TyKind::Dynamic) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "union operand has no value")
            })?;
            return self.gen_union_narrow(ptr, to);
        }
        // Downcast an interface object to a concrete type: verify the stored
        // type id, then return the data pointer (panic on mismatch).
        if self.is_interface_ty(from) && !self.is_interface_ty(to) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "interface operand has no value")
            })?;
            return self.gen_dyn_downcast(ptr, to);
        }
        // Upcast a concrete value to an interface object (build its vtable box).
        if !self.is_interface_ty(from) && self.is_interface_ty(to) {
            let v = self.gen_expr(inner)?;
            return Ok(Some(self.gen_widen_dyn(v, from, to, inner.span)?));
        }
        let v = self.gen_expr(inner)?.ok_or_else(|| {
            CodegenError::new(inner.span, "cast operand has no value")
        })?;
        let tcx = &self.cx.analysis.tcx;
        // Casts to `str` go through the runtime stringifiers.
        if matches!(tcx.kind(to), TyKind::Str) {
            return Ok(Some(self.cast_to_str(v, from, inner.span)?));
        }
        let from_k = tcx.kind(from).clone();
        let to_k = tcx.kind(to).clone();
        let out = match (&from_k, &to_k) {
            (TyKind::Int(a), TyKind::Int(b)) => self.convert_int(v, *a, int_clty(*b), a.is_signed()),
            // char is a 32-bit unsigned scalar; the integer must be a valid
            // Unicode scalar value or the cast panics (`docs/14` §2).
            (TyKind::Int(a), TyKind::Char) => {
                let cp = self.resize_int(v, a.is_signed(), int_clty(*a), types::I32);
                self.guard_valid_char(cp);
                cp
            }
            (TyKind::Char, TyKind::Int(b)) => self.resize_int(v, false, types::I32, int_clty(*b)),
            (TyKind::Int(a), TyKind::Float(f)) => {
                let ft = float_clty(*f);
                if a.is_signed() { self.b.ins().fcvt_from_sint(ft, v) }
                else { self.b.ins().fcvt_from_uint(ft, v) }
            }
            // float → int panics on NaN or out-of-range (`docs/14` §2/§6).
            (TyKind::Float(f), TyKind::Int(b)) => self.gen_float_to_int(v, *f, *b),
            (TyKind::Float(a), TyKind::Float(b)) => match (a, b) {
                (FloatTy::F32, FloatTy::F64) => self.b.ins().fpromote(types::F64, v),
                (FloatTy::F64, FloatTy::F32) => self.b.ins().fdemote(types::F32, v),
                _ => v,
            },
            _ if from == to => v,
            // Union narrowing of a represented value is identity for the
            // primitive subset (no tagged unions compiled yet).
            _ => v,
        };
        Ok(Some(out))
    }

    fn cast_to_str(&mut self, v: Value, from: Ty, span: Span) -> CgResult<Value> {
        let from_k = self.cx.analysis.tcx.kind(from).clone();
        let result = match from_k {
            TyKind::Int(it) => {
                let widened = self.resize_int(v, it.is_signed(), int_clty(it), types::I64);
                let func = if it.is_signed() { "lang_int_to_str" } else { "lang_uint_to_str" };
                self.call_intrinsic(func, &[types::I64], Some(PTR), &[widened])
            }
            TyKind::Float(f) => {
                let promoted = if matches!(f, FloatTy::F32) {
                    self.b.ins().fpromote(types::F64, v)
                } else {
                    v
                };
                self.call_intrinsic("lang_float_to_str", &[types::F64], Some(PTR), &[promoted])
            }
            TyKind::Bool => self.call_intrinsic("lang_bool_to_str", &[types::I8], Some(PTR), &[v]),
            TyKind::Char => self.call_intrinsic("lang_char_to_str", &[types::I32], Some(PTR), &[v]),
            // `str as str` is the identity (e.g. a derived `to_str` casting a
            // `str` field).
            TyKind::Str => return Ok(v),
            TyKind::Null => return Ok(self.const_str("null")),
            _ => return Err(CodegenError::new(span, "cannot stringify this type")),
        };
        Ok(result.expect("stringifier returns a value"))
    }

    /// Resize an integer value between two Cranelift int types per signedness.
    fn resize_int(&mut self, v: Value, signed: bool, fromc: ClType, toc: ClType) -> Value {
        use std::cmp::Ordering::*;
        match toc.bits().cmp(&fromc.bits()) {
            Greater => {
                if signed { self.b.ins().sextend(toc, v) } else { self.b.ins().uextend(toc, v) }
            }
            Less => self.b.ins().ireduce(toc, v),
            Equal => v,
        }
    }

    fn convert_int(&mut self, v: Value, from: IntTy, toc: ClType, signed: bool) -> Value {
        self.resize_int(v, signed, int_clty(from), toc)
    }

    // -- unions --------------------------------------------------------------

    /// Compute `id ∈ {type_id(v) : v ∈ variants(to)}` as an i8 boolean, where
    /// `id` is a union box's stored type id.
    fn tag_in_target(&mut self, id: Value, to: Ty) -> Value {
        let mut acc: Option<Value> = None;
        for vt in self.cx.analysis.tcx.variants(to) {
            let tid = self.type_id_of(vt);
            let c = {
                let k = self.b.ins().iconst(types::I64, tid);
                self.b.ins().icmp(IntCC::Equal, id, k)
            };
            acc = Some(match acc {
                None => c,
                Some(a) => self.b.ins().bor(a, c),
            });
        }
        acc.unwrap_or_else(|| self.b.ins().iconst(types::I8, 0))
    }

    /// `v as T` where `v` is a union/`dynamic` box `ptr`. Panics if the stored
    /// type id is not in `to`'s variant set; otherwise unboxes (single variant)
    /// or returns the box (narrowing to a sub-union).
    fn gen_union_narrow(&mut self, ptr: Value, to: Ty) -> CgResult<Option<Value>> {
        let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 0);
        let ok = self.tag_in_target(id, to);

        let cont = self.b.create_block();
        let panic_bb = self.b.create_block();
        self.b.ins().brif(ok, cont, &[], panic_bb, &[]);
        self.term = true;

        self.switch(panic_bb);
        let msg = self.const_str("cast failed: value is not the requested type");
        self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;

        self.switch(cont);
        // Narrowing to a sub-union keeps the box; to a single variant unboxes.
        if matches!(self.cx.analysis.tcx.kind(to), TyKind::Union(_) | TyKind::Dynamic) {
            return Ok(Some(ptr));
        }
        match clty_of(self.cx.analysis, to) {
            Some(ct) => Ok(Some(self.b.ins().load(ct, MemFlags::trusted(), ptr, 8))),
            None => Ok(None), // narrowed to `null`
        }
    }

    /// Whether `ty` (resolved) is an interface object type.
    fn is_interface_ty(&self, ty: Ty) -> bool {
        matches!(
            self.cx.analysis.tcx.kind(resolve_shallow(self.cx.analysis, ty, &self.subst)),
            TyKind::Named { def, .. } if self.cx.analysis.program.def(*def).kind == DefKind::Interface
        )
    }

    /// Downcast an interface object `ptr` to concrete type `to`: check the
    /// stored type id, panic on mismatch, and return the data pointer.
    fn gen_dyn_downcast(&mut self, ptr: Value, to: Ty) -> CgResult<Option<Value>> {
        let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 16);
        let want = self.type_id_of(to);
        let want_v = self.b.ins().iconst(types::I64, want);
        let ok = self.b.ins().icmp(IntCC::Equal, id, want_v);

        let cont = self.b.create_block();
        let panic_bb = self.b.create_block();
        self.b.ins().brif(ok, cont, &[], panic_bb, &[]);
        self.term = true;

        self.switch(panic_bb);
        let msg = self.const_str("cast failed: interface object is not the requested type");
        self.call_intrinsic("lang_panic", &[PTR], None, &[msg]);
        let tc = cranelift_codegen::ir::TrapCode::user(1).unwrap();
        self.b.ins().trap(tc);
        self.term = true;

        self.switch(cont);
        // The data pointer (offset 8) is the concrete value.
        Ok(Some(self.b.ins().load(PTR, MemFlags::trusted(), ptr, 8)))
    }

    /// `v is T` — a runtime tag check on a union/`dynamic`, an interface object's
    /// stored type id, or a static answer for a concrete operand.
    fn gen_is(&mut self, inner: &Expr, from: Ty, to: Ty) -> CgResult<Option<Value>> {
        if matches!(self.cx.analysis.tcx.kind(from), TyKind::Union(_) | TyKind::Dynamic) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "`is` operand has no value")
            })?;
            let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 0);
            return Ok(Some(self.tag_in_target(id, to)));
        }
        // Interface object: compare the concrete type id stored at offset 16.
        if self.is_interface_ty(from) {
            let ptr = self.gen_expr(inner)?.ok_or_else(|| {
                CodegenError::new(inner.span, "`is` operand has no value")
            })?;
            let id = self.b.ins().load(types::I64, MemFlags::trusted(), ptr, 16);
            let want = self.type_id_of(to);
            let want_v = self.b.ins().iconst(types::I64, want);
            return Ok(Some(self.b.ins().icmp(IntCC::Equal, id, want_v)));
        }
        // Concrete operand: the answer is known at compile time.
        self.gen_expr(inner)?; // evaluate for any side effects
        let answer = self.cx.analysis.tcx.variants(to).contains(&from);
        Ok(Some(self.b.ins().iconst(types::I8, i64::from(answer))))
    }

    // -- name resolution helpers --------------------------------------------

    fn resolve_local(&self, span: Span) -> CgResult<LocalId> {
        match self.cx.analysis.results.resolution(span) {
            Some(ValueRes::Local(id)) => Ok(id),
            _ => Err(CodegenError::new(span, "expected a local binding")),
        }
    }

}

/// Map a language type to a Cranelift value type, or `None` for zero-sized
/// (`null`/`never`) or not-yet-lowerable aggregate types.
fn clty_of(analysis: &Analysis, ty: Ty) -> Option<ClType> {
    match analysis.tcx.kind(ty) {
        TyKind::Int(it) => Some(int_clty(*it)),
        TyKind::Float(FloatTy::F32) => Some(types::F32),
        TyKind::Float(FloatTy::F64) => Some(types::F64),
        TyKind::Bool => Some(types::I8),
        TyKind::Char => Some(types::I32),
        // `str` is a managed reference — a pointer (to a runtime `LangStr`).
        TyKind::Str => Some(PTR),
        // Structs are managed references (a pointer to the field block); an
        // interface object is a pointer to a `{vtable, data}` fat-pointer box.
        TyKind::Named { def, .. }
            if matches!(
                analysis.program.def(*def).kind,
                DefKind::Struct | DefKind::ExternStruct | DefKind::Interface
            ) =>
        {
            Some(PTR)
        }
        // Anonymous tuples are heap-boxed records — a pointer.
        TyKind::Tuple(_) => Some(PTR),
        // A closure value is a pointer to its heap environment.
        TyKind::Func { .. } => Some(PTR),
        // A union/dynamic value is a pointer to a `{type_id, data}` box.
        TyKind::Union(_) | TyKind::Dynamic => Some(PTR),
        // A raw FFI pointer `*T` is a machine pointer (`docs/19`).
        TyKind::Ptr(_) => Some(PTR),
        TyKind::Null | TyKind::Never => None,
        _ => None,
    }
}

/// A stable runtime type id for a (non-union) type, stored in a union/dynamic
/// box so `is`/`as` can identify the inhabited variant. Conceptually the
/// "type pointer" of `docs/16` §3, collapsed to an integer for now.
fn type_id(analysis: &Analysis, ty: Ty) -> i64 {
    match analysis.tcx.kind(ty) {
        TyKind::Int(it) => match it {
            IntTy::I8 => 1,
            IntTy::I16 => 2,
            IntTy::I32 => 3,
            IntTy::I64 => 4,
            IntTy::U8 => 5,
            IntTy::U16 => 6,
            IntTy::U32 => 7,
            IntTy::U64 => 8,
            IntTy::Isize => 9,
            IntTy::Usize => 10,
        },
        TyKind::Float(FloatTy::F32) => 11,
        TyKind::Float(FloatTy::F64) => 12,
        TyKind::Bool => 13,
        TyKind::Char => 14,
        TyKind::Str => 15,
        TyKind::Null => 16,
        // Nominal types get ids past the primitive range, keyed by def.
        TyKind::Named { def, .. } => 1000 + def.index() as i64,
        // Tuples/functions in unions are not yet supported; -1 never matches.
        _ => -1,
    }
}

/// The `(MIN, MAX)` bit patterns (as `i64` for `iconst`) of an integer type.
fn int_min_max(it: IntTy) -> (i64, i64) {
    let bits = it.bits().unwrap_or(64);
    if it.is_signed() {
        if bits >= 64 {
            (i64::MIN, i64::MAX)
        } else {
            let m = 1i64 << (bits - 1);
            (-m, m - 1)
        }
    } else if bits >= 64 {
        (0, -1) // u64::MAX is all-ones (read back as i64 = -1)
    } else {
        (0, (1i64 << bits) - 1)
    }
}

fn int_clty(it: IntTy) -> ClType {
    match it {
        IntTy::I8 | IntTy::U8 => types::I8,
        IntTy::I16 | IntTy::U16 => types::I16,
        IntTy::I32 | IntTy::U32 => types::I32,
        IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => types::I64,
    }
}

fn float_clty(ft: FloatTy) -> ClType {
    match ft {
        FloatTy::F32 => types::F32,
        FloatTy::F64 => types::F64,
    }
}

/// The in-memory layout of a struct's field block: per-field byte offset and
/// Cranelift type (`None` for zero-sized `null` fields), plus the total size.
struct Layout {
    names: Vec<String>,
    offsets: Vec<u32>,
    cltys: Vec<Option<ClType>>,
    size: u32,
    /// Byte offsets of fields that hold managed pointers (the GC trace map).
    ptr_offsets: Vec<u32>,
}

impl Layout {
    fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }
}

fn align_up(x: u32, align: u32) -> u32 {
    x.div_ceil(align) * align
}

/// Is a value of `ty` a managed-heap pointer (so the collector must trace it)?
/// Primitives are not; `str`, tuples, unions/`dynamic`, and managed structs
/// (including `List`) are. Foreign (`extern`) structs are not managed.
fn is_managed_ptr(analysis: &Analysis, ty: Ty) -> bool {
    match analysis.tcx.kind(ty) {
        TyKind::Str | TyKind::Tuple(_) | TyKind::Union(_) | TyKind::Dynamic => true,
        // A closure value is a pointer to a managed environment.
        TyKind::Func { is_extern: false, .. } => true,
        TyKind::Named { def, .. } => {
            matches!(analysis.program.def(*def).kind, DefKind::Struct | DefKind::Interface)
        }
        _ => false,
    }
}

/// Compute a field-block layout from named, lowered field types. Field offsets
/// respect each field's natural alignment; the total size is rounded up to the
/// aggregate's alignment (`docs/02` §9). Records which fields are managed
/// pointers for the GC trace map.
fn layout_of_fields(analysis: &Analysis, fields: &[(String, Ty)]) -> Layout {
    let mut offset = 0u32;
    let mut offsets = Vec::new();
    let mut cltys = Vec::new();
    let mut names = Vec::new();
    let mut ptr_offsets = Vec::new();
    let mut max_align = 1u32;
    for (name, ty) in fields {
        let ct = clty_of(analysis, *ty);
        let (size, align) = match ct {
            Some(c) => (c.bytes(), c.bytes().max(1)),
            None => (0, 1),
        };
        offset = align_up(offset, align);
        offsets.push(offset);
        cltys.push(ct);
        names.push(name.clone());
        if is_managed_ptr(analysis, *ty) {
            ptr_offsets.push(offset);
        }
        offset += size;
        max_align = max_align.max(align);
    }
    Layout { names, offsets, cltys, size: align_up(offset, max_align).max(1), ptr_offsets }
}

/// The field-block layout of a (non-generic) struct, by its recorded fields.
fn compute_layout(analysis: &Analysis, def: DefId, args: &[Ty]) -> Layout {
    let fields: Vec<(String, Ty)> = match analysis.results.struct_fields.get(&def) {
        Some(StructFields::Record(fs)) => fs.clone(),
        Some(StructFields::Tuple(ts)) => {
            ts.iter().enumerate().map(|(i, t)| (i.to_string(), *t)).collect()
        }
        _ => Vec::new(),
    };
    // For a generic struct, the field types reference the struct's own
    // parameters; substitute the instantiation's arguments.
    let ssubst: HashMap<DefId, Ty> =
        analysis.program.def(def).generics.iter().copied().zip(args.iter().copied()).collect();
    let resolved: Vec<(String, Ty)> = fields
        .into_iter()
        .map(|(n, t)| (n, resolve_shallow(analysis, t, &ssubst)))
        .collect();
    layout_of_fields(analysis, &resolved)
}

/// The layout of an anonymous tuple, positions named "0", "1", ….
fn tuple_layout(analysis: &Analysis, elems: &[Ty]) -> Layout {
    let fields: Vec<(String, Ty)> =
        elems.iter().enumerate().map(|(i, t)| (i.to_string(), *t)).collect();
    layout_of_fields(analysis, &fields)
}

/// Process the supported backslash escapes of a string-literal text run.
fn unescape_into(text: &str, out: &mut Vec<u8>) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('\'') => out.push(b'\''),
            Some('"') => out.push(b'"'),
            Some('$') => out.push(b'$'),
            Some('0') => out.push(0),
            Some('u') => {
                // \u{H..} — consume the brace-delimited hex.
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut hex = String::new();
                    while let Some(&h) = chars.peek() {
                        if h == '}' { chars.next(); break; }
                        hex.push(h);
                        chars.next();
                    }
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            Some(other) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => {}
        }
    }
}

/// Decode a `char` literal (with surrounding quotes) to its scalar value.
fn parse_char(raw: &str) -> Option<u32> {
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first != '\\' {
        return if chars.next().is_none() { Some(first as u32) } else { None };
    }
    let esc = chars.next()?;
    let val = match esc {
        'n' => '\n' as u32,
        'r' => '\r' as u32,
        't' => '\t' as u32,
        '\\' => '\\' as u32,
        '\'' => '\'' as u32,
        '"' => '"' as u32,
        '0' => 0,
        'u' => {
            // \u{...}
            let rest: String = chars.collect();
            let hex = rest.strip_prefix('{')?.strip_suffix('}')?;
            return u32::from_str_radix(hex, 16).ok();
        }
        _ => return None,
    };
    if chars.next().is_none() { Some(val) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compiler::lexer::lex;
    use compiler::parser::parse;
    use compiler::sema::analyze;
    use compiler::span::FileId;

    /// Analyze, JIT-compile, and call a zero-arg `i64` function by name.
    fn run(src: &str, func: &str) -> i64 {
        let (tokens, le) = lex(src, FileId(0));
        assert!(le.is_empty(), "lex: {le:?}");
        let (module, pe) = parse(src, &tokens);
        assert!(pe.is_empty(), "parse: {pe:?}");
        let analysis = analyze(&module);
        assert!(analysis.errors.is_empty(), "sema: {:?}", analysis.errors);
        let jit = compile(&analysis).expect("codegen");
        unsafe { jit.call_i64(func).expect("function present") }
    }

    /// Call a zero-arg function returning `str` and read back its UTF-8 bytes.
    fn run_str(src: &str, func: &str) -> String {
        let (tokens, le) = lex(src, FileId(0));
        assert!(le.is_empty(), "lex: {le:?}");
        let (module, pe) = parse(src, &tokens);
        assert!(pe.is_empty(), "parse: {pe:?}");
        let analysis = analyze(&module);
        assert!(analysis.errors.is_empty(), "sema: {:?}", analysis.errors);
        let jit = compile(&analysis).expect("codegen");
        let bits = unsafe { jit.call_i64(func).expect("function present") };
        let p = bits as usize as *const runtime::LangStr;
        unsafe { String::from_utf8_lossy(runtime::str_bytes(p)).into_owned() }
    }

    #[test]
    fn returns_constant() {
        assert_eq!(run("function answer(): i64 { 42 }", "answer"), 42);
    }

    #[test]
    fn arithmetic() {
        assert_eq!(run("function f(): i64 { 40 + 2 }", "f"), 42);
        assert_eq!(run("function f(): i64 { (6 - 2) * 10 + 2 }", "f"), 42);
        assert_eq!(run("function f(): i64 { 84 / 2 }", "f"), 42);
        assert_eq!(run("function f(): i64 { 85 % 43 }", "f"), 42);
    }

    #[test]
    fn locals_and_assignment() {
        assert_eq!(
            run("function f(): i64 { var x: i64 = 40; x = x + 2; x }", "f"),
            42
        );
    }

    #[test]
    fn shadowing_distinct_locals() {
        assert_eq!(
            run("function f(): i64 { var x: i64 = 1; var y: i64 = { var x: i64 = 40; x + 1 }; x + y + 0 }", "f"),
            42
        );
    }

    #[test]
    fn if_else_value() {
        assert_eq!(
            run("function f(): i64 { if 1 < 2 { 42 } else { 0 } }", "f"),
            42
        );
        assert_eq!(
            run("function f(): i64 { if 2 < 1 { 0 } else { 42 } }", "f"),
            42
        );
    }

    #[test]
    fn early_return() {
        assert_eq!(
            run("function f(): i64 { if 1 < 2 { return 42 } 7 }", "f"),
            42
        );
    }

    #[test]
    fn logical_short_circuit() {
        // (true && true) -> branch picks 42
        assert_eq!(
            run("function f(): i64 { if (1 < 2) && (3 < 4) { 42 } else { 0 } }", "f"),
            42
        );
        assert_eq!(
            run("function f(): i64 { if (1 < 2) || (4 < 3) { 42 } else { 0 } }", "f"),
            42
        );
    }

    #[test]
    fn calls_other_function() {
        assert_eq!(
            run("function add(a: i64, b: i64): i64 { a + b }\nfunction f(): i64 { add(40, 2) }", "f"),
            42
        );
    }

    #[test]
    fn recursion_factorial() {
        let src = "function fac(n: i64): i64 { if n <= 1 { 1 } else { n * fac(n - 1) } }\n\
                   function f(): i64 { fac(5) }";
        assert_eq!(run(src, "f"), 120);
    }

    #[test]
    fn narrower_int_width() {
        // i32 arithmetic returns correctly through the i32 ABI then widened.
        assert_eq!(run("function f(): i32 { 40i32 + 2i32 }", "f") as i32, 42);
    }

    #[test]
    fn bitwise_and_shifts() {
        assert_eq!(run("function f(): i64 { 1 << 5 }", "f"), 32);
        assert_eq!(run("function f(): i64 { 0xFF & 0x0F }", "f"), 15);
        assert_eq!(run("function f(): i64 { 5 ^ 6 }", "f"), 3);
    }

    // --- strings -----------------------------------------------------------

    #[test]
    fn string_literal_roundtrip() {
        assert_eq!(run_str("function f(): str { \"hello\" }", "f"), "hello");
        assert_eq!(run_str("function f(): str { \"\" }", "f"), "");
    }

    #[test]
    fn string_escapes() {
        assert_eq!(run_str("function f(): str { \"a\\nb\\tc\" }", "f"), "a\nb\tc");
        assert_eq!(run_str("function f(): str { \"quote: \\\"x\\\"\" }", "f"), "quote: \"x\"");
    }

    #[test]
    fn string_concat() {
        assert_eq!(run_str("function f(): str { \"foo\" + \"bar\" }", "f"), "foobar");
        assert_eq!(
            run_str("function f(): str { \"a\" + \"b\" + \"c\" + \"d\" }", "f"),
            "abcd"
        );
    }

    #[test]
    fn int_to_str() {
        assert_eq!(run_str("function f(): str { 42 as str }", "f"), "42");
        assert_eq!(run_str("function f(): str { (0 - 7) as str }", "f"), "-7");
        assert_eq!(run_str("function f(): str { 255u8 as str }", "f"), "255");
    }

    #[test]
    fn float_bool_char_to_str() {
        assert_eq!(run_str("function f(): str { 3.5 as str }", "f"), "3.5");
        assert_eq!(run_str("function f(): str { true as str }", "f"), "true");
        assert_eq!(run_str("function f(): str { false as str }", "f"), "false");
        assert_eq!(run_str("function f(): str { 'A' as str }", "f"), "A");
    }

    #[test]
    fn concat_with_stringified_number() {
        assert_eq!(
            run_str("function f(): str { \"n = \" + (42 as str) }", "f"),
            "n = 42"
        );
    }

    // --- numeric conversions (cast) ---------------------------------------

    #[test]
    fn int_widen_and_narrow() {
        // i32 -> i64 widening preserves value.
        assert_eq!(run("function f(): i64 { var x: i32 = 300; x as i64 }", "f"), 300);
        // i64 -> i8 truncation, then back to i64 sign-extended.
        assert_eq!(
            run("function f(): i64 { var x: i64 = 300; (x as i8) as i64 }", "f"),
            300i64 as i8 as i64
        );
    }

    #[test]
    fn float_to_int_truncates() {
        assert_eq!(run("function f(): i64 { 3.9 as i64 }", "f"), 3);
        assert_eq!(run("function f(): i64 { (0.0 - 3.9) as i64 }", "f"), -3);
    }

    #[test]
    fn int_to_float_roundtrip() {
        // 7 -> f64 -> i64 == 7
        assert_eq!(run("function f(): i64 { (7 as f64) as i64 }", "f"), 7);
    }

    #[test]
    fn char_int_roundtrip() {
        assert_eq!(run("function f(): i64 { 'A' as i64 }", "f"), 65);
        // 66 -> char -> i64 == 66
        assert_eq!(run("function f(): i64 { (66 as char) as i64 }", "f"), 66);
    }

    // --- loops -------------------------------------------------------------

    #[test]
    fn while_sum() {
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     var total: i64 = 0;\n\
                     while i < 10 { total = total + i; i = i + 1; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 45);
    }

    #[test]
    fn while_with_break_and_continue() {
        // Sum even numbers below 10, stop at 100 (never reached): 0+2+4+6+8 = 20
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     var total: i64 = 0;\n\
                     while i < 10 {\n\
                       i = i + 1;\n\
                       if (i - 1) % 2 == 1 { continue }\n\
                       total = total + (i - 1);\n\
                     }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 20);
    }

    #[test]
    fn loop_breaks_with_value() {
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     loop {\n\
                       if i >= 42 { break i }\n\
                       i = i + 1;\n\
                     }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn loop_with_plain_break() {
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     var n: i64 = 0;\n\
                     loop {\n\
                       n = n + i;\n\
                       i = i + 1;\n\
                       if i > 5 { break }\n\
                     }\n\
                     n\n\
                   }";
        // i runs 0..=5 accumulating before the post-increment check: 0+1+2+3+4+5 = 15
        assert_eq!(run(src, "f"), 15);
    }

    // --- structs -----------------------------------------------------------

    #[test]
    fn record_struct_construct_and_access() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var p = P { x: 40, y: 2 }; p.x + p.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn record_field_mutation() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var p = P { x: 1, y: 2 }; p.x = 40; p.x + p.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn field_init_shorthand() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var x: i64 = 40; var y: i64 = 2; var p = P { x, y }; p.x + p.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_spread_update() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var p = P { x: 1, y: 2 }; var q = P { ..p, y: 41 }; q.x + q.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn nested_structs() {
        let src = "struct Inner { v: i64 }\n\
                   struct Outer { inner: Inner, k: i64 }\n\
                   function f(): i64 {\n\
                     var o = Outer { inner: Inner { v: 40 }, k: 2 };\n\
                     o.inner.v + o.k\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_struct_construct_and_index() {
        let src = "struct Pair(i64, i64)\n\
                   function f(): i64 { var p = Pair(40, 2); p.0 + p.1 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_with_string_field() {
        let src = "struct Person { name: str, age: i64 }\n\
                   function f(): str { var p = Person { name: \"Alice\", age: 30 }; p.name }";
        assert_eq!(run_str(src, "f"), "Alice");
    }

    #[test]
    fn mixed_width_field_layout() {
        // a: u8 (off 0), b: i32 (off 4 after padding), c: i64 (off 8) — verifies
        // alignment-respecting offsets by reading each field back.
        let src = "struct M { a: u8, b: i32, c: i64 }\n\
                   function f(): i64 {\n\
                     var m = M { a: 7u8, b: 1000i32, c: 9000000000 };\n\
                     (m.a as i64) + (m.b as i64) + m.c\n\
                   }";
        assert_eq!(run(src, "f"), 7 + 1000 + 9_000_000_000);
    }

    #[test]
    fn struct_passed_to_function() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function sum(p: P): i64 { p.x + p.y }\n\
                   function f(): i64 { sum(P { x: 40, y: 2 }) }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- operator overloading ----------------------------------------------

    #[test]
    fn overloaded_add_on_struct() {
        let src = "struct V { x: i64, y: i64 }\n\
                   extend V: Add { function add(self, rhs: V): V { V { x: self.x + rhs.x, y: self.y + rhs.y } } }\n\
                   function f(): i64 { var c = V { x: 40, y: 1 } + V { x: 2, y: 99 }; c.x }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn overloaded_eq_on_struct() {
        let src = "struct P { v: i64 }\n\
                   extend P: Eq { function eq(self, o: P): bool { self.v == o.v } }\n\
                   function f(): i64 {\n\
                     var a = P { v: 5 };\n\
                     if a == (P { v: 5 }) { 42 } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn overloaded_ne_negates_eq() {
        let src = "struct P { v: i64 }\n\
                   extend P: Eq { function eq(self, o: P): bool { self.v == o.v } }\n\
                   function f(): i64 { var a = P { v: 5 }; if a != (P { v: 9 }) { 42 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn overloaded_lt_on_struct() {
        let src = "struct M { n: i64 }\n\
                   extend M: Ord { function lt(self, o: M): bool { self.n < o.n } }\n\
                   function f(): i64 { if (M { n: 1 }) < (M { n: 2 }) { 42 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- List<T> -----------------------------------------------------------

    #[test]
    fn list_literal_and_index() {
        let src = "function f(): i64 { var xs: List<i64> = [10, 20, 12]; xs[0] + xs[2] }";
        assert_eq!(run(src, "f"), 22);
    }

    #[test]
    fn list_push_and_size() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2];\n\
                     xs.push(3);\n\
                     xs.push(4);\n\
                     xs.size()\n\
                   }";
        assert_eq!(run(src, "f"), 4);
    }

    #[test]
    fn list_sum_with_while() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3, 4, 5, 6, 7, 8, 9];\n\
                     var i: i64 = 0;\n\
                     var total: i64 = 0;\n\
                     while i < xs.size() { total = total + xs[i]; i = i + 1; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 45);
    }

    #[test]
    fn list_indexed_assignment() {
        let src = "function f(): i64 { var xs = [1, 2, 3]; xs[1] = 40; xs[0] + xs[1] + xs[2] }";
        assert_eq!(run(src, "f"), 44);
    }

    #[test]
    fn list_of_strings() {
        let src = "function f(): str { var xs: List<str> = [\"a\", \"b\", \"c\"]; xs[1] }";
        assert_eq!(run_str(src, "f"), "b");
    }

    #[test]
    fn list_of_structs() {
        let src = "struct P { v: i64 }\n\
                   function f(): i64 { var xs = [P { v: 40 }, P { v: 2 }]; xs[0].v + xs[1].v }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_get_in_range() {
        let src = "function f(): i64 {\n\
                     var xs = [10, 42, 30];\n\
                     match xs.get(1) { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_get_out_of_range_is_null() {
        let src = "function f(): i64 {\n\
                     var xs = [10, 20];\n\
                     match xs.get(5) { i64 n => n, null => 42 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_get_str_element() {
        let src = "function f(): str {\n\
                     var xs: List<str> = [\"a\", \"hit\", \"c\"];\n\
                     match xs.get(1) { str s => s, null => \"miss\" }\n\
                   }";
        assert_eq!(run_str(src, "f"), "hit");
    }

    #[test]
    fn list_is_empty() {
        let src = "function f(): i64 {\n\
                     var xs: List<i64> = [];\n\
                     if xs.is_empty() { 42 } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    // -- Map<K, V> -----------------------------------------------------------

    #[test]
    fn map_literal_get_str_keys() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"x\": 1, \"y\": 42 };\n\
                     match m.get(\"y\") { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_get_missing_is_null() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"x\": 1 };\n\
                     match m.get(\"nope\") { i64 n => n, null => 42 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_set_overwrites_and_size() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"a\": 1 };\n\
                     m.set(\"a\", 10);\n\
                     m.set(\"b\", 32);\n\
                     match m.get(\"a\") { i64 n => n + m.size(), null => 0 }\n\
                   }";
        // a=10 (overwritten, not added) + size 2 (a,b) = ... wait: 10 + 2 = 12
        assert_eq!(run(src, "f"), 12);
    }

    #[test]
    fn map_int_keys() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 100, 2: 200 };\n\
                     m.set(3, 300);\n\
                     match m.get(3) { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 300);
    }

    #[test]
    fn map_contains_and_remove() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 10, 2: 20 };\n\
                     var before = if m.contains(1) { 1 } else { 0 };\n\
                     m.remove(1);\n\
                     var after = if m.contains(1) { 1 } else { 0 };\n\
                     before * 100 + after * 10 + m.size()\n\
                   }";
        // before=1 -> 100, after=0 -> 0, size=1 -> 1 => 101
        assert_eq!(run(src, "f"), 101);
    }

    #[test]
    fn map_remove_returns_value() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 5: 42 };\n\
                     match m.remove(5) { i64 n => n, null => -1 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_remove_missing_is_null() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 5: 42 };\n\
                     match m.remove(9) { i64 n => n, null => 7 }\n\
                   }";
        assert_eq!(run(src, "f"), 7);
    }

    #[test]
    fn map_empty_constructor_is_empty() {
        let src = "function f(): i64 {\n\
                     var m = Map<str, i64>();\n\
                     if m.is_empty() { 42 } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_new_constructor() {
        let src = "function f(): i64 {\n\
                     var m = Map.new<i64, i64>();\n\
                     m.set(1, 42);\n\
                     match m.get(1) { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_str_values() {
        let src = "function f(): str {\n\
                     var m: Map<i64, str> = { 1: \"one\", 2: \"two\" };\n\
                     match m.get(2) { str s => s, null => \"?\" }\n\
                   }";
        assert_eq!(run_str(src, "f"), "two");
    }

    #[test]
    fn map_keys_sum() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 10: 1, 20: 2, 12: 3 };\n\
                     var total = 0;\n\
                     for k in m.keys() { total = total + k; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_values_sum() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 10, 2: 20, 3: 12 };\n\
                     var total = 0;\n\
                     for v in m.values() { total = total + v; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_clear() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 1, 2: 2, 3: 3 };\n\
                     m.clear();\n\
                     if m.is_empty() { 42 } else { m.size() }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_literal_spread_rightmost_wins() {
        let src = "function f(): i64 {\n\
                     var a: Map<str, i64> = { \"p\": 1, \"q\": 2 };\n\
                     var b: Map<str, i64> = { \"q\": 40 };\n\
                     var c: Map<str, i64> = { ..a, ..b, \"r\": 1 };\n\
                     var q = match c.get(\"q\") { i64 n => n, null => 0 };\n\
                     q + c.size()\n\
                   }";
        // q = 40 (b wins), size = 3 (p, q, r) => 43
        assert_eq!(run(src, "f"), 43);
    }

    #[test]
    fn map_grows_past_initial_capacity() {
        let src = "function f(): i64 {\n\
                     var m = Map.new<i64, i64>();\n\
                     var i = 0;\n\
                     while i < 50 { m.set(i, i * 2); i = i + 1; }\n\
                     match m.get(40) { i64 n => n + m.size(), null => 0 }\n\
                   }";
        // get(40) = 80, size = 50 => 130
        assert_eq!(run(src, "f"), 130);
    }

    // -- record-struct generic inference & the Iterator protocol -------------

    const RANGE_SRC: &str = "\
struct Range { current: i64, end: i64 }\n\
extend Range: Iterator<i64> {\n\
  function next(self): Item<i64> | Done {\n\
    if self.current >= self.end { Done {} }\n\
    else { var v = self.current; self.current = self.current + 1; Item { value: v } }\n\
  }\n\
}\n";

    #[test]
    fn record_struct_generic_arg_inferred_from_field() {
        // `Box { item: 42 }` infers `Box<i64>` from the field value.
        let src = "struct Box<T> { item: T }\n\
                   function f(): i64 { var b = Box { item: 42 }; b.item }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn for_over_iterator_sums() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var total = 0;\n\
               for x in (Range {{ current: 1, end: 5 }}) {{ total = total + x; }}\n\
               total\n\
             }}"
        );
        // 1 + 2 + 3 + 4 = 10
        assert_eq!(run(&src, "f"), 10);
    }

    #[test]
    fn for_over_iterator_break_continue() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var sum = 0;\n\
               var r = Range {{ current: 0, end: 100 }};\n\
               for n in r {{\n\
                 if n == 2 {{ continue; }}\n\
                 if n == 5 {{ break; }}\n\
                 sum = sum + n;\n\
               }}\n\
               sum\n\
             }}"
        );
        // 0 + 1 + (skip 2) + 3 + 4 + (break at 5) = 8
        assert_eq!(run(&src, "f"), 8);
    }

    #[test]
    fn iterator_next_match_directly() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var r = Range {{ current: 7, end: 9 }};\n\
               match r.next() {{ Item<i64> it => it.value, Done d => -1 }}\n\
             }}"
        );
        assert_eq!(run(&src, "f"), 7);
    }

    #[test]
    fn iterator_done_after_exhaustion() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var r = Range {{ current: 0, end: 1 }};\n\
               match r.next() {{ Item<i64> a => 0, Done d => -1 }};\n\
               match r.next() {{ Item<i64> b => 0, Done d => 42 }}\n\
             }}"
        );
        assert_eq!(run(&src, "f"), 42);
    }

    // -- generic bounds & monomorphized interface-method dispatch ------------

    const SHOW_SRC: &str = "\
interface Show { function show(self): i64; }\n\
struct Dog {}\n\
struct Cat {}\n\
extend Dog: Show { function show(self): i64 { 1 } }\n\
extend Cat: Show { function show(self): i64 { 2 } }\n";

    #[test]
    fn bound_method_dispatch_per_instance() {
        // `tell` is monomorphized for Dog and Cat to their own `show` impls.
        let src = format!(
            "{SHOW_SRC}\
             function tell<T: Show>(x: T): i64 {{ x.show() }}\n\
             function f(): i64 {{ tell(Dog {{}}) * 10 + tell(Cat {{}}) }}"
        );
        // Dog.show=1, Cat.show=2 => 12
        assert_eq!(run(&src, "f"), 12);
    }

    #[test]
    fn bound_method_with_argument() {
        let src = "interface Adder { function add(self, n: i64): i64; }\n\
                   struct C { base: i64 }\n\
                   extend C: Adder { function add(self, n: i64): i64 { self.base + n } }\n\
                   function bump<T: Adder>(x: T, by: i64): i64 { x.add(by) }\n\
                   function f(): i64 { bump(C { base: 100 }, 23) }";
        assert_eq!(run(src, "f"), 123);
    }

    #[test]
    fn bound_generic_interface_iterator() {
        let src = format!(
            "{RANGE_SRC}\
             function first<T: Iterator<i64>>(it: T): i64 {{\n\
               match it.next() {{ Item<i64> x => x.value, Done d => -1 }}\n\
             }}\n\
             function f(): i64 {{ first(Range {{ current: 9, end: 20 }}) }}"
        );
        assert_eq!(run(&src, "f"), 9);
    }

    // -- List higher-order methods (closures + trailing-closure sugar) -------

    #[test]
    fn list_map_with_trailing_closure_and_it() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3, 4];\n\
                     var d = xs.map { it * 2 };\n\
                     d.fold(0, (a: i64, x: i64): i64 => a + x)\n\
                   }";
        // (2+4+6+8) = 20
        assert_eq!(run(src, "f"), 20);
    }

    #[test]
    fn list_filter_keeps_matching() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3, 4, 5, 6];\n\
                     xs.filter { it % 2 == 0 }.size()\n\
                   }";
        assert_eq!(run(src, "f"), 3);
    }

    #[test]
    fn list_fold_sums() {
        let src = "function f(): i64 {\n\
                     [10, 20, 12].fold(0, (a: i64, x: i64): i64 => a + x)\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_map_changes_element_type() {
        // `map` over `List<str>` producing `List<i64>` (the lengths).
        let src = "function f(): i64 {\n\
                     var names: List<str> = [\"a\", \"bb\", \"ccc\"];\n\
                     var lens = names.map { it.size() };\n\
                     lens[0] + lens[1] + lens[2]\n\
                   }";
        // 1 + 2 + 3 = 6
        assert_eq!(run(src, "f"), 6);
    }

    #[test]
    fn list_map_explicit_closure_arg() {
        // The closure may also be passed as a normal argument (not trailing).
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3];\n\
                     var inc = xs.map((n: i64): i64 => n + 100);\n\
                     inc[2]\n\
                   }";
        assert_eq!(run(src, "f"), 103);
    }

    // -- closures ------------------------------------------------------------

    #[test]
    fn closure_basic_call() {
        let src = "function f(): i64 { var inc = (n: i64): i64 => n + 1; inc(41) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn closure_captures_local_by_value() {
        let src = "function f(): i64 {\n\
                     var base = 100;\n\
                     var add = (n: i64): i64 => n + base;\n\
                     add(5)\n\
                   }";
        assert_eq!(run(src, "f"), 105);
    }

    #[test]
    fn closure_passed_to_function() {
        let src = "function apply(g: (i64) => i64, x: i64): i64 { g(x) }\n\
                   function f(): i64 {\n\
                     var dbl = (n: i64): i64 => n * 2;\n\
                     apply(dbl, 21)\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn closure_returned_from_function() {
        let src = "function adder(by: i64): (i64) => i64 { (n: i64): i64 => n + by }\n\
                   function f(): i64 {\n\
                     var add10 = adder(10);\n\
                     var add100 = adder(100);\n\
                     add10(5) + add100(5)\n\
                   }";
        // 15 + 105 = 120
        assert_eq!(run(src, "f"), 120);
    }

    #[test]
    fn closure_captures_str() {
        let src = "function f(): str {\n\
                     var prefix = \"hi \";\n\
                     var greet = (name: str): str => prefix + name;\n\
                     greet(\"ada\")\n\
                   }";
        assert_eq!(run_str(src, "f"), "hi ada");
    }

    #[test]
    fn closures_in_a_list() {
        let src = "function adder(by: i64): (i64) => i64 { (n: i64): i64 => n + by }\n\
                   function f(): i64 {\n\
                     var fns: List<(i64) => i64> = [adder(1), adder(2), adder(3)];\n\
                     var total = 0;\n\
                     for g in fns { total = total + g(10); }\n\
                     total\n\
                   }";
        // (10+1) + (10+2) + (10+3) = 36
        assert_eq!(run(src, "f"), 36);
    }

    #[test]
    fn map_index_read_and_write() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"a\": 1 };\n\
                     m[\"b\"] = 20;\n\
                     m[\"a\"] = 10;\n\
                     m[\"a\"] + m[\"b\"]\n\
                   }";
        assert_eq!(run(src, "f"), 30);
    }

    #[test]
    fn map_index_int_keys() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 100 };\n\
                     m[2] = 50;\n\
                     m[1] + m[2]\n\
                   }";
        assert_eq!(run(src, "f"), 150);
    }

    #[test]
    fn for_entry_in_map_sums_values() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 10, 2: 20, 3: 12 };\n\
                     var total = 0;\n\
                     for e in m { total = total + e.key + e.value; }\n\
                     total\n\
                   }";
        // keys 1+2+3=6, values 10+20+12=42 => 48
        assert_eq!(run(src, "f"), 48);
    }

    #[test]
    fn for_entry_in_map_str_keys() {
        let src = "function f(): str {\n\
                     var m: Map<str, str> = { \"k\": \"v\" };\n\
                     var out = \"\";\n\
                     for e in m { out = out + e.key + \"=\" + e.value; }\n\
                     out\n\
                   }";
        assert_eq!(run_str(src, "f"), "k=v");
    }

    // -- generic `extend` method resolution ----------------------------------

    const PAIR_SRC: &str = "\
struct Pair<A, B> { first: A, second: B }\n\
extend<A, B> Pair<A, B> {\n\
  function fst(self): A { self.first }\n\
  function snd(self): B { self.second }\n\
  function swap(self): Pair<B, A> { Pair { first: self.second, second: self.first } }\n\
}\n";

    #[test]
    fn generic_extend_method_returns_param() {
        let src = format!(
            "{PAIR_SRC}function f(): i64 {{ var p = Pair {{ first: 42, second: 7 }}; p.fst() }}"
        );
        assert_eq!(run(&src, "f"), 42);
    }

    #[test]
    fn generic_extend_method_second_param() {
        let src = format!(
            "{PAIR_SRC}function f(): str {{\n\
               var p = Pair {{ first: 1, second: \"hi\" }};\n\
               p.snd()\n\
             }}"
        );
        assert_eq!(run_str(&src, "f"), "hi");
    }

    #[test]
    fn generic_extend_method_returning_generic_struct() {
        let src = format!(
            "{PAIR_SRC}function f(): i64 {{\n\
               var p = Pair {{ first: 5, second: 9 }};\n\
               var s = p.swap();\n\
               s.fst() * 10 + s.snd()\n\
             }}"
        );
        // swap => Pair{first:9, second:5}; 9*10+5 = 95
        assert_eq!(run(&src, "f"), 95);
    }

    #[test]
    fn generic_extend_mutating_method() {
        let src = "struct Cell<T> { value: T }\n\
                   extend<T> Cell<T> {\n\
                     function get(self): T { self.value }\n\
                     function put(self, v: T) { self.value = v; }\n\
                   }\n\
                   function f(): i64 { var c = Cell { value: 1 }; c.put(77); c.get() }";
        assert_eq!(run(src, "f"), 77);
    }

    #[test]
    fn for_over_bounded_type_parameter() {
        let src = format!(
            "{RANGE_SRC}\
             function sum<T: Iterator<i64>>(it: T): i64 {{\n\
               var total = 0;\n\
               for x in it {{ total = total + x; }}\n\
               total\n\
             }}\n\
             function f(): i64 {{ sum(Range {{ current: 1, end: 5 }}) }}"
        );
        // 1 + 2 + 3 + 4 = 10
        assert_eq!(run(&src, "f"), 10);
    }

    #[test]
    fn interface_object_satisfies_its_own_bound() {
        // A `dyn Iterator<i64>` value is accepted where `T: Iterator<i64>`.
        let src = format!(
            "{RANGE_SRC}\
             function sum<T: Iterator<i64>>(it: T): i64 {{\n\
               var t = 0;\n\
               for x in it {{ t = t + x; }}\n\
               t\n\
             }}\n\
             function f(): i64 {{\n\
               var it: Iterator<i64> = Range {{ current: 1, end: 4 }};\n\
               sum(it)\n\
             }}"
        );
        // 1 + 2 + 3 = 6
        assert_eq!(run(&src, "f"), 6);
    }

    #[test]
    fn for_over_interface_object_iterator() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var it: Iterator<i64> = Range {{ current: 10, end: 13 }};\n\
               var s = 0;\n\
               for y in it {{ s = s + y; }}\n\
               s\n\
             }}"
        );
        // 10 + 11 + 12 = 33
        assert_eq!(run(&src, "f"), 33);
    }

    #[test]
    fn generic_iterator_in_for_loop() {
        let src = "struct Count<T> { value: T, n: i64 }\n\
                   extend<T> Count<T>: Iterator<T> {\n\
                     function next(self): Item<T> | Done {\n\
                       if self.n <= 0 { Done {} }\n\
                       else { self.n = self.n - 1; Item { value: self.value } }\n\
                     }\n\
                   }\n\
                   function f(): i64 {\n\
                     var total = 0;\n\
                     for x in (Count { value: 7, n: 4 }) { total = total + x; }\n\
                     total\n\
                   }";
        // 7 added 4 times = 28
        assert_eq!(run(src, "f"), 28);
    }

    // -- interface objects / dynamic dispatch --------------------------------

    const ANIMALS_SRC: &str = "\
interface Sound { function code(self): i64; }\n\
struct Dog {}\n\
struct Cat {}\n\
extend Dog: Sound { function code(self): i64 { 1 } }\n\
extend Cat: Sound { function code(self): i64 { 2 } }\n";

    #[test]
    fn dyn_dispatch_through_interface_value() {
        let src = format!(
            "{ANIMALS_SRC}\
             function f(): i64 {{\n\
               var a: Sound = Dog {{}};\n\
               var b: Sound = Cat {{}};\n\
               a.code() * 10 + b.code()\n\
             }}"
        );
        // Dog=1, Cat=2 => 12
        assert_eq!(run(&src, "f"), 12);
    }

    #[test]
    fn dyn_dispatch_through_parameter() {
        let src = format!(
            "{ANIMALS_SRC}\
             function tell(s: Sound): i64 {{ s.code() }}\n\
             function f(): i64 {{ tell(Dog {{}}) + tell(Cat {{}}) * 100 }}"
        );
        // 1 + 2*100 = 201
        assert_eq!(run(&src, "f"), 201);
    }

    #[test]
    fn dyn_dispatch_over_heterogeneous_list() {
        let src = format!(
            "{ANIMALS_SRC}\
             function f(): i64 {{\n\
               var zoo: List<Sound> = [Dog {{}}, Cat {{}}, Cat {{}}];\n\
               var total = 0;\n\
               for s in zoo {{ total = total + s.code(); }}\n\
               total\n\
             }}"
        );
        // 1 + 2 + 2 = 5
        assert_eq!(run(&src, "f"), 5);
    }

    #[test]
    fn dyn_method_with_argument_and_str() {
        let src = "interface Greeter { function greet(self, name: str): str; }\n\
                   struct Formal { title: str }\n\
                   extend Formal: Greeter {\n\
                     function greet(self, name: str): str { self.title + name }\n\
                   }\n\
                   function f(): str {\n\
                     var g: Greeter = Formal { title: \"Dr \" };\n\
                     g.greet(\"Ada\")\n\
                   }";
        assert_eq!(run_str(src, "f"), "Dr Ada");
    }

    #[test]
    fn dyn_is_checks_concrete_type() {
        let src = format!(
            "{ANIMALS_SRC}\
             function f(): i64 {{\n\
               var s: Sound = Dog {{}};\n\
               var a = if s is Dog {{ 1 }} else {{ 0 }};\n\
               var b = if s is Cat {{ 1 }} else {{ 0 }};\n\
               a * 10 + b\n\
             }}"
        );
        // is Dog => 1, is Cat => 0 => 10
        assert_eq!(run(&src, "f"), 10);
    }

    #[test]
    fn dyn_as_downcasts_to_concrete() {
        let src = "interface Show { function show(self): i64; }\n\
                   struct Boxed { n: i64 }\n\
                   extend Boxed: Show { function show(self): i64 { self.n } }\n\
                   function f(): i64 {\n\
                     var s: Show = Boxed { n: 42 };\n\
                     var b = s as Boxed;\n\
                     b.n\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn dyn_narrowing_then_field_access() {
        let src = "interface Show { function show(self): i64; }\n\
                   struct A { x: i64 }\n\
                   struct B {}\n\
                   extend A: Show { function show(self): i64 { self.x } }\n\
                   extend B: Show { function show(self): i64 { -1 } }\n\
                   function f(): i64 {\n\
                     var s: Show = A { x: 9 };\n\
                     if s is A { var a = s as A; a.x } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 9);
    }

    #[test]
    fn for_in_list_sum() {
        let src = "function f(): i64 {\n\
                     var total: i64 = 0;\n\
                     for x in [10, 20, 12] { total = total + x; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn for_in_list_with_break() {
        let src = "function f(): i64 {\n\
                     var total: i64 = 0;\n\
                     for x in [1, 2, 3, 100, 200] {\n\
                       if x > 10 { break }\n\
                       total = total + x;\n\
                     }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 6);
    }

    #[test]
    fn for_in_list_with_continue() {
        let src = "function f(): i64 {\n\
                     var total: i64 = 0;\n\
                     for x in [1, 2, 3, 4, 5, 6] {\n\
                       if x % 2 == 1 { continue }\n\
                       total = total + x;\n\
                     }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 12);
    }

    #[test]
    fn for_over_list_param() {
        let src = "function sum(xs: List<i64>): i64 {\n\
                     var t: i64 = 0;\n\
                     for x in xs { t = t + x; }\n\
                     t\n\
                   }\n\
                   function f(): i64 { sum([3, 4, 5, 30]) }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- generics (monomorphization) ---------------------------------------

    #[test]
    fn generic_identity_inferred() {
        let src = "function id<T>(x: T): T { x }\n\
                   function f(): i64 { id(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_identity_two_instances() {
        // Used at i64 and str — two monomorphizations from one body.
        let src = "function id<T>(x: T): T { x }\n\
                   function f(): i64 { id(40) + 2 }\n\
                   function s(): str { id(\"hi\") }";
        assert_eq!(run(src, "f"), 42);
        assert_eq!(run_str(src, "s"), "hi");
    }

    #[test]
    fn generic_explicit_type_args() {
        let src = "function id<T>(x: T): T { x }\n\
                   function f(): i64 { id<i64>(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_two_params() {
        let src = "function first<A, B>(a: A, b: B): A { a }\n\
                   function f(): i64 { first(42, \"ignored\") }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_transitive_instantiation() {
        // `wrap` calls `id` with its own `T`; instantiating wrap<i64> must
        // transitively instantiate id<i64>.
        let src = "function id<T>(x: T): T { x }\n\
                   function wrap<T>(x: T): T { id(x) }\n\
                   function f(): i64 { wrap(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_struct_box() {
        let src = "struct Box<T> { value: T }\n\
                   function f(): i64 { var b = Box<i64> { value: 42 }; b.value }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_max_with_bound() {
        let src = "function pick<T>(a: T, b: T, useA: bool): T { if useA { a } else { b } }\n\
                   function f(): i64 { pick(42, 7, true) }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- `?` propagation ---------------------------------------------------

    const TRY_SRC: &str = "\
        function parse(ok: bool): i64 | str { if ok { 42 } else { \"bad\" } }\n\
        function f(ok: bool): str {\n\
          var n: i64 = parse(ok)?;\n\
          \"n=\" + (n as str)\n\
        }\n";

    #[test]
    fn try_success_path_unwraps() {
        let src = format!("{TRY_SRC}function g(): str {{ f(true) }}");
        assert_eq!(run_str(&src, "g"), "n=42");
    }

    #[test]
    fn try_error_path_propagates() {
        // The `str` error variant is returned early as `f`'s `str` result.
        let src = format!("{TRY_SRC}function g(): str {{ f(false) }}");
        assert_eq!(run_str(&src, "g"), "bad");
    }

    #[test]
    fn try_propagates_into_union_return() {
        let src = "function parse(ok: bool): i64 | str { if ok { 7 } else { \"e\" } }\n\
                   function f(ok: bool): bool | str { var n: i64 = parse(ok)?; n > 3 }\n\
                   function g(): i64 { var r = f(true); if r is bool { if r as bool { 42 } else { 0 } } else { 1 } }";
        assert_eq!(run(&src, "g"), 42);
    }

    // --- flow narrowing ----------------------------------------------------

    #[test]
    fn narrowing_uses_variant_without_explicit_as() {
        // In the `is i64` branch, `x` is usable as `i64` directly.
        let src = "function f(x: i64 | str): i64 { if x is i64 { x + 1 } else { 0 } }\n\
                   function g(): i64 { f(41) }";
        assert_eq!(run(src, "g"), 42);
    }

    #[test]
    fn narrowing_else_branch_complement() {
        // After `if x is null { .. }`, the else branch narrows to `i64`.
        let src = "function f(x: i64 | null): i64 { if x is null { 0 } else { x + 2 } }\n\
                   function g(): i64 { f(40) }";
        assert_eq!(run(src, "g"), 42);
    }

    #[test]
    fn narrowing_struct_field_access() {
        let src = "struct P { v: i64 }\n\
                   function f(x: P | null): i64 { if x is P { x.v } else { 0 } }\n\
                   function g(): i64 { f(P { v: 42 }) }";
        assert_eq!(run(src, "g"), 42);
    }

    #[test]
    fn narrowing_else_if_chain_composes() {
        // The final else narrows to the single remaining variant (str).
        let src = "function d(x: i64 | str | null): str {\n\
                     if x is null { \"none\" }\n\
                     else if x is i64 { \"int $x\" }\n\
                     else { \"s:$x\" }\n\
                   }\n\
                   function g(): str { d(\"hi\") }";
        assert_eq!(run_str(src, "g"), "s:hi");
    }

    #[test]
    fn narrowing_str_branch_interpolates() {
        let src = "function f(x: i64 | str): str { if x is str { \"got $x\" } else { \"num\" } }\n\
                   function g(): str { f(\"hi\") }";
        assert_eq!(run_str(src, "g"), "got hi");
    }

    // --- string interpolation ----------------------------------------------

    #[test]
    fn interpolate_ident() {
        let src = "function f(): str { var name = \"Alice\"; \"hi $name\" }";
        assert_eq!(run_str(src, "f"), "hi Alice");
    }

    #[test]
    fn interpolate_expr_and_number() {
        let src = "function f(): str { var n: i64 = 40; \"total: ${n + 2}\" }";
        assert_eq!(run_str(src, "f"), "total: 42");
    }

    #[test]
    fn interpolate_multiple_parts() {
        let src = "function f(): str {\n\
                     var x: i64 = 3;\n\
                     var y: i64 = 4;\n\
                     \"$x + $y = ${x + y}\"\n\
                   }";
        assert_eq!(run_str(src, "f"), "3 + 4 = 7");
    }

    #[test]
    fn interpolate_bool_and_char() {
        let src = "function f(): str { var b: bool = true; var c: char = 'Z'; \"$b $c\" }";
        assert_eq!(run_str(src, "f"), "true Z");
    }

    #[test]
    fn interpolate_escaped_dollar() {
        let src = "function f(): str { \"price: \\$5\" }";
        assert_eq!(run_str(src, "f"), "price: $5");
    }

    // --- match -------------------------------------------------------------

    #[test]
    fn match_union_variants() {
        let src = "function describe(x: i64 | str): i64 {\n\
                     match x { i64 n => n, str s => 0 }\n\
                   }\n\
                   function f(): i64 { describe(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_union_picks_right_arm() {
        let src = "function describe(x: i64 | str): i64 {\n\
                     match x { i64 n => n + 100, str s => 42 }\n\
                   }\n\
                   function f(): i64 { describe(\"hi\") }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_literals_with_wildcard() {
        let src = "function f(): i64 { var n: i64 = 2; match n { 1 => 10, 2 => 42, _ => 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_null_union() {
        let src = "function f(): i64 { var x: i64 | null = 42; match x { i64 n => n, null => 0 } }";
        assert_eq!(run(src, "f"), 42);
        let src2 = "function f(): i64 { var x: i64 | null = null; match x { i64 n => n, null => 42 } }";
        assert_eq!(run(src2, "f"), 42);
    }

    #[test]
    fn match_with_guard() {
        let src = "function f(): i64 { var n: i64 = 5; match n { i64 x if x > 3 => 42, _ => 0 } }";
        assert_eq!(run(src, "f"), 42);
        let src2 = "function f(): i64 { var n: i64 = 1; match n { i64 x if x > 3 => 0, _ => 42 } }";
        assert_eq!(run(src2, "f"), 42);
    }

    #[test]
    fn match_unit_struct_variants() {
        let src = "struct Red;\nstruct Green;\nstruct Blue;\n\
                   type Color = Red | Green | Blue;\n\
                   function code(c: Color): i64 { match c { Red => 1, Green => 2, Blue => 42 } }\n\
                   function f(): i64 { code(Blue) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_tuple_destructure() {
        let src = "function f(): i64 { var t = (40, 2); match t { (a, b) => a + b } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_extracts_struct_payload() {
        let src = "struct P { v: i64 }\n\
                   function f(): i64 { var x: P | null = P { v: 42 }; match x { P p => p.v, null => 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- str methods & comparison ------------------------------------------

    #[test]
    fn str_size_and_byte_size() {
        // "héllo": 5 scalars, 6 UTF-8 bytes.
        assert_eq!(run("function f(): i64 { \"héllo\".size() }", "f"), 5);
        assert_eq!(run("function f(): i64 { \"héllo\".byte_size() }", "f"), 6);
    }

    #[test]
    fn str_substring_and_case() {
        assert_eq!(run_str("function f(): str { \"hello world\".substring(0, 5) }", "f"), "hello");
        assert_eq!(run_str("function f(): str { \"abc\".to_upper() }", "f"), "ABC");
        assert_eq!(run_str("function f(): str { \"  hi  \".trim() }", "f"), "hi");
    }

    #[test]
    fn str_predicates() {
        assert_eq!(run("function f(): bool { \"hello\".contains(\"ell\") }", "f"), 1);
        assert_eq!(run("function f(): bool { \"hello\".starts_with(\"he\") }", "f"), 1);
        assert_eq!(run("function f(): bool { \"hello\".ends_with(\"lo\") }", "f"), 1);
        assert_eq!(run("function f(): bool { \"\".is_empty() }", "f"), 1);
    }

    #[test]
    fn str_equality_by_content() {
        // Two distinct str objects with equal content must compare equal.
        let src = "function f(): i64 { var a = \"x\" + \"y\"; var b = \"xy\"; if a == b { 42 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
        let ne = "function f(): i64 { if \"a\" != \"b\" { 42 } else { 0 } }";
        assert_eq!(run(ne, "f"), 42);
    }

    #[test]
    fn str_ordering_lexicographic() {
        assert_eq!(run("function f(): bool { \"apple\" < \"banana\" }", "f"), 1);
        assert_eq!(run("function f(): bool { \"banana\" < \"apple\" }", "f"), 0);
        assert_eq!(run("function f(): bool { \"abc\" <= \"abc\" }", "f"), 1);
    }

    // --- discriminated unions ----------------------------------------------

    #[test]
    fn union_widen_then_narrow() {
        let src = "function f(): i64 { var x: i64 | str = 40; (x as i64) + 2 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn union_is_test() {
        let yes = "function f(): i64 { var x: i64 | str = \"hi\"; if x is str { 42 } else { 0 } }";
        assert_eq!(run(yes, "f"), 42);
        let no = "function f(): i64 { var x: i64 | str = \"hi\"; if x is i64 { 0 } else { 42 } }";
        assert_eq!(run(no, "f"), 42);
    }

    #[test]
    fn optional_null_union() {
        let null_case =
            "function f(): i64 { var x: i64 | null = null; if x is null { 42 } else { 0 } }";
        assert_eq!(run(null_case, "f"), 42);
        let some_case =
            "function f(): i64 { var x: i64 | null = 40; if x is i64 { (x as i64) + 2 } else { 0 } }";
        assert_eq!(run(some_case, "f"), 42);
    }

    #[test]
    fn union_returned_from_function() {
        let src = "function pick(b: bool): i64 | null { if b { 42 } else { null } }\n\
                   function f(): i64 { var r = pick(true); if r is i64 { r as i64 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_in_union() {
        let src = "struct P { v: i64 }\n\
                   function f(): i64 { var x: P | null = P { v: 42 }; (x as P).v }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn widen_union_to_wider_union() {
        let src = "function f(): i64 {\n\
                     var a: i64 | str = 40;\n\
                     var b: i64 | str | bool = a;\n\
                     (b as i64) + 2\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn union_str_variant_roundtrip() {
        let src = "function f(): str { var x: i64 | str = \"hello\"; x as str }";
        assert_eq!(run_str(src, "f"), "hello");
    }

    // --- anonymous tuples --------------------------------------------------

    #[test]
    fn tuple_construct_and_index() {
        let src = "function f(): i64 { var t = (40, 2); t.0 + t.1 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_destructure() {
        let src = "function f(): i64 { var (a, b) = (40, 2); a + b }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_nested_destructure() {
        let src = "function f(): i64 { var (a, (b, c)) = (40, (1, 1)); a + b + c }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_mixed_types() {
        // (i64, str, bool) — read the i64 and use the str via run.
        let src = "function f(): str { var t = (42, \"hi\", true); t.1 }";
        assert_eq!(run_str(src, "f"), "hi");
    }

    #[test]
    fn tuple_returned_from_function() {
        let src = "function pair(): (i64, i64) { (40, 2) }\n\
                   function f(): i64 { var t = pair(); t.0 + t.1 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_index_wildcard_destructure() {
        let src = "function f(): i64 { var (a, _, c) = (40, 99, 2); a + c }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- methods (extend) --------------------------------------------------

    #[test]
    fn inherent_method_reads_self() {
        let src = "struct P { x: i64, y: i64 }\n\
                   extend P { function sum(self): i64 { self.x + self.y } }\n\
                   function f(): i64 { var p = P { x: 40, y: 2 }; p.sum() }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_with_arguments() {
        let src = "struct Counter { n: i64 }\n\
                   extend Counter { function add(self, k: i64): i64 { self.n + k } }\n\
                   function f(): i64 { var c = Counter { n: 40 }; c.add(2) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_mutates_self() {
        let src = "struct Counter { n: i64 }\n\
                   extend Counter { function bump(self) { self.n = self.n + 1; } }\n\
                   function f(): i64 { var c = Counter { n: 41 }; c.bump(); c.n }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_calls_method() {
        let src = "struct P { x: i64 }\n\
                   extend P {\n\
                     function doubled(self): i64 { self.x + self.x }\n\
                     function quad(self): i64 { self.doubled() + self.doubled() }\n\
                   }\n\
                   function f(): i64 { var p = P { x: 10 }; p.quad() + 2 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn methods_on_different_types_same_name() {
        // Two types each define `val`; mangled symbols must not collide.
        let src = "struct A { a: i64 }\n\
                   struct B { b: i64 }\n\
                   extend A { function val(self): i64 { self.a } }\n\
                   extend B { function val(self): i64 { self.b * 2 } }\n\
                   function f(): i64 {\n\
                     var x = A { a: 40 };\n\
                     var y = B { b: 1 };\n\
                     x.val() + y.val()\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_returning_self_type() {
        let src = "struct V { x: i64 }\n\
                   extend V { function plus(self, o: V): V { V { x: self.x + o.x } } }\n\
                   function f(): i64 { var a = V { x: 40 }; var b = V { x: 2 }; a.plus(b).x }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_aliasing_shares_heap() {
        // b = a aliases the same object; mutating through b is visible via a.
        let src = "struct P { x: i64 }\n\
                   function f(): i64 { var a = P { x: 1 }; var b = a; b.x = 42; a.x }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn nested_loops_break_innermost() {
        let src = "function f(): i64 {\n\
                     var count: i64 = 0;\n\
                     var i: i64 = 0;\n\
                     while i < 3 {\n\
                       var j: i64 = 0;\n\
                       while j < 100 {\n\
                         if j >= 2 { break }\n\
                         count = count + 1;\n\
                         j = j + 1;\n\
                       }\n\
                       i = i + 1;\n\
                     }\n\
                     count\n\
                   }";
        // inner loop adds 2 each of 3 outer iterations = 6
        assert_eq!(run(src, "f"), 6);
    }
}
