//! End-to-end tests: write a `.otter` file, invoke the `otter_fusion` binary, and check
//! its stdout/exit status. Exercises the full pipeline including `print`.

use std::process::{Child, Command, Output, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Toolchain imports prepended to single-file test programs (near-empty prelude,
/// `docs/17` §17.8): the former magic builtins plus common interfaces/std types.
/// `core:collections` is intentionally omitted so it never collides with tests
/// that import `List`/`Map` explicitly; programs naming those import them.
const CLI_PRELUDE: &str = "import { List, Map, Set, Entry } from \"core:collections\";\n\
    import { print, println } from \"std:io\";\n\
    import { panic, panic_with } from \"core:prelude\";\n\
    import { exit, abort } from \"std:process\";\n\
    import { Clone, ToStr, Eq, Ord, Hash, Iterator, Item, Done, Try, FromResidual, Drop, Future, Ready, Pending, Context } from \"core:prelude\";\n\
    import { Shared, LockBusy, Sender, Receiver, ChannelClosed, MpmcSender, MpmcReceiver, channel, channel_bounded, channel_mpmc, channel_mpmc_bounded } from \"std:sync\";\n\
    import { Thread, JoinHandle, Joined, Panicked } from \"std:thread\";\n\
    import { AsyncIterator } from \"core:async\";\n\
    import { TimedOut, yield_now, sleep, timeout } from \"std:async\";\n\
    import { Foreign, CString, CStr, Buffer } from \"core:ffi\";\n";

/// Prepend [`CLI_PRELUDE`] to a program's source.
fn pre(src: &str) -> String {
    format!("{CLI_PRELUDE}{src}")
}

/// Prepend the prelude only to `.otter` files (project files such as
/// `project.toml` are passed through unchanged).
fn pre_file(rel: &str, src: &str) -> String {
    if rel.ends_with(".otter") {
        pre(src)
    } else {
        src.to_string()
    }
}

fn native_build_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn native_build_guard() -> MutexGuard<'static, ()> {
    native_build_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ProcessSlots {
    in_use: Mutex<usize>,
    available: Condvar,
    limit: usize,
}

struct ProcessSlotGuard(&'static ProcessSlots);

impl Drop for ProcessSlotGuard {
    fn drop(&mut self) {
        let mut in_use = self.0.in_use.lock().unwrap_or_else(|err| err.into_inner());
        *in_use = in_use.saturating_sub(1);
        self.0.available.notify_one();
    }
}

fn cli_process_slots() -> &'static ProcessSlots {
    static SLOTS: OnceLock<ProcessSlots> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let default_limit = 1;
        let limit = std::env::var("OTTER_CLI_TEST_PROCESS_SLOTS")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|slots| *slots > 0)
            .unwrap_or(default_limit);
        ProcessSlots {
            in_use: Mutex::new(0),
            available: Condvar::new(),
            limit,
        }
    })
}

fn cli_process_slot() -> ProcessSlotGuard {
    let slots = cli_process_slots();
    let mut in_use = slots.in_use.lock().unwrap_or_else(|err| err.into_inner());
    while *in_use >= slots.limit {
        in_use = slots
            .available
            .wait(in_use)
            .unwrap_or_else(|err| err.into_inner());
    }
    *in_use += 1;
    ProcessSlotGuard(slots)
}

fn native_test_timeout() -> Duration {
    std::env::var("OTTER_NATIVE_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(60))
}

fn cli_test_timeout() -> Duration {
    std::env::var("OTTER_CLI_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(60))
}

fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_watchdog_process_group(cmd);
    let _slot = cli_process_slot();
    let child = cmd.spawn().map_err(|e| format!("spawn command: {e}"))?;
    output_from_child_with_timeout(child, timeout)
}

#[cfg(unix)]
unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn configure_watchdog_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_watchdog_process_group(_cmd: &mut Command) {}

fn kill_timed_out_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        if pgid > 0 {
            unsafe {
                let _ = kill(-pgid, 9);
            }
        }
    }
    let _ = child.kill();
}

fn output_from_child_with_timeout(mut child: Child, timeout: Duration) -> Result<Output, String> {
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|e| format!("wait command: {e}"))? {
            Some(_) => {
                return child
                    .wait_with_output()
                    .map_err(|e| format!("collect command output: {e}"));
            }
            None if start.elapsed() >= timeout => {
                kill_timed_out_child(&mut child);
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("collect timed-out command output: {e}"))?;
                let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                stderr.push_str(&format!(
                    "\ncommand timed out after {:.3}s\n",
                    start.elapsed().as_secs_f64()
                ));
                return Err(stderr);
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn command_failure_stderr(label: &str, output: &Output) -> String {
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(&format!("{label} exited with status {}\n", output.status));
    stderr
}

#[test]
#[cfg(unix)]
fn command_timeout_kills_process_group_descendants() {
    let marker = std::env::temp_dir().join(format!("otter_timeout_marker_{}", nonce()));
    let _ = std::fs::remove_file(&marker);
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("(sleep 1; echo leaked > \"$OTTER_TIMEOUT_MARKER\") & wait")
        .env("OTTER_TIMEOUT_MARKER", &marker);

    let err = output_with_timeout(&mut command, Duration::from_millis(50))
        .expect_err("background child should be killed by the timeout watchdog");
    assert!(err.contains("command timed out"), "stderr: {err}");

    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        !marker.exists(),
        "timeout watchdog killed the shell but left its background child alive"
    );
}

/// Run `otter_fusion <cmd> <file>` with `src` written to a temp file; return
/// (stdout, stderr, success).
fn lang(cmd: &str, src: &str) -> (String, String, bool) {
    lang_env(cmd, src, &[])
}

/// Like [`otter_fusion`], with extra command-line flags after the file (e.g.
/// `--release`).
fn lang_flag(cmd: &str, src: &str, flags: &[&str]) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_test_{}.otter", nonce()));
    std::fs::write(&path, pre(src)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.arg(cmd).arg(&path);
    for f in flags {
        command.arg(f);
    }
    let out = match output_with_timeout(&mut command, cli_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&path);
    if !out.status.success() {
        return (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            command_failure_stderr("jit command", &out),
            false,
        );
    }
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Like [`otter_fusion`], with extra environment variables.
fn lang_env(cmd: &str, src: &str, env: &[(&str, &str)]) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_test_{}.otter", nonce()));
    std::fs::write(&path, pre(src)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.arg(cmd).arg(&path);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = match output_with_timeout(&mut command, cli_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&path);
    if !out.status.success() {
        return (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            command_failure_stderr("jit command", &out),
            false,
        );
    }
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Compile `src` to a native executable with `lang build -o`, run it, and
/// return (stdout, stderr, success). Exercises the object-emit + link path.
fn lang_build_run(src: &str, env: &[(&str, &str)]) -> (String, String, bool) {
    let _native_guard = native_build_guard();
    let dir = std::env::temp_dir();
    let n = nonce();
    let path = dir.join(format!("lang_test_{n}.otter"));
    let exe = dir.join(format!("lang_test_bin_{n}"));
    std::fs::write(&path, pre(src)).unwrap();
    let mut build_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    build_cmd.arg("build").arg(&path).arg("-o").arg(&exe);
    let build = match output_with_timeout(&mut build_cmd, native_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&exe);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&path);
    if !build.status.success() {
        return (
            String::new(),
            command_failure_stderr("native build", &build),
            false,
        );
    }
    let mut run = Command::new(&exe);
    for (k, v) in env {
        run.env(k, v);
    }
    let out = match output_with_timeout(&mut run, native_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&exe);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&exe);
    if !out.status.success() {
        return (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            command_failure_stderr("native executable", &out),
            false,
        );
    }
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Like [`lang`], but do not prepend the test prelude. Use this for module
/// surfaces whose names intentionally overlap with the prelude imports.
fn lang_raw(cmd: &str, src: &str) -> (String, String, bool) {
    lang_raw_env(cmd, src, &[])
}

fn lang_raw_env(cmd: &str, src: &str, env: &[(&str, &str)]) -> (String, String, bool) {
    lang_raw_env_with_timeout(cmd, src, env, cli_test_timeout())
}

fn lang_raw_env_with_timeout(
    cmd: &str,
    src: &str,
    env: &[(&str, &str)],
    timeout: Duration,
) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_test_raw_{}.otter", nonce()));
    std::fs::write(&path, src).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.arg(cmd).arg(&path);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = match output_with_timeout(&mut command, timeout) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&path);
    if !out.status.success() {
        return (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            command_failure_stderr("jit command", &out),
            false,
        );
    }
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Native-build counterpart of [`lang_raw`].
fn lang_build_run_raw(src: &str, env: &[(&str, &str)]) -> (String, String, bool) {
    let _native_guard = native_build_guard();
    lang_build_run_raw_unlocked(src, env)
}

fn lang_build_run_raw_unlocked(src: &str, env: &[(&str, &str)]) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let n = nonce();
    let path = dir.join(format!("lang_test_raw_{n}.otter"));
    let exe = dir.join(format!("lang_test_raw_bin_{n}"));
    std::fs::write(&path, src).unwrap();
    let mut build_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    build_cmd.arg("build").arg(&path).arg("-o").arg(&exe);
    let build = match output_with_timeout(&mut build_cmd, native_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&exe);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&path);
    if !build.status.success() {
        return (
            String::new(),
            command_failure_stderr("native build", &build),
            false,
        );
    }
    let mut run = Command::new(&exe);
    for (k, v) in env {
        run.env(k, v);
    }
    let out = match output_with_timeout(&mut run, native_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&exe);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&exe);
    if !out.status.success() {
        return (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            command_failure_stderr("native executable", &out),
            false,
        );
    }
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Write a multi-file program into a fresh temp directory and run it.
/// `files` maps a relative path (e.g. `"app/util.otter"`) to its source; the
/// entry is `entry` (relative to the temp dir). Returns (stdout, stderr, ok).
fn lang_run_project(entry: &str, files: &[(&str, &str)]) -> (String, String, bool) {
    let root = std::env::temp_dir().join(format!("lang_proj_{}", nonce()));
    for (rel, src) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, pre_file(rel, src)).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.arg("run").arg(root.join(entry));
    let out = match output_with_timeout(&mut command, cli_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&root);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_dir_all(&root);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn nonce() -> u64 {
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = C.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ((std::process::id() as u64) << 32) | n
}

/// Run `otter_fusion <args...>` with the working directory set to `dir`.
fn lang_in_dir(dir: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.current_dir(dir).args(args);
    let out = match output_with_timeout(&mut command, cli_test_timeout()) {
        Ok(out) => out,
        Err(err) => return (String::new(), err, false),
    };
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Run `otter_fusion <args...>` in `dir` with extra environment variables.
fn lang_in_dir_env(
    dir: &std::path::Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (String, String, bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.current_dir(dir).args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = match output_with_timeout(&mut command, cli_test_timeout()) {
        Ok(out) => out,
        Err(err) => return (String::new(), err, false),
    };
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Materialize files (relative path → contents) under a fresh temp dir, returning it.
fn write_tree(files: &[(&str, &str)]) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("lang_deps_{}", nonce()));
    for (rel, src) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, pre_file(rel, src)).unwrap();
    }
    root
}

/// Run `otter_fusion emit <ir> <file>` with `src` in a temp file. The program is
/// used verbatim (IR dumps must reflect exactly the given source).
fn emit_ir(ir: &str, src: &str) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_test_{}.otter", nonce()));
    std::fs::write(&path, src).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.arg("emit").arg(ir).arg(&path);
    let out = match output_with_timeout(&mut command, cli_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn emit_hir_is_typed_resolved_and_dispatched() {
    let (out, err, ok) = emit_ir(
        "hir",
        "import { println } from \"std:io\";\n\
         function add(x: i64, y: i64): i64 { x + y }\n\
         function main() { var z = add(2, 3); println(\"hi\"); }",
    );
    assert!(ok, "stderr: {err}");
    // Every expression carries its type; the operator is a primitive Add.
    assert!(out.contains("(binary Add"), "no typed binary:\n{out}");
    assert!(out.contains(":i64"), "no type annotations:\n{out}");
    // Names are resolved; the call's dispatch kind is explicit.
    assert!(out.contains("name local"), "no resolved local:\n{out}");
    assert!(out.contains("call direct"), "no direct dispatch:\n{out}");
    assert!(
        out.contains("call builtin Println"),
        "no builtin dispatch:\n{out}"
    );
}

#[test]
fn emit_hir_shows_baked_coercions_and_match_dispatch() {
    // The data that used to live in the deleted `adjustments` / `pattern_types`
    // span tables is now on the HIR nodes — and therefore observable in `--emit
    // hir`: an `is`-narrowed read shows an unbox/widen adjust, and a `match`
    // shows its arm patterns.
    let (out, err, ok) = emit_ir(
        "hir",
        "function f(x: i64 | str): i64 { match x { i64 n => n + 1, str s => 0 } }",
    );
    assert!(ok, "stderr: {err}");
    assert!(out.contains("match"), "no match in HIR:\n{out}");
    // The narrowed `n` (a union variant matched as `i64`) is used as an `i64`.
    assert!(
        out.contains("(binary Add"),
        "narrowed arithmetic missing:\n{out}"
    );
    // A widening coercion is printed as an explicit adjust wrapper somewhere.
    let (w, _, _) = emit_ir("hir", "function g(): i64 | str { var u: i64 | str = 5; u }");
    assert!(
        w.contains("widen") || w.contains("adjust"),
        "no baked widen adjust:\n{w}"
    );
}

#[test]
fn emit_hir_is_deterministic() {
    let src = "struct P { x: i64, y: i64 }\n\
               function main() { var p = P { x: 1, y: 2 }; }";
    let a = emit_ir("hir", src).0;
    let b = emit_ir("hir", src).0;
    assert_eq!(a, b, "emit hir must be byte-for-byte deterministic");
    assert!(
        a.contains("record(x: i64, y: i64)"),
        "no struct layout:\n{a}"
    );
}

#[test]
fn emit_tokens_lists_kinds_and_spans() {
    let (out, err, ok) = emit_ir("tokens", "function main() {}");
    assert!(ok, "stderr: {err}");
    assert!(out.contains("Kw(Function) @ 0..8"), "tokens:\n{out}");
    assert!(out.lines().count() >= 5, "too few tokens:\n{out}");
}

#[test]
fn emit_ast_dumps_the_tree() {
    let (out, err, ok) = emit_ir("ast", "function main() {}");
    assert!(ok, "stderr: {err}");
    assert!(out.contains("Module"), "no Module node:\n{out}");
    assert!(out.contains("Function"), "no Function item:\n{out}");
}

#[test]
fn emit_clif_dumps_cranelift_ir() {
    let src = "function add(a: i64, b: i64): i64 { a + b }\n\
               function main(): i64 { add(40, 2) }";
    let (out, err, ok) = emit_ir("clif", src);
    assert!(ok, "stderr: {err}");
    // Per-function Cranelift IR with the source-symbol header.
    assert!(out.contains("; add"), "missing `add` header:\n{out}");
    assert!(out.contains("; main"), "missing `main` header:\n{out}");
    // Cranelift function syntax + real instructions (debug builds emit a
    // checked `sadd_overflow`; release emits a plain `iadd`).
    assert!(out.contains("function u0:"), "no clif function:\n{out}");
    assert!(out.contains("block0:"), "no entry block:\n{out}");
    assert!(
        out.contains("sadd_overflow") || out.contains("iadd"),
        "expected an integer add:\n{out}"
    );
    assert!(out.contains("return"), "expected a return:\n{out}");
}

#[test]
fn emit_clif_is_deterministic() {
    let src = "struct P { x: i64 }\n\
               function main(): i64 { var p = P { x: 7 }; p.x }";
    let a = emit_ir("clif", src).0;
    let b = emit_ir("clif", src).0;
    assert_eq!(a, b, "emit clif must be byte-for-byte deterministic");
}

/// Run `otter_fusion expand <file>` with `src` in a temp file, returning the
/// rendered source on stdout (and stderr / success).
fn expand_src(src: &str) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_expand_{}.otter", nonce()));
    std::fs::write(&path, src).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    command.arg("expand").arg(&path);
    let out = match output_with_timeout(&mut command, cli_test_timeout()) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            return (String::new(), err, false);
        }
    };
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn expand_renders_normalized_source() {
    let (out, err, ok) = expand_src("function   main(  ) {var x=1+2;println(x as str)}");
    assert!(ok, "stderr: {err}");
    // Normalized: canonical spacing/indentation, one statement per line.
    assert!(out.contains("function main() {"), "got:\n{out}");
    assert!(out.contains("var x = 1 + 2;"), "got:\n{out}");
}

#[test]
fn expand_is_idempotent() {
    // Printing the printer's own output must be a fixed point.
    let src = "struct P{x:i64,y:i64}\n\
               function main():i64{var p=P{x:1,y:2};if p.x>0{p.x}else{p.y}}";
    let first = expand_src(src).0;
    let second = expand_src(&first).0;
    assert_eq!(first, second, "expand is not idempotent");
}

#[test]
fn expand_output_reparses_and_runs() {
    // The rendered source is real source: it compiles and runs identically.
    let src = "function main(){var xs=[1,2,3];var t=0;for n in xs{t=t+n;};println(t as str)}";
    let expanded = expand_src(src).0;
    let (out, err, ok) = lang("run", &expanded);
    assert!(
        ok,
        "expanded source failed to run; stderr: {err}\n--- expanded ---\n{expanded}"
    );
    assert_eq!(out, "6\n");
}

#[test]
fn hello_world() {
    let (out, err, ok) = lang("run", "function main() { println(\"hello, world\") }");
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hello, world\n");
}

#[test]
fn print_no_newline() {
    let (out, _, ok) = lang("run", "function main() { print(\"a\"); print(\"b\") }");
    assert!(ok);
    assert_eq!(out, "ab");
}

#[test]
fn print_computed_number() {
    let src = "function main() { println(\"answer: \" + (fib(10) as str)) }\n\
               function fib(n: i64): i64 { if n < 2 { n } else { fib(n-1) + fib(n-2) } }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "answer: 55\n");
}

#[test]
fn structs_end_to_end() {
    let src = "struct Point { x: i64, y: i64 }\n\
               function main() {\n\
                 var p = Point { x: 40, y: 2 };\n\
                 println((p.x + p.y) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn divide_by_zero_panics() {
    // A non-constant zero divisor traps the runtime panic path.
    let src = "function main() { var d: i64 = 0; var x: i64 = 10 / d; println(x as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok, "expected a panic; stdout={out}");
    assert!(err.contains("divide by zero"), "stderr: {err}");
}

#[test]
fn error_propagation_with_question_mark() {
    let src = "struct User { name: str }\n\
               function find(id: i64): User | str { if id == 1 { User { name: \"A\" } } else { \"nf\" } }\n\
               function greet(id: i64): str { var u: User = find(id)?; \"hi \" + u.name }\n\
               function main() { println(greet(1)); println(greet(2)); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hi A\nnf\n");
}

#[test]
fn gc_collects_garbage_keeping_live_roots() {
    // With collection forced on every allocation (`OTTER_FUSION_GC=stress`), the live
    // `keep` string and `b` struct must survive the loop's garbage allocations.
    // A missed root would free them and corrupt the output.
    let src = "struct Box { v: i64 }\n\
               function main() {\n\
                 var keep = \"important data\";\n\
                 var b = Box { v: 100 };\n\
                 var total: i64 = 0;\n\
                 var i: i64 = 0;\n\
                 while i < 300 {\n\
                   var garbage = [i, i, i];\n\
                   var s = \"tmp\" + (i as str);\n\
                   total = total + garbage[1] + b.v;\n\
                   i = i + 1;\n\
                 }\n\
                 println(keep);\n\
                 println(total as str);\n\
                 println(b.v as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // total = sum over i in 0..300 of (i + 100) = (0+1+...+299) + 300*100
    let expected_total: i64 = (0..300).sum::<i64>() + 300 * 100;
    assert_eq!(out, format!("important data\n{expected_total}\n100\n"));
}

#[test]
fn map_survives_gc_stress() {
    // A `Map<str, str>` whose keys and values are heap strings must keep all of
    // its entries alive across the loop's garbage allocations under stress GC.
    let src = "function main() {\n\
                 var m: Map<str, str> = { \"name\": \"ada\", \"lang\": \"rust\" };\n\
                 var i: i64 = 0;\n\
                 while i < 200 {\n\
                   var garbage = \"junk\" + (i as str);\n\
                   m.set(\"iter\", garbage);\n\
                   i = i + 1;\n\
                 }\n\
                 match m.get(\"name\") { str s => println(s), null => println(\"lost\") };\n\
                 match m.get(\"lang\") { str s => println(s), null => println(\"lost\") };\n\
                 println(\"size=${m.size()}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // name/lang preserved; size = 3 (name, lang, iter).
    assert_eq!(out, "ada\nrust\nsize=3\n");
}

#[test]
fn for_over_user_iterator_under_gc_stress() {
    // The `Iterator` protocol `for` loop allocates an `Item`/union box every
    // step; under stress GC, the loop variable and the iterator must stay sound.
    let src = "struct Range { current: i64, end: i64 }\n\
               extend Range: Iterator<i64> {\n\
                 function next(self): Item<i64> | Done {\n\
                   if self.current >= self.end { Done {} }\n\
                   else { var v = self.current; self.current = self.current + 1; Item { value: v } }\n\
                 }\n\
               }\n\
               function main() {\n\
                 var acc = \"\";\n\
                 for x in (Range { current: 0, end: 5 }) {\n\
                   acc = acc + \"${x},\";\n\
                 }\n\
                 println(acc);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "0,1,2,3,4,\n");
}

#[test]
fn dyn_dispatch_heterogeneous_under_gc_stress() {
    // Interface objects box `{vtable, data}`; the GC must trace each data
    // pointer (here a struct with a heap `str` field) through the box.
    let src = "interface Show { function show(self): str; }\n\
               struct Named { name: str }\n\
               struct Anon {}\n\
               extend Named: Show { function show(self): str { self.name } }\n\
               extend Anon: Show { function show(self): str { \"anon\" } }\n\
               function main() {\n\
                 var zoo: List<Show> = [Named { name: \"ada\" }, Anon {}, Named { name: \"bob\" }];\n\
                 var out = \"\";\n\
                 for s in zoo { out = out + s.show() + \",\"; }\n\
                 println(out);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "ada,anon,bob,\n");
}

#[test]
fn interface_local_devirtualization_matches_native() {
    // Backend optimization: a straight-line interface local with a known
    // concrete source can direct-call the impl instead of going through the
    // vtable. JIT and native must agree, including after a later known
    // concrete reassignment.
    let src = "interface Shape { function area(self): i64; }\n\
               struct Rect { w: i64 }\n\
               struct Circle { r: i64 }\n\
               extend Rect: Shape { function area(self): i64 { self.w } }\n\
               extend Circle: Shape { function area(self): i64 { self.r + 1 } }\n\
               function main() {\n\
                 var s: Shape = Rect { w: 1 } as Shape;\n\
                 s = Circle { r: 41 } as Shape;\n\
                 println(\"area=${s.area()}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "area=42\n");
}

#[test]
fn interface_if_initializer_devirtualization_matches_native() {
    // Backend optimization: when both branches of an interface-valued `if`
    // initializer produce the same concrete implementor, later local method
    // calls can direct-call that implementation.
    let src = "interface Shape { function area(self): i64; }\n\
               struct Rect { w: i64 }\n\
               struct Circle { r: i64 }\n\
               extend Rect: Shape { function area(self): i64 { self.w } }\n\
               extend Circle: Shape { function area(self): i64 { self.r } }\n\
               function choose(flag: bool): i64 {\n\
                 var s: Shape = if flag { Rect { w: 42 } as Shape } else { Rect { w: 7 } as Shape };\n\
                 s.area()\n\
               }\n\
               function main() { println(\"areas=${choose(true) + choose(false)}\"); }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "areas=49\n");
}

#[test]
fn interface_match_initializer_devirtualization_matches_native() {
    // Backend optimization: when every match arm produces the same concrete
    // implementor for an interface-valued initializer, later local calls can
    // direct-call that implementation.
    let src = "interface Shape { function area(self): i64; }\n\
               struct Rect { w: i64 }\n\
               struct Circle { r: i64 }\n\
               extend Rect: Shape { function area(self): i64 { self.w } }\n\
               extend Circle: Shape { function area(self): i64 { self.r } }\n\
               function choose(n: i64): i64 {\n\
                 var s: Shape = match n {\n\
                   0 => Rect { w: 40 } as Shape,\n\
                   1 => Rect { w: 1 } as Shape,\n\
                   _ => Rect { w: 2 } as Shape,\n\
                 };\n\
                 s.area()\n\
               }\n\
               function main() { println(\"areas=${choose(0) + choose(1) + choose(2)}\"); }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "areas=43\n");
}

#[test]
fn scalar_helper_inlining_matches_native() {
    // Backend optimization: a single-call scalar expression helper can inline
    // into its caller. The observable result must stay identical in JIT/native.
    let src = "function mix(a: i64, b: i64): i64 { ((a & b) ^ (a | b)) ^ (a ^ b) }\n\
               function main() { println(\"mix=${mix(8, 3)}\"); }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "mix=0\n");
}

#[test]
fn scalar_let_helper_inlining_matches_native() {
    // Backend optimization: a single-call scalar helper with simple let-bound
    // temporaries can inline into its caller.
    let src = "function adjust(a: i64, b: i64): i64 {\n\
                 var both = a & b;\n\
                 var either = a | b;\n\
                 (both ^ either) ^ (a ^ b)\n\
               }\n\
               function main() { println(\"adjust=${adjust(8, 3)}\"); }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "adjust=0\n");
}

#[test]
fn stack_allocated_scalar_struct_local_matches_native() {
    // Backend optimization: a non-escaping final struct local with scalar fields
    // can use a stack field block. JIT and native must preserve the same result.
    let src = "struct P { x: i64, y: i64 }\n\
               function main() {\n\
                 var p = P { x: 40, y: 2 };\n\
                 p.x = p.x + p.y;\n\
                 println(\"point=${p.x}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "point=42\n");
}

#[test]
fn stack_allocated_scalar_tuple_struct_local_matches_native() {
    // Backend optimization: a non-escaping scalar tuple-struct local can use a
    // stack field block just like a record struct local.
    let src = "struct Pair(i64, i64)\n\
               function main() {\n\
                 var p = Pair(40, 2);\n\
                 p.0 = p.0 + p.1;\n\
                 println(\"pair=${p.0}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "pair=42\n");
}

#[test]
fn empty_struct_tag_thinning_matches_native() {
    // Backend optimization: empty final structs use a null payload sentinel.
    // Union/interface wrappers still carry the semantic tag/vtable.
    let src = "interface Named { function value(self): i64; }\n\
               struct Empty {}\n\
               extend Empty: Named { function value(self): i64 { 41 } }\n\
               function main() {\n\
                 var x: Empty | null = Empty {};\n\
                 var n: Named = Empty {} as Named;\n\
                 if x is Empty { println(\"empty=${n.value() + 1}\"); }\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "empty=42\n");
}

#[test]
fn union_if_tag_fast_path_matches_native() {
    // Backend optimization: a union-valued `if` that is immediately tested or
    // narrowed can lower branch-by-branch instead of first constructing a
    // temporary union box. A union that escapes into a local still uses the
    // ordinary boxed representation.
    let src = "function immediate(flag: bool): i64 {\n\
                 if (if flag { 40 } else { null }) is i64 { 20 } else { 1 }\n\
               }\n\
               function narrowed(flag: bool): i64 {\n\
                 ((if flag { 20 } else { null }) as i64) + 2\n\
               }\n\
               function boxed(flag: bool): i64 {\n\
                 var x = if flag { 40 } else { null };\n\
                 if x is i64 { 20 } else { 1 }\n\
               }\n\
               function main() {\n\
                 println(\"union-fast=${immediate(true) + immediate(false)} ${narrowed(true)} ${boxed(true) + boxed(false)}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "union-fast=21 22 21\n");
}

#[test]
fn union_match_tag_fast_path_matches_native() {
    // Backend optimization: a union-valued `match` follows the same rule as
    // `if`: immediate `is`/concrete `as` consumers can avoid a temporary box,
    // while values that escape to locals keep the boxed union representation.
    let src = "function immediate(n: i64): i64 {\n\
                 if (match n { 0 => 40, _ => null }) is i64 { 20 } else { 1 }\n\
               }\n\
               function narrowed(n: i64): i64 {\n\
                 ((match n { 0 => 20, _ => null }) as i64) + 2\n\
               }\n\
               function boxed(n: i64): i64 {\n\
                 var x = match n { 0 => 40, _ => null };\n\
                 if x is i64 { 20 } else { 1 }\n\
               }\n\
               function main() {\n\
                 println(\"match-fast=${immediate(0) + immediate(1)} ${narrowed(0)} ${boxed(0) + boxed(1)}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "match-fast=21 22 21\n");
}

#[test]
fn interface_branch_tag_fast_path_matches_native() {
    // Backend optimization: an interface-valued `if`/`match` immediately tested
    // or downcasted can skip the temporary `{vtable,data,type_id}` wrapper.
    // Interface values that escape into locals still keep that representation.
    let src = "interface Shape { function area(self): i64; }\n\
               struct Rect { w: i64 }\n\
               struct Circle { r: i64 }\n\
               extend Rect: Shape { function area(self): i64 { self.w } }\n\
               extend Circle: Shape { function area(self): i64 { self.r } }\n\
               function immediate_if(flag: bool): i64 {\n\
                 if (if flag { Rect { w: 40 } as Shape } else { Circle { r: 1 } as Shape }) is Rect { 20 } else { 1 }\n\
               }\n\
               function immediate_match(n: i64): i64 {\n\
                 if (match n { 0 => Rect { w: 40 } as Shape, _ => Circle { r: 1 } as Shape }) is Rect { 20 } else { 1 }\n\
               }\n\
               function immediate_direct(): i64 {\n\
                 if (Rect { w: 40 } as Shape) is Rect { 20 } else { 1 }\n\
               }\n\
               function narrowed(): i64 {\n\
                 ((if true { Rect { w: 20 } as Shape } else { Circle { r: 1 } as Shape }) as Rect).w + 2\n\
               }\n\
               function narrowed_match(): i64 {\n\
                 ((match 0 { 0 => Rect { w: 20 } as Shape, _ => Circle { r: 1 } as Shape }) as Rect).w + 3\n\
               }\n\
               function narrowed_direct(): i64 {\n\
                 ((Rect { w: 20 } as Shape) as Rect).w + 4\n\
               }\n\
               function boxed(flag: bool): i64 {\n\
                 var s: Shape = if flag { Rect { w: 40 } as Shape } else { Circle { r: 1 } as Shape };\n\
                 if s is Rect { 20 } else { 1 }\n\
               }\n\
               function main() {\n\
                 println(\"iface-fast=${immediate_if(true) + immediate_if(false)} ${immediate_match(0) + immediate_match(1)} ${immediate_direct()} ${narrowed()} ${narrowed_match()} ${narrowed_direct()} ${boxed(true) + boxed(false)}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "iface-fast=21 21 20 22 23 24 21\n");
}

#[test]
fn interface_branch_receiver_devirtualization_matches_native() {
    // Backend optimization: an interface-valued `if`/`match` used immediately
    // as a method receiver can direct-call each branch's concrete impl instead
    // of building a wrapper and issuing a vtable call.
    let src = "interface Shape { function area(self): i64; }\n\
               struct Rect { w: i64 }\n\
               struct Circle { r: i64 }\n\
               extend Rect: Shape { function area(self): i64 { self.w } }\n\
               extend Circle: Shape { function area(self): i64 { self.r + 1 } }\n\
               function immediate_if(flag: bool): i64 {\n\
                 (if flag { Rect { w: 40 } as Shape } else { Circle { r: 1 } as Shape }).area()\n\
               }\n\
               function immediate_match(n: i64): i64 {\n\
                 (match n { 0 => Rect { w: 40 } as Shape, _ => Circle { r: 1 } as Shape }).area()\n\
               }\n\
               function boxed(flag: bool): i64 {\n\
                 var s: Shape = if flag { Rect { w: 40 } as Shape } else { Circle { r: 1 } as Shape };\n\
                 s.area()\n\
               }\n\
               function main() {\n\
                 println(\"iface-recv=${immediate_if(true) + immediate_if(false)} ${immediate_match(0) + immediate_match(1)} ${boxed(true) + boxed(false)}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (native, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, native);
    assert_eq!(jit, "iface-recv=42 42 42\n");
}

#[test]
fn check_rejects_unsatisfied_bound() {
    let src = "interface Show { function show(self): str; }\n\
               struct Cat {}\n\
               function speak<T: Show>(x: T): str { x.show() }\n\
               function main() { var s = speak(Cat {}); }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("does not implement `Show`"), "stderr: {err}");
}

#[test]
fn closures_capture_survive_gc_stress() {
    // A closure's heap environment (here capturing a `str`) and the closures
    // held in a list must survive stress-mode collection.
    let src = "function tagger(tag: str): (str) => str {\n\
                 (s: str): str => tag + s\n\
               }\n\
               function main() {\n\
                 var fns: List<(str) => str> = [tagger(\"a:\"), tagger(\"b:\")];\n\
                 var out = \"\";\n\
                 var i = 0;\n\
                 while i < 100 { var junk = \"x\" + (i as str); i = i + 1; }\n\
                 for f in fns { out = out + f(\"v\") + \" \"; }\n\
                 println(out);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "a:v b:v \n");
}

#[test]
fn for_entry_in_map_under_gc_stress() {
    // `for entry in map` builds an `Entry<str,str>` (two heap fields) each step;
    // the GC must keep the map, the key snapshot, and each entry sound.
    let src = "function main() {\n\
                 var m: Map<str, str> = { \"a\": \"x\", \"b\": \"y\" };\n\
                 m[\"c\"] = \"z\";\n\
                 var out = \"\";\n\
                 for e in m { out = out + e.key + e.value; }\n\
                 println(out);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // probe order is deterministic for these keys
    assert_eq!(out, "czaxby\n");
}

#[test]
fn check_rejects_non_iterable() {
    let src = "struct P { x: i64 }\n\
               function f() { for n in (P { x: 1 }) { } }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("is not iterable"), "stderr: {err}");
}

#[test]
fn check_rejects_non_hashable_map_key() {
    // A user struct that does NOT derive `Eq`/`Hash` cannot be a map key
    // (`docs/15` §7). `bool` is a primitive and *is* a valid key.
    let src = "struct K { x: i64 }\n\
               function f() {\n\
                 var m: Map<K, i64> = Map<K, i64>();\n\
                 m.set(K { x: 1 }, 1);\n\
               }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("cannot be used as a map key"), "stderr: {err}");
}

#[test]
fn check_reports_type_error() {
    let (_, err, ok) = lang("check", "function f(): i64 { true }");
    assert!(!ok);
    assert!(err.contains("expected `i64`"), "stderr: {err}");
}

#[test]
fn native_build_hello_world() {
    // `lang build` emits a relocatable object, links it against the runtime
    // static library, and the resulting standalone binary prints on its own.
    let (out, err, ok) = lang_build_run(
        "function main() { println(\"hello from a native binary\") }",
        &[],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hello from a native binary\n");
}

#[test]
fn native_build_matches_jit_output() {
    // A program touching strings, structs, and recursion must produce the same
    // output whether JIT-run or compiled to a native executable.
    let src = "struct Point { x: i64, y: i64 }\n\
               function fib(n: i64): i64 { if n < 2 { n } else { fib(n-1) + fib(n-2) } }\n\
               function main() {\n\
                 var p = Point { x: 40, y: 2 };\n\
                 println(\"sum=${p.x + p.y} fib=${fib(15)}\");\n\
               }";
    let (jit_out, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit_out, nat_out);
    assert_eq!(nat_out, "sum=42 fib=610\n");
}

#[test]
fn native_build_variadic_ffi_matches_jit() {
    // A `@Variadic` extern (`docs/19` §13) is lowered through `libffi`, never as
    // a plain Cranelift call. The marshalled int/string/double/char arguments —
    // with the C default promotions applied — must produce identical output in
    // the JIT and in the linked native executable (which links `-lffi`).
    let src = "@Variadic\n\
               extern function snprintf(buf: *u8, size: u64, fmt: *u8): i32;\n\
               function main() {\n\
                 var b = Buffer.alloc(128u64) as Buffer;\n\
                 var fmt = CString.from_str(\"%d %s %.2f %c neg=%d f32=%.1f\");\n\
                 var s = CString.from_str(\"hi\");\n\
                 var x: f32 = 7.5f32;\n\
                 var n = snprintf(b.data, 128u64, fmt.as_ptr(), 42i32, s.as_ptr(), 3.14159f64, 65i32, -9i32, x);\n\
                 println(\"n=${n} [${CStr.from_ptr(b.data).to_str()}]\");\n\
                 b.free();\n\
               }";
    let (jit_out, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit_out, nat_out, "JIT/native variadic output diverged");
    assert_eq!(nat_out, "n=27 [42 hi 3.14 A neg=-9 f32=7.5]\n");
}

#[test]
fn native_build_gc_stress_keeps_live_roots() {
    // The native entry registers each function's GC safepoints at startup
    // (function address + code offset → precise pc). Under stress collection a
    // missed root would free the live data and corrupt the output, exactly as
    // in the JIT path — this proves precise stack maps work in the linked binary.
    let src = "struct Box { v: i64 }\n\
               function main() {\n\
                 var keep = \"important data\";\n\
                 var b = Box { v: 100 };\n\
                 var total: i64 = 0;\n\
                 var i: i64 = 0;\n\
                 while i < 300 {\n\
                   var garbage = [i, i, i];\n\
                   var s = \"tmp\" + (i as str);\n\
                   total = total + garbage[1] + b.v;\n\
                   i = i + 1;\n\
                 }\n\
                 println(keep);\n\
                 println(total as str);\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    let expected_total: i64 = (0..300).sum::<i64>() + 300 * 100;
    assert_eq!(out, format!("important data\n{expected_total}\n"));
}

#[test]
fn native_build_async_thread_spawn_matches_jit() {
    // An async `Thread.spawn` worker (`() => Future<R>`) drives its future to
    // completion on its own OS thread, awaiting and locking a `Shared<T>`
    // (docs/20 §1/§4). The handle joins on the awaited `R`. The result must be
    // identical whether JIT-run or compiled to a native executable.
    let src = "struct C { value: i64 }\n\
               function value_of(r: Joined<i64> | Panicked): i64 {\n\
                 match r { Joined<i64> j => j.value, Panicked p => -1 }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var state: Shared<C> = Shared.new(C { value: 0 });\n\
                 var s: Shared<C> = state.clone();\n\
                 var h: JoinHandle<i64> = Thread.spawn(() async => {\n\
                   var i: i64 = 0;\n\
                   while i < 50 {\n\
                     await s.lock((c) => { c.value = c.value + 1; 0 });\n\
                     i = i + 1;\n\
                   }\n\
                   await s.lock((c) => c.value)\n\
                 });\n\
                 var r: Joined<i64> | Panicked = await h.join();\n\
                 var total: i64 = await state.lock((c) => c.value);\n\
                 println(\"worker=${value_of(r)} total=${total}\");\n\
               }";
    let (jit_out, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit_out, nat_out);
    assert_eq!(nat_out, "worker=50 total=50\n");
}

#[test]
fn native_build_thread_spawn_detach_gc_many_live_lists_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/thread_spawn_detach_gc_many_live_lists.otter"
    );
    let env = &[("OTTER_FUSION_GC", "stress")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native detached GC-stress Thread.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "count=64 total=6240 closed=1\n");
}

#[test]
fn native_build_panic_exits_101() {
    // A runtime panic (divide by zero) in a native binary must exit with 101,
    // the same code the runtime uses under the JIT.
    let src = "function main() { var d: i64 = 0; var x: i64 = 10 / d; println(x as str); }";
    let (_, err, ok) = lang_build_run(src, &[]);
    assert!(!ok);
    assert!(err.contains("divide by zero"), "stderr: {err}");
}

#[test]
fn integer_overflow_panics() {
    // `docs/14` §2: integer overflow panics in debug builds. `i64::MAX + 1`
    // must panic with an overflow message, not silently wrap.
    let src = "function main() {\n\
                 var x: i64 = 9223372036854775807;\n\
                 var y: i64 = x + 1;\n\
                 println(y as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok, "expected an overflow panic; stdout={out}");
    assert!(err.contains("add with overflow"), "stderr: {err}");
}

#[test]
fn multiplication_overflow_panics() {
    let src = "function main() {\n\
                 var x: i64 = 4000000000;\n\
                 var y: i64 = x * x;\n\
                 println(y as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("multiply with overflow"), "stderr: {err}");
}

#[test]
fn signed_division_overflow_panics() {
    // `INT_MIN / -1` is the one signed-division overflow; it must panic, not
    // raise a hardware trap (`docs/14` §2).
    let src = "function main() {\n\
                 var a: i64 = -9223372036854775807 - 1;\n\
                 var b: i64 = -1;\n\
                 println((a / b) as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("divide with overflow"), "stderr: {err}");
}

#[test]
fn shift_past_width_panics() {
    // `docs/14` §2: a shift by `>=` the bit width always panics.
    let src = "function main() {\n\
                 var w: i64 = 64;\n\
                 var x: i64 = 1;\n\
                 var y: i64 = x << w;\n\
                 println(y as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("shift amount"), "stderr: {err}");
}

#[test]
fn valid_arithmetic_does_not_false_panic() {
    // Large-but-in-range arithmetic and shifts must compute, not panic.
    let src = "function main() {\n\
                 var a: i64 = 1000000;\n\
                 println((a * a) as str);\n\
                 println((1 << 10) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "1000000000000\n1024\n");
}

#[test]
fn panic_builtin_terminates_with_message() {
    // `docs/14`: `panic(message: str): never`. Code before the panic runs;
    // code after is unreachable; the process exits 101 with the message.
    let src = "function check(n: i64): i64 { if n < 0 { panic(\"neg: \" + (n as str)) } n }\n\
               function main() {\n\
                 println(check(5) as str);\n\
                 println(check(-2) as str);\n\
                 println(\"unreachable\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok);
    assert_eq!(out, "5\n");
    assert!(err.contains("neg: -2"), "stderr: {err}");
}

#[test]
fn panic_is_usable_in_value_position() {
    // `never` is a subtype of every type, so `panic(...)` type-checks as the
    // else-arm of an `i64`-typed `if`.
    let src = "function pos(n: i64): i64 {\n\
                 var x: i64 = if n >= 0 { n } else { panic(\"neg\") };\n\
                 x + 1\n\
               }\n\
               function main() { println(pos(41) as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn exit_builtin_sets_process_code() {
    // `docs/24`: `exit(code: i32): never`. Output before runs; the exit code
    // is the argument.
    let src = "function main() { println(\"before\"); exit(3i32); println(\"after\"); }";
    let (out, _, ok) = lang("run", src);
    assert!(!ok);
    assert_eq!(out, "before\n");
}

#[test]
fn float_to_int_in_range_truncates() {
    // `docs/14` §2: in-range float→int truncates toward zero, no panic.
    let src = "function main() {\n\
                 var f: f64 = 3.9;\n\
                 println((f as i64) as str);\n\
                 var g: f64 = -2.7;\n\
                 println((g as i64) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3\n-2\n");
}

#[test]
fn float_to_int_out_of_range_panics() {
    // A float beyond i32's range must panic, not silently wrap (`docs/14` §2).
    let src = "function main() {\n\
                 var f: f64 = 5000000000.0;\n\
                 println((f as i32) as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok, "expected an out-of-range panic");
    assert!(err.contains("out of range"), "stderr: {err}");
}

#[test]
fn float_nan_to_int_panics() {
    // NaN (0.0/0.0, floats never panic on division) panics when cast to int.
    let src = "function main() {\n\
                 var z: f64 = 0.0;\n\
                 var nan: f64 = z / z;\n\
                 println((nan as i64) as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok, "expected a NaN cast panic");
    assert!(
        err.contains("out of range") || err.contains("NaN"),
        "stderr: {err}"
    );
}

#[test]
fn int_to_char_valid_and_invalid() {
    // A valid scalar casts; an out-of-range code point panics (`docs/14` §2).
    let ok_src = "function main() {\n\
                    var n: i64 = 65;\n\
                    println((n as char) as str);\n\
                  }";
    let (out, err, ok) = lang("run", ok_src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "A\n");

    let bad_src = "function main() {\n\
                     var n: i64 = 1114112;\n\
                     var c: char = n as char;\n\
                     println(c as str);\n\
                   }";
    let (_, err, ok) = lang("run", bad_src);
    assert!(!ok, "expected an invalid-char panic");
    assert!(err.contains("Unicode"), "stderr: {err}");
}

#[test]
fn int_to_char_surrogate_panics() {
    // Surrogate code points (0xD800..=0xDFFF) are not valid scalars.
    let src = "function main() {\n\
                 var n: i64 = 55296;\n\
                 var c: char = n as char;\n\
                 println(c as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok, "expected a surrogate panic");
    assert!(err.contains("surrogate"), "stderr: {err}");
}

#[test]
fn panic_with_terminates() {
    // `docs/14` §1: `panic_with(value): never` terminates the thread; the value
    // can be any type (widened to `dynamic`); code after is unreachable.
    let src = "struct Bug { code: i64 }\n\
               function main() {\n\
                 println(\"before\");\n\
                 panic_with(Bug { code: 7 });\n\
                 println(\"after\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok, "expected panic_with to terminate");
    assert_eq!(out, "before\n");
    assert!(err.contains("panic"), "stderr: {err}");
}

#[test]
fn release_profile_wraps_overflow() {
    // `docs/14` §5: in release, overflowing `+`/`-`/`*` wrap (two's complement)
    // instead of panicking. `i32::MAX + 1` wraps to `i32::MIN`.
    let src = "function main() {\n\
                 var a: i32 = 2147483647;\n\
                 println((a + 1) as str);\n\
               }";
    let (out, err, ok) = lang_flag("run", src, &["--release"]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "-2147483648\n");

    // The same program panics under the (default) debug profile.
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("overflow"), "stderr: {err}");
}

#[test]
fn release_profile_wraps_signed_div_overflow() {
    // `INT_MIN / -1` wraps to `INT_MIN` and `INT_MIN % -1` to `0` in release.
    let src = "function main() {\n\
                 var a: i64 = -9223372036854775807 - 1;\n\
                 var d: i64 = -1;\n\
                 println((a / d) as str);\n\
                 println((a % d) as str);\n\
               }";
    let (out, err, ok) = lang_flag("run", src, &["--release"]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "-9223372036854775808\n0\n");
}

#[test]
fn release_profile_still_panics_on_bad_shift() {
    // Shifts past the bit width panic in *both* profiles (`docs/14` §5).
    let src = "function main() {\n\
                 var a: i32 = 1;\n\
                 var s: i32 = 40;\n\
                 println((a << s) as str);\n\
               }";
    let (_, err, ok) = lang_flag("run", src, &["--release"]);
    assert!(!ok, "expected a shift panic even in release");
    assert!(err.contains("shift"), "stderr: {err}");
}

#[test]
fn numeric_namespace_intrinsics() {
    // `docs/18` §10, `docs/14` §5: constants and overflow-arithmetic families on
    // primitive numeric types, plus float predicates.
    let src = "function chk(r: i32 | null): str { match r { i32 v => v as str, null => \"of\" } }\n\
               function main() {\n\
                 var m: i32 = 2147483647;\n\
                 println(i32.MAX as str);\n\
                 println(i32.MIN as str);\n\
                 println(u8.MAX as str);\n\
                 println(i32.wrapping_add(m, 1) as str);\n\
                 println(i32.saturating_add(m, 9) as str);\n\
                 println(chk(i32.checked_add(m, 1)));\n\
                 println(chk(i32.checked_add(1, 2)));\n\
                 var z: f64 = 0.0;\n\
                 println(f64.is_nan(z / z) as str);\n\
                 println(f64.is_finite(f64.INFINITY) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(
        out,
        "2147483647\n-2147483648\n255\n-2147483648\n2147483647\nof\n3\ntrue\nfalse\n"
    );
}

#[test]
fn numeric_div_rem_families() {
    // `docs/14` §5: the {wrapping,saturating,checked,overflowing}_{div,rem}
    // families on signed `i32`. The only real overflow is `INT_MIN / -1`
    // (resp. `INT_MIN % -1` → 0).
    let src = "function chk(r: i32 | null): str { match r { i32 v => v as str, null => \"of\" } }\n\
               function main() {\n\
                 var neg1: i32 = (0 - 1) as i32;\n\
                 println(i32.wrapping_div(i32.MIN, neg1) as str);\n\
                 println(i32.wrapping_rem(i32.MIN, neg1) as str);\n\
                 println(i32.saturating_div(i32.MIN, neg1) as str);\n\
                 println(chk(i32.checked_div(10 as i32, 0 as i32)));\n\
                 println(chk(i32.checked_div(i32.MIN, neg1)));\n\
                 println(chk(i32.checked_div(15 as i32, 4 as i32)));\n\
                 var od = i32.overflowing_div(i32.MIN, neg1);\n\
                 println(\"${od.0} ${od.1}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(
        out,
        "-2147483648\n0\n2147483647\nof\nof\n3\n-2147483648 true\n"
    );
}

#[test]
fn numeric_div_by_zero_panics_except_checked() {
    // `wrapping_div(_, 0)` panics like the `/` operator does; `checked_div`
    // folds the divide-by-zero into the null branch.
    let src = "function main() {\n\
                 var z: i32 = 0 as i32;\n\
                 println(i32.wrapping_div(10 as i32, z) as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok, "expected a panic");
    assert!(err.contains("divide by zero"), "stderr: {err}");
}

#[test]
fn numeric_neg_family() {
    // `docs/14` §5: `_neg` overflows on signed `INT_MIN` (wraps to `INT_MIN`,
    // saturates to `INT_MAX`, returns null in `checked`, flags overflow in
    // `overflowing`). Unsigned `neg(0) = 0` is the only non-overflowing case.
    let src = "function chk(r: i32 | null): str { match r { i32 v => v as str, null => \"of\" } }\n\
               function main() {\n\
                 println(i32.wrapping_neg(i32.MIN) as str);\n\
                 println(i32.saturating_neg(i32.MIN) as str);\n\
                 println(chk(i32.checked_neg(i32.MIN)));\n\
                 println(chk(i32.checked_neg(7 as i32)));\n\
                 var on = i32.overflowing_neg(i32.MIN);\n\
                 println(\"${on.0} ${on.1}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "-2147483648\n2147483647\nof\n-7\n-2147483648 true\n");
}

#[test]
fn numeric_shift_family() {
    // `docs/14` §5: `_shl` / `_shr` take a `u32` shift count. Overflow is
    // `count >= BITS`. Saturating shift saturates the *count* to `BITS - 1`.
    let src = "function chk(r: i32 | null): str { match r { i32 v => v as str, null => \"of\" } }\n\
               function main() {\n\
                 println(i32.wrapping_shl(1 as i32, 5u32) as str);\n\
                 println(chk(i32.checked_shl(1 as i32, 31u32)));\n\
                 println(chk(i32.checked_shl(1 as i32, 64u32)));\n\
                 var os = i32.overflowing_shl(1 as i32, 64u32);\n\
                 println(\"${os.0} ${os.1}\");\n\
                 println(i32.saturating_shr(8 as i32, 2u32) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "32\n-2147483648\nof\n1 true\n2\n");
}

#[test]
fn numeric_extended_intrinsics_native_build() {
    // JIT/native parity for the new ops.
    let src = "function main() {\n\
                 println(i32.wrapping_neg(i32.MIN) as str);\n\
                 println(i32.wrapping_shl(1 as i32, 4u32) as str);\n\
                 var od = i32.overflowing_div(i32.MIN, (0 - 1) as i32);\n\
                 println(\"${od.0} ${od.1}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "-2147483648\n16\n-2147483648 true\n");
}

#[test]
fn drop_finalizer_runs_on_collection() {
    // `docs/16` §8: a `Drop` impl's `drop(self)` runs when the collector
    // reclaims an unreachable object. Under stress GC each loop temporary is
    // finalized promptly; a live value is not.
    let src = "struct Resource { id: i64 }\n\
               extend Resource: Drop {\n\
                 function drop(self) { println(\"drop \" + (self.id as str)); }\n\
               }\n\
               function main() {\n\
                 var i: i64 = 0;\n\
                 while i < 3 { var r: Resource = Resource { id: i }; i = i + 1; }\n\
                 println(\"done\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // All three temporaries are finalized; order is deterministic single-thread.
    assert!(out.contains("drop 0"), "out: {out}");
    assert!(out.contains("drop 1"), "out: {out}");
    assert!(out.contains("drop 2"), "out: {out}");
    assert!(out.contains("done"), "out: {out}");
}

#[test]
fn shared_mutex_serializes_concurrent_increments() {
    // `docs/20` §4: `Shared<T>` is an ASYNC mutex. Two `spawn` workers each
    // increment a shared counter 5000 times under `await … lock`; the lock
    // serializes them so no update is lost. Lock-using workers must be async, so
    // each runs via the `spawn` keyword and is awaited as a `Future`.
    let src = "struct Counter { value: i64 }\n\
               function bump(s: Shared<Counter>, n: i64): Future<null> async {\n\
                 var i: i64 = 0;\n\
                 while i < n { await s.lock((c) => { c.value = c.value + 1; 0 }); i = i + 1; }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var state: Shared<Counter> = Shared.new(Counter { value: 0 });\n\
                 var a: Shared<Counter> = state.clone();\n\
                 var b: Shared<Counter> = state.clone();\n\
                 var h1: Future<null> = spawn bump(a, 5000);\n\
                 var h2: Future<null> = spawn bump(b, 5000);\n\
                 await h1;\n\
                 await h2;\n\
                 println((await state.lock((c) => c.value)) as str);\n\
               }";
    let (out, err, ok) = lang_raw_env_with_timeout("run", &pre(src), &[], Duration::from_secs(90));
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "10000\n");
}

#[test]
fn shared_try_lock_returns_value_or_lock_busy() {
    // `try_lock` is async and yields `R | LockBusy`; on an uncontended lock it
    // succeeds (after `await`).
    let src = "struct Box { v: i64 }\n\
               function main(): Future<null> async {\n\
                 var s: Shared<Box> = Shared.new(Box { v: 42 });\n\
                 match await s.try_lock((b) => b.v) {\n\
                   i64 n => println(\"got \" + (n as str)),\n\
                   LockBusy busy => println(\"busy\"),\n\
                 }\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "got 42\n");
}

#[test]
fn channel_iterator_terminates_on_last_sender_drop() {
    // `docs/20` §2 / `docs/16` §8 — the headline: a worker thread sends, then
    // its captured `Sender` is *deterministically released* when the worker
    // returns, closing the channel. The consumer's `for n in rx` (a synchronous
    // `Receiver: Iterator`) drains the queue and then **terminates** on close —
    // no GC collection required for the close to happen.
    let src = "function produce(tx: Sender<i64>) {\n\
                 var i: i64 = 1;\n\
                 while i <= 5 { tx.send(i * 10); i = i + 1; }\n\
               }\n\
               function consume(rx: Receiver<i64>): i64 {\n\
                 var total: i64 = 0;\n\
                 for n in rx { total = total + n; }\n\
                 total\n\
               }\n\
               function main() {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => { produce(tx); 0 });\n\
                 var total: i64 = consume(rx);\n\
                 println(total as str);\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out1, "150\n"); // 10+20+30+40+50, then Done
    let (out2, err2, ok2) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok2, "GC stress stderr: {err2}");
    assert_eq!(out1, out2, "GC stress changed the channel result");
}

#[test]
fn channel_recv_surfaces_channel_closed() {
    // `docs/20` §2: `recv()` resolves to `T | ChannelClosed`. After the worker
    // sends two values and drops its sender, an async consumer awaits recv in a
    // loop, matching the closed variant to stop. Drains *then* sees closed.
    let src = "function produce(tx: Sender<i64>) { tx.send(11); tx.send(22); }\n\
               function consume(rx: Receiver<i64>): Future<i64> async {\n\
                 var total: i64 = 0;\n\
                 var run: bool = true;\n\
                 while run {\n\
                   var m: i64 | ChannelClosed = await rx.recv();\n\
                   match m {\n\
                     i64 v => { total = total + v; },\n\
                     ChannelClosed c => { run = false; },\n\
                   }\n\
                 }\n\
                 total\n\
               }\n\
               function main(): Future<null> async {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => { produce(tx); 0 });\n\
                 var total: i64 = await consume(rx);\n\
                 println(total as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "33\n"); // 11 + 22, then ChannelClosed
}

#[test]
fn channel_multiple_senders_close_after_all_dropped() {
    // `docs/20` §2: a cloned `Sender` is another producer. The channel closes
    // only once *every* sender (the original captured into one worker, the
    // clone into another) has been released. The iterator drains all messages
    // from both workers before terminating.
    let src = "function produce(tx: Sender<i64>, base: i64) {\n\
                 var i: i64 = 0;\n\
                 while i < 3 { tx.send(base + i); i = i + 1; }\n\
               }\n\
               function main() {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 var tx2: Sender<i64> = tx.clone();\n\
                 var h1: JoinHandle<i64> = Thread.spawn(() => { produce(tx, 100); 0 });\n\
                 var h2: JoinHandle<i64> = Thread.spawn(() => { produce(tx2, 200); 0 });\n\
                 var count: i64 = 0;\n\
                 var total: i64 = 0;\n\
                 for n in rx { count = count + 1; total = total + n; }\n\
                 println(count as str);\n\
                 println(total as str);\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    // 6 messages total: 100,101,102 + 200,201,202 = 906.
    assert_eq!(out1, "6\n906\n");
    let (out2, _, ok2) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok2);
    assert_eq!(out1, out2, "GC stress changed the multi-sender result");
}

#[test]
fn channel_send_after_receiver_dropped_is_channel_closed() {
    // `docs/20` §2: when the receiver is released, the channel is closed for
    // sending; `send` returns `ChannelClosed`. The receiver is captured into a
    // worker that drops it immediately; once it has, the main sender observes
    // closed. We retry until the worker has released the receiver.
    let src = "function drain(rx: Receiver<i64>): i64 { 0 }\n\
               function main(): Future<null> async {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => { drain(rx); 0 });\n\
                 var r: Joined<i64> | Panicked = await h.join();\n\
                 var closed: bool = false;\n\
                 match tx.send(7) {\n\
                   null => { println(\"open\"); },\n\
                   ChannelClosed c => { closed = true; },\n\
                 }\n\
                 if closed { println(\"closed\"); }\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "closed\n");
}

#[test]
fn channel_recv_of_managed_element_survives_gc_stress() {
    // `docs/20` §2: a managed element (`str`) flows over the channel; the
    // `for s in rx` iterator drains it. The message rides the queue (pinned as
    // a GC root) and is unpinned into the result on recv — a collection in the
    // hand-off must not free it. The worker drops the sender → iterator ends.
    let src = "function produce(tx: Sender<str>) {\n\
                 tx.send(\"a\"); tx.send(\"b\"); tx.send(\"c\");\n\
               }\n\
               function consume(rx: Receiver<str>): str {\n\
                 var acc: str = \"\";\n\
                 for s in rx { acc = acc + s; }\n\
                 acc\n\
               }\n\
               function main() {\n\
                 var pair: (Sender<str>, Receiver<str>) = channel<str>();\n\
                 var tx: Sender<str> = pair.0;\n\
                 var rx: Receiver<str> = pair.1;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => { produce(tx); 0 });\n\
                 println(consume(rx));\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out1, "abc\n");
    let (out2, _, ok2) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok2);
    assert_eq!(out1, out2, "GC stress changed the channel result");
}

#[test]
fn channel_iterator_native_build_matches_jit() {
    // The deterministic close + `Receiver: Iterator` must behave identically
    // under `otter_fusion build` (native object + linked runtime) and the JIT.
    let src = "function produce(tx: Sender<i64>) {\n\
                 var i: i64 = 1; while i <= 4 { tx.send(i); i = i + 1; }\n\
               }\n\
               function main() {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => { produce(tx); 0 });\n\
                 var total: i64 = 0;\n\
                 for n in rx { total = total + n; }\n\
                 println(total as str);\n\
               }";
    let (jit_out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(jit_out, "10\n"); // 1+2+3+4
    let (native_out, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit_out, native_out, "native build diverged from JIT");
}

#[test]
fn channel_try_recv_is_non_blocking() {
    // `try_recv` returns `T | null`: the value if present, else `null`.
    let src = "function main() {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 tx.send(7);\n\
                 match rx.try_recv() { i64 n => println(\"got \" + (n as str)), null => println(\"empty\") }\n\
                 match rx.try_recv() { i64 n => println(\"got \" + (n as str)), null => println(\"empty\") }\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "got 7\nempty\n");
}

#[test]
fn static_method_on_concrete_type() {
    // `docs/09` §6: `Type.static_method(args)` calls a static (no-`self`) method
    // declared in an `extend` of the type.
    let src = "struct Point { x: i64, y: i64 }\n\
               extend Point {\n\
                 function at(x: i64, y: i64): Point { Point { x: x, y: y } }\n\
                 function sum(self): i64 { self.x + self.y }\n\
               }\n\
               function main() { println(Point.at(3, 4).sum() as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "7\n");
}

#[test]
fn static_method_through_generic_bound() {
    // `docs/10`: `T.static_method()` for `T: Trait` resolves to the concrete
    // impl, monomorphized.
    let src = "interface Default { function default(): Self; }\n\
               struct Widget { id: i64 }\n\
               extend Widget: Default { function default(): Widget { Widget { id: 42 } } }\n\
               function make<T: Default>(): T { T.default() }\n\
               function main() { var w: Widget = make<Widget>(); println(w.id as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn static_method_on_generic_struct_infers_type_args() {
    // `docs/11` §3: `Box.new(99)` infers `Box<i64>` from the argument's type —
    // no explicit `<i64>` needed. Mirrors generic free-function inference.
    let src = "struct Box<T> { value: T }\n\
               extend<T> Box<T> {\n\
                 function new(v: T): Box<T> { Box { value: v } }\n\
               }\n\
               function main() {\n\
                 var b = Box.new(99);\n\
                 var s = Box.new(\"hello\");\n\
                 println(\"${b.value} ${s.value}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "99 hello\n");
}

#[test]
fn static_method_on_generic_struct_self_return() {
    // A static method returning `Self` on a generic struct: inference must
    // flow through the return type so the var's type is the concrete instance.
    let src = "struct Counter<T> { count: T }\n\
               extend<T> Counter<T> {\n\
                 function start(v: T): Self { Counter { count: v } }\n\
               }\n\
               function main() {\n\
                 var c = Counter.start(42);\n\
                 println(\"${c.count}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn static_method_on_generic_struct_multi_args() {
    // Inference from multiple positional args binds multiple struct generics.
    let src = "struct Pair<A, B> { left: A, right: B }\n\
               extend<A, B> Pair<A, B> {\n\
                 function make(l: A, r: B): Pair<A, B> { Pair { left: l, right: r } }\n\
               }\n\
               function main() {\n\
                 var p = Pair.make(7, \"x\");\n\
                 println(\"${p.left} ${p.right}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "7 x\n");
}

#[test]
fn static_method_on_generic_struct_native_build() {
    // JIT/native parity for the new inference path.
    let src = "struct Box<T> { value: T }\n\
               extend<T> Box<T> {\n\
                 function new(v: T): Box<T> { Box { value: v } }\n\
               }\n\
               function main() {\n\
                 var b = Box.new(123);\n\
                 println(\"${b.value}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "123\n");
}

#[test]
fn method_level_generics_on_extend_method_infer() {
    // `docs/11` §3: a method declared with its own generic params on a generic
    // extend (`function map<U>(self, f: (T) => U): Box<U>`) infers `U` from the
    // closure's return type — no `<str>` annotation needed.
    let src = "struct Box<T> { value: T }\n\
               extend<T> Box<T> {\n\
                 function new(v: T): Box<T> { Box { value: v } }\n\
                 function map<U>(self, f: (T) => U): Box<U> {\n\
                   Box { value: f(self.value) }\n\
                 }\n\
               }\n\
               function main() {\n\
                 var b = Box.new(7);\n\
                 var s = b.map((x: i64): str => \"v=${x}\");\n\
                 println(s.value);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "v=7\n");
}

#[test]
fn method_level_generics_explicit_annotation() {
    // The same call with explicit `<str>` works too; both forms target the
    // same monomorphized instance.
    let src = "struct Box<T> { value: T }\n\
               extend<T> Box<T> {\n\
                 function new(v: T): Box<T> { Box { value: v } }\n\
                 function map<U>(self, f: (T) => U): Box<U> {\n\
                   Box { value: f(self.value) }\n\
                 }\n\
               }\n\
               function main() {\n\
                 var b = Box.new(7);\n\
                 var s: Box<str> = b.map<str>((x: i64): str => \"v=${x}\");\n\
                 println(s.value);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "v=7\n");
}

#[test]
fn method_level_generics_chain_with_native_build() {
    // Method-level inference chains through multiple calls — `map` returning
    // a different element type, then `map` again. JIT/native parity.
    let src = "struct Box<T> { value: T }\n\
               extend<T> Box<T> {\n\
                 function new(v: T): Box<T> { Box { value: v } }\n\
                 function map<U>(self, f: (T) => U): Box<U> {\n\
                   Box { value: f(self.value) }\n\
                 }\n\
               }\n\
               function main() {\n\
                 var b = Box.new(42);\n\
                 var s = b.map((x: i64): str => \"x=${x}\");\n\
                 var n = s.map((t: str): i64 => t.size());\n\
                 println(\"len=${n.value}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "len=4\n");
}

#[test]
fn static_method_on_generic_struct_uninferable_errors() {
    // No arguments to infer from + no explicit annotation: error must be
    // clear and anchored to the struct, not the method.
    let src = "struct Marker<T> { tag: i64 }\n\
               extend<T> Marker<T> {\n\
                 function blank(): Marker<T> { Marker { tag: 0 } }\n\
               }\n\
               function main() { var m = Marker.blank(); println(\"${m.tag}\"); }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected an unsolved-generic error");
    assert!(
        err.contains("cannot infer generic argument") && err.contains("Marker"),
        "stderr: {err}"
    );
}

#[test]
fn calling_instance_method_statically_errors() {
    // An instance method (takes `self`) cannot be called as `Type.method()`.
    let src = "struct P { x: i64 }\n\
               extend P { function get(self): i64 { self.x } }\n\
               function main() { println(P.get() as str); }";
    let (_, err, ok) = lang("check", src);
    assert!(
        !ok,
        "expected an error calling an instance method statically"
    );
    assert!(err.contains("instance method"), "stderr: {err}");
}

#[test]
fn thread_spawn_join_returns_result() {
    // `docs/20` §1: `Thread.spawn(() => R)` runs a closure on a new OS thread;
    // `join()` is async — it returns a `Future<Joined<R> | Panicked>` awaited
    // inside the async main (or any async body).
    let src = "function take(r: Joined<i64> | Panicked): i64 {\n\
                 match r { Joined<i64> j => j.value, Panicked p => 0 - 1 }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var base: i64 = 10;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => {\n\
                   var s: i64 = 0; var i: i64 = 0;\n\
                   while i < 1000 { s = s + i; i = i + 1; }\n\
                   s + base\n\
                 });\n\
                 var r: Joined<i64> | Panicked = await h.join();\n\
                 println(take(r) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "499510\n"); // sum(0..1000) + 10
}

#[test]
fn many_threads_under_gc_stress() {
    // Several workers each allocate heavily; the result must be deterministic
    // and memory-safe under `OTTER_FUSION_GC=stress` (stop-the-world coordination). An
    // async gatherer awaits each `join()` in turn so the main task suspends
    // (rather than parking the OS thread) between worker completions.
    let src = "function work(id: i64): i64 {\n\
                 var acc: i64 = 0; var i: i64 = 0;\n\
                 while i < 3000 { var s: str = \"n-\" + (i as str); acc = acc + s.size(); i = i + 1; }\n\
                 acc + id\n\
               }\n\
               function take(r: Joined<i64> | Panicked): i64 {\n\
                 match r { Joined<i64> j => j.value, Panicked p => 0 }\n\
               }\n\
               function gather(a: JoinHandle<i64>, b: JoinHandle<i64>, c: JoinHandle<i64>): Future<i64> async {\n\
                 var ra: Joined<i64> | Panicked = await a.join();\n\
                 var rb: Joined<i64> | Panicked = await b.join();\n\
                 var rc: Joined<i64> | Panicked = await c.join();\n\
                 take(ra) + take(rb) + take(rc)\n\
               }\n\
               function main(): Future<null> async {\n\
                 var a: JoinHandle<i64> = Thread.spawn(() => work(1));\n\
                 var b: JoinHandle<i64> = Thread.spawn(() => work(2));\n\
                 var c: JoinHandle<i64> = Thread.spawn(() => work(3));\n\
                 var total: i64 = await gather(a, b, c);\n\
                 println(total as str);\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    let (out2, _, ok2) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok2);
    assert_eq!(
        out1, out2,
        "GC stress changed the result (memory corruption)"
    );
}

#[test]
fn worker_panic_is_isolated_and_reported_on_join() {
    // `docs/20` §1 / `docs/21` §11: a `panic` in a `Thread.spawn` worker fails
    // ONLY that worker — it surfaces as `Panicked { message }` on `join`, with
    // the exact panic message, while a sibling spawned alongside completes. The
    // process must NOT abort (the pre-isolation behavior).
    let src = "function maybe(n: i64): i64 { if n == 0 { panic(\"down \" + (n as str)); } n * n }\n\
               function show(label: str, r: Joined<i64> | Panicked) {\n\
                 match r {\n\
                   Joined<i64> j => println(label + \" ok \" + (j.value as str)),\n\
                   Panicked p    => println(label + \" panic \" + p.message),\n\
                 };\n\
               }\n\
               function main(): Future<null> async {\n\
                 var a: JoinHandle<i64> = Thread.spawn(() => maybe(0));\n\
                 var b: JoinHandle<i64> = Thread.spawn(() => maybe(7));\n\
                 show(\"a\", await a.join());\n\
                 show(\"b\", await b.join());\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "the process must not abort; stderr: {err}");
    assert_eq!(out, "a panic down 0\nb ok 49\n");
    // Native parity.
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out, "JIT and native disagree");
}

#[test]
fn worker_panic_releases_held_shared_lock() {
    // `docs/20` §4: a panic inside a lock body unwinds to the worker boundary
    // and RELEASES the lock (no poisoning). The async worker surfaces as
    // `Panicked` on join; afterwards the cell is re-lockable and holds the
    // mutation the panicking body made before it died.
    let src = "struct C { value: i64 }\n\
               function main(): Future<null> async {\n\
                 var state: Shared<C> = Shared.new(C { value: 0 });\n\
                 var s: Shared<C> = state.clone();\n\
                 var h: JoinHandle<null> = Thread.spawn(() async => {\n\
                   await s.lock((c) => { c.value = 1; panic(\"in body\"); 0 });\n\
                   null\n\
                 });\n\
                 match await h.join() {\n\
                   Joined<null> j => println(\"worker ok\"),\n\
                   Panicked p     => println(\"worker \" + p.message),\n\
                 };\n\
                 match await state.try_lock((c) => c.value) {\n\
                   i64 n         => println(\"reacquired \" + (n as str)),\n\
                   LockBusy busy => println(\"still held\"),\n\
                 };\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "worker in body\nreacquired 1\n");
}

#[test]
fn spawn_await_repropagates_worker_panic() {
    // `docs/21` §11: a `spawn EXPR` task panic is RE-PROPAGATED at the awaiter
    // (promise-rejection model). With the awaiter on `main` (no boundary), the
    // program terminates with the worker's message and a non-zero exit. The
    // recoverable form is `JoinHandle.join` (tested above), not awaiting `spawn`.
    let src = "function work(): Future<i64> async { panic(\"rejected\"); 0 }\n\
               function main(): Future<null> async {\n\
                 var h: Future<i64> = spawn work();\n\
                 var v: i64 = await h;\n\
                 println(\"unreachable\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(
        !ok,
        "awaiting a panicked spawn must propagate (non-zero exit)"
    );
    assert!(
        err.contains("rejected"),
        "message should propagate; stderr: {err}"
    );
    assert!(
        out.is_empty(),
        "nothing after the propagated panic; stdout: {out}"
    );
}

#[test]
fn worker_panic_isolation_is_stable_under_gc_stress() {
    // Panic isolation + concurrent reclamation: a worker that allocates heavily
    // then panics leaves the heap consistent while siblings allocate; the result
    // is identical with and without `OTTER_FUSION_GC=stress` (no corruption).
    let src = "function churn(id: i64, doom: bool): i64 {\n\
                 var acc: i64 = 0; var i: i64 = 0;\n\
                 while i < 2000 { var t: str = \"x-${id}-${i}\"; acc = acc + t.size(); i = i + 1; }\n\
                 if doom { panic(\"doom \" + (id as str)); }\n\
                 acc\n\
               }\n\
               function tag(r: Joined<i64> | Panicked): str {\n\
                 match r { Joined<i64> j => \"ok\", Panicked p => p.message }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var a: JoinHandle<i64> = Thread.spawn(() => churn(0, true));\n\
                 var b: JoinHandle<i64> = Thread.spawn(() => churn(1, false));\n\
                 var c: JoinHandle<i64> = Thread.spawn(() => churn(2, true));\n\
                 println(tag(await a.join()));\n\
                 println(tag(await b.join()));\n\
                 println(tag(await c.join()));\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    let (out2, _, ok2) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok2);
    assert_eq!(out1, "doom 0\nok\ndoom 2\n");
    assert_eq!(
        out1, out2,
        "GC stress changed the result (memory corruption)"
    );
}

#[test]
fn spawn_rejects_mutable_capture() {
    // A spawned closure capturing a mutable (struct) value is rejected until
    // deep-clone of captures lands (`docs/20` §1).
    let src = "struct Counter { n: i64 }\n\
               function main() {\n\
                 var c: Counter = Counter { n: 0 };\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => c.n);\n\
                 println(\"unreachable\");\n\
               }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected a capture rejection");
    assert!(
        err.contains("immutable") || err.contains("clone"),
        "stderr: {err}"
    );
}

#[test]
fn derived_clone_is_a_deep_copy() {
    // `docs/15` §8: `@Derive(Clone)` is field-by-field deep copy. Mutating a
    // clone (incl. through a nested struct) must not affect the original.
    let src = "@Derive(Clone)\n\
               struct Point { x: i64, y: i64 }\n\
               @Derive(Clone)\n\
               struct Line { from: Point, to: Point }\n\
               function main() {\n\
                 var a: Line = Line { from: Point { x: 0, y: 0 }, to: Point { x: 3, y: 4 } };\n\
                 var b: Line = a.clone();\n\
                 b.to.x = 99;\n\
                 println((a.to.x as str) + \" \" + (b.to.x as str));\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3 99\n");
}

#[test]
fn clone_through_clone_bound() {
    // A `T: Clone` bound dispatches `.clone()` to the intrinsic clone for
    // builtins and to the derived impl for user types (`docs/11`/`docs/15`).
    let src = "@Derive(Clone)\n\
               struct P { x: i64 }\n\
               function dup<T: Clone>(v: T): T { v.clone() }\n\
               function main() {\n\
                 println(dup<i64>(5) as str);\n\
                 println(dup<str>(\"hi\"));\n\
                 var p: P = P { x: 10 };\n\
                 var q: P = dup<P>(p);\n\
                 q.x = 77;\n\
                 println((p.x as str) + \" \" + (q.x as str));\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "5\nhi\n10 77\n");
}

#[test]
fn generic_struct_derives_clone() {
    // `@Derive(Clone)` on a generic struct synthesises `extend<T: Clone> S<T>:
    // Clone`. The clone is a deep copy: mutating a nested field of the copy must
    // not touch the original, and a primitive payload clones intrinsically.
    let src = "@Derive(Clone)\n\
               struct Box<T> { value: T }\n\
               @Derive(Clone)\n\
               struct Point { x: i64, y: i64 }\n\
               function main() {\n\
                 var a = Box { value: Point { x: 1, y: 2 } };\n\
                 var b = a.clone();\n\
                 b.value.x = 99;\n\
                 println((a.value.x as str) + \" \" + (b.value.x as str));\n\
                 var n = Box { value: 7 };\n\
                 println(n.clone().value as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "1 99\n7\n");
    // Native build must match the JIT output byte-for-byte.
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn generic_tuple_struct_construction_and_clone() {
    // Generic tuple-struct construction infers its type arguments from the
    // positional arguments (`Pair(5, "x")` → `Pair<i64, str>`), and `Clone`
    // derives on it.
    let src = "@Derive(Clone)\n\
               struct Pair<A, B>(A, B)\n\
               function main() {\n\
                 var t = Pair(5, \"x\");\n\
                 var u = t.clone();\n\
                 println((u.0 as str) + u.1);\n\
                 var p = Pair(1, 2);\n\
                 println((p.0 + p.1) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "5x\n3\n");
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn generic_struct_derives_eq_and_ord() {
    // `@Derive(Eq, Ord)` on a generic struct: per-field `==`/`<` become
    // `.eq()`/`.lt()` calls dispatched through each field's `Eq`/`Ord` bound.
    // Works for primitive fields (intrinsic compare) and user-type fields
    // (their own derived impl).
    let src = "@Derive(Eq, Ord)\n\
               struct Pair<A, B> { a: A, b: B }\n\
               @Derive(Eq, Ord)\n\
               struct Point { x: i64, y: i64 }\n\
               function main() {\n\
                 var p1 = Pair { a: 1, b: \"x\" };\n\
                 var p2 = Pair { a: 1, b: \"y\" };\n\
                 println((p1 == p1) as str);\n\
                 println((p1 < p2) as str);\n\
                 println((p2 < p1) as str);\n\
                 println((p1 != p2) as str);\n\
                 var q1 = Pair { a: Point { x: 1, y: 2 }, b: 0 };\n\
                 var q2 = Pair { a: Point { x: 1, y: 3 }, b: 0 };\n\
                 println((q1 < q2) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\ntrue\nfalse\ntrue\ntrue\n");
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn generic_derived_type_satisfies_eq_bound() {
    // A concrete `@Derive(Eq)` type satisfies a `T: Eq` bound (its synthesised
    // `extend` declares the interface), so it can be a generic struct's element.
    let src = "@Derive(Eq)\n\
               struct Wrap<T> { inner: T }\n\
               @Derive(Eq)\n\
               struct Id { n: i64 }\n\
               function main() {\n\
                 var a = Wrap { inner: Id { n: 7 } };\n\
                 var b = Wrap { inner: Id { n: 7 } };\n\
                 var c = Wrap { inner: Id { n: 9 } };\n\
                 println((a == b) as str);\n\
                 println((a == c) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn generic_struct_derives_to_str() {
    // `@Derive(ToStr)` on a generic struct: each field is rendered via a
    // `.to_str()` call dispatched through its `ToStr` bound — a primitive field
    // via the intrinsic `as str`, a user-type field via its own `to_str`. Also
    // exercised through string interpolation.
    let src = "@Derive(ToStr)\n\
               struct Box<T> { value: T }\n\
               @Derive(ToStr)\n\
               struct Point { x: i64, y: i64 }\n\
               @Derive(ToStr)\n\
               struct Pair<A, B>(A, B)\n\
               function main() {\n\
                 println(Box { value: 7 }.to_str());\n\
                 println(Box { value: \"hi\" }.to_str());\n\
                 println(Box { value: Point { x: 1, y: 2 } }.to_str());\n\
                 println(Pair(5, \"x\").to_str());\n\
                 println(\"interp: ${Box { value: 9 }}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(
        out,
        "Box { value: 7 }\nBox { value: hi }\nBox { value: Point { x: 1, y: 2 } }\nPair(5, x)\ninterp: Box { value: 9 }\n"
    );
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn list_clone_is_independent_under_gc_stress() {
    // A cloned `List` is independent of the original, and the clone allocation
    // is GC-safe (the new handle/buffer are rooted across collection).
    let src = "function main() {\n\
                 var i: i64 = 0;\n\
                 var total: i64 = 0;\n\
                 while i < 200 {\n\
                   var xs: List<i64> = [i, i + 1];\n\
                   var ys: List<i64> = xs.clone();\n\
                   ys.push(0);\n\
                   total = total + xs.size() + ys.size();\n\
                   i = i + 1;\n\
                 }\n\
                 println(total as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // each iteration: xs.size()=2, ys.size()=3 → 5 * 200 = 1000
    assert_eq!(out, "1000\n");
}

#[test]
fn clone_rejects_list_of_non_clone_elements() {
    // A `List<T>` where `T` is mutable and has no `Clone` impl still cannot
    // be cloned — the diagnostic names the element type so users know to
    // derive or hand-write `Clone`.
    let src = "struct P { x: i64 }\n\
               function main() {\n\
                 var xs: List<P> = [P { x: 1 }];\n\
                 var ys: List<P> = xs.clone();\n\
                 println(ys.size() as str);\n\
               }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected a clone rejection");
    assert!(
        err.contains("clone") && err.contains("Clone"),
        "stderr: {err}"
    );
}

#[test]
fn list_deep_clone_of_user_struct() {
    // `docs/10`: a `List` of a mutable user type that implements `Clone` now
    // clones element-by-element. Mutating an element of the clone must NOT
    // affect the original.
    let src = "@Derive(Clone)\n\
               struct Counter { value: i64 }\n\
               extend Counter {\n\
                 function bump(self) { self.value = self.value + 1000; }\n\
               }\n\
               function main() {\n\
                 var xs: List<Counter> = [Counter { value: 1 }, Counter { value: 2 }];\n\
                 var ys = xs.clone();\n\
                 ys[0].bump();\n\
                 println(\"xs0=${xs[0].value} ys0=${ys[0].value}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "xs0=1 ys0=1001\n");
}

#[test]
fn map_deep_clone_of_user_struct_value() {
    // `Map<str, Counter>` deep-clones values element-by-element. Keys must
    // stay immutable (their hashes would otherwise become unstable).
    let src = "@Derive(Clone)\n\
               struct Counter { value: i64 }\n\
               extend Counter {\n\
                 function bump(self) { self.value = self.value + 100; }\n\
               }\n\
               function main() {\n\
                 var m: Map<str, Counter> = { \"a\": Counter { value: 1 } };\n\
                 var n = m.clone();\n\
                 n[\"a\"].bump();\n\
                 var ma = m[\"a\"];\n\
                 var na = n[\"a\"];\n\
                 println(\"m=${ma.value} n=${na.value}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "m=1 n=101\n");
}

#[test]
fn list_deep_clone_native_build_and_gc_stress() {
    // JIT/native parity + GC-stress safety: per-element clone allocates
    // inside the loop, so the new list pointer must stay rooted.
    let src = "@Derive(Clone)\n\
               struct Counter { value: i64 }\n\
               extend Counter {\n\
                 function bump(self) { self.value = self.value + 1; }\n\
               }\n\
               function main() {\n\
                 var xs: List<Counter> = [Counter { value: 10 }, Counter { value: 20 }];\n\
                 var ys = xs.clone();\n\
                 ys[0].bump();\n\
                 ys[1].bump();\n\
                 println(\"xs=${xs[0].value},${xs[1].value} ys=${ys[0].value},${ys[1].value}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "xs=10,20 ys=11,21\n");
}

#[test]
fn multi_file_named_imports() {
    // In the entry, `mod util;` loads the sibling `src/util.otter` (`docs/17`
    // §17.2); `self:util` named imports bring its public items into scope.
    let main = "mod util;\n\
                 import { add, Point } from \"self:util\";\n\
                 function main() {\n\
                   println(\"sum=${add(40, 2)}\");\n\
                   var p: Point = Point { x: 3, y: 4 };\n\
                   println(\"pt=(${p.x},${p.y})\");\n\
                 }";
    let util = "pub function add(a: i64, b: i64): i64 { a + b }\n\
                pub struct Point { x: i64, y: i64 }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/util.otter", util),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "sum=42\npt=(3,4)\n");
}

#[test]
fn cross_module_interface_default() {
    // An interface declared `pub` in one module carries a default body; a type
    // in another module that imports it and omits the method inherits the
    // default (`docs/10` — cross-module interface defaults).
    let main = "mod traits;\n\
                 import { Greeter } from \"self:traits\";\n\
                 struct Person { who: str }\n\
                 extend Person: Greeter { function name(self): str { self.who } }\n\
                 function main() {\n\
                   var p: Person = Person { who: \"Otter\" };\n\
                   println(p.greet());\n\
                 }";
    let traits = "pub interface Greeter {\n\
                    function name(self): str;\n\
                    function greet(self): str { \"Hello, \" + self.name() }\n\
                  }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/traits.otter", traits),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "Hello, Otter\n");
}

#[test]
fn cross_module_generic_interface_default() {
    // A *generic* `pub` interface from another module: its type parameter is
    // substituted with the impl's argument in the copied default body.
    let main = "mod traits;\n\
                 import { Boxed } from \"self:traits\";\n\
                 struct Cell { v: i64 }\n\
                 extend Cell: Boxed<i64> { function get(self): i64 { self.v } }\n\
                 function main() {\n\
                   var c: Cell = Cell { v: 42 };\n\
                   println(\"dup=${c.dup()}\");\n\
                 }";
    let traits = "pub interface Boxed<T> {\n\
                    function get(self): T;\n\
                    function dup(self): T { self.get() }\n\
                  }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/traits.otter", traits),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "dup=42\n");
}

#[test]
fn cross_module_default_is_overridable() {
    // A local override of a cross-module default wins over the copied body.
    let main = "mod traits;\n\
                 import { Greeter } from \"self:traits\";\n\
                 struct Person { who: str }\n\
                 extend Person: Greeter {\n\
                   function name(self): str { self.who }\n\
                   function greet(self): str { \"Yo \" + self.who }\n\
                 }\n\
                 function main() {\n\
                   println(Person { who: \"O\" }.greet());\n\
                 }";
    let traits = "pub interface Greeter {\n\
                    function name(self): str;\n\
                    function greet(self): str { \"Hello, \" + self.name() }\n\
                  }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/traits.otter", traits),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "Yo O\n");
}

#[test]
fn generic_drop_gc_managed_runs_under_stress() {
    // A GC-managed (non-`@RefCounted`) generic Drop type: each monomorphization
    // gets its own finalizer. Under stress GC every loop temporary becomes
    // unreachable and is finalized; both `Tracked<i64>` and `Tracked<str>` fire
    // their `drop` the expected number of times (order is unspecified, so we
    // count occurrences).
    let src = "struct Tracked<T> { kind: i64 }\n\
               extend<T> Tracked<T>: Drop {\n\
                 function drop(self) {\n\
                   if self.kind == 0 { println(\"drop-int\"); }\n\
                   if self.kind == 1 { println(\"drop-str\"); }\n\
                 }\n\
               }\n\
               function spin() {\n\
                 var i: i64 = 0;\n\
                 while i < 4 {\n\
                   var a: Tracked<i64> = Tracked<i64> { kind: 0 };\n\
                   var b: Tracked<str> = Tracked<str> { kind: 1 };\n\
                   i = i + 1;\n\
                 }\n\
               }\n\
               // Allocate churn (kind 2 ⇒ silent drop) after `spin` returns so a\n\
               // collection finalizes every now-unreachable tracked object before\n\
               // the program exits (exit-time finalization is best-effort).\n\
               function churn() {\n\
                 var i: i64 = 0;\n\
                 while i < 100 {\n\
                   var t: Tracked<bool> = Tracked<bool> { kind: 2 };\n\
                   i = i + 1;\n\
                 }\n\
               }\n\
               function main() { spin(); churn(); }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    let ints = out.lines().filter(|l| *l == "drop-int").count();
    let strs = out.lines().filter(|l| *l == "drop-str").count();
    assert_eq!(ints, 4, "expected 4 int drops, got {ints}:\n{out}");
    assert_eq!(strs, 4, "expected 4 str drops, got {strs}:\n{out}");
}

#[test]
fn multi_file_rejects_private_import() {
    // A non-`pub` item cannot be imported across modules (`docs/17` §3).
    let main = "mod util;\n\
                 import { secret } from \"self:util\";\n\
                 function main() { println(secret() as str); }";
    let util = "function secret(): i64 { 99 }";
    let (_, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/util.otter", util),
        ],
    );
    assert!(!ok);
    assert!(err.contains("`secret` is private"), "stderr: {err}");
}

#[test]
fn multi_file_strict_module_scoping() {
    // Names do not cross module boundaries without `import`: a submodule cannot
    // see a crate-root function it never imported (`docs/17` §3).
    let main = "mod util;\n\
                 pub function root_only(): i64 { 7 }\n\
                 function main() { println(\"${root_only()}\"); }";
    let util = "pub function uses_root(): i64 { root_only() }";
    let (_, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/util.otter", util),
        ],
    );
    assert!(!ok);
    assert!(
        err.contains("cannot find value `root_only`"),
        "stderr: {err}"
    );
}

#[test]
fn import_as_namespace_calls() {
    // `import "self:mathx" as M` binds a namespace; `M.foo(..)` calls the
    // module's public functions (`docs/17` §3).
    let main = "mod mathx;\n\
                 import \"self:mathx\" as M;\n\
                 function main() { println(\"${M.add(40, 2)} ${M.square(7)}\"); }";
    let mathx = "pub function add(a: i64, b: i64): i64 { a + b }\n\
                 pub function square(n: i64): i64 { n * n }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/mathx.otter", mathx),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42 49\n");
}

#[test]
fn import_as_namespace_rejects_private() {
    // Namespaced access reaches only the module's public definitions.
    let main = "mod mathx;\n\
                 import \"self:mathx\" as M;\n\
                 function main() { println(\"${M.hidden()}\"); }";
    let mathx = "function hidden(): i64 { 0 }";
    let (_, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/mathx.otter", mathx),
        ],
    );
    assert!(!ok);
    assert!(err.contains("no public value `hidden`"), "stderr: {err}");
}

#[test]
fn multi_file_nested_submodule() {
    // A submodule may itself declare a file-backed submodule, loaded from a
    // directory named for its parent file's stem (`src/util/` for `util.otter`).
    let main = "mod util;\n\
                 import { triple } from \"self:util\";\n\
                 function main() { println(\"${triple(5)}\"); }";
    let util = "mod math;\n\
                import { times } from \"self:util/math\";\n\
                pub function triple(n: i64): i64 { times(n, 3) }";
    let math = "pub function times(a: i64, b: i64): i64 { a * b }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/util.otter", util),
            ("src/util/math.otter", math),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "15\n");
}

#[test]
fn self_relative_import_resolves_sibling() {
    // `self:./b` resolves relative to the importing file's directory — `a` and
    // `b` are siblings under `src/` (`docs/17` §17.4).
    let main = "mod a;\n\
                 mod b;\n\
                 import { lib_fn } from \"self:a\";\n\
                 function main() { println(\"${lib_fn()}\"); }";
    let a = "import { core } from \"self:./b\";\n\
             pub function lib_fn(): i64 { core() + 1 }";
    let b = "pub function core(): i64 { 41 }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/a.otter", a),
            ("src/b.otter", b),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn self_relative_parent_escape_is_rejected() {
    // A `self:../…` chain that climbs above the source root is a hard error
    // (`docs/17` §17.4).
    let main = "import { x } from \"self:../../outside\";\n\
                 function main() {}";
    let (_, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
        ],
    );
    assert!(!ok);
    assert!(err.contains("escapes package"), "stderr: {err}");
}

#[test]
fn prefixless_import_is_rejected() {
    // Every import path needs an explicit scheme (`docs/17` §17.4).
    let main = "mod util;\n\
                 import { f } from \"util\";\n\
                 function main() {}";
    let util = "pub function f(): i64 { 1 }";
    let (_, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/util.otter", util),
        ],
    );
    assert!(!ok);
    assert!(err.contains("no scheme prefix"), "stderr: {err}");
}

#[test]
fn self_import_without_project_is_hard_error() {
    // `self:` needs project context; a loose file run by `exec` has none.
    let src = "import { f } from \"self:util\";\nfunction main() {}";
    let (_, err, ok) = lang("exec", src);
    assert!(!ok);
    assert!(err.contains("requires a project"), "stderr: {err}");
}

#[test]
fn core_import_works_without_a_project() {
    // `core:` is a toolchain module, available even in direct/loose mode
    // (`docs/17` §17.13). Importing `List` by name resolves and runs.
    let src = "import { println } from \"std:io\";\n\
               import { List } from \"core:collections\";\n\
               function main() {\n\
                 var xs: List<i64> = [1, 2, 3];\n\
                 println(\"${xs.size()}\");\n\
               }";
    let (out, err, ok) = lang_raw("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3\n");
}

#[test]
fn pkg_import_without_project_is_hard_error() {
    // `pkg:` needs a project manifest to resolve against (`docs/17` §17.13).
    let src = "import { Foo } from \"pkg:somelib\";\nfunction main() {}";
    let (_, err, ok) = lang("exec", src);
    assert!(!ok);
    assert!(err.contains("requires a project manifest"), "stderr: {err}");
}

#[test]
fn file_import_escaping_package_needs_allowlist() {
    // An escaping `file:` path with no `[file-imports] allow` entry is rejected
    // (`docs/17` §17.4).
    let main = "import { rows } from \"file:../../secret/data\";\n\
                 function main() {}";
    let (_, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
        ],
    );
    assert!(!ok);
    assert!(
        err.contains("not authorized by `[file-imports] allow`"),
        "stderr: {err}"
    );
}

#[test]
fn file_import_binds_an_in_package_data_module() {
    // `file:./data` loads a sibling `.otter` file (not in the `mod` tree) and
    // binds its `pub` names (`docs/17` §17.4). In-package paths need no allowlist.
    let main = "import { rows } from \"file:./data\";\n\
                 function main() { println(\"${rows()}\"); }";
    let data = "pub function rows(): i64 { 7 }";
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            ("src/main.otter", main),
            ("src/data.otter", data),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "7\n");
}

#[test]
fn file_import_binds_in_loose_direct_mode() {
    // In direct/`exec` mode `file:` is unrestricted; a loose script binds names
    // from a sibling file (`docs/17` §17.13).
    let root = write_tree(&[
        (
            "script.otter",
            "import { val } from \"file:./helper\";\nfunction main() { var x = val(); }",
        ),
        ("helper.otter", "pub function val(): i64 { 5 }"),
    ]);
    let (_o, err, ok) = lang_in_dir(&root, &["exec", "script.otter"]);
    assert!(ok, "stderr: {err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reserved_url_scheme_is_rejected() {
    // The URL scheme family is reserved, not implemented (`docs/17` §17.14).
    let src = "import { Foo } from \"https://example.com/x\";\nfunction main() {}";
    let (_, err, ok) = lang("exec", src);
    assert!(!ok);
    assert!(err.contains("reserved"), "stderr: {err}");
}

#[test]
fn ffi_extern_function_call() {
    // `docs/19`: an `extern function` is called across the C ABI. `abs` from
    // libc is resolved by the JIT (dlsym) / the linker (native).
    let src = "extern function abs(n: i32): i32;\n\
               function main() { println(\"abs=${abs(-5i32)}\"); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "abs=5\n");
}

#[test]
fn ffi_extern_call_native_matches_jit() {
    // The same FFI program must behave identically JIT-run and natively linked.
    let src = "extern function abs(n: i32): i32;\n\
               function main() { println(\"${abs(-7i32) + abs(3i32)}\"); }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "10\n");
}

#[test]
fn ffi_extern_struct_memcpy_round_trip() {
    // `docs/19` §2/§3: an `extern struct` is C-laid-out and stack-allocated;
    // `&value` is its address (no pin needed). A C `memcpy` fills it via an
    // out-pointer, then fields read the written bytes; `field = v` mutates.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               extern function memcpy(dst: *Pair, src: *Pair, n: u64): *Pair;\n\
               function main() {\n\
                 var x = Pair { a: 10, b: 20 };\n\
                 var y = Pair { a: 0, b: 0 };\n\
                 memcpy(&y, &x, 16u64);\n\
                 println(\"${y.a} ${y.b}\");\n\
                 y.a = 99;\n\
                 println(\"${y.a}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "10 20\n99\n");
}

#[test]
fn ffi_extern_struct_survives_gc_stress() {
    // Stack-allocated extern structs and their raw pointers must not perturb the
    // GC (their fields are scalars / pointers, never traced). Interleave managed
    // `str` interpolation so collections fire mid-program.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               extern function memcpy(dst: *Pair, src: *Pair, n: u64): *Pair;\n\
               function main() {\n\
                 var total = 0;\n\
                 var i = 0;\n\
                 while i < 200 {\n\
                   var x = Pair { a: i, b: i + 1 };\n\
                   var y = Pair { a: 0, b: 0 };\n\
                   memcpy(&y, &x, 16u64);\n\
                   var s = \"iter ${y.a}/${y.b}\";\n\
                   total = total + y.a + y.b;\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"${total}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // sum over i in 0..200 of (i + i+1) = sum(2i+1) = 200^2 = 40000.
    assert_eq!(out, "40000\n");
}

#[test]
fn ffi_packed_decorator_changes_layout() {
    // `docs/19` §3: `@Packed` caps field alignment, shifting offsets. The same
    // 8 source bytes read different `y` values under packed vs natural layout.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               extern function memcpy(dst: *u8, src: *u8, n: u64): *u8;\n\
               @Packed\n\
               extern struct Packed { x: u8, y: u32 }\n\
               extern struct Unpacked { x: u8, y: u32 }\n\
               function main() {\n\
                 var src = Pair { a: 0x0807060504030201, b: 0 };\n\
                 var p = Packed { x: 0, y: 0 };\n\
                 var u = Unpacked { x: 0, y: 0 };\n\
                 memcpy((&p) as *u8, (&src) as *u8, 8u64);\n\
                 memcpy((&u) as *u8, (&src) as *u8, 8u64);\n\
                 println(\"${p.x} ${p.y}\");\n\
                 println(\"${u.x} ${u.y}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    // packed: y at offset 1 → bytes 2,3,4,5 (LE) = 0x05040302 = 84148994.
    // natural: y at offset 4 → bytes 5,6,7,8 (LE) = 0x08070605 = 134678021.
    assert_eq!(out, "1 84148994\n1 134678021\n");
}

#[test]
fn ffi_align_decorator_over_aligns() {
    // `docs/19` §3: `@Align(64)` over-aligns the struct beyond the stack's
    // natural alignment; the address is a multiple of 64.
    let src = "@Align(64)\n\
               extern struct CacheLine { v: i64 }\n\
               function main() {\n\
                 var c = CacheLine { v: 7 };\n\
                 var addr = (&c) as usize;\n\
                 println(\"${addr % 64usize}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "0\n");
}

#[test]
fn ffi_union_decorator_overlays_fields() {
    // `docs/19` §3: `@Union` overlays all fields at offset 0. Writing the `f32`
    // field and reading the `u32` field yields the float's bit pattern.
    let src = "@Union\n\
               extern struct FloatBits { f: f32, i: u32 }\n\
               function main() {\n\
                 var fb = FloatBits { f: 1.0f32 };\n\
                 println(\"${fb.i}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    // IEEE-754 bits of 1.0f32 = 0x3F800000 = 1065353216.
    assert_eq!(out, "1065353216\n");
}

#[test]
fn ffi_pointer_deref_load_and_store() {
    // `docs/19` §2: `*p` reads through a raw pointer; `*p = v` writes. A
    // pointer reinterpret (`as`) aliases the first field as a scalar.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               function main() {\n\
                 var x = Pair { a: 7, b: 8 };\n\
                 var pi = (&x) as *i64;\n\
                 println(\"${*pi}\");\n\
                 *pi = 42;\n\
                 println(\"${x.a}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "7\n42\n");
}

#[test]
fn ffi_null_deref_panics() {
    // `docs/19` §2: dereferencing a null pointer panics (exit 101).
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               function main() {\n\
                 var p = 0usize as *Pair;\n\
                 println(\"before\");\n\
                 println(\"${(*p).a}\");\n\
                 println(\"after\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok);
    assert_eq!(out, "before\n");
    assert!(err.contains("null pointer"), "stderr: {err}");
}

#[test]
fn ffi_extern_struct_native_matches_jit() {
    // The whole extern-struct surface must behave identically JIT and native.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               extern function memcpy(dst: *Pair, src: *Pair, n: u64): *Pair;\n\
               @Union\n\
               extern struct FloatBits { f: f32, i: u32 }\n\
               function main() {\n\
                 var x = Pair { a: 3, b: 4 };\n\
                 var y = Pair { a: 0, b: 0 };\n\
                 memcpy(&y, &x, 16u64);\n\
                 var fb = FloatBits { f: 2.0f32 };\n\
                 println(\"${y.a + y.b} ${fb.i}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // 3+4=7; bits of 2.0f32 = 0x40000000 = 1073741824.
    assert_eq!(nat, "7 1073741824\n");
}

#[test]
fn ffi_extern_var_read_and_write() {
    // `docs/19` §4: an `extern var` is a C global. `optind` (from getopt) is
    // defined by libc and initialized to 1 before any getopt call; reading it
    // yields 1, and assigning writes through the global.
    let src = "extern var optind: i32;\n\
               function main() {\n\
                 println(\"${optind}\");\n\
                 optind = 5i32;\n\
                 println(\"${optind}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "1\n5\n");
}

#[test]
fn ffi_extern_var_native_matches_jit() {
    let src = "extern var optind: i32;\n\
               function main() { optind = 9i32; println(\"${optind}\"); }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "9\n");
}

#[test]
fn ffi_fixed_array_field_get_set_and_zero_init() {
    // `docs/19` §4: a fixed array `[T; N]` is a valid extern struct field;
    // `arr[i]` reads/writes elements; an omitted field zero-inits the C block.
    let src = "extern struct Buf { len: u8, data: [u8; 4] }\n\
               function main() {\n\
                 var b = Buf { len: 0u8 };\n\
                 println(\"${b.data[0]} ${b.data[3]}\");\n\
                 b.data[0] = 65u8;\n\
                 b.data[3] = 90u8;\n\
                 b.len = 2u8;\n\
                 println(\"${b.data[0]} ${b.data[3]} len=${b.len}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "0 0\n65 90 len=2\n");
}

#[test]
fn ffi_fixed_array_element_address_memcpy() {
    // `&arr[i]` is the address of a fixed-array element — pass it to a C
    // function. `memcpy` fills the array from a known byte pattern.
    let src = "extern struct Buf { len: u8, data: [u8; 4] }\n\
               extern struct Word { v: i64 }\n\
               extern function memcpy(dst: *u8, src: *u8, n: u64): *u8;\n\
               function main() {\n\
                 var w = Word { v: 0x04030201 };\n\
                 var b = Buf { len: 0u8 };\n\
                 memcpy((&b.data[0]) as *u8, (&w) as *u8, 4u64);\n\
                 println(\"${b.data[0]} ${b.data[1]} ${b.data[2]} ${b.data[3]}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "1 2 3 4\n");
}

#[test]
fn ffi_fixed_array_survives_gc_stress() {
    let src = "extern struct Buf { tag: u32, data: [i64; 3] }\n\
               function main() {\n\
                 var total = 0;\n\
                 var i = 0;\n\
                 while i < 150 {\n\
                   var b = Buf { tag: 0u32 };\n\
                   b.data[0] = i;\n\
                   b.data[1] = i + 1;\n\
                   b.data[2] = i + 2;\n\
                   var s = \"row ${b.data[0]}\";\n\
                   total = total + b.data[0] + b.data[1] + b.data[2];\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"${total}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // sum over i in 0..150 of (3i+3) = 3*(sum i) + 450 = 3*11175 + 450 = 33975.
    assert_eq!(out, "33975\n");
}

#[test]
fn stdlib_str_index_of() {
    // `docs/18`: `str.index_of(s): i64 | null` — byte index or null if absent.
    let src = "function main() {\n\
                 var i = \"hello world\".index_of(\"world\");\n\
                 if i is i64 { println(\"${i as i64}\"); }\n\
                 println(\"${\"abc\".index_of(\"z\") is null}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "6\ntrue\n");
}

#[test]
fn stdlib_list_insert_and_remove() {
    // `docs/18`: `List.insert(i, v)` shifts right; `List.remove(i): T | null`.
    let src = "function main() {\n\
                 var xs = [1, 2, 4];\n\
                 xs.insert(2, 3);\n\
                 println(\"${xs[0]} ${xs[1]} ${xs[2]} ${xs[3]}\");\n\
                 var r = xs.remove(0);\n\
                 if r is i64 { println(\"${r as i64} ${xs.size()}\"); }\n\
                 println(\"${xs.remove(99) is null}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "1 2 3 4\n1 3\ntrue\n");
}

#[test]
fn stdlib_str_repeat_and_replace() {
    // `docs/18`: `str.repeat(n)` and `str.replace(old, new)`.
    let src = "function main() {\n\
                 println(\"ab\".repeat(3));\n\
                 println(\"a,b,a\".replace(\"a\", \"X\"));\n\
                 println(\"x\".repeat(0));\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "ababab\nX,b,X\n\n");
}

#[test]
fn stdlib_list_pop_and_clear() {
    // `docs/18`: `List.pop(): T | null` and `List.clear()`.
    let src = "function main() {\n\
                 var xs = [10, 20, 30];\n\
                 var last = xs.pop();\n\
                 if last is i64 { println(\"${last as i64} ${xs.size()}\"); }\n\
                 xs.clear();\n\
                 println(\"${xs.size()}\");\n\
                 var empty: List<i64> = [];\n\
                 println(\"${empty.pop() is null}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "30 2\n0\ntrue\n");
}

#[test]
fn stdlib_str_split_and_get() {
    // `docs/18` §4: `str.split(sep): List<str>` and `str.get(i): char | null`.
    let src = "function main() {\n\
                 var ps = \"a,b,c\".split(\",\");\n\
                 println(\"${ps.size()}\");\n\
                 for p in ps { println(p); }\n\
                 println(\"${\"hi\".split(\"\").size()}\");\n\
                 var g = \"abc\".get(1);\n\
                 if g is char { println(\"${g as char}\"); }\n\
                 println(\"${\"x\".get(9) is null}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "3\na\nb\nc\n2\nb\ntrue\n");
}

#[test]
fn stdlib_str_split_survives_gc_stress() {
    // The split result list + its element strings are freshly allocated; a
    // collection mid-build must not reclaim them.
    let src = "function main() {\n\
                 var total = 0;\n\
                 var i = 0;\n\
                 while i < 50 {\n\
                   var ps = \"one two three four\".split(\" \");\n\
                   total = total + ps.size();\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"${total}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "200\n");
}

#[test]
fn stdlib_list_truncate() {
    // `docs/18` §5: `List.truncate(n)` shortens to at most `n` (no-op if larger).
    let src = "function main() {\n\
                 var xs = [1, 2, 3, 4, 5];\n\
                 xs.truncate(2);\n\
                 println(\"${xs.size()}\");\n\
                 xs.truncate(99);\n\
                 println(\"${xs.size()} ${xs[0]} ${xs[1]}\");\n\
                 xs.truncate(0);\n\
                 println(\"${xs.size()}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "2\n2 1 2\n0\n");
}

#[test]
fn stdlib_list_contains_and_index_of_primitives() {
    // `docs/18` §5: `List.contains(v): bool` / `index_of(v): i64 | null` over
    // `i64` (intrinsic `Eq`) and `str` (content equality).
    let src = "function main() {\n\
                 var ys = [10, 20, 30];\n\
                 println(\"${ys.contains(20)} ${ys.contains(99)}\");\n\
                 var ix = ys.index_of(30);\n\
                 if ix is i64 { println(\"${ix as i64}\"); }\n\
                 println(\"${ys.index_of(7) is null}\");\n\
                 var ss: List<str> = [\"foo\", \"bar\"];\n\
                 println(\"${ss.contains(\"bar\")} ${ss.contains(\"zzz\")}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "true false\n2\ntrue\ntrue false\n");
}

#[test]
fn stdlib_list_contains_user_eq_type_under_gc_stress() {
    // Element equality on a user type dispatches through its `Eq` impl; managed
    // elements must stay rooted across the search safepoints.
    let src = "@Derive(Eq)\n\
               struct Point(i64, i64)\n\
               function main() {\n\
                 var ps: List<Point> = [Point(1, 2), Point(3, 4)];\n\
                 println(\"${ps.contains(Point(3, 4))} ${ps.contains(Point(9, 9))}\");\n\
                 var ix = ps.index_of(Point(1, 2));\n\
                 if ix is i64 { println(\"${ix as i64}\"); }\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true false\n0\n");
}

#[test]
fn stdlib_list_contains_requires_eq() {
    // A non-`Eq` element type is rejected at type-check time.
    let src = "struct Bare(i64)\n\
               function main() {\n\
                 var ps: List<Bare> = [Bare(1)];\n\
                 println(\"${ps.contains(Bare(1))}\");\n\
               }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(
        err.contains("requires the element type to implement `Eq`"),
        "got: {err}"
    );
}

#[test]
fn async_anf_hoists_nested_awaits() {
    // `docs/21`: `await` in operand positions (call args, binary operands,
    // index) is hoisted into preceding `var` bindings, preserving evaluation
    // order, so the async state machine can suspend at every `await`.
    let src = "function id(x: i64): Future<i64> async { x }\n\
               function add(a: i64, b: i64): Future<i64> async { a + b }\n\
               function compute(): Future<i64> async {\n\
                 var a = await add(await id(10), await id(20));\n\
                 var b = await id(5) + await id(7);\n\
                 var xs: List<i64> = [100, 200, 300];\n\
                 var c = xs[await id(1)];\n\
                 var d = await add(await id(a), await id(b));\n\
                 a + b + c + d\n\
               }\n\
               function main(): Future<null> async {\n\
                 var r = await compute();\n\
                 println(\"${r}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "284\n"); // 30 + 12 + 200 + 42
}

#[test]
fn async_anf_in_conditions_and_aggregates() {
    // `await` in an `if` condition, `struct` field, tuple, and string
    // interpolation are all unconditional positions and are hoisted; the
    // program runs end to end (GC-stress to exercise managed temporaries).
    let src = "function tf(): Future<bool> async { true }\n\
               function ti(): Future<i64> async { 42 }\n\
               struct P { x: i64, y: i64 }\n\
               function compute(): Future<i64> async {\n\
                 var acc = 0;\n\
                 if await tf() { acc = acc + 1; }\n\
                 var p = P { x: await ti(), y: await ti() };\n\
                 acc = acc + p.x + p.y;\n\
                 var t = (await ti(), await ti());\n\
                 acc + t.0 + t.1\n\
               }\n\
               function main(): Future<null> async {\n\
                 var r = await compute();\n\
                 println(\"${r}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "169\n"); // 1 + 42 + 42 + 42 + 42
}

#[test]
fn async_timeout_value_and_timed_out() {
    // `docs/21` §9: `timeout(fut, ms): Future<T | TimedOut>` resolves to the
    // value when the future wins the race, and to `TimedOut` when the deadline
    // does — the success value is reboxed into the `T` variant.
    let src = "function slow(): Future<i64> async { var _ = await sleep(60); 99 }\n\
               function fast(): Future<i64> async { 42 }\n\
               function main(): Future<null> async {\n\
                 var r1 = await timeout(fast(), 100);\n\
                 match r1 { i64 n => println(\"v ${n}\"), TimedOut => println(\"to\") }\n\
                 var r2 = await timeout(slow(), 5);\n\
                 match r2 { i64 n => println(\"v ${n}\"), TimedOut => println(\"to\") }\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "v 42\nto\n");
}

#[test]
fn async_timeout_managed_value_survives_gc() {
    // A managed (`str`) success value is traced through the timeout future's
    // reboxing (`t_is_ptr`), so GC stress does not corrupt it.
    let src = "function greet(): Future<str> async { \"hello\" }\n\
               function main(): Future<null> async {\n\
                 var r = await timeout(greet(), 100);\n\
                 match r { str s => println(s), TimedOut => println(\"to\") }\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hello\n");
}

#[test]
fn async_closure_captures_and_suspends() {
    // `docs/21` §7: `(p) async => E` is a closure that returns a future. It
    // captures the outer environment and may `await` inside; calling it builds
    // the future, `await` drives it. GC-stress exercises managed state.
    let src = "function id(x: i64): Future<i64> async { x }\n\
               function main(): Future<null> async {\n\
                 var base = 100;\n\
                 var f = (x: i64): Future<i64> async => {\n\
                   var y = await id(x);\n\
                   base + y\n\
                 };\n\
                 var r1 = await f(5);\n\
                 var r2 = await f(20);\n\
                 println(\"${r1} ${r2}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "105 120\n");
    let (gc, gerr, gok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(gok, "gc: {gerr}");
    assert_eq!(gc, "105 120\n");
}

#[test]
fn doc_generates_markdown_for_public_items() {
    // `docs/23`: `otter_fusion doc` emits Markdown for `pub` items — doc comments
    // as prose, signatures sliced from source — and omits private items.
    let src = "/// Adds two integers.\n\
               pub function add(a: i64, b: i64): i64 { a + b }\n\
               function secret(): i64 { 0 }\n\
               /// A 2-D point.\n\
               pub struct Point { x: i64, y: i64 }\n";
    let (out, err, ok) = lang("doc", src);
    assert!(ok, "stderr: {err}");
    assert!(out.contains("## function `add`"), "got:\n{out}");
    assert!(
        out.contains("pub function add(a: i64, b: i64): i64"),
        "got:\n{out}"
    );
    assert!(out.contains("Adds two integers."), "got:\n{out}");
    assert!(out.contains("## struct `Point`"), "got:\n{out}");
    assert!(out.contains("A 2-D point."), "got:\n{out}");
    // Private items are not documented, and bodies are not shown for functions.
    assert!(!out.contains("secret"), "private item leaked:\n{out}");
    assert!(!out.contains("a + b"), "function body leaked:\n{out}");
}

#[test]
fn project_manifest_resolves_entry() {
    // `docs/17` §17.1: `otter_fusion run <dir>` reads `project.toml` and runs the
    // declared (or default `src/main.otter`) entry.
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"demo\"\nkind = \"binary\"\nentry = \"src/main.otter\"\n",
            ),
            (
                "src/main.otter",
                "function main() { println(\"manifest ok\"); }",
            ),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "manifest ok\n");
}

#[test]
fn project_manifest_default_entry_and_submodule() {
    // No explicit `entry` → defaults to `src/main.otter`; the entry's `mod util;`
    // loads the sibling `src/util.otter` (`docs/17` §17.2).
    let (out, err, ok) = lang_run_project(
        "",
        &[
            (
                "project.toml",
                "[package]\nname = \"app\"\nkind = \"binary\"\n",
            ),
            (
                "src/main.otter",
                "mod util;\nimport { double } from \"self:util\";\nfunction main() { println(\"${double(21)}\"); }",
            ),
            (
                "src/util.otter",
                "pub function double(x: i64): i64 { x * 2 }",
            ),
        ],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn project_manifest_missing_entry_errors() {
    // A manifest whose entry file does not exist is a hard error.
    let (_out, err, ok) = lang_run_project(
        "",
        &[(
            "project.toml",
            "[package]\nname = \"app\"\nkind = \"binary\"\n",
        )],
    );
    assert!(!ok);
    assert!(
        err.contains("cannot read") || err.contains("main.otter"),
        "got: {err}"
    );
}

#[test]
fn interface_default_methods() {
    // `docs/10`: an interface method may carry a default body; an implementer
    // that does not override it uses the default, which can call other methods
    // through `self` (dispatching to the concrete type, incl. overrides).
    let src = "interface Greet {\n\
                 function name(self): str;\n\
                 function hello(self): str { \"Hi, \" + self.name() }\n\
                 function loud(self): str { self.hello() + \"!\" }\n\
               }\n\
               struct Cat { n: str }\n\
               struct Dog { n: str }\n\
               extend Cat: Greet { function name(self): str { self.n } }\n\
               extend Dog: Greet {\n\
                 function name(self): str { self.n }\n\
                 function hello(self): str { \"Woof \" + self.n }\n\
               }\n\
               function via_dyn(g: Greet): str { g.hello() }\n\
               function main() {\n\
                 var c = Cat { n: \"Tom\" };\n\
                 var d = Dog { n: \"Rex\" };\n\
                 println(c.hello());\n\
                 println(c.loud());\n\
                 println(d.hello());\n\
                 println(d.loud());\n\
                 println(via_dyn(c));\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "Hi, Tom\nHi, Tom!\nWoof Rex\nWoof Rex!\nHi, Tom\n");
}

#[test]
fn or_patterns_in_match() {
    // `docs/07`: `A | B | C` matches if any alternative does (alternatives must
    // not bind variables).
    let src = "function classify(n: i64): str {\n\
                 match n {\n\
                   1 | 2 | 3 => \"small\",\n\
                   10 | 20 => \"round\",\n\
                   _ => \"other\",\n\
                 }\n\
               }\n\
               function main() {\n\
                 println(classify(2));\n\
                 println(classify(20));\n\
                 println(classify(99));\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "small\nround\nother\n");
}

#[test]
fn or_pattern_binding_alternative_is_rejected() {
    let src = "function f(x: i64 | str): i64 { match x { i64 n | str n => 0, } }\n\
               function main() { println(\"${f(1)}\"); }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("may not bind variables"), "got: {err}");
}

#[test]
fn list_patterns_in_match() {
    // `docs/07`: list patterns `[]` / `[x]` / `[a, b]` / `[head, ..tail]` with a
    // length test and a `..tail` sub-list binding; `[..]` is the catch-all.
    let src = "function describe(xs: List<i64>): str {\n\
                 match xs {\n\
                   [] => \"empty\",\n\
                   [x] => \"one: ${x}\",\n\
                   [a, b] => \"two: ${a},${b}\",\n\
                   [head, ..tail] => \"head ${head} rest ${tail.size()}\",\n\
                   [..] => \"x\",\n\
                 }\n\
               }\n\
               function main() {\n\
                 println(describe([]));\n\
                 println(describe([7]));\n\
                 println(describe([3, 4]));\n\
                 println(describe([1, 2, 3, 4, 5]));\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "empty\none: 7\ntwo: 3,4\nhead 1 rest 4\n");
    // GC-stress: the `..tail` slice and managed temporaries survive.
    let (gc, gerr, gok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(gok, "gc: {gerr}");
    assert_eq!(gc, jit);
}

#[test]
fn struct_destructuring_in_var() {
    // `docs/07`: record (`Point { x, y }` / `{ x: a, .. }`) and tuple-struct
    // (`Pair(a, b)`) destructuring in `var`, including nesting and field rename.
    let src = "struct Point { x: i64, y: i64 }\n\
               struct Pair(i64, i64)\n\
               struct Wrap { p: Point, tag: str }\n\
               function main() {\n\
                 var Point { x, y } = Point { x: 3, y: 4 };\n\
                 println(\"${x} ${y}\");\n\
                 var Pair(a, b) = Pair(10, 20);\n\
                 println(\"${a} ${b}\");\n\
                 var Point { x: px, .. } = Point { x: 7, y: 9 };\n\
                 println(\"${px}\");\n\
                 var Wrap { p: Point { x: nx, y: ny }, tag } = Wrap { p: Point { x: 1, y: 2 }, tag: \"hi\" };\n\
                 println(\"${nx} ${ny} ${tag}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "3 4\n10 20\n7\n1 2 hi\n");
}

#[test]
fn struct_patterns_in_match() {
    // Struct variant patterns in `match` over a union, with field binding; the
    // two variants make the match exhaustive without a `_` arm.
    let src = "struct Circle { radius: i64 }\n\
               struct Rect(i64, i64)\n\
               function area(s: Circle | Rect): i64 {\n\
                 match s {\n\
                   Circle { radius } => radius * radius * 3,\n\
                   Rect(w, h) => w * h,\n\
                 }\n\
               }\n\
               function main() {\n\
                 println(\"${area(Circle { radius: 10 })}\");\n\
                 println(\"${area(Rect(4, 5))}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "300\n20\n");
    // GC-stress parity (managed payloads survive the tag tests).
    let (gc, gerr, gok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(gok, "gc: {gerr}");
    assert_eq!(gc, "300\n20\n");
}

#[test]
fn struct_pattern_match_non_exhaustive_is_rejected() {
    // Omitting a union variant's struct pattern is a non-exhaustive match.
    let src = "struct A { v: i64 }\n\
               struct B { v: i64 }\n\
               function f(x: A | B): i64 { match x { A { v } => v, } }\n\
               function main() { println(\"${f(A { v: 1 })}\"); }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("non-exhaustive"), "got: {err}");
}

#[test]
fn anonymous_function_expressions() {
    // `docs/09` §4: `function(params): Ret { body }` is the same kind of value
    // as an arrow closure — it captures by reference and is usable wherever a
    // closure is (here: bound, capturing, and as a `map` argument).
    let src = "function main() {\n\
                 var double = function(x: i64): i64 { x * 2 };\n\
                 println(\"${double(21)}\");\n\
                 var n = 10;\n\
                 var addn = function(y: i64): i64 { y + n };\n\
                 println(\"${addn(5)}\");\n\
                 var nums: List<i64> = [1, 2, 3];\n\
                 var doubled = nums.map(function(x: i64): i64 { x * 2 });\n\
                 println(\"${doubled[0]} ${doubled[1]} ${doubled[2]}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "42\n15\n2 4 6\n");
}

#[test]
fn anonymous_function_async() {
    // An `async` anonymous function expression returns a future when called.
    let src = "function main(): Future<null> async {\n\
                 var g = function(n: i64): Future<i64> async { n * 2 };\n\
                 var r = await g(21);\n\
                 println(\"${r}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn async_for_await_over_non_variable_stream() {
    // `docs/21` §10: `for await x in EXPR` where the stream is a call (not a
    // bare variable) — the stream is hoisted into a `var` so it survives the
    // per-iteration suspends.
    let src = "struct Range { current: i64, end: i64 }\n\
               extend Range: AsyncIterator<i64> {\n\
                 function next_async(self): Future<Item<i64> | Done> {\n\
                   async {\n\
                     if self.current >= self.end { Done {} }\n\
                     else {\n\
                       var _ = await yield_now();\n\
                       var v: i64 = self.current;\n\
                       self.current = self.current + 1;\n\
                       Item { value: v }\n\
                     }\n\
                   }\n\
                 }\n\
               }\n\
               function make(n: i64): Range { Range { current: 0, end: n } }\n\
               function main(): Future<null> async {\n\
                 var sum = 0;\n\
                 for await x in make(5) { sum = sum + x; }\n\
                 println(\"${sum}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "10\n"); // 0+1+2+3+4
}

#[test]
fn async_for_await_over_interface_object() {
    // `docs/21` §10: `for await` over an `AsyncIterator<T>` *interface object*
    // (a `dyn` value), dispatching `next_async` through the vtable.
    let src = "struct Range { current: i64, end: i64 }\n\
               extend Range: AsyncIterator<i64> {\n\
                 function next_async(self): Future<Item<i64> | Done> {\n\
                   async {\n\
                     if self.current >= self.end { Done {} }\n\
                     else {\n\
                       var _ = await yield_now();\n\
                       var v: i64 = self.current;\n\
                       self.current = self.current + 1;\n\
                       Item { value: v }\n\
                     }\n\
                   }\n\
                 }\n\
               }\n\
               function drain(s: AsyncIterator<i64>): Future<i64> async {\n\
                 var sum = 0;\n\
                 for await x in s { sum = sum + x; }\n\
                 sum\n\
               }\n\
               function main(): Future<null> async {\n\
                 var r: Range = Range { current: 0, end: 5 };\n\
                 var total = await drain(r);\n\
                 println(\"${total}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "10\n");
}

#[test]
fn async_closure_as_higher_order_argument() {
    // An async closure passed where a `(i64) => Future<i64>` is expected.
    let src = "function apply(f: (i64) => Future<i64>, x: i64): Future<i64> async {\n\
                 var r = await f(x);\n\
                 r\n\
               }\n\
               function main(): Future<null> async {\n\
                 var mult = 3;\n\
                 var r = await apply((n: i64): Future<i64> async => n * mult, 14);\n\
                 println(\"${r}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "42\n");
}

#[test]
fn async_await_in_and_right_operand_short_circuits() {
    // `await` in the *right* operand of `&&` is a genuine suspend point that runs
    // only when the left operand is `true` (`docs/21` §4). The future — and the
    // side effect inside it — must NOT run when the left short-circuits.
    let src = "function side(tag: str, b: bool): Future<bool> async {\n\
                 await yield_now();\n\
                 print(tag);\n\
                 b\n\
               }\n\
               function main(): Future<null> async {\n\
                 var a = false && await side(\"A\", true);\n\
                 var b = true && await side(\"B\", true);\n\
                 println(\"|${a}|${b}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // Only "B" prints (A short-circuits); the operator results are false / true.
    assert_eq!(nat, "B|false|true\n");
}

#[test]
fn async_await_in_or_right_operand_short_circuits() {
    // `await` in the right operand of `||` runs only when the left is `false`.
    let src = "function side(tag: str, b: bool): Future<bool> async {\n\
                 await yield_now();\n\
                 print(tag);\n\
                 b\n\
               }\n\
               function main(): Future<null> async {\n\
                 var c = true || await side(\"C\", true);\n\
                 var d = false || await side(\"D\", true);\n\
                 println(\"|${c}|${d}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // Only "D" prints (C short-circuits); results are true / true.
    assert_eq!(nat, "D|true|true\n");
}

#[test]
fn async_nested_await_in_short_circuit_operand_is_scoped() {
    // An `await` nested *inside* the right operand (not the whole operand) is
    // hoisted into a block that only runs when the operand is reached — it never
    // becomes unconditional.
    let src = "function n(tag: str, v: i64): Future<i64> async {\n\
                 await yield_now();\n\
                 print(tag);\n\
                 v\n\
               }\n\
               function main(): Future<null> async {\n\
                 var x = false && ((await n(\"X\", 3)) + 1 == 4);\n\
                 var y = true && ((await n(\"Y\", 3)) + 1 == 4);\n\
                 println(\"|${x}|${y}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // Only "Y" prints; x is false (short-circuit), y is true.
    assert_eq!(nat, "Y|false|true\n");
}

#[test]
fn async_await_in_while_condition_suspends_each_iteration() {
    // An `await` in a `while` condition suspends once per iteration: the
    // condition future is polled afresh every time the loop tests it (`docs/21`).
    let src = "function probe(i: i64): Future<i64> async {\n\
                 await yield_now();\n\
                 print(\"c\");\n\
                 i\n\
               }\n\
               function main(): Future<null> async {\n\
                 var i = 0;\n\
                 while (await probe(i)) < 3 {\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"|i=${i}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // The condition runs for i = 0,1,2,3 (four polls), then exits at i = 3.
    assert_eq!(nat, "cccc|i=3\n");
}

#[test]
fn async_await_in_while_condition_short_circuit_and_body_await() {
    // A `while` condition mixing short-circuit `&&` with a nested `await`, plus a
    // body that also awaits: the condition await only runs when the left is true.
    let src = "function probe(tag: str, i: i64): Future<i64> async {\n\
                 await yield_now();\n\
                 print(tag);\n\
                 i\n\
               }\n\
               function main(): Future<null> async {\n\
                 var i = 0;\n\
                 var sum = 0;\n\
                 while (i < 3) && ((await probe(\"c\", i)) >= 0) {\n\
                   sum = sum + (await probe(\"b\", i));\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"|sum=${sum}|i=${i}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // i=0,1,2: "cb" each; i=3: `i < 3` is false so the cond await never runs.
    assert_eq!(nat, "cbcbcb|sum=3|i=3\n");
}

#[test]
fn async_await_in_short_circuit_operand_under_unary() {
    // The block ANF introduces for a nested-`await` operand can sit under another
    // operator (`!`); the suspend site is still discovered and saved correctly.
    let src = "function n(v: i64): Future<i64> async {\n\
                 await yield_now();\n\
                 v\n\
               }\n\
               function main(): Future<null> async {\n\
                 var z = !(true && ((await n(5)) > 0));\n\
                 println(\"${z}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "false\n");
}

#[test]
fn async_await_in_sync_function_still_rejected() {
    // `await` is still only legal inside an async body — a short-circuit operand
    // does not change that.
    let src = "function tf(): Future<bool> async { true }\n\
               function compute(): bool { true && await tf() }\n\
               function main(): Future<null> async { var _ = await compute(); }";
    let (_out, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(
        err.contains("`await` is only allowed inside an `async`"),
        "got: {err}"
    );
}

#[test]
fn async_await_in_and_preserves_evaluation_order() {
    // The left operand of `&&` is unconditional and evaluated *first*; the right
    // operand (carrying the `await`) runs only afterwards, and only when the left
    // is `true` (`docs/21` §4). An effectful sync left operand must therefore
    // print before the awaited side effect, and not at all when it short-circuits.
    let src = "function mark(tag: str, b: bool): bool { print(tag); b }\n\
               function side(tag: str, b: bool): Future<bool> async {\n\
                 await yield_now();\n\
                 print(tag);\n\
                 b\n\
               }\n\
               function main(): Future<null> async {\n\
                 var r1 = mark(\"L\", true) && await side(\"R\", true);\n\
                 var r2 = mark(\"M\", false) && await side(\"Q\", true);\n\
                 println(\"|${r1}|${r2}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // r1: "L" (left) then "R" (right await runs). r2: "M" only — left is false so
    // the right operand (and "Q") never runs. Order is L, R, M.
    assert_eq!(nat, "LRM|true|false\n");
}

#[test]
fn async_await_in_both_operands_of_and() {
    // `await a() && await b()`: the left `await` is unconditional (hoisted to a
    // temporary, evaluated first); the right `await` is the conditional scope and
    // runs only when the left resolves to `true`. Evaluation order is left-await
    // then right-await, and a `false` left suppresses the right entirely.
    let src = "function side(tag: str, b: bool): Future<bool> async {\n\
                 await yield_now();\n\
                 print(tag);\n\
                 b\n\
               }\n\
               function main(): Future<null> async {\n\
                 var r1 = await side(\"A\", true) && await side(\"B\", false);\n\
                 var r2 = await side(\"C\", false) && await side(\"D\", true);\n\
                 println(\"|${r1}|${r2}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // r1: A (true) then B (false) → false. r2: C (false) short-circuits, so D never
    // runs → false. Order is A, B, C.
    assert_eq!(nat, "ABC|false|false\n");
}

#[test]
fn async_await_in_chained_and_short_circuits() {
    // Left-associative `a && b && await c()` parses as `(a && b) && await c()`.
    // The `await` is in the right operand of the *outer* `&&`, so it runs only
    // when the whole left subexpression `(a && b)` is `true`.
    let src = "function side(tag: str, b: bool): Future<bool> async {\n\
                 await yield_now();\n\
                 print(tag);\n\
                 b\n\
               }\n\
               function main(): Future<null> async {\n\
                 var x = (true && false) && await side(\"X\", true);\n\
                 var y = (true && true) && await side(\"Y\", true);\n\
                 println(\"|${x}|${y}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // x: `true && false` is false → "X" suppressed → false. y: `true && true` is
    // true → "Y" runs → true.
    assert_eq!(nat, "Y|false|true\n");
}

#[test]
fn async_bare_await_as_while_condition_suspends_each_iteration() {
    // The whole `while` condition is a bare `await` (the direct-await-preserved
    // path, distinct from an `await` nested inside a larger condition). It is
    // re-polled once per iteration — including the final iteration that ends the
    // loop — and never lifted out of the loop.
    let src = "function tick(i: i64): Future<bool> async {\n\
                 await yield_now();\n\
                 print(\"t\");\n\
                 i < 3\n\
               }\n\
               function main(): Future<null> async {\n\
                 var i = 0;\n\
                 while await tick(i) {\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"|i=${i}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // tick polled at i = 0,1,2 (true) and i = 3 (false) → four suspends, "tttt".
    assert_eq!(nat, "tttt|i=3\n");
}

#[test]
fn async_await_in_or_within_while_condition() {
    // A `while` condition `(i < 3) || await more()`: the `||` right operand awaits
    // only on the iteration where the left is `false`. While `i < 3` holds the
    // left short-circuits and `more` never runs; once `i` reaches 3 the await runs
    // (returning `false`) and ends the loop.
    let src = "function more(): Future<bool> async {\n\
                 await yield_now();\n\
                 print(\"m\");\n\
                 false\n\
               }\n\
               function main(): Future<null> async {\n\
                 var i = 0;\n\
                 while (i < 3) || await more() {\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"|i=${i}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    // i = 0,1,2: left true, "m" suppressed, body runs. i = 3: left false → "m"
    // runs, returns false → exit. So "m" prints exactly once.
    assert_eq!(nat, "m|i=3\n");
}

#[test]
fn async_await_in_while_condition_managed_value_survives_gc_stress() {
    // A managed (`str`) accumulator must survive the per-iteration suspend of a
    // `while` condition whose `&&` right operand awaits — including the transient
    // allocations of the condition itself — under stress GC.
    let src = "function probe(i: i64): Future<bool> async {\n\
                 await yield_now();\n\
                 i < 3\n\
               }\n\
               function main(): Future<null> async {\n\
                 var acc: str = \"\";\n\
                 var i = 0;\n\
                 while (acc.size() < 1000) && await probe(i) {\n\
                   var garbage = \"junk\" + (i as str);\n\
                   acc = acc + \"x\" + (i as str);\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"|i=${i}|acc=${acc}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // probe true for i = 0,1,2 then false at i = 3; acc accumulates "x0x1x2".
    assert_eq!(out, "|i=3|acc=x0x1x2\n");
}

#[test]
fn stdlib_list_iter() {
    // `docs/18` §5: `List.iter(): Iterator<E>` — a cursor view driven by the
    // `Iterator` protocol.
    let src = "function main() {\n\
                 var xs = [5, 10, 15];\n\
                 var sum = 0;\n\
                 for v in xs.iter() { sum = sum + v; }\n\
                 println(\"${sum}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "30\n");
}

#[test]
fn stdlib_str_chars_and_bytes_iterators() {
    // `docs/18` §4: `str.chars(): Iterator<char>` and `bytes(): Iterator<u8>`
    // drive `for` via the standard `Iterator` protocol (snapshot-backed).
    let src = "function main() {\n\
                 var n = 0;\n\
                 for ch in \"héllo\".chars() { n = n + 1; }\n\
                 println(\"${n}\");\n\
                 var sum = 0;\n\
                 for b in \"abc\".bytes() { sum = sum + (b as i64); }\n\
                 println(\"${sum}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "5\n294\n");
}

#[test]
fn stdlib_for_char_in_str_desugars_to_chars() {
    // `docs/18` §4: `for ch in s` ≡ `for ch in s.chars()`.
    let src = "function main() {\n\
                 var sum = 0;\n\
                 for ch in \"héllo\" { sum = sum + (ch as i64); }\n\
                 println(\"${sum}\");\n\
                 var e = 0;\n\
                 for ch in \"\" { e = e + 1; }\n\
                 println(\"${e}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // h=104 é=233 l=108 l=108 o=111 → 664
    assert_eq!(out, "664\n0\n");
}

#[test]
fn ffi_transparent_newtype_has_inner_abi() {
    // `docs/19` §3: a `@Transparent` newtype has its single field's
    // representation and ABI — `Num(-5)` is just an `i32`, so it can be passed
    // to libc `abs` (declared over `Num`), and `.0` reads the inner value.
    let src = "@Transparent\n\
               struct Num(i32)\n\
               extern function abs(n: Num): i32;\n\
               function main() {\n\
                 var x = Num(-5i32);\n\
                 println(\"${x.0}\");\n\
                 println(\"${abs(x)}\");\n\
                 println(\"${abs(Num(-42i32))}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "-5\n5\n42\n");
}

#[test]
fn check_rejects_transparent_multi_field() {
    let src = "@Transparent\nstruct Bad(i32, i32)\nfunction main() {}";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("exactly one field"), "stderr: {err}");
}

#[test]
fn ffi_link_decorator_resolves_library_symbol() {
    // `docs/19` §13: `@Link(lib = "z")` makes a symbol from zlib resolvable —
    // the JIT `dlopen`s `libz`, the native build links `-lz`. `crc32` of
    // "hello" is the deterministic 0x3610A686 = 907060870.
    let src = "@Link(lib = \"z\")\n\
               extern function crc32(crc: u64, buf: *u8, len: u32): u64;\n\
               function main() {\n\
                 var s = CString.from_str(\"hello\");\n\
                 println(\"${crc32(0u64, s.as_ptr(), 5u32)}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "907060870\n");
}

#[test]
fn ffi_nested_extern_structs() {
    // `docs/19` §3: a nested `extern struct` field is laid out *inline* (its
    // bytes, not a pointer). Construction byte-copies it in; field access reads
    // through the inline offset; scalar and whole-struct mutation both work.
    // (Correct offsets prove inline layout: `stime` at +16, `maxrss` at +32.)
    let src = "extern struct Timeval { sec: i64, usec: i64 }\n\
               extern struct Rusage { utime: Timeval, stime: Timeval, maxrss: i64 }\n\
               function main() {\n\
                 var u = Rusage {\n\
                   utime: Timeval { sec: 5, usec: 100 },\n\
                   stime: Timeval { sec: 7, usec: 200 },\n\
                   maxrss: 999,\n\
                 };\n\
                 println(\"${u.utime.sec} ${u.stime.usec} ${u.maxrss}\");\n\
                 u.utime.sec = 42;\n\
                 u.stime = Timeval { sec: 11, usec: 22 };\n\
                 println(\"${u.utime.sec} ${u.stime.sec} ${u.stime.usec}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "5 200 999\n42 11 22\n");
}

#[test]
fn ffi_foreign_realloc_and_alloc_flex() {
    // `docs/19` §5: `Foreign.realloc<T>(p, new_size)` resizes a foreign block
    // preserving its bytes; `Foreign.alloc_flex<T, E>(n)` allocates
    // `sizeof(T) + n*sizeof(E)` (a flexible array member).
    let src = "extern struct Hdr { kind: u32, length: u32, data: *u8 }\n\
               function main() {\n\
                 var p = Foreign.alloc<i64>();\n\
                 if p is null { println(\"oom\"); } else {\n\
                   *p = 12345;\n\
                   var q = Foreign.realloc<i64>(p, 64usize);\n\
                   if q is null { println(\"oom\"); } else {\n\
                     println(\"keep=${*q}\");\n\
                     Foreign.free(q);\n\
                   }\n\
                 }\n\
                 var m = Foreign.alloc_flex<Hdr, u8>(8usize);\n\
                 if m is null { println(\"oom\"); } else {\n\
                   (*m).kind = 7u32;\n\
                   println(\"kind=${(*m).kind}\");\n\
                   Foreign.free(m);\n\
                 }\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "keep=12345\nkind=7\n");
}

#[test]
fn ffi_cstring_marshaling_round_trip() {
    // `docs/19` §6: `CString.from_str` marshals a `str` into an owning,
    // NUL-terminated C string (its `*u8` passed to libc `strlen` via `as_ptr`),
    // `byte_len` is C `strlen`, and `to_str` copies the C string back into a
    // managed `str`. The `CString` frees its buffer on scope exit (`Drop`).
    let src = "extern function strlen(s: *u8): u64;\n\
               function main() {\n\
                 var cs = CString.from_str(\"hello, C\");\n\
                 println(\"len=${strlen(cs.as_ptr())} blen=${cs.byte_len()}\");\n\
                 var back = cs.to_str();\n\
                 println(\"eq=${back == \"hello, C\"}\");\n\
                 var view = CStr.from_ptr(cs.as_ptr());\n\
                 println(\"view=${view.to_str()} vlen=${view.byte_len()}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "len=8 blen=8\neq=true\nview=hello, C vlen=8\n");
}

#[test]
fn ffi_cstring_survives_gc_stress() {
    // `to_str` allocates a managed `str`; the owning `CString` frees its buffer
    // on each iteration's scope exit (with the GC backstop under stress). Round-
    // trip 200 times; the foreign-block count must return to its starting value.
    let src = "extern function strlen(s: *u8): u64;\n\
               extern function lang_foreign_outstanding(): i64;\n\
               function roundtrip(): i64 {\n\
                 var total = 0;\n\
                 var i = 0;\n\
                 while i < 200 {\n\
                   var p = CString.from_str(\"item ${i}\");\n\
                   var back = p.to_str();\n\
                   total = total + (strlen(p.as_ptr()) as i64) + back.size();\n\
                   i = i + 1;\n\
                 }\n\
                 total\n\
               }\n\
               function main() {\n\
                 var base = lang_foreign_outstanding();\n\
                 var total = roundtrip();\n\
                 println(\"${total} leak=${lang_foreign_outstanding() - base}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "2980 leak=0\n");
}

#[test]
fn ffi_cstring_drop_frees_buffer() {
    // `docs/19` §6: `CString` is `@RefCounted` and frees its foreign buffer on
    // scope exit (its `Drop`). `lang_foreign_outstanding` proves the buffer was
    // released — the count returns to baseline after the owning scope ends.
    let src = "extern function lang_foreign_outstanding(): i64;\n\
               function scope() { var cs = CString.from_str(\"owned, freed on drop\"); }\n\
               function main() {\n\
                 var base = lang_foreign_outstanding();\n\
                 scope();\n\
                 println(\"leak=${lang_foreign_outstanding() - base}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "leak=0\n");
}

#[test]
fn ffi_buffer_alloc_get_set_free() {
    // `docs/18` §9: `Buffer` is an extern struct with manual lifetime. `alloc` is
    // fallible (`Buffer | null`), `get`/`set` are bounds-checked (OOB read → null,
    // OOB write → no-op), `free` releases the region. JIT + native parity.
    let src = "extern function lang_foreign_outstanding(): i64;\n\
               function main() {\n\
                 var base = lang_foreign_outstanding();\n\
                 var m = Buffer.alloc(4u64);\n\
                 if m is null { println(\"oom\"); } else {\n\
                   var b = m as Buffer;\n\
                   b.set(0u64, 72u8);\n\
                   b.set(9u64, 1u8);\n\
                   var g = b.get(0u64);\n\
                   var oob = b.get(4u64);\n\
                   println(\"g=${g as u8} oob=${oob is null} size=${b.size} live=${lang_foreign_outstanding() - base}\");\n\
                   b.free();\n\
                   println(\"freed=${lang_foreign_outstanding() - base}\");\n\
                 }\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "g=72 oob=true size=4 live=1\nfreed=0\n");
}

#[test]
fn ffi_callconv_each_convention_calls_libc() {
    // `docs/19` §7: `@CallConv("c"|"system"|"stdcall"|"fastcall")` on extern
    // imports. On 64-bit targets the four coincide with the platform C ABI; each
    // correctly calls real libc. JIT + native parity.
    let src = "@CallConv(\"c\")\n\
               extern function strlen(s: *u8): u64;\n\
               @CallConv(\"system\")\n\
               extern function strcmp(a: *u8, b: *u8): i32;\n\
               @CallConv(\"stdcall\")\n\
               extern function memcmp(a: *u8, b: *u8, n: u64): i32;\n\
               @CallConv(\"fastcall\")\n\
               extern function strncmp(a: *u8, b: *u8, n: u64): i32;\n\
               function main() {\n\
                 var a = CString.from_str(\"hello\");\n\
                 var b = CString.from_str(\"hello\");\n\
                 println(\"${strlen(a.as_ptr())} ${strcmp(a.as_ptr(), b.as_ptr())} ${memcmp(a.as_ptr(), b.as_ptr(), 5u64)} ${strncmp(a.as_ptr(), b.as_ptr(), 5u64)}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "5 0 0 0\n");
}

#[test]
fn ffi_extern_struct_by_value_union_round_trip() {
    // `docs/19` §3: a by-value extern struct boxed into a non-NPO union
    // (`Pt | null`) round-trips — its 16 bytes are copied to the managed heap,
    // not truncated to a dangling stack pointer. JIT + native parity.
    let src = "extern struct Pt { x: i64, y: i64 }\n\
               function pick(n: i64): Pt | null {\n\
                 if n < 0 { null } else { Pt { x: n, y: n * 2 } }\n\
               }\n\
               function main() {\n\
                 var a = pick(5);\n\
                 var b = pick(-1);\n\
                 if a is null { println(\"a null\"); } else { var p = a as Pt; println(\"a=${p.x},${p.y}\"); }\n\
                 println(\"b_null=${b is null}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "a=5,10\nb_null=true\n");
}

#[test]
fn ffi_foreign_alloc_free_round_trip() {
    // `docs/19` §5: `Foreign.alloc<T>()` allocates `sizeof(T)` bytes on the
    // foreign heap and returns a raw `*T | null` (NPO). Write fields through the
    // pointer, read them back, then `Foreign.free`. `alloc_zeroed` zeroes.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               function main() {\n\
                 var p = Foreign.alloc<Pair>();\n\
                 if p is null { println(\"oom\"); }\n\
                 else {\n\
                   (*p).a = 11;\n\
                   (*p).b = 22;\n\
                   println(\"${(*p).a} ${(*p).b}\");\n\
                   Foreign.free(p);\n\
                 }\n\
                 var q = Foreign.alloc_zeroed<Pair>();\n\
                 if q is null { println(\"oom\"); }\n\
                 else { println(\"z ${(*q).a} ${(*q).b}\"); Foreign.free(q); }\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "11 22\nz 0 0\n");
}

#[test]
fn ffi_foreign_alloc_survives_gc_stress() {
    // Foreign allocations are unmanaged (raw `*T | null`); the collector must
    // not trace them. Churn managed `str`s while holding a foreign pointer.
    let src = "extern struct Cell { v: i64 }\n\
               function main() {\n\
                 var sum = 0;\n\
                 var i = 0;\n\
                 while i < 200 {\n\
                   var c = Foreign.alloc<Cell>();\n\
                   if c is null { } else {\n\
                     (*c).v = i;\n\
                     var s = \"n ${i}\";\n\
                     sum = sum + (*c).v;\n\
                     Foreign.free(c);\n\
                   }\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"${sum}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // sum over 0..200 = 19900.
    assert_eq!(out, "19900\n");
}

#[test]
fn check_rejects_foreign_alloc_without_type_arg() {
    let src = "function main() { var p = Foreign.alloc(); }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("type argument"), "stderr: {err}");
}

#[test]
fn ffi_npo_nullable_pointer_malloc_round_trip() {
    // `docs/19` §2: `*T | null` is laid out as a raw nullable pointer (NPO).
    // libc `malloc` returns `*Pair | null`; an `if p is null` check narrows it,
    // and a heap write through the (reinterpreted) pointer round-trips.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               extern function malloc(n: usize): *Pair | null;\n\
               extern function free(p: *Pair);\n\
               function main() {\n\
                 var p = malloc(16usize);\n\
                 if p is null {\n\
                   println(\"oom\");\n\
                 } else {\n\
                   *((p) as *i64) = 42;\n\
                   println(\"a=${(*p).a}\");\n\
                   free(p);\n\
                 }\n\
                 var z: *Pair | null = 0usize as *Pair;\n\
                 println(\"znull=${z is null}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "a=42\nznull=true\n");
}

#[test]
fn ffi_npo_pointer_survives_gc_stress() {
    // An NPO `*T | null` value is a RAW pointer (into a stack extern struct
    // here), not a managed box — the collector must not trace it. Hold it live
    // across many managed allocations under stress.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               function main() {\n\
                 var x = Pair { a: 100, b: 200 };\n\
                 var p: *Pair | null = &x;\n\
                 var total = 0;\n\
                 var i = 0;\n\
                 while i < 300 {\n\
                   var s = \"iter ${i}\";\n\
                   if p is null { total = total + 0; } else { total = total + (*p).a; }\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"${total}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "30000\n");
}

#[test]
fn check_rejects_match_on_nullable_pointer() {
    // `match` on an NPO `*T | null` is not yet supported — use `if p is null`.
    let src = "extern struct P { a: i64 }\n\
               extern function malloc(n: usize): *P | null;\n\
               function main() {\n\
                 var p = malloc(8usize);\n\
                 match p { null => println(\"n\"), x => println(\"x\") }\n\
               }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("nullable pointer"), "stderr: {err}");
}

#[test]
fn ffi_opaque_type_handle_round_trips() {
    // `docs/19` §4: an `extern type` is an opaque C handle, used only behind a
    // pointer. `tmpfile()` returns a `*File`, which round-trips back to C
    // (`fileno`/`fclose`) — JIT and native.
    let src = "extern type File;\n\
               extern function tmpfile(): *File;\n\
               extern function fileno(f: *File): i32;\n\
               extern function fclose(f: *File): i32;\n\
               function main() {\n\
                 var f = tmpfile();\n\
                 println(\"${fileno(f) >= 0i32}\");\n\
                 println(\"${fclose(f) == 0i32}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "true\ntrue\n");
}

#[test]
fn check_rejects_deref_of_opaque_type() {
    // An opaque `extern type` has no value representation — it cannot be
    // dereferenced to a value, only passed around as `*T`.
    let src = "extern type File;\n\
               extern function tmpfile(): *File;\n\
               function main() { var f = tmpfile(); var x = *f; }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("opaque extern type"), "stderr: {err}");
}

#[test]
fn check_rejects_non_repr_c_extern_field() {
    // `docs/19` §3: extern struct fields must be C-ABI-compatible. A `str`
    // (managed) field has no sound C layout.
    let src = "extern struct Bad { name: str }\n\
               function main() {}";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("C-ABI-compatible"), "stderr: {err}");
}

#[test]
fn check_rejects_callconv_invalid_value() {
    // `docs/19` §7: `@CallConv` accepts only "c"/"system"/"stdcall"/"fastcall".
    let src = "@CallConv(\"pascal\")\n\
               extern function f(s: *u8): u64;\n\
               function main() {}";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(
        err.contains("@CallConv` takes one string argument"),
        "stderr: {err}"
    );
}

#[test]
fn check_rejects_callconv_on_non_extern() {
    // `docs/19` §7: `@CallConv` is only meaningful on an `extern function`.
    let src = "@CallConv(\"c\")\n\
               function foo() {}\n\
               function main() {}";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(
        err.contains("only valid on an `extern function`"),
        "stderr: {err}"
    );
}

#[test]
fn ffi_extern_struct_by_value_return() {
    // Returning an `extern struct` by value is supported: the value is copied to
    // a managed heap block so the pointer does not dangle past the callee's
    // frame (the same escape handling as boxing one into a union). JIT + native.
    let src = "extern struct Pair { a: i64, b: i64 }\n\
               function make(x: i64, y: i64): Pair { Pair { a: x, b: y } }\n\
               function main() {\n\
                 var p = make(11, 22);\n\
                 println(\"${p.a} ${p.b}\");\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "11 22\n");
}

#[test]
fn check_rejects_address_of_non_extern() {
    // `&` is currently limited to extern struct places.
    let src = "function main() { var n = 5; var p = &n; }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("address-of"), "stderr: {err}");
}

#[test]
fn check_rejects_deref_of_non_pointer() {
    // `*` requires a raw pointer operand.
    let src = "function main() { var n = 5; var m = *n; }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("raw pointer"), "stderr: {err}");
}

#[test]
fn check_rejects_union_on_managed_struct() {
    // The C-layout decorators only apply to `extern struct`.
    let src = "@Union\n\
               struct Bad { a: i64, b: i64 }\n\
               function main() {}";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(
        err.contains("only valid on an `extern struct`"),
        "stderr: {err}"
    );
}

#[test]
fn derive_eq_synthesizes_struct_equality() {
    // `docs/22` §11: `@Derive(Eq)` synthesises field-by-field `eq`; `==`/`!=`
    // then work on the struct (record, tuple, and unit forms).
    let src = "@Derive(Eq)\n\
               struct Point { x: i64, y: i64 }\n\
               @Derive(Eq)\n\
               struct Pair(i64, str)\n\
               @Derive(Eq)\n\
               struct Unit {}\n\
               function main() {\n\
                 println(\"${Point { x: 1, y: 2 } == Point { x: 1, y: 2 }}\");\n\
                 println(\"${Point { x: 1, y: 2 } != Point { x: 1, y: 9 }}\");\n\
                 println(\"${Pair(7, \"hi\") == Pair(7, \"hi\")}\");\n\
                 println(\"${Pair(7, \"hi\") == Pair(7, \"bye\")}\");\n\
                 println(\"${Unit {} == Unit {}}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\ntrue\ntrue\nfalse\ntrue\n");
}

#[test]
fn derive_tostr_renders_struct() {
    // `@Derive(ToStr)` synthesises a `to_str(self): str` rendering each field.
    let src = "@Derive(ToStr)\n\
               struct Point { x: i64, y: str }\n\
               @Derive(ToStr)\n\
               struct Pair(i64, bool)\n\
               function main() {\n\
                 println(Point { x: 1, y: \"hi\" }.to_str());\n\
                 println(Pair(7, true).to_str());\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "Point { x: 1, y: hi }\nPair(7, true)\n");
}

#[test]
fn derive_ord_lexicographic_comparison() {
    // `@Derive(Ord)` synthesises lexicographic `<`/`<=`/`>`/`>=` by field
    // declaration order, and implies `Eq` (`docs/22` §11).
    let src = "@Derive(Eq, Ord)\n\
               struct Version { major: i64, minor: i64 }\n\
               function main() {\n\
                 var a = Version { major: 1, minor: 2 };\n\
                 var b = Version { major: 1, minor: 5 };\n\
                 println(\"${a < b} ${b < a} ${a <= a} ${b > a} ${a >= b} ${a == a}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true false true true false true\n");
}

#[test]
fn hash_intrinsic_on_primitives() {
    // `.hash()` is intrinsic on primitives + `str` (`docs/15` §7): equal values
    // hash equally; distinct values almost certainly hash differently.
    let src = "function main() {\n\
                 println(\"${(42 as i64).hash() == (42 as i64).hash()}\");\n\
                 println(\"${\"hello\".hash() == \"hello\".hash()}\");\n\
                 println(\"${(1 as i64).hash() != (2 as i64).hash()}\");\n\
                 println(\"${true.hash() != false.hash()}\");\n\
                 println(\"${'a'.hash() != 'b'.hash()}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\ntrue\ntrue\ntrue\ntrue\n");
}

#[test]
fn derive_hash_user_keyed_map() {
    // `@Derive(Eq, Hash)` (`docs/22` §11, `docs/15` §7) makes a user struct
    // usable as a `Map<K, V>` key. Set/get/contains/remove all route through
    // the synthesised `hash`/`eq` methods (passed to the runtime as function
    // pointers when the map is constructed).
    let src = "@Derive(Eq, Hash)\n\
               struct Point { x: i64, y: i64 }\n\
               function main() {\n\
                 var m: Map<Point, str> = Map<Point, str>();\n\
                 m.set(Point { x: 1, y: 2 }, \"alpha\");\n\
                 m.set(Point { x: 3, y: 4 }, \"beta\");\n\
                 m.set(Point { x: 1, y: 2 }, \"alpha-prime\");\n\
                 println(\"size=${m.size()}\");\n\
                 var p = Point { x: 1, y: 2 };\n\
                 match m.get(p) { str s => println(s), null => println(\"missing!\") };\n\
                 var q = Point { x: 3, y: 4 };\n\
                 match m.get(q) { str s => println(s), null => println(\"missing!\") };\n\
                 var r = Point { x: 99, y: 99 };\n\
                 match m.get(r) { str s => println(s), null => println(\"not-found\") };\n\
                 println(\"contains=${m.contains(Point { x: 1, y: 2 })}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "size=2\nalpha-prime\nbeta\nnot-found\ncontains=true\n");
}

#[test]
fn derive_hash_user_keyed_map_under_gc_stress() {
    // The map handle now carries function pointers to the key type's compiled
    // `hash`/`eq` methods; under stress GC, the handle, slot buffer, and
    // managed values must survive collection across the loop.
    let src = "@Derive(Eq, Hash)\n\
               struct Key { tag: str, n: i64 }\n\
               function main() {\n\
                 var m: Map<Key, str> = Map<Key, str>();\n\
                 m.set(Key { tag: \"keep\", n: 1 }, \"first\");\n\
                 m.set(Key { tag: \"keep\", n: 2 }, \"second\");\n\
                 var i: i64 = 0;\n\
                 while i < 200 {\n\
                   var k = Key { tag: \"churn\", n: i };\n\
                   var v = \"junk\" + (i as str);\n\
                   m.set(k, v);\n\
                   i = i + 1;\n\
                 }\n\
                 match m.get(Key { tag: \"keep\", n: 1 }) { str s => println(s), null => println(\"lost\") };\n\
                 match m.get(Key { tag: \"keep\", n: 2 }) { str s => println(s), null => println(\"lost\") };\n\
                 println(\"size=${m.size()}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // Both `keep` keys preserved; size = 2 (keeps) + 200 (churn) — but the
    // last churn key (i=199) overwrites if it collides; here every churn key
    // is distinct, so 202.
    assert_eq!(out, "first\nsecond\nsize=202\n");
}

#[test]
fn derive_hash_user_keyed_map_native() {
    // JIT/native parity for user-keyed maps: native `lang build` must take the
    // same `hash`/`eq` function-pointer paths as the JIT.
    let src = "@Derive(Eq, Hash)\n\
               struct Coord(i64, i64)\n\
               function main() {\n\
                 var m: Map<Coord, i64> = Map<Coord, i64>();\n\
                 m.set(Coord(0, 0), 100);\n\
                 m.set(Coord(1, 1), 200);\n\
                 m.set(Coord(0, 0), 999);\n\
                 println(\"size=${m.size()}\");\n\
                 println(\"a=${m.get(Coord(0, 0)) as i64}\");\n\
                 println(\"b=${m.get(Coord(1, 1)) as i64}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "size=2\na=999\nb=200\n");
}

#[test]
fn closure_mutates_captured_primitive() {
    // Closures capture by reference (`docs/09` §7): a primitive captured into
    // a closure goes into a heap cell so the outer scope sees mutations.
    let src = "function main() {\n\
                 var total: i64 = 0;\n\
                 [1, 2, 3, 4, 5].each { it => total = total + it; };\n\
                 println(\"total=${total}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "total=15\n");
}

#[test]
fn closure_mutates_captured_str() {
    // Reference types are also captured by reference; reassigning the captured
    // variable (not just field mutation) is visible to the outer scope.
    let src = "function main() {\n\
                 var name: str = \"alice\";\n\
                 var set_name = (s: str): i64 => { name = s; 0 };\n\
                 set_name(\"bob\");\n\
                 println(\"name=${name}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "name=bob\n");
}

#[test]
fn multiple_closures_share_state() {
    // Two closures capturing the same local share its cell — observing each
    // other's writes — the classic counter / pair-of-getter-setter pattern.
    let src = "function main() {\n\
                 var n: i64 = 0;\n\
                 var inc = (by: i64): i64 => { n = n + by; n };\n\
                 var bump_then_read = (extra: i64): i64 => { n = n + extra; n };\n\
                 inc(3);\n\
                 inc(4);\n\
                 var combined: i64 = bump_then_read(0);\n\
                 println(\"n=${combined}\");\n\
                 println(\"direct=${n}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "n=7\ndirect=7\n");
}

#[test]
fn closure_captures_self_and_mutates_field() {
    // A method's `self` captured by an inner closure is cell-backed; the
    // closure's field writes mutate the same struct the outer scope sees.
    let src = "struct Counter { value: i64 }\n\
               extend Counter {\n\
                 function bump(self, by: i64): i64 {\n\
                   var inc = (a: i64): i64 => {\n\
                     self.value = self.value + a; self.value\n\
                   };\n\
                   inc(by);\n\
                   inc(by);\n\
                   self.value\n\
                 }\n\
               }\n\
               function main() {\n\
                 var c = Counter { value: 10 };\n\
                 println(c.bump(3) as str);\n\
                 println(c.value as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "16\n16\n");
}

#[test]
fn by_ref_captures_survive_gc_stress() {
    // The cells holding captured primitives are themselves managed heap
    // objects; under stress GC they must stay live as long as the closure
    // (and the outer scope) reaches them. The cell's descriptor traces a
    // managed-pointer content (here `str`).
    let src = "function main() {\n\
                 var total: i64 = 0;\n\
                 var label: str = \"sum\";\n\
                 var add = (n: i64): i64 => {\n\
                   total = total + n;\n\
                   label = label + \"!\";\n\
                   total\n\
                 };\n\
                 var i: i64 = 0;\n\
                 while i < 50 {\n\
                   add(i);\n\
                   var junk = \"x\" + (i as str);\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"total=${total}\");\n\
                 println(\"label=${label}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // total = 0+1+...+49 = 1225; label = "sum" + "!" * 50.
    let want_label: String = (0..50).fold("sum".to_string(), |acc, _| acc + "!");
    assert_eq!(out, format!("total=1225\nlabel={}\n", want_label));
}

#[test]
fn by_ref_captures_native_build() {
    // JIT/native parity for the cell-backed local infrastructure.
    let src = "function main() {\n\
                 var acc: i64 = 0;\n\
                 [10, 20, 30].each { it => acc = acc + it; };\n\
                 println(\"acc=${acc}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "acc=60\n");
}

#[test]
fn zero_arg_closure_call_returns_value() {
    // `() => expr` followed by `f()` is a regular closure call with no args.
    // Previously the parser collapsed the call's span onto the callee Ident,
    // so `expr_types[callee.span]` was overwritten with the return type and
    // codegen lost the `Func` type — hitting "call target not lowerable".
    let src = "function main() {\n\
                 var f = () => 42;\n\
                 var r = f();\n\
                 println(\"r=${r}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "r=42\n");
}

#[test]
fn zero_arg_closure_captures_and_mutates() {
    // 0-arg closures still capture and observe outer state through the cells.
    let src = "function main() {\n\
                 var n: i64 = 0;\n\
                 var step = () => { n = n + 1; n };\n\
                 step();\n\
                 step();\n\
                 var last: i64 = step();\n\
                 println(\"n=${n} last=${last}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "n=3 last=3\n");
}

#[test]
fn zero_arg_closure_returns_str() {
    // The closure's return type is a managed `str` — exercises the
    // managed-return path of `gen_closure_call` at zero arity.
    let src = "function main() {\n\
                 var greet = () => \"hello\";\n\
                 var g: str = greet();\n\
                 println(g);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hello\n");
}

#[test]
fn zero_arg_closure_call_native_build() {
    // JIT/native parity for 0-arg closure calls.
    let src = "function main() {\n\
                 var f = () => 7;\n\
                 println(\"r=${f()}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "r=7\n");
}

#[test]
fn map_keys_returns_iterator() {
    // `Map.keys()` returns a `MapKeys<K>` that implements `Iterator<K>`
    // (`docs/18` §6) — driveable by `for k in m.keys()` and composable like
    // any other iterator.
    let src = "function main() {\n\
                 var m: Map<str, i64> = { \"a\": 1, \"b\": 2, \"c\": 3 };\n\
                 var n: i64 = 0;\n\
                 for k in m.keys() {\n\
                   if k == \"a\" { n = n + 1; }\n\
                   if k == \"b\" { n = n + 1; }\n\
                   if k == \"c\" { n = n + 1; }\n\
                 }\n\
                 println(\"matched=${n}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "matched=3\n");
}

#[test]
fn map_values_iterator_sums() {
    // `Map.values(): MapValues<V>` — `for v in m.values()` still works for
    // the existing snapshot-walking pattern.
    let src = "function main() {\n\
                 var m: Map<str, i64> = { \"a\": 10, \"b\": 20, \"c\": 30 };\n\
                 var sum: i64 = 0;\n\
                 for v in m.values() { sum = sum + v; }\n\
                 println(\"sum=${sum}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "sum=60\n");
}

#[test]
fn map_entries_iterator_yields_entry_struct() {
    // `Map.entries(): MapEntries<K, V>` — yields `Entry<K, V>` values
    // (`docs/18` §6, same struct `for entry in map` yields).
    let src = "function main() {\n\
                 var m: Map<str, i64> = { \"alpha\": 1, \"beta\": 2 };\n\
                 var saw_a: bool = false;\n\
                 var saw_b: bool = false;\n\
                 for e in m.entries() {\n\
                   if e.key == \"alpha\" { saw_a = e.value == 1; }\n\
                   if e.key == \"beta\" { saw_b = e.value == 2; }\n\
                 }\n\
                 println(\"a=${saw_a} b=${saw_b}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "a=true b=true\n");
}

#[test]
fn map_iterators_survive_gc_stress() {
    // The iterator structs hold managed `List` / `Map` references; under
    // stress GC, the snapshot list must stay live across each `next()` step,
    // and freshly-built `Entry` boxes must not be reclaimed before the body
    // observes them.
    let src = "function main() {\n\
                 var m: Map<str, str> = Map<str, str>();\n\
                 m.set(\"keep\", \"alive\");\n\
                 m.set(\"also\", \"live\");\n\
                 var i: i64 = 0;\n\
                 var saw_keep: bool = false;\n\
                 for e in m.entries() {\n\
                   if e.key == \"keep\" { saw_keep = e.value == \"alive\"; }\n\
                   var junk = \"junk\" + (i as str);\n\
                   i = i + 1;\n\
                 }\n\
                 println(\"keep=${saw_keep} loops=${i}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "keep=true loops=2\n");
}

#[test]
fn map_iterators_native_build() {
    // Native-build parity for the new iterator paths.
    let src = "function main() {\n\
                 var m: Map<i64, i64> = { 1: 100, 2: 200 };\n\
                 var s: i64 = 0;\n\
                 for e in m.entries() { s = s + e.key + e.value; }\n\
                 println(\"total=${s}\");\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    // 1 + 100 + 2 + 200 = 303
    assert_eq!(out, "total=303\n");
}

#[test]
fn generic_struct_derives_hash() {
    // `@Derive(Hash)` on a generic struct synthesises a generic
    // `extend<T: Hash> S<T>: Hash` whose `hash` body XOR-combines each field's
    // `.hash()` (dispatched through `T: Hash` per field).
    let src = "@Derive(Eq, Hash)\n\
               struct Pair<T> { a: T, b: T }\n\
               function main() {\n\
                 var p = Pair<i64> { a: 1, b: 2 };\n\
                 var q = Pair<i64> { a: 1, b: 2 };\n\
                 var r = Pair<i64> { a: 5, b: 6 };\n\
                 println(\"${p.hash() == q.hash()}\");\n\
                 println(\"${p.hash() != r.hash()}\");\n\
                 var sp = Pair<str> { a: \"hi\", b: \"yo\" };\n\
                 var sq = Pair<str> { a: \"hi\", b: \"yo\" };\n\
                 println(\"${sp.hash() == sq.hash()}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\ntrue\ntrue\n");
}

#[test]
fn logical_not_on_bool_negates() {
    // Regression: `!` on a `bool` is logical negation (0↔1), not bitwise
    // complement (which left both `!true` and `!false` truthy). `!` on an
    // integer stays bitwise (`docs/15`).
    let src = "function main() {\n\
                 var t: bool = true;\n\
                 println(\"${!t} ${!false} ${!(1 < 2)}\");\n\
                 var x: i64 = 5;\n\
                 println(\"${!x}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "false true false\n-6\n");
}

#[test]
fn interpolate_user_type_via_to_str() {
    // A user type with a `to_str(): str` method (derived or hand-written) is
    // interpolatable in a string (`docs/01` §8 ToStr protocol).
    let src = "@Derive(ToStr)\n\
               struct Point { x: i64, y: i64 }\n\
               struct Tag {}\n\
               extend Tag { function to_str(self): str { \"tag!\" } }\n\
               function main() {\n\
                 var p = Point { x: 3, y: 4 };\n\
                 println(\"p=${p} t=${Tag {}}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "p=Point { x: 3, y: 4 } t=tag!\n");
}

#[test]
fn interpolate_rejects_type_without_to_str() {
    // A struct with no `to_str` is still rejected in interpolation.
    let src = "struct Bare { x: i64 }\n\
               function main() { var b = Bare { x: 1 }; println(\"${b}\"); }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("no `to_str(): str` method"), "stderr: {err}");
}

#[test]
fn question_mark_with_from_residual_conversion() {
    // `docs/13` §4: `?` converts an error variant not in the return type via a
    // `FromResidual` impl on the return's error type.
    let src = "struct Config { ok: i64 }\n\
               struct AppError { code: i64 }\n\
               struct IoError {}\n\
               struct ParseError {}\n\
               extend AppError: FromResidual<IoError> { function from_residual(r: IoError): AppError { AppError { code: 1 } } }\n\
               extend AppError: FromResidual<ParseError> { function from_residual(r: ParseError): AppError { AppError { code: 2 } } }\n\
               function load(f: bool): i64 | IoError { if f { IoError {} } else { 10 } }\n\
               function parse(f: bool): i64 | ParseError { if f { ParseError {} } else { 20 } }\n\
               function run(a: bool, b: bool): Config | AppError {\n\
                 var x: i64 = load(a)?;\n\
                 var y: i64 = parse(b)?;\n\
                 Config { ok: x + y }\n\
               }\n\
               function code(a: bool, b: bool): i64 {\n\
                 match run(a, b) { Config c => c.ok, AppError e => 0 - e.code }\n\
               }\n\
               function main() {\n\
                 println(\"${code(false, false)} ${code(true, false)} ${code(false, true)}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    // success=30; IoError→-1; ParseError→-2
    assert_eq!(out, "30 -1 -2\n");
}

#[test]
fn try_on_wrapper_with_branch() {
    // `docs/13` §3: a non-union wrapper struct opts into `?` by implementing
    // `Try<Output, Residual>`. `branch(self)` returns the `Output | Residual`
    // union the rest of `?` then partitions.
    let src = "struct Either<T> { ok: T, err: str, has_err: bool }\n\
               extend<T> Either<T>: Try<T, str> {\n\
                 function branch(self): T | str {\n\
                   if self.has_err { self.err } else { self.ok }\n\
                 }\n\
               }\n\
               function find(n: i64): Either<i64> {\n\
                 if n < 0 { Either { ok: 0, err: \"neg\", has_err: true } }\n\
                 else { Either { ok: n + 100, err: \"\", has_err: false } }\n\
               }\n\
               function process(n: i64): i64 | str {\n\
                 var x = find(n)?;\n\
                 x + 1\n\
               }\n\
               function main() {\n\
                 match process(5) { i64 n => println(\"ok=${n}\"), str s => println(\"err=${s}\") }\n\
                 match process(0 - 1) { i64 n => println(\"ok=${n}\"), str s => println(\"err=${s}\") }\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "ok=106\nerr=neg\n");
}

#[test]
fn try_on_wrapper_with_from_residual_conversion() {
    // A wrapper's `Residual` not in R can still propagate through a
    // `FromResidual` impl on a return-type variant (`docs/13` §3 + §4).
    let src = "struct Either<T> { ok: T, err: str, has_err: bool }\n\
               extend<T> Either<T>: Try<T, str> {\n\
                 function branch(self): T | str {\n\
                   if self.has_err { self.err } else { self.ok }\n\
                 }\n\
               }\n\
               struct AppError { msg: str }\n\
               extend AppError: FromResidual<str> {\n\
                 function from_residual(r: str): AppError { AppError { msg: r } }\n\
               }\n\
               function find(n: i64): Either<i64> {\n\
                 if n < 0 { Either { ok: 0, err: \"bad\", has_err: true } }\n\
                 else { Either { ok: n * 2, err: \"\", has_err: false } }\n\
               }\n\
               function process(n: i64): i64 | AppError {\n\
                 var v = find(n)?;\n\
                 v + 10\n\
               }\n\
               function main() {\n\
                 match process(5) {\n\
                   i64 n => println(\"ok=${n}\"),\n\
                   AppError e => println(\"err=${e.msg}\"),\n\
                 }\n\
                 match process(0 - 1) {\n\
                   i64 n => println(\"ok=${n}\"),\n\
                   AppError e => println(\"err=${e.msg}\"),\n\
                 }\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "ok=20\nerr=bad\n");
}

#[test]
fn try_on_wrapper_native_build() {
    // JIT/native parity for the new `Try` lowering.
    let src = "struct Wrap<T> { ok: T, fail: bool }\n\
               extend<T> Wrap<T>: Try<T, str> {\n\
                 function branch(self): T | str {\n\
                   if self.fail { \"err\" } else { self.ok }\n\
                 }\n\
               }\n\
               function load(n: i64): Wrap<i64> { Wrap { ok: n, fail: false } }\n\
               function go(n: i64): i64 | str { var v = load(n)?; v * 3 }\n\
               function main() {\n\
                 match go(7) { i64 n => println(\"v=${n}\"), str s => println(\"e=${s}\") }\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "v=21\n");
}

#[test]
fn try_on_plain_type_errors() {
    // A non-union with no `Try` impl can't have `?` applied to it.
    let src = "function get(): i64 { 5 }\n\
               function f(): str { var n = get()?; \"hi\" }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected an error");
    assert!(err.contains("propagate"), "stderr: {err}");
}

#[test]
fn check_clean_program_succeeds() {
    let (out, _, ok) = lang("check", "function main() { var x: i64 = 1 + 2; }");
    assert!(ok);
    assert!(out.contains("ok"));
}

// -- async (docs/21) ---------------------------------------------------------

#[test]
fn async_fn_driven_by_async_main() {
    // An async function lowers to a Future state machine; an async main awaits
    // it directly. (No `await` in `answer`'s body — the core pipeline slice.)
    let src = "function answer(): Future<i64> async { 40 + 2 }\n\
               function main(): Future<null> async {\n\
                 var x: i64 = await answer();\n\
                 println(x as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn async_block_captures_and_async_main_awaits() {
    // A bare `async { … }` block is a zero-arg inline future literal that
    // captures enclosing locals; an async main awaits it.
    let src = "function main(): Future<null> async {\n\
                 var n: i64 = 40;\n\
                 var f: i64 = await async { n + 2 };\n\
                 println(f as str);\n\
                 var g: str = await async { \"async str\" };\n\
                 println(g);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\nasync str\n");
}

#[test]
fn async_main_awaited_str_survives_gc_stress() {
    // The future's managed (str) result must survive collections triggered
    // during the poll/await machinery.
    let src = "function greet(name: str): Future<str> async { \"hi, \" + name }\n\
               function main(): Future<null> async {\n\
                 var s: str = await greet(\"world\");\n\
                 println(s);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hi, world\n");
}

#[test]
fn async_native_build_matches_jit() {
    let src = "function answer(): Future<i64> async { 42 }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await answer();\n\
                 println(v as str);\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "42\n");
}

#[test]
fn await_chains_async_functions() {
    // `outer` awaits `inner` twice; each await polls the inner future, unwraps
    // its Ready value, and continues the state machine.
    let src = "function inner(): Future<i64> async { 5 }\n\
               function outer(): Future<i64> async {\n\
                 var x: i64 = await inner();\n\
                 var y: i64 = await inner();\n\
                 x + y + 1\n\
               }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await outer();\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "11\n");
}

#[test]
fn await_in_control_flow_and_managed_results() {
    let src = "function fetch(name: str): Future<str> async { \"data:\" + name }\n\
               function small(): Future<i64> async { 7 }\n\
               function pick(c: bool): Future<i64> async {\n\
                 var r: i64 = 0;\n\
                 if c { var a: i64 = await small(); r = a + 10; } else { r = await small(); }\n\
                 r\n\
               }\n\
               function gather(): Future<str> async {\n\
                 var s: str = await fetch(\"x\");\n\
                 var n: i64 = await pick(true);\n\
                 \"${s} n=${n}\"\n\
               }\n\
               function main(): Future<null> async {\n\
                 var s: str = await gather();\n\
                 println(s);\n\
                 var v: i64 = await pick(false);\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "data:x n=17\n7\n");
}

#[test]
fn await_genuine_suspension_via_yield_under_gc_stress() {
    // `yield_now()` returns Pending once (parking the executor) then Ready. In a
    // loop this forces the state machine to suspend, the executor to park and be
    // re-woken, and resumption to reload the loop's locals — three times.
    let src = "function counter(): Future<i64> async {\n\
                 var sum: i64 = 0;\n\
                 var i: i64 = 0;\n\
                 while i < 3 {\n\
                   var _ = await yield_now();\n\
                   sum = sum + i;\n\
                   i = i + 1;\n\
                 }\n\
                 sum\n\
               }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await counter();\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3\n");
    // Native parity.
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nat, "3\n");
}

#[test]
fn await_inside_async_block() {
    // An async main awaits an inline `async { await … }` block — the
    // root future is driven by the internal executor entry.
    let src = "function tick(): Future<i64> async { var _ = await yield_now(); 7 }\n\
               function main(): Future<null> async {\n\
                 var r: i64 = await async {\n\
                   var a: i64 = await tick();\n\
                   var b: i64 = await tick();\n\
                   a + b + 100\n\
                 };\n\
                 println(r as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "114\n");
}

#[test]
fn async_spawn_drives_futures_on_workers() {
    // `spawn EXPR` (keyword) schedules a future on a worker and itself
    // evaluates to a `Future<T>` — awaiting it yields T directly (`docs/21`).
    let src = "function work(n: i64): Future<i64> async { var _ = await yield_now(); n * n }\n\
               function main(): Future<null> async {\n\
                 var h: Future<i64> = spawn work(6);\n\
                 var h2: Future<i64> = spawn work(7);\n\
                 var a: i64 = await h;\n\
                 var b: i64 = await h2;\n\
                 println(a as str);\n\
                 println(b as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "36\n49\n");
}

#[test]
fn for_await_over_async_iterator() {
    // `for await` drives an `AsyncIterator` whose `next_async` returns an
    // `async { … }` future that suspends (yield_now) and mutates captured `self`.
    let src = "struct Counter { current: i64, end: i64 }\n\
               extend Counter: AsyncIterator<i64> {\n\
                 function next_async(self): Future<Item<i64> | Done> {\n\
                   async {\n\
                     if self.current >= self.end { Done {} }\n\
                     else {\n\
                       var _ = await yield_now();\n\
                       var v: i64 = self.current;\n\
                       self.current = self.current + 1;\n\
                       Item { value: v }\n\
                     }\n\
                   }\n\
                 }\n\
               }\n\
               function sum_stream(c: Counter): Future<i64> async {\n\
                 var total: i64 = 0;\n\
                 for await n in c { total = total + n; }\n\
                 total\n\
               }\n\
               function main(): Future<null> async {\n\
                 var c = Counter { current: 0, end: 5 };\n\
                 var v: i64 = await sum_stream(c);\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "10\n");
}

#[test]
fn async_sleep_completes_after_delay() {
    // `sleep(ms)` is a `Future<null>` driven by a timer thread that wakes the
    // executor; awaiting it suspends until the delay elapses.
    let src = "function delayed(x: i64): Future<i64> async { var _ = await sleep(5); x * 2 }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await delayed(21);\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn native_build_async_stdio_write_matches_jit() {
    // Async stdio writes are runtime-built futures: poll registers a reactor
    // waiter, a helper performs the blocking stream operation, and readiness
    // wakes the executor task. Cover both JIT symbol registration and native
    // linking.
    let src = "import { Bytes } from \"std:bytes\";\n\
               import { stdout, stderr } from \"std:io\";\n\
               function main(): Future<null> async {\n\
                 var out = stdout();\n\
                 var err = stderr();\n\
                 var a = await out.write_all_async(Bytes.from_str(\"async-out\\n\"));\n\
                 var b = await out.flush_async();\n\
                 var c = await err.write_all_async(Bytes.from_str(\"async-err\\n\"));\n\
                 var d = await err.flush_async();\n\
                 println(\"${a is null} ${b is null} ${c is null} ${d is null}\");\n\
               }";
    let (jit_out, jit_err, jit_ok) = lang("run", src);
    assert!(jit_ok, "stderr: {jit_err}");
    assert_eq!(jit_out, "async-out\ntrue true true true\n");
    assert_eq!(jit_err, "async-err\n");

    let (native_out, native_err, native_ok) = lang_build_run(src, &[]);
    assert!(native_ok, "stderr: {native_err}");
    assert_eq!(
        native_out, jit_out,
        "native async stdio stdout diverged from JIT"
    );
    assert_eq!(
        native_err, jit_err,
        "native async stdio stderr diverged from JIT"
    );
}

#[test]
fn async_cancel_is_a_safe_noop() {
    // `.cancel()` on a compute-only future releases nothing and is repeatable.
    let src = "function task(): Future<i64> async { var _ = await yield_now(); 5 }\n\
               function main(): Future<null> async {\n\
                 var f: Future<i64> = task();\n\
                 f.cancel();\n\
                 f.cancel();\n\
                 var v: i64 = await task();\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "5\n");
}

#[test]
fn check_rejects_forgotten_future() {
    // docs/21 §5: a Future created as a statement and discarded is an error.
    let src = "function answer(): Future<i64> async { 1 }\n\
               function main() { answer(); }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("never used"), "stderr: {err}");
}

// --- Dependency / lockfile commands (docs/23 §3, §7) ------------------------

#[test]
fn dep_add_and_remove_edit_the_manifest() {
    let root = write_tree(&[(
        "project.toml",
        "[package]\nname = \"app\"\nkind = \"binary\"\n",
    )]);
    let (_o, e, ok) = lang_in_dir(&root, &["add", "leftpad", "1.2"]);
    assert!(ok, "stderr: {e}");
    let manifest = std::fs::read_to_string(root.join("project.toml")).unwrap();
    assert!(
        manifest.contains("leftpad = \"1.2\""),
        "manifest: {manifest}"
    );

    let (_o, e, ok) = lang_in_dir(&root, &["remove", "leftpad"]);
    assert!(ok, "stderr: {e}");
    let manifest = std::fs::read_to_string(root.join("project.toml")).unwrap();
    assert!(!manifest.contains("leftpad"), "manifest: {manifest}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dep_add_path_form() {
    let root = write_tree(&[(
        "project.toml",
        "[package]\nname = \"app\"\nkind = \"binary\"\n",
    )]);
    let (_o, e, ok) = lang_in_dir(&root, &["add", "mylib", "--path", "../mylib"]);
    assert!(ok, "stderr: {e}");
    let manifest = std::fs::read_to_string(root.join("project.toml")).unwrap();
    assert!(
        manifest.contains("mylib = { path = \"../mylib\" }"),
        "manifest: {manifest}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dep_lock_tree_and_why_with_a_path_dep() {
    // app depends on a sibling path library; lock + tree + why work offline.
    let root = write_tree(&[
        (
            "app/project.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [dependencies]\nmylib = { path = \"../mylib\" }\n",
        ),
        ("app/src/main.otter", "function main() {}"),
        (
            "mylib/project.toml",
            "[package]\nname = \"mylib\"\nversion = \"0.4.2\"\nkind = \"library\"\n",
        ),
        ("mylib/src/lib.otter", "pub function f(): i64 { 1 }"),
    ]);
    let app = root.join("app");

    let (_o, e, ok) = lang_in_dir(&app, &["lock"]);
    assert!(ok, "stderr: {e}");
    let lock = std::fs::read_to_string(app.join("project.lock")).unwrap();
    assert!(lock.contains("name     = \"mylib\""), "lock: {lock}");
    assert!(
        lock.contains("source   = \"path+../mylib\""),
        "lock: {lock}"
    );

    let (out, e, ok) = lang_in_dir(&app, &["tree"]);
    assert!(ok, "stderr: {e}");
    assert!(out.starts_with("app\n"), "tree: {out}");
    assert!(out.contains("mylib v0.4.2"), "tree: {out}");

    let (out, e, ok) = lang_in_dir(&app, &["why", "mylib"]);
    assert!(ok, "stderr: {e}");
    assert!(out.contains("app → mylib"), "why: {out}");

    // `lock --check` now succeeds (the lockfile is up to date).
    let (_o, _e, ok) = lang_in_dir(&app, &["lock", "--check"]);
    assert!(ok);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dep_lock_check_detects_drift() {
    let root = write_tree(&[
        (
            "app/project.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [dependencies]\nmylib = { path = \"../mylib\" }\n",
        ),
        ("app/src/main.otter", "function main() {}"),
        (
            "mylib/project.toml",
            "[package]\nname = \"mylib\"\nversion = \"0.4.2\"\nkind = \"library\"\n",
        ),
        ("mylib/src/lib.otter", "pub function f(): i64 { 1 }"),
    ]);
    let app = root.join("app");
    // No lockfile yet → `lock --check` must fail (it would write one).
    let (_o, e, ok) = lang_in_dir(&app, &["lock", "--check"]);
    assert!(!ok);
    assert!(e.contains("out of date"), "stderr: {e}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pkg_import_from_a_path_dependency_runs_end_to_end() {
    // `app` depends on the sibling library `greeter`; `pkg:greeter` binds its
    // public `greet` and the program runs (`docs/17` §17.4).
    let root = write_tree(&[
        (
            "app/project.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [dependencies]\ngreeter = { path = \"../greeter\" }\n",
        ),
        (
            "app/src/main.otter",
            "import { greet } from \"pkg:greeter\";\n\
             function main() { println(\"${greet()}\"); }",
        ),
        (
            "greeter/project.toml",
            "[package]\nname = \"greeter\"\nversion = \"0.1.0\"\nkind = \"library\"\n",
        ),
        (
            "greeter/src/lib.otter",
            "pub function greet(): i64 { 42 }\nfunction hidden(): i64 { 0 }",
        ),
    ]);
    let (out, err, ok) = lang_in_dir(&root.join("app"), &["run"]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pkg_import_of_a_private_item_is_rejected_e2e() {
    let root = write_tree(&[
        (
            "app/project.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [dependencies]\ngreeter = { path = \"../greeter\" }\n",
        ),
        (
            "app/src/main.otter",
            "import { hidden } from \"pkg:greeter\";\nfunction main() {}",
        ),
        (
            "greeter/project.toml",
            "[package]\nname = \"greeter\"\nversion = \"0.1.0\"\nkind = \"library\"\n",
        ),
        (
            "greeter/src/lib.otter",
            "pub function greet(): i64 { 1 }\nfunction hidden(): i64 { 0 }",
        ),
    ]);
    let (_o, err, ok) = lang_in_dir(&root.join("app"), &["run"]);
    assert!(!ok);
    assert!(err.contains("`hidden` is private"), "stderr: {err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pkg_import_subpath_through_pub_mod_runs() {
    // `pkg:greeter/text` reaches a `pub mod` submodule of the dependency.
    let root = write_tree(&[
        (
            "app/project.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [dependencies]\ngreeter = { path = \"../greeter\" }\n",
        ),
        (
            "app/src/main.otter",
            "import { shout } from \"pkg:greeter/text\";\n\
             function main() { println(\"${shout()}\"); }",
        ),
        (
            "greeter/project.toml",
            "[package]\nname = \"greeter\"\nversion = \"0.1.0\"\nkind = \"library\"\n",
        ),
        ("greeter/src/lib.otter", "pub mod text;\n"),
        ("greeter/src/text.otter", "pub function shout(): i64 { 99 }"),
    ]);
    let (out, err, ok) = lang_in_dir(&root.join("app"), &["run"]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "99\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn login_and_logout_manage_credentials() {
    let home = write_tree(&[]);
    let proj = write_tree(&[(
        "project.toml",
        "[package]\nname = \"app\"\nkind = \"binary\"\n",
    )]);
    let home_s = home.to_str().unwrap();
    let (_o, e, ok) = lang_in_dir_env(
        &proj,
        &["login", "--token", "secret-abc", "--registry", "public"],
        &[("OTTER_FUSION_HOME", home_s)],
    );
    assert!(ok, "stderr: {e}");
    let creds = std::fs::read_to_string(home.join("credentials.toml")).unwrap();
    assert!(creds.contains("[registries.public]"), "creds: {creds}");
    assert!(creds.contains("token = \"secret-abc\""), "creds: {creds}");

    let (_o, e, ok) = lang_in_dir_env(
        &proj,
        &["logout", "--registry", "public"],
        &[("OTTER_FUSION_HOME", home_s)],
    );
    assert!(ok, "stderr: {e}");
    let creds = std::fs::read_to_string(home.join("credentials.toml")).unwrap();
    assert!(!creds.contains("secret-abc"), "creds: {creds}");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn vendor_copies_a_path_dependency() {
    let root = write_tree(&[
        (
            "app/project.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n\
             [dependencies]\nmylib = { path = \"../mylib\" }\n",
        ),
        ("app/src/main.otter", "function main() {}"),
        (
            "mylib/project.toml",
            "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nkind = \"library\"\n",
        ),
        ("mylib/src/lib.otter", "pub function f(): i64 { 1 }"),
    ]);
    let (out, e, ok) = lang_in_dir(&root.join("app"), &["vendor"]);
    assert!(ok, "stderr: {e}");
    assert!(out.contains("vendored 1 package"), "out: {out}");
    assert!(root.join("app/vendor/mylib/src/lib.otter").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn publish_dry_run_packages_a_library() {
    let root = write_tree(&[
        (
            "project.toml",
            "[package]\nname = \"mylib\"\nversion = \"2.1.0\"\nkind = \"library\"\n",
        ),
        ("src/lib.otter", "pub function f(): i64 { 1 }"),
    ]);
    let (out, e, ok) = lang_in_dir(&root, &["publish", "--dry-run"]);
    assert!(ok, "stderr: {e}");
    assert!(out.contains("packaged mylib v2.1.0"), "out: {out}");
    assert!(out.contains("sha256:"), "out: {out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn publish_rejects_a_binary_package() {
    let root = write_tree(&[
        (
            "project.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nkind = \"binary\"\n",
        ),
        ("src/main.otter", "function main() {}"),
    ]);
    let (_o, e, ok) = lang_in_dir(&root, &["publish", "--dry-run"]);
    assert!(!ok);
    assert!(
        e.contains("only library packages can be published"),
        "stderr: {e}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// -- `otter_fusion test` (the test framework, `docs/23`) ----------------------

#[test]
fn test_runner_reports_pass_and_fail() {
    // Three tests: two pass, one panics. The runner runs each in its own process,
    // reports per-test status + a summary, and exits non-zero because one failed.
    let src = "\
        function add(a: i64, b: i64): i64 { a + b }\n\
        test \"addition\" { if add(2, 3) != 5 { panic(\"bad\"); } }\n\
        test \"commutative\" { if add(1, 2) != add(2, 1) { panic(\"bad\"); } }\n\
        test \"deliberately broken\" { if add(2, 2) != 5 { panic(\"2+2 is not 5\"); } }\n";
    let (out, _err, ok) = lang("test", src);
    assert!(
        !ok,
        "suite with a failing test must exit non-zero; out: {out}"
    );
    assert!(out.contains("test addition ... ok"), "out: {out}");
    assert!(out.contains("test commutative ... ok"), "out: {out}");
    assert!(
        out.contains("test deliberately broken ... FAILED"),
        "out: {out}"
    );
    assert!(out.contains("2 passed; 1 failed"), "out: {out}");
    // The failing test's panic message is surfaced.
    assert!(out.contains("2+2 is not 5"), "out: {out}");
}

#[test]
fn test_runner_all_pass_exits_zero() {
    let src = "\
        function sq(n: i64): i64 { n * n }\n\
        test \"squares\" { if sq(4) != 16 { panic(\"bad\"); } }\n\
        test \"zero\" { if sq(0) != 0 { panic(\"bad\"); } }\n";
    let (out, _err, ok) = lang("test", src);
    assert!(ok, "all-passing suite must exit zero; out: {out}");
    assert!(out.contains("2 passed; 0 failed"), "out: {out}");
}

#[test]
fn test_keyword_does_not_reserve_identifier() {
    // `test` is a contextual keyword: it is only special as `test "..." { }` at
    // item position, so `test` remains usable as an ordinary identifier.
    let (out, _err, ok) = lang(
        "run",
        "function main() { var test = 42; println(\"${test}\"); }",
    );
    assert!(ok, "out: {out}");
    assert_eq!(out.trim(), "42");
}

#[test]
fn bench_runner_times_and_separates_from_tests() {
    // `bench` runs only `bench` declarations (timing them); `test` runs only
    // `test` declarations. A `bench`-mode run reports ns/iter and exits zero.
    let src = "\
        function fib(n: i64): i64 { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\n\
        bench \"fib(10)\" { var r = fib(10); if r != 55 { panic(\"wrong\"); } }\n\
        test \"correctness\" { if fib(7) != 13 { panic(\"wrong\"); } }\n";
    let (out, _err, ok) = lang("bench", src);
    assert!(ok, "bench run should succeed; out: {out}");
    assert!(out.contains("running 1 bench(s)"), "out: {out}");
    assert!(
        out.contains("bench fib(10) ... ") && out.contains("ns/iter"),
        "out: {out}"
    );
    // The `test` declaration is NOT run by `bench`.
    assert!(
        !out.contains("correctness"),
        "bench must not run tests; out: {out}"
    );

    // And `test` runs only the test, not the bench.
    let (tout, _e, tok) = lang("test", src);
    assert!(tok, "out: {tout}");
    assert!(tout.contains("test correctness ... ok"), "out: {tout}");
    assert!(
        !tout.contains("fib(10)"),
        "test must not run benches; out: {tout}"
    );
}

#[test]
fn lint_flags_unused_local_and_function() {
    // `never_called` (private, uncalled) and `dead` (bound, unread) are flagged;
    // `_ignored`, `kept` (read), `used` (called), and `main` are not.
    let src = "\
        function used() { println(\"hi\"); }\n\
        function never_called(): i64 { 42 }\n\
        function main() {\n\
          var dead = 5;\n\
          var kept = 10;\n\
          var _ignored = 99;\n\
          used();\n\
          println(\"${kept}\");\n\
        }\n";
    let (out, err, ok) = lang("lint", src);
    assert!(
        ok,
        "lint is informational and must exit zero; out: {out} err: {err}"
    );
    // Diagnostics render to stderr; the count summary goes to stdout.
    assert!(err.contains("unused function `never_called`"), "err: {err}");
    assert!(err.contains("unused variable `dead`"), "err: {err}");
    assert!(
        !err.contains("`_ignored`"),
        "underscore vars are exempt; err: {err}"
    );
    assert!(
        !err.contains("`kept`") && !err.contains("`used`"),
        "err: {err}"
    );
    assert!(out.contains("2 warnings"), "out: {out}");
}

#[test]
fn lint_clean_program_has_no_warnings() {
    let (out, _e, ok) = lang("lint", "function main() { var x = 5; println(\"${x}\"); }");
    assert!(ok, "out: {out}");
    assert!(out.contains("no lint warnings"), "out: {out}");
}

#[test]
fn fix_prefixes_unused_variables() {
    // `otter_fusion fix` rewrites the file in place, renaming an unused `var` to
    // `_name`. After the fix the unused-variable lint is silenced.
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_fix_{}.otter", nonce()));
    std::fs::write(
        &path,
        pre("function main() { var gone = 5; var kept = 1; println(\"${kept}\"); }"),
    )
    .unwrap();

    // --check reports without writing.
    let mut check_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    check_cmd.arg("fix").arg(&path).arg("--check");
    let check = output_with_timeout(&mut check_cmd, cli_test_timeout()).unwrap();
    assert!(check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("would fix 1 unused"));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("var gone ="),
        "--check must not modify the file"
    );

    // Apply.
    let mut fix_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    fix_cmd.arg("fix").arg(&path);
    let fix = output_with_timeout(&mut fix_cmd, cli_test_timeout()).unwrap();
    assert!(fix.status.success());
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("var _gone ="),
        "expected the unused var renamed; got:\n{after}"
    );
    assert!(after.contains("var kept ="), "used var untouched");

    // Lint is now clean (no unused-variable warning).
    let mut lint_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    lint_cmd.arg("lint").arg(&path);
    let lint = output_with_timeout(&mut lint_cmd, cli_test_timeout()).unwrap();
    assert!(!String::from_utf8_lossy(&lint.stderr).contains("unused variable"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn fmt_reindents_and_check_gates() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_fmt_{}.otter", nonce()));
    // Deliberately mis-indented (no prelude needed — fmt is text-only).
    std::fs::write(
        &path,
        "function main() {\nvar x = 1;\n  if x > 0 {\nx = 2;\n}\n}\n",
    )
    .unwrap();

    // --check: reports it needs formatting and exits non-zero, without writing.
    let mut chk_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    chk_cmd.arg("fmt").arg(&path).arg("--check");
    let chk = output_with_timeout(&mut chk_cmd, cli_test_timeout()).unwrap();
    assert!(
        !chk.status.success(),
        "check must fail on unformatted input"
    );
    assert!(String::from_utf8_lossy(&chk.stdout).contains("need formatting"));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("\nvar x = 1;"),
        "check must not modify"
    );

    // Apply: reindents to two spaces per level.
    let mut fix_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    fix_cmd.arg("fmt").arg(&path);
    let fix = output_with_timeout(&mut fix_cmd, cli_test_timeout()).unwrap();
    assert!(fix.status.success());
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after, "function main() {\n  var x = 1;\n  if x > 0 {\n    x = 2;\n  }\n}\n",
        "got:\n{after}"
    );

    // --check now passes (idempotent / already formatted).
    let mut chk2_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    chk2_cmd.arg("fmt").arg(&path).arg("--check");
    let chk2 = output_with_timeout(&mut chk2_cmd, cli_test_timeout()).unwrap();
    assert!(chk2.status.success(), "formatted file must pass --check");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn repl_persists_state_and_recovers_from_errors() {
    use std::io::Write;
    use std::process::Stdio;
    let session = "1 + 2 * 3\nvar x = 10\nx * x\nfunction sq(n: i64): i64 { n * n }\nsq(x)\nbad_name()\nx\n:quit\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_otter_fusion"))
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn repl");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(session.as_bytes())
        .unwrap();
    let out = output_from_child_with_timeout(child, cli_test_timeout()).unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("7"), "expr eval; stdout: {stdout}");
    assert!(
        stdout.contains("100"),
        "x*x with persisted x; stdout: {stdout}"
    );
    // sq(x) = sq(10) = 100 (function persisted, binding visible)
    assert!(
        stderr.contains("cannot find value `bad_name`"),
        "error reported; stderr: {stderr}"
    );
    // After the error, `x` still evaluates — session state survived.
    assert!(
        stdout.matches("10").count() >= 1,
        "state intact after error; stdout: {stdout}"
    );
}

#[test]
fn lint_flags_unreachable_code() {
    // A statement after `return` and after a `panic` (type `never`) is dead.
    let src = "\
        function f(): i64 { return 1; var dead = 2; dead }\n\
        function g() { panic(\"x\"); println(\"never\"); }\n\
        function main() { println(\"${f()}\"); g(); }\n";
    let (_out, err, ok) = lang("lint", src);
    assert!(ok, "lint is informational; err: {err}");
    assert!(err.matches("unreachable code").count() >= 2, "err: {err}");
}

#[test]
fn explain_prints_code_explanation_and_diagnostics_carry_codes() {
    // A type mismatch is reported with its code, and `explain` elaborates it.
    let (_o, err, ok) = lang("check", "function f(): i64 { \"x\" }\nfunction main() {}");
    assert!(!ok);
    assert!(
        err.contains("error[E0006]"),
        "diagnostic should carry its code; err: {err}"
    );

    let mut explain_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    explain_cmd.arg("explain").arg("e0006");
    let out = output_with_timeout(&mut explain_cmd, cli_test_timeout()).unwrap(); // case-insensitive
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("E0006") && s.contains("type mismatch"),
        "stdout: {s}"
    );

    // Unknown code fails and lists the available codes.
    let mut bad_cmd = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
    bad_cmd.arg("explain").arg("E9999");
    let bad = output_with_timeout(&mut bad_cmd, cli_test_timeout()).unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("available codes"));
}

#[test]
fn promoted_member_diagnostics_carry_codes() {
    // The "no such member" family (method/field/struct-literal) now carries
    // stable codes, and each has an `explain` entry.
    let cases = [
        // (source, expected diagnostic code)
        (
            "struct P { x: i64 }\nfunction main() { var p = P { x: 1 }; p.nope(); }",
            "E0013",
        ),
        (
            "struct P { x: i64 }\nfunction main() { var p = P { x: 1 }; var y = p.y; }",
            "E0014",
        ),
        (
            "struct P { x: i64 }\nfunction main() { var p = P { x: 1, z: 2 }; }",
            "E0015",
        ),
        (
            "struct P { x: i64, y: i64 }\nfunction main() { var p = P { x: 1 }; }",
            "E0016",
        ),
        (
            "struct P { x: i64 }\nfunction main() { var p = P { x: 1, x: 2 }; }",
            "E0017",
        ),
        (
            "function f(v: i64 | str): i64 { match v { i64 n => n } }\nfunction main() {}",
            "E0018",
        ),
        ("function main() { break; }", "E0019"),
    ];
    for (src, code) in cases {
        let (_o, err, ok) = lang("check", src);
        assert!(!ok, "expected `{code}` source to fail:\n{src}");
        assert!(
            err.contains(&format!("error[{code}]")),
            "diagnostic should carry {code}; got:\n{err}"
        );
        // `explain <code>` succeeds and echoes the code.
        let mut command = Command::new(env!("CARGO_BIN_EXE_otter_fusion"));
        command.arg("explain").arg(code);
        let out = output_with_timeout(&mut command, cli_test_timeout()).unwrap();
        assert!(out.status.success(), "explain {code} failed");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(code),
            "explain {code} should echo the code"
        );
    }
}

// ===========================================================================
// User procedural macros (`docs/22`) — decorator form (slice 1)
// ===========================================================================

/// A decorator macro that re-emits the annotated item plus a synthesised
/// `extend` adding a `label()` method returning the type's own name. Exercises
/// `input.name()`, `input.text()`, and `ctx.parse_items` end-to-end through the
/// compile-time macro JIT.
#[test]
fn macro_decorator_generates_method() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function WithName(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var n = input.name();\n\
          ctx.parse_items(input.text() + \" extend \" + n + \" { function label(self): str { \\\"\" + n + \"\\\" } }\")\n\
        }\n\
        @WithName\n\
        struct Widget { id: i64 }\n\
        function main() { var w = Widget { id: 1 }; println(w.label()); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "Widget\n");
}

/// A macro can inspect the *syntactic shape* of its input (field count) — types
/// don't exist yet at expansion time, but structure does (`docs/22` §4).
#[test]
fn macro_inspects_struct_field_count() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Arity(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var n = input.name();\n\
          var c = input.field_count();\n\
          ctx.parse_items(input.text() + \" extend \" + n + \" { function arity(self): i64 { \" + (c as str) + \" } }\")\n\
        }\n\
        @Arity\n\
        struct Trio { a: i64, b: i64, c: i64 }\n\
        function main() { var t = Trio { a: 1, b: 2, c: 3 }; println(t.arity() as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3\n");
}

/// A decorator macro reads its invocation arguments via `ctx.arg(i)` and splices
/// the argument's source back into generated code.
#[test]
fn macro_reads_invocation_arguments() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Tag(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var n = input.name();\n\
          var t = ctx.arg(0).text();\n\
          ctx.parse_items(input.text() + \" extend \" + n + \" { function tag(self): str { \" + t + \" } }\")\n\
        }\n\
        @Tag(\"hello\")\n\
        struct S { x: i64 }\n\
        function main() { var s = S { x: 0 }; println(s.tag()); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hello\n");
}

/// A macro that emits a diagnostic via `ctx.error` and returns
/// `ASTNode.error_marker()` makes compilation fail with that message
/// (`docs/22` §7).
#[test]
fn macro_error_is_reported() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function MustBeStruct(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          if input.kind() != \"struct\" {\n\
            ctx.error(input.span(), \"MustBeStruct only applies to structs\");\n\
            return ASTNode.error_marker();\n\
          }\n\
          input\n\
        }\n\
        @MustBeStruct\n\
        function not_a_struct() {}\n\
        function main() {}";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok, "expected a macro error");
    assert!(
        err.contains("MustBeStruct only applies to structs"),
        "stderr: {err}"
    );
}

/// The macro-surface methods are compile-time only: a program that defines a
/// macro but whose runtime code never touches `core:compiler` still builds to a
/// native executable (the surface methods are not seeded into the object).
#[test]
fn macro_program_builds_native() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Ident(ctx: MacroContext, input: ASTNode): ASTNode { input }\n\
        @Ident\n\
        struct Cfg { n: i64 }\n\
        function main() { var c = Cfg { n: 7 }; println(c.n as str); }";
    let (out, err, ok) = lang_build_run(src, &[]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "7\n");
}

// ===========================================================================
// User procedural macros — expression & block forms (slice 2, `docs/22` §2)
// ===========================================================================

/// Expression-form `@Name(args)`: the macro returns an expression that replaces
/// the invocation, including when nested inside a larger expression.
#[test]
fn macro_expression_form() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Double(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var a = ctx.arg(0).text();\n\
          ctx.parse_expr(\"(\" + a + \") * 2\")\n\
        }\n\
        function main() {\n\
          println(@Double(21) as str);\n\
          println((@Double(5) + 1) as str);\n\
        }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n11\n");
}

/// Block-form `@Name { … }`: the macro receives the block as input and may
/// return it (or a transformed block/expression) to replace the invocation.
#[test]
fn macro_block_form_passthrough() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function AsBlock(ctx: MacroContext, input: ASTNode): ASTNode { input }\n\
        function main() {\n\
          var x = @AsBlock { var t = 3; t + 4 };\n\
          println(x as str);\n\
        }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "7\n");
}

/// A block-form macro can synthesise wrapping code around the user's block —
/// here doubling the block's value via `parse_expr`.
#[test]
fn macro_block_form_transforms() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function DoubleBlock(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          ctx.parse_expr(\"(\" + input.text() + \") * 2\")\n\
        }\n\
        function main() {\n\
          println(@DoubleBlock { 3 + 4 } as str);\n\
        }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "14\n");
}

/// An undefined `@Name(...)` in expression position is reported as an
/// unknown-macro error by the checker (it survives expansion unchanged).
#[test]
fn unknown_expression_macro_is_reported() {
    let src = "function main() { println(@Nope(1) as str); }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok, "expected unknown-macro error");
    assert!(err.contains("cannot find macro `@Nope`"), "stderr: {err}");
}

// ===========================================================================
// User procedural macros — hygiene (slice 3, `docs/22` §5)
// ===========================================================================

/// `ctx.fresh_ident` mints a guaranteed-unique name: a macro-introduced binding
/// does not capture or shadow a caller binding of the same spelling. Here the
/// macro introduces its own `t` while the caller already has a `t`; both keep
/// their values.
#[test]
fn macro_fresh_ident_is_hygienic() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Squared(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var t = ctx.fresh_ident(\"t\").name();\n\
          var a = ctx.arg(0).text();\n\
          ctx.parse_block(\"var \" + t + \" = \" + a + \"; \" + t + \" * \" + t)\n\
        }\n\
        function main() {\n\
          var t = 5;\n\
          var r = @Squared(t);\n\
          println(r as str);\n\
          println(t as str);\n\
        }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    // r = t*t = 25 (the arg `t` is the caller's 5); the caller's `t` is still 5.
    assert_eq!(out, "25\n5\n");
}

/// `ctx.unhygienic` produces the name verbatim, so a macro can deliberately
/// introduce a caller-callable name (the mechanism `@Derive` relies on).
#[test]
fn macro_unhygienic_name_is_callable() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Describe(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var n = input.name();\n\
          var m = ctx.unhygienic(\"describe\").name();\n\
          ctx.parse_items(input.text() + \" extend \" + n + \" { function \" + m + \"(self): str { \\\"\" + n + \"\\\" } }\")\n\
        }\n\
        @Describe\n\
        struct W { x: i64 }\n\
        function main() { var w = W { x: 0 }; println(w.describe()); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "W\n");
}

/// A hygienic name is *not* the literal spelling, so it is not callable under
/// that spelling — the dual of the previous test, confirming `fresh_ident`
/// really mangles.
#[test]
fn macro_fresh_ident_name_is_not_the_literal() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Hidden(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var n = input.name();\n\
          var m = ctx.fresh_ident(\"secret\").name();\n\
          ctx.parse_items(input.text() + \" extend \" + n + \" { function \" + m + \"(self): i64 { 1 } }\")\n\
        }\n\
        @Hidden\n\
        struct W { x: i64 }\n\
        function main() { var w = W { x: 0 }; println(w.secret() as str); }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok, "calling the hygienic name should fail");
    assert!(err.contains("secret"), "stderr: {err}");
}

/// Used twice in one scope, a macro's fresh bindings are distinct each time, so
/// repeated expansions never collide.
#[test]
fn macro_fresh_ident_unique_per_expansion() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Inc(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          var t = ctx.fresh_ident(\"t\").name();\n\
          var a = ctx.arg(0).text();\n\
          ctx.parse_block(\"var \" + t + \" = \" + a + \" + 1; \" + t)\n\
        }\n\
        function main() {\n\
          var p = @Inc(10);\n\
          var q = @Inc(20);\n\
          println((p + q) as str);\n\
        }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "32\n");
}

// ===========================================================================
// User procedural macros — recursion, depth limit, chain (slice 4, `docs/22` §10)
// ===========================================================================

/// A macro whose output invokes another macro is re-expanded to a fixed point.
#[test]
fn macro_nested_expansion() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Inner(ctx: MacroContext, input: ASTNode): ASTNode { ctx.parse_expr(\"10\") }\n\
        @ProcMacro\n\
        pub function Outer(ctx: MacroContext, input: ASTNode): ASTNode { ctx.parse_expr(\"@Inner() + 5\") }\n\
        function main() { println(@Outer() as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "15\n");
}

/// A self-emitting macro hits the recursion limit and is rejected, with an
/// invocation chain in the message (`docs/22` §10).
#[test]
fn macro_runaway_recursion_is_rejected() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Loop(ctx: MacroContext, input: ASTNode): ASTNode { ctx.parse_expr(\"@Loop()\") }\n\
        function main() { println(@Loop() as str); }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok, "runaway macro must be rejected");
    assert!(err.contains("recursion limit"), "stderr: {err}");
    assert!(err.contains("@Loop"), "chain should be reported: {err}");
}

/// A decorator macro re-emitting its own decorator also hits the limit.
#[test]
fn macro_runaway_decorator_recursion_is_rejected() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Grow(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          ctx.parse_items(\"@Grow\\n\" + input.text())\n\
        }\n\
        @Grow\n\
        struct S { x: i64 }\n\
        function main() {}";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok, "runaway decorator must be rejected");
    assert!(err.contains("recursion limit"), "stderr: {err}");
}

/// `[macros] recursion_limit` in the manifest overrides the default depth.
#[test]
fn macro_recursion_limit_is_configurable() {
    let files = &[
        (
            "project.toml",
            "[package]\nname = \"m\"\nkind = \"binary\"\n[macros]\nrecursion_limit = 3\n",
        ),
        (
            "src/main.otter",
            "import { MacroContext, ASTNode } from \"core:compiler\";\n\
             @ProcMacro\n\
             pub function Loop(ctx: MacroContext, input: ASTNode): ASTNode { ctx.parse_expr(\"@Loop()\") }\n\
             function main() { println(@Loop() as str); }",
        ),
    ];
    let (_out, err, ok) = lang_run_project("project.toml", files);
    assert!(!ok, "runaway macro must be rejected");
    assert!(
        err.contains("recursion limit of 3"),
        "configured limit should appear: {err}"
    );
}

// ===========================================================================
// User procedural macros — sandbox & diagnostics (slice 5, `docs/22` §6/§7)
// ===========================================================================

/// A `@ProcMacro` that uses a `std:` name is rejected: macros are sandboxed and
/// cannot perform I/O (`docs/22` §6). Non-macro code using `std:` is unaffected.
#[test]
fn macro_sandbox_rejects_std_usage() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Bad(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          println(\"compile-time side effect\");\n\
          input\n\
        }\n\
        @Bad\n\
        struct S { x: i64 }\n\
        function main() {}";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok, "macro using std: must be rejected");
    assert!(err.contains("sandboxed"), "stderr: {err}");
    assert!(err.contains("println"), "stderr: {err}");
}

/// `ctx.warn` is informational: it appears on stderr but does not fail the
/// build (`docs/22` §7).
#[test]
fn macro_warn_is_non_fatal() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Warned(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          ctx.warn(input.span(), \"heads up\");\n\
          input\n\
        }\n\
        @Warned\n\
        struct S { x: i64 }\n\
        function main() {}";
    let (_out, err, ok) = lang("check", src);
    assert!(ok, "warn must not fail the build; stderr: {err}");
    assert!(
        err.contains("heads up"),
        "warning text should appear: {err}"
    );
}

/// A macro that reports its own error and returns `ASTNode.error_marker()`
/// yields exactly that error — no spurious "cannot find macro" follow-on
/// (`docs/22` §7).
#[test]
fn macro_error_marker_suppresses_followon() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function Fail(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          ctx.error(ctx.invocation_span(), \"boom\");\n\
          ASTNode.error_marker()\n\
        }\n\
        function main() { var x = @Fail(); }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("boom"), "stderr: {err}");
    assert!(
        !err.contains("cannot find macro"),
        "no follow-on error: {err}"
    );
}

/// A parse error in macro-generated source is reported (not a crash): the
/// `parse_*` helper records a diagnostic and hands back an error marker.
#[test]
fn macro_generated_parse_error_is_reported() {
    let src = "import { MacroContext, ASTNode } from \"core:compiler\";\n\
        @ProcMacro\n\
        pub function BadGen(ctx: MacroContext, input: ASTNode): ASTNode {\n\
          ctx.parse_expr(\"1 +\")\n\
        }\n\
        function main() { var x = @BadGen(); }";
    let (_out, err, ok) = lang("check", src);
    assert!(!ok, "generated parse error must surface");
    assert!(err.contains("parse_expr"), "stderr: {err}");
}

// ===========================================================================
// Async closures (`docs/21` §7). An async closure `(p) async => E` /
// `function(p): Future<T> async { … }` is desugared by `sema::anf` into a sync
// closure returning a bare async block — `(p) => async { E }` — so it reuses
// the closure-environment + async-block state-machine codegen verbatim. Calling
// it builds an inert `Future<T>` capturing `p` + the outer environment; `await`
// (or `spawn`) drives it. These tests pin every surface: arrow + `function`
// forms, capture-by-reference (incl. mutation visibility), the closure value
// typed as `(…) => Future<T>`, storage in a struct/list, `for await` inside the
// body, `spawn`-ing the produced future, the `extern` rejection, and JIT/native
// + GC-stress parity.
// ===========================================================================

#[test]
fn async_closure_bound_and_awaited() {
    // The `function(p): Future<T> async { … }` form, capturing an outer local
    // and suspending on `await sleep` before producing its value.
    let src = "function main(): Future<null> async {\n\
                 var base: i64 = 100;\n\
                 var f = function(x: i64): Future<i64> async {\n\
                   var _ = await sleep(1);\n\
                   base + x\n\
                 };\n\
                 var v: i64 = await f(5);\n\
                 println(\"v=${v}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "v=105\n");
}

#[test]
fn async_closure_arrow_form() {
    // The `(p) async => E` arrow form (no `await` in the body — an immediately
    // ready future), capturing `base`.
    let src = "function main(): Future<null> async {\n\
                 var base: i64 = 10;\n\
                 var g = (n: i64): Future<i64> async => base * n;\n\
                 println(\"g=${await g(4)}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "g=40\n");
}

#[test]
fn async_closure_capture_by_reference_mutation_visible() {
    // Captures follow ordinary closure rules (`docs/09` §7 — by reference): the
    // async closure and the outer scope share the captured cell, so a mutation
    // inside the body (across an `await`) is visible to the next call and to the
    // enclosing function. Two drives bump the shared counter to 2.
    let src = "function main(): Future<null> async {\n\
                 var counter: i64 = 0;\n\
                 var bump = function(): Future<i64> async {\n\
                   var _ = await sleep(1);\n\
                   counter = counter + 1;\n\
                   counter\n\
                 };\n\
                 var a: i64 = await bump();\n\
                 var b: i64 = await bump();\n\
                 println(\"a=${a} b=${b} counter=${counter}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "a=1 b=2 counter=2\n");
}

#[test]
fn async_closure_typed_as_future_returning_callable() {
    // The async closure's *value* type is `(i64) => Future<i64>`: it can be bound
    // to a function-typed annotation and called to produce a `Future<i64>`.
    let src = "function main(): Future<null> async {\n\
                 var base: i64 = 3;\n\
                 var f: (i64) => Future<i64> =\n\
                   function(x: i64): Future<i64> async { var _ = await sleep(1); base + x };\n\
                 var v: i64 = await f(9);\n\
                 println(\"v=${v}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "v=12\n");
}

#[test]
fn extern_async_function_rejected() {
    // `docs/21` §7: an `async` body is a `Future` state machine and so cannot be
    // `extern` (a body-less FFI symbol). The checker rejects it.
    let src = "extern function ext(x: i64): Future<i64> async;\n\
               function main() {}";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected an extern-async rejection");
    assert!(
        err.contains("cannot be `async`") && err.contains("extern"),
        "stderr: {err}",
    );
}

#[test]
fn async_closure_stored_in_struct_and_driven() {
    // An async closure stored in a struct field (`(i64) => Future<i64>`), read
    // back out, and driven with `await`.
    let src = "struct Holder { job: (i64) => Future<i64> }\n\
               function main(): Future<null> async {\n\
                 var k: i64 = 7;\n\
                 var h = Holder {\n\
                   job: function(x: i64): Future<i64> async { var _ = await sleep(1); k * x }\n\
                 };\n\
                 var f = h.job;\n\
                 println(\"r=${await f(6)}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "r=42\n");
}

#[test]
fn async_closure_stored_in_list_and_driven() {
    // A list of async closures, each driven in turn. The first suspends on
    // `await sleep`; the second is immediately ready. base + 2 = 102, base * 2 = 200.
    let src = "function main(): Future<null> async {\n\
                 var base: i64 = 100;\n\
                 var jobs: List<(i64) => Future<i64>> = [];\n\
                 jobs.push(function(x: i64): Future<i64> async { var _ = await sleep(1); base + x });\n\
                 jobs.push(function(x: i64): Future<i64> async { base * x });\n\
                 var total: i64 = 0;\n\
                 for j in jobs { total = total + await j(2); }\n\
                 println(\"total=${total}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "total=302\n");
}

#[test]
fn async_closure_for_await_inside_body() {
    // `for await` over an async stream *inside* an async closure body: the
    // closure captures `bonus`, then folds the stream 0..n into it.
    let src = "struct Range { current: i64, end: i64 }\n\
               extend Range: AsyncIterator<i64> {\n\
                 function next_async(self): Future<Item<i64> | Done> {\n\
                   async {\n\
                     if self.current >= self.end { Done {} }\n\
                     else { var _ = await sleep(1); var v = self.current; self.current = self.current + 1; Item { value: v } }\n\
                   }\n\
                 }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var bonus: i64 = 1000;\n\
                 var run = function(n: i64): Future<i64> async {\n\
                   var r = Range { current: 0, end: n };\n\
                   var total: i64 = bonus;\n\
                   for await x in r { total = total + x; }\n\
                   total\n\
                 };\n\
                 println(\"t=${await run(5)}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "t=1010\n");
}

#[test]
fn spawn_async_closure_call() {
    // `spawn EXPR` over an async-closure call: calling `work(7)` builds the
    // future (capturing `base`), which `spawn` hands to a worker; awaiting the
    // returned handle resolves to the worker's result.
    let src = "function main(): Future<null> async {\n\
                 var base: i64 = 50;\n\
                 var work = function(x: i64): Future<i64> async { var _ = await sleep(1); base + x };\n\
                 var h: Future<i64> = spawn work(7);\n\
                 println(\"v=${await h}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "v=57\n");
}

#[test]
fn native_build_async_closure_matches_jit() {
    // An async closure capturing an outer local and suspending on `await sleep`
    // must produce identical output whether JIT-run or compiled to a native
    // executable (`docs/21` + `docs/23`).
    let src = "function main(): Future<null> async {\n\
                 var base: i64 = 21;\n\
                 var f = function(x: i64): Future<i64> async { var _ = await sleep(1); base + x };\n\
                 var v: i64 = await f(21);\n\
                 println(\"v=${v}\");\n\
               }";
    let (jit_out, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit_out, nat_out, "native build diverged from JIT");
    assert_eq!(nat_out, "v=42\n");
}

#[test]
fn native_build_task_spawn_matches_jit() {
    // `std:task::Task.spawn` is separate from `std:thread::Thread.spawn`; run
    // this without the test prelude so the task JoinHandle/Joined/Cancelled
    // names are imported exactly as a real program writes them.
    let src = "import { println } from \"std:io\";\n\
               import { Future } from \"core:prelude\";\n\
               import { Shared, LockBusy } from \"std:sync\";\n\
               import { sleep } from \"std:async\";\n\
               import { Task, JoinHandle, Joined, Panicked, Cancelled } from \"std:task\";\n\
               struct C { value: i64 }\n\
               function val(r: Joined<i64> | Panicked | Cancelled): i64 {\n\
                 match r { Joined<i64> j => j.value, Panicked p => -1, Cancelled c => -2 }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var state: Shared<C> = Shared.new(C { value: 0 });\n\
                 var sync_h: JoinHandle<i64> = Task.spawn(() => 42);\n\
                 println(\"sync=${val(await sync_h.join())}\");\n\
                 var s: Shared<C> = state.clone();\n\
                 var async_h: JoinHandle<i64> = Task.spawn(() async => {\n\
                   var i: i64 = 0;\n\
                   while i < 5 {\n\
                     await s.lock((c) => { c.value = c.value + 1; 0 });\n\
                     i = i + 1;\n\
                   }\n\
                   await s.lock((c) => c.value)\n\
                 });\n\
                 var worker: i64 = val(await async_h.join());\n\
                 var total: i64 = await state.lock((c) => c.value);\n\
                 println(\"async=${worker} total=${total}\");\n\
                 var s2: Shared<C> = state.clone();\n\
                 var cancel_h: JoinHandle<i64> = Task.spawn(() async => {\n\
                   await s2.lock((c) async => {\n\
                     c.value = 99;\n\
                     var _ = await sleep(1000);\n\
                     c.value = 100;\n\
                     c.value\n\
                   })\n\
                 });\n\
                 var _started = await sleep(20);\n\
                 cancel_h.cancel();\n\
                 println(\"cancel=${val(await cancel_h.join())}\");\n\
                 match await state.try_lock((c) => c.value) {\n\
                   i64 n => println(\"after=${n}\"),\n\
                   LockBusy busy => println(\"after=busy\"),\n\
                 };\n\
               }";
    let (jit_out, jerr, jok) = lang_raw("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "sync=42\nasync=5 total=5\ncancel=-2\nafter=99\n");
}

#[test]
fn native_build_task_spawn_panic_join_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_panic_join.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn panic join output diverged from JIT"
    );
    assert_eq!(
        nat_out,
        "sync panic: sync boom\nasync panic: async boom\nok joined: 99\n"
    );
}

#[test]
fn native_build_task_spawn_panic_releases_lock_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_panic_releases_lock.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "1")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn panic lock-release output diverged from JIT"
    );
    assert_eq!(nat_out, "bad = panicked: task lock boom\nsibling = 42\n");
}

#[test]
fn native_build_task_spawn_detach_panic_isolated_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_detach_panic_isolated.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native detached Task.spawn panic isolation output diverged from JIT"
    );
    assert_eq!(nat_out, "sibling=42\n");
}

#[test]
fn native_build_task_spawn_thousand_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_thousand_tasks.otter");
    let (jit_out, jerr, jok) = lang_raw("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native thousand Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "total=499500\n");
}

#[test]
fn native_build_spawn_executor_4096_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/spawn_executor_4096_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 4096-task spawn keyword output diverged from JIT"
    );
    assert_eq!(nat_out, "total=8386560\n");
}

#[test]
fn native_build_spawn_executor_8192_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/spawn_executor_8192_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 8192-task spawn keyword output diverged from JIT"
    );
    assert_eq!(nat_out, "total=33550336\n");
}

#[test]
fn native_build_spawn_executor_16384_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/spawn_executor_16384_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 16384-task spawn keyword output diverged from JIT"
    );
    assert_eq!(nat_out, "total=134209536\n");
}

#[test]
fn native_build_spawn_executor_32768_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/spawn_executor_32768_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 32768-task spawn keyword output diverged from JIT"
    );
    assert_eq!(nat_out, "total=536854528\n");
}

#[test]
fn native_build_spawn_executor_1024_sleeping_tasks_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_executor_1024_sleeping_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 1024-sleeping-task spawn keyword output diverged from JIT"
    );
    assert_eq!(nat_out, "total=523776\n");
}

#[test]
fn native_build_spawn_executor_channel_fairness_2048_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_executor_channel_fairness_2048.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 2048-task spawn keyword channel fairness output diverged from JIT"
    );
    assert_eq!(nat_out, "received=2096128 awaited=2096128\n");
}

#[test]
fn native_build_spawn_executor_channel_fairness_4096_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_executor_channel_fairness_4096.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 4096-task spawn keyword channel fairness output diverged from JIT"
    );
    assert_eq!(nat_out, "received=8386560 awaited=8386560\n");
}

#[test]
fn native_build_spawn_executor_channel_fairness_8192_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_executor_channel_fairness_8192.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 8192-task spawn keyword channel fairness output diverged from JIT"
    );
    assert_eq!(nat_out, "received=33550336 awaited=33550336\n");
}

#[test]
fn native_build_spawn_executor_repeated_wave_fanout_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_executor_repeated_wave_fanout.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native repeated-wave spawn keyword output diverged from JIT"
    );
    assert_eq!(nat_out, "received=128020480 awaited=128020480\n");
}

#[test]
fn native_build_spawn_executor_gc_many_live_lists_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_executor_gc_many_live_lists.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native spawn keyword GC-stress live-list output diverged from JIT"
    );
    assert_eq!(nat_out, "total=98688\n");
}

#[test]
fn native_build_spawn_executor_map_gc_stress_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/spawn_executor_map_gc_stress.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native spawn keyword GC-stress Map output diverged from JIT"
    );
    assert_eq!(nat_out, "weights=1240 awaited=7440\n");
}

#[test]
fn native_build_spawn_executor_mixed_high_contention_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_executor_mixed_high_contention.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native mixed-contention spawn keyword output diverged from JIT"
    );
    assert_eq!(nat_out, "shared=3072 received=784896 awaited=784896\n");
}

#[test]
fn native_build_spawn_executor_managed_result_gc_stress_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/spawn_executor_managed_result_gc_stress.otter"
    );
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native spawn keyword managed-result GC-stress output diverged from JIT"
    );
    assert_eq!(nat_out, "total=33280\n");
}

#[test]
fn executor_high_concurrency_soak_cases_are_stable() {
    // This intentionally churns several high-concurrency executor programs.
    // Share the native-build lock so it does not run alongside native GC-stress
    // tests in this same Rust test binary and turn timing into noise.
    let _guard = native_build_guard();
    let cases: &[(&str, &str, &[(&str, &str)], &str)] = &[
        (
            "spawn keyword channel fairness 2048",
            include_str!(
                "../../../tests/cases/concurrency/spawn_executor_channel_fairness_2048.otter"
            ),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=2096128 awaited=2096128\n",
        ),
        (
            "spawn keyword channel fairness 4096",
            include_str!(
                "../../../tests/cases/concurrency/spawn_executor_channel_fairness_4096.otter"
            ),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=8386560 awaited=8386560\n",
        ),
        (
            "spawn keyword channel fairness 8192",
            include_str!(
                "../../../tests/cases/concurrency/spawn_executor_channel_fairness_8192.otter"
            ),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=33550336 awaited=33550336\n",
        ),
        (
            "Task.spawn channel fairness 2048",
            include_str!("../../../tests/cases/concurrency/task_spawn_channel_fairness_2048.otter"),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=2096128 joined=2096128\n",
        ),
        (
            "Task.spawn channel fairness 4096",
            include_str!("../../../tests/cases/concurrency/task_spawn_channel_fairness_4096.otter"),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=8386560 joined=8386560\n",
        ),
        (
            "Task.spawn channel fairness 8192",
            include_str!("../../../tests/cases/concurrency/task_spawn_channel_fairness_8192.otter"),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=33550336 joined=33550336\n",
        ),
        (
            "Task.spawn panic lock release",
            include_str!("../../../tests/cases/concurrency/task_spawn_panic_releases_lock.otter"),
            &[("OTTER_FUSION_TASK_WORKERS", "1")],
            "bad = panicked: task lock boom\nsibling = 42\n",
        ),
        (
            "spawn keyword repeated wave fanout",
            include_str!(
                "../../../tests/cases/concurrency/spawn_executor_repeated_wave_fanout.otter"
            ),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=128020480 awaited=128020480\n",
        ),
        (
            "Task.spawn repeated wave fanout",
            include_str!("../../../tests/cases/concurrency/task_spawn_repeated_wave_fanout.otter"),
            &[("OTTER_FUSION_TASK_WORKERS", "2")],
            "received=128020480 joined=128020480\n",
        ),
    ];
    for iter in 0..3 {
        for (name, src, env, expected) in cases {
            let (out, err, ok) = lang_raw_env("run", src, env);
            assert!(ok, "{name} failed on soak iteration {iter}: {err}");
            assert_eq!(
                &out, expected,
                "{name} changed output on soak iteration {iter}"
            );
        }
    }
}

#[test]
fn native_build_task_spawn_1024_sleeping_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_1024_sleeping_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 1024-sleeping-task Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "total=523776\n");
}

#[test]
fn native_build_task_spawn_clone_capture_snapshot_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_clone_capture_snapshot.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn Clone-capture snapshot output diverged from JIT"
    );
    assert_eq!(nat_out, "captured=1 parent=9\n");
}

#[test]
fn native_build_task_spawn_generic_clone_capture_snapshot_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/task_spawn_generic_clone_capture_snapshot.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn generic Clone-capture snapshot output diverged from JIT"
    );
    assert_eq!(nat_out, "captured=1 parent=9\nlist_size=1 parent_size=2\n");
}

#[test]
fn native_build_thread_spawn_generic_clone_capture_snapshot_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/thread_spawn_generic_clone_capture_snapshot.otter"
    );
    let (jit_out, jerr, jok) = lang_raw("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Thread.spawn generic Clone-capture snapshot output diverged from JIT"
    );
    assert_eq!(nat_out, "captured=1 parent=9\nlist_size=1 parent_size=2\n");
}

#[test]
fn native_build_task_joinhandle_abort_releases_lock_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_joinhandle_abort_releases_lock.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn abort lock-release output diverged from JIT"
    );
    assert_eq!(nat_out, "join = cancelled\nafter abort = 1\n");
}

#[test]
fn native_build_task_join_timeout_does_not_cancel_task_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/task_join_timeout_does_not_cancel_task.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn join-timeout output diverged from JIT"
    );
    assert_eq!(nat_out, "first = timed out\nsecond = joined 42\n");
}

#[test]
fn native_build_task_join_multiple_waiters_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_join_multiple_waiters.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn multi-join-waiter output diverged from JIT"
    );
    assert_eq!(nat_out, "first=42 second=42 total=84\n");
}

#[test]
fn native_build_task_join_cancel_wakes_multiple_waiters_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/task_join_cancel_wakes_multiple_waiters.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn multi-join cancellation output diverged from JIT"
    );
    assert_eq!(nat_out, "first=-2 second=-2 total=-4\n");
}

#[test]
fn native_build_task_join_cancel_many_spawned_waiters_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/task_join_cancel_many_spawned_waiters.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn many spawned join-waiter cancellation output diverged from JIT"
    );
    assert_eq!(nat_out, "cancelled=128 total=-256\n");
}

#[test]
fn native_build_task_join_cancel_many_spawned_waiters_single_worker_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/task_join_cancel_many_spawned_waiters_single_worker.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "1")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native single-worker Task.spawn many spawned join-waiter cancellation output diverged from JIT"
    );
    assert_eq!(nat_out, "cancelled=512 total=-1024\n");
}

#[test]
fn native_build_task_join_cancel_many_spawned_waiters_single_worker_gc_stress_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/task_join_cancel_many_spawned_waiters_single_worker_gc_stress.otter"
    );
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "1"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native single-worker stress-GC Task.spawn many spawned join-waiter cancellation output diverged from JIT"
    );
    assert_eq!(nat_out, "cancelled=256 total=-512\n");
}

#[test]
fn native_build_spawn_future_cancel_many_releases_channel_endpoints_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/spawn_future_cancel_many_releases_channel_endpoints.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native mass-cancel spawn future output diverged from JIT"
    );
    assert_eq!(nat_out, "closed=4096\n");
}

#[test]
fn native_build_spawn_future_cancel_many_gc_stress_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/spawn_future_cancel_many_gc_stress.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = {
        let _native_guard = native_build_guard();
        let stress_timeout = std::cmp::max(cli_test_timeout(), Duration::from_secs(90));
        lang_raw_env_with_timeout("run", src, env, stress_timeout)
    };
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native GC-stress mass-cancel spawn future output diverged from JIT"
    );
    assert_eq!(nat_out, "closed=512\n");
}

#[test]
fn native_build_spawn_future_cancel_repeated_wave_gc_stress_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/spawn_future_cancel_repeated_wave_gc_stress.otter"
    );
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native repeated-wave GC-stress spawn future cancellation output diverged from JIT"
    );
    assert_eq!(nat_out, "closed=128\n");
}

#[test]
fn native_build_timeout_cancels_many_spawn_losers_releases_channels_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/timeout_cancels_many_spawn_losers_releases_channels.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native timeout mass-cancel spawn loser output diverged from JIT"
    );
    assert_eq!(nat_out, "timed_out=512 closed=512\n");
}

#[test]
fn native_build_timeout_cancels_many_spawn_losers_single_worker_gc_stress_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/timeout_cancels_many_spawn_losers_single_worker_gc_stress.otter"
    );
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "1"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native single-worker stress-GC timeout mass-cancel spawn loser output diverged from JIT"
    );
    assert_eq!(nat_out, "timed_out=512 closed=512\n");
}

#[test]
fn native_build_channel_endpoint_list_ownership_gc_stress_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/channel_endpoint_list_ownership_gc_stress.otter"
    );
    let env = &[("OTTER_FUSION_GC", "stress")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native stress-GC List channel endpoint ownership output diverged from JIT"
    );
    assert_eq!(nat_out, "sent=64 sum=2016\n");
}

#[test]
fn native_build_channel_endpoint_list_index_load_ownership_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/channel_endpoint_list_index_load_ownership.otter"
    );
    let env = &[("OTTER_FUSION_GC", "stress")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native stress-GC List endpoint index-load ownership output diverged from JIT"
    );
    assert_eq!(nat_out, "before_clear=open\nafter_clear=closed\n");
}

#[test]
fn native_build_timeout_cancels_many_recv_losers_releases_waiters_matches_jit() {
    let src = include_str!(
        "../../../tests/cases/concurrency/timeout_cancels_many_recv_losers_releases_waiters.otter"
    );
    let env = &[("OTTER_FUSION_GC", "stress")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native timeout mass-cancel recv loser output diverged from JIT"
    );
    assert_eq!(nat_out, "timed_out=512 received=77\n");
}

#[test]
fn native_build_task_spawn_4096_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_4096_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 4096-task Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "total=8386560\n");
}

#[test]
fn native_build_task_spawn_8192_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_8192_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 8192-task Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "total=33550336\n");
}

#[test]
fn native_build_task_spawn_16384_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_16384_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 16384-task Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "total=134209536\n");
}

#[test]
fn native_build_task_spawn_32768_tasks_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_32768_tasks.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 32768-task Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "total=536854528\n");
}

#[test]
fn native_build_task_spawn_detach_many_channel_close_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_detach_many_channel_close.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native detached Task.spawn fanout output diverged from JIT"
    );
    assert_eq!(nat_out, "count=1024 total=523776 closed=1\n");
}

#[test]
fn native_build_task_spawn_detach_gc_many_live_lists_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_detach_gc_many_live_lists.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native detached GC-stress Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "count=256 total=98688 closed=1\n");
}

#[test]
fn native_build_task_spawn_yield_fairness_channel_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_yield_fairness_channel.otter");
    let (jit_out, jerr, jok) = lang_raw("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native uneven-yield Task.spawn channel fan-in output diverged from JIT"
    );
    assert_eq!(nat_out, "received=4560 joined=9120\n");
}

#[test]
fn native_build_task_spawn_channel_fairness_2048_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_channel_fairness_2048.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 2048-task Task.spawn channel fairness output diverged from JIT"
    );
    assert_eq!(nat_out, "received=2096128 joined=2096128\n");
}

#[test]
fn native_build_task_spawn_channel_fairness_4096_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_channel_fairness_4096.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 4096-task Task.spawn channel fairness output diverged from JIT"
    );
    assert_eq!(nat_out, "received=8386560 joined=8386560\n");
}

#[test]
fn native_build_task_spawn_channel_fairness_8192_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_channel_fairness_8192.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native 8192-task Task.spawn channel fairness output diverged from JIT"
    );
    assert_eq!(nat_out, "received=33550336 joined=33550336\n");
}

#[test]
fn native_build_task_spawn_repeated_wave_fanout_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_repeated_wave_fanout.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "2")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native repeated-wave Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "received=128020480 joined=128020480\n");
}

#[test]
fn native_build_task_spawn_mixed_high_contention_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_mixed_high_contention.otter");
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native mixed-contention Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "shared=3072 received=784896 joined=784896\n");
}

#[test]
fn native_build_task_spawn_gc_stress_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_gc_stress.otter");
    let env = &[("OTTER_FUSION_GC", "stress")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native GC-stress Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "total = 10\n");
}

#[test]
fn native_build_task_spawn_map_gc_stress_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_spawn_map_gc_stress.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn GC-stress Map output diverged from JIT"
    );
    assert_eq!(nat_out, "weights=1240 joined=7440\n");
}

#[test]
fn native_build_task_spawn_managed_result_gc_stress_matches_jit() {
    let src =
        include_str!("../../../tests/cases/concurrency/task_spawn_managed_result_gc_stress.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native Task.spawn managed-result GC-stress output diverged from JIT"
    );
    assert_eq!(nat_out, "total=33280\n");
}

#[test]
fn native_build_task_cancel_many_releases_channel_endpoints_matches_jit() {
    // This case creates and cancels thousands of executor tasks plus thousands
    // of channels. Keep the whole parity run out of the rest of the native/
    // soak churn so the 30s native executable watchdog measures the program,
    // not unrelated scheduler load in the Rust test process.
    let _guard = native_build_guard();
    let src = include_str!(
        "../../../tests/cases/concurrency/task_cancel_many_releases_channel_endpoints.otter"
    );
    let env = &[("OTTER_FUSION_TASK_WORKERS", "4")];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw_unlocked(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native mass-cancel Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "cancelled=4096 recv=closed\n");
}

#[test]
fn native_build_task_cancel_many_gc_stress_matches_jit() {
    let src = include_str!("../../../tests/cases/concurrency/task_cancel_many_gc_stress.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native GC-stress mass-cancel Task.spawn output diverged from JIT"
    );
    assert_eq!(nat_out, "cancelled=512 recv=closed\n");
}

#[test]
fn native_build_task_cancel_repeated_wave_gc_stress_matches_jit() {
    let _guard = native_build_guard();
    let src =
        include_str!("../../../tests/cases/concurrency/task_cancel_repeated_wave_gc_stress.otter");
    let env = &[
        ("OTTER_FUSION_TASK_WORKERS", "4"),
        ("OTTER_FUSION_GC", "stress"),
    ];
    let (jit_out, jerr, jok) = lang_raw_env("run", src, env);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run_raw_unlocked(src, env);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(
        jit_out, nat_out,
        "native repeated-wave GC-stress Task.spawn cancellation output diverged from JIT"
    );
    assert_eq!(nat_out, "cancelled=128 recv=closed\n");
}

#[test]
fn async_closure_gc_stress_keeps_captures_live() {
    // Under `OTTER_FUSION_GC=stress` (collect on every allocation), the captured
    // cell and the state machine's saved slots must stay live across the
    // suspends — the closure's repeated drives still see the shared counter.
    let src = "function main(): Future<null> async {\n\
                 var counter: i64 = 0;\n\
                 var s: str = \"seed\";\n\
                 var bump = function(tag: str): Future<str> async {\n\
                   var _ = await sleep(1);\n\
                   counter = counter + 1;\n\
                   s = s + \"-\" + tag;\n\
                   s + \"#\" + (counter as str)\n\
                 };\n\
                 var a: str = await bump(\"a\");\n\
                 var b: str = await bump(\"b\");\n\
                 println(a);\n\
                 println(b);\n\
                 println(\"counter=${counter}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("OTTER_FUSION_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "seed-a#1\nseed-a-b#2\ncounter=2\n");
}
