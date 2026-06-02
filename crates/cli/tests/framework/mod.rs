//! A small, self-contained end-to-end testing framework for Otter Fusion.
//!
//! Each test case is a real `.otter` source file living under the repository's
//! top-level `tests/cases/` tree, organised by feature. The *expectations* for a
//! case are written **inside the source file** as comment directives, so every
//! file is simultaneously a runnable program and a complete, reviewable test
//! specification:
//!
//! ```otter
//! //@ kind: run
//! //@ description: integer arithmetic and operator precedence
//! //@ stdout:
//! //~ 7
//! //~ -1
//! function main() {
//!   println("${1 + 2 * 3}");
//!   println("${1 - 2}");
//! }
//! ```
//!
//! Directives (lines beginning with `//@`):
//!   * `kind: run | compile-error | panic`  — the expected outcome (default `run`).
//!   * `exit: <int>`                          — expected process exit code.
//!   * `stderr: <substring>`                  — a substring required in stderr
//!                                              (repeatable; for errors/panics).
//!   * `release`                              — run under `--release`.
//!   * `serial`                                — run alone (after the parallel
//!                                              batch); for OS-thread-spawning
//!                                              cases that are fragile under
//!                                              cross-process CPU contention.
//!   * `env: KEY=VALUE`                        — set an environment variable for
//!                                              the run (repeatable; e.g.
//!                                              `OTTER_FUSION_GC=stress` to hammer the GC).
//!   * `known-bug: <note>`                     — this case states the *desired*
//!                                              (spec-correct) behaviour, which
//!                                              the implementation does NOT yet
//!                                              satisfy. It is expected to fail
//!                                              today (reported `XFAIL`); if it
//!                                              ever passes it is flagged `XPASS`
//!                                              and the suite fails so the marker
//!                                              is removed. This is how the suite
//!                                              catalogs the unfinished surface
//!                                              instead of hiding it.
//!   * `description: <text>`                  — free-form documentation.
//!
//! Every case is a complete program that declares its own `import`s (the
//! near-empty prelude rule, `docs/17` §17.8); the framework never injects
//! anything, so each file runs unchanged via `otter_fusion run <file>`.
//!
//! Expected stdout (lines beginning with `//~`): the exact stdout the program
//! must produce, one line per directive, in order. `//~` with no text is an
//! empty output line.
//!
//! Outcomes:
//!   * `run`           — compiles, runs, exits 0 (unless `exit:` overrides),
//!                       and prints exactly the `//~` lines.
//!   * `compile-error` — fails to compile (non-zero exit, empty stdout) and
//!                       stderr contains every `stderr:` substring.
//!   * `panic`         — compiles, runs, prints the `//~` lines, then aborts
//!                       (exit 101 unless overridden) with the `stderr:` text.
//!
//! Timing: `run`/`panic` cases are executed with `--time`, and the framework
//! parses the runtime's `execution time: … (N ns)` line so the report shows the
//! pure execution time (excluding compilation) of every case.
//!
//! Blessing: running the suite with `OTTER_TEST_BLESS=1` rewrites the `//~`
//! expected-stdout block of each passing `run` case to the program's actual
//! stdout — the standard "update expectations" workflow. Always review the
//! resulting diff.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The expected outcome kind of a test case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Compiles, runs, and exits successfully with exact stdout.
    Run,
    /// Fails to compile; stderr must contain the declared substrings.
    CompileError,
    /// Compiles and runs, then aborts (a runtime panic / `exit`).
    Panic,
}

/// A parsed test case: its source, location, and declared expectations.
pub struct Case {
    /// Path to the `.otter` file (for reporting and blessing).
    pub path: PathBuf,
    /// Short display name relative to the cases root (e.g. `arithmetic/add`).
    pub name: String,
    /// The full original source text (including directive comments).
    pub source: String,
    pub kind: Kind,
    /// Expected exit code, if explicitly declared.
    pub exit: Option<i32>,
    /// Substrings required in stderr.
    pub stderr_contains: Vec<String>,
    /// Exact expected stdout lines (from `//~` directives).
    pub stdout_lines: Vec<String>,
    pub release: bool,
    /// Run this case alone, after the parallel batch — no other `otter_fusion`
    /// subprocess running concurrently. Used for OS-thread-spawning cases: the
    /// runtime's thread/abort machinery is fragile under heavy *cross-process*
    /// CPU contention (many processes each spawning threads can abort), which is
    /// a real but load-dependent issue; serial execution keeps these cases
    /// deterministic. See `tests/README.md`.
    pub serial: bool,
    /// Environment variables to set for the run (e.g. `OTTER_FUSION_GC=stress`).
    pub env: Vec<(String, String)>,
    /// If set, this case documents *desired* (spec-correct) behaviour that the
    /// implementation does **not** yet satisfy — a known bug or unimplemented
    /// feature. It is expected to currently FAIL its stated expectations; the
    /// runner reports it as an `XFAIL` (does not fail the suite). If it ever
    /// starts *meeting* its expectations, the runner flags it as `XPASS` and
    /// fails the suite, so the marker gets removed when the bug is fixed.
    pub known_bug: Option<String>,
    #[allow(dead_code)]
    pub description: Option<String>,
}

impl Case {
    /// Parse a case from a source file's path and contents.
    pub fn parse(path: &Path, root: &Path, source: &str) -> Result<Case, String> {
        let name = path
            .strip_prefix(root)
            .unwrap_or(path)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");

        let mut kind = Kind::Run;
        let mut exit = None;
        let mut stderr_contains = Vec::new();
        let mut stdout_lines = Vec::new();
        let mut release = false;
        let mut serial = false;
        let mut env = Vec::new();
        let mut known_bug = None;
        let mut description = None;

        for raw in source.lines() {
            let line = raw.trim_start();
            if let Some(rest) = line.strip_prefix("//@") {
                let rest = rest.trim();
                let (key, val) = match rest.split_once(':') {
                    Some((k, v)) => (k.trim(), v.trim().to_string()),
                    None => (rest, String::new()),
                };
                match key {
                    "kind" => {
                        kind = match val.as_str() {
                            "run" => Kind::Run,
                            "compile-error" => Kind::CompileError,
                            "panic" => Kind::Panic,
                            other => return Err(format!("unknown kind `{other}`")),
                        }
                    }
                    "exit" => {
                        exit = Some(val.parse().map_err(|_| format!("bad exit code `{val}`"))?)
                    }
                    "stderr" => stderr_contains.push(val),
                    "release" => release = true,
                    "serial" => serial = true,
                    "env" => {
                        let (k, v) = val
                            .split_once('=')
                            .ok_or_else(|| format!("bad env `{val}` (expected KEY=VALUE)"))?;
                        env.push((k.trim().to_string(), v.trim().to_string()));
                    }
                    "known-bug" => known_bug = Some(val),
                    "description" => description = Some(val),
                    // A pure readability marker introducing the `//~` block.
                    "stdout" => {}
                    other => return Err(format!("unknown directive `{other}`")),
                }
            } else if let Some(rest) = line.strip_prefix("//~") {
                // One expected stdout line. Strip at most one leading space so
                // `//~ x` means the line `x` and `//~` means an empty line.
                stdout_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }

        Ok(Case {
            path: path.to_path_buf(),
            name,
            source: source.to_string(),
            kind,
            exit,
            stderr_contains,
            stdout_lines,
            release,
            serial,
            env,
            known_bug,
            description,
        })
    }

    /// The program text handed to the compiler. Every case is a complete,
    /// self-contained `.otter` program: it declares its own `import`s (the
    /// near-empty prelude rule, `docs/17` §17.8), exactly as a real program
    /// would. Nothing is injected.
    fn program(&self) -> String {
        self.source.clone()
    }
}

/// How a case turned out, relative to its expectations (and any `known-bug`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Met its expectations.
    Pass,
    /// Did not meet its expectations (and is not a known bug) — a real failure.
    Fail,
    /// A `known-bug` case that (as expected) still does not meet its
    /// expectations — the documented gap is still present. Not a suite failure.
    XFail,
    /// A `known-bug` case that unexpectedly *met* its expectations — the bug
    /// looks fixed, so the marker should be removed. A suite failure.
    XPass,
}

/// The result of executing one case.
pub struct Outcome {
    pub name: String,
    pub status: Status,
    /// Pure execution time (excluding compilation), parsed from `--time`.
    pub exec_time: Option<Duration>,
    /// A human-readable explanation (failure detail, or the known-bug note).
    pub failure: Option<String>,
    /// The program's actual stdout (used by the bless workflow).
    pub actual_stdout: String,
}

impl Outcome {
    /// Whether this outcome should *not* fail the suite (Pass or XFail).
    pub fn ok(&self) -> bool {
        matches!(self.status, Status::Pass | Status::XFail)
    }
}

/// Run a single case through the `otter_fusion` binary and check it against its
/// declared expectations.
pub fn run_case(otter: &Path, case: &Case) -> Outcome {
    let nonce = unique_nonce();
    let dir = std::env::temp_dir();
    let file = dir.join(format!("otter_suite_{nonce}.otter"));
    if let Err(e) = std::fs::write(&file, case.program()) {
        return fail(case, format!("could not write temp file: {e}"));
    }

    let mut cmd = Command::new(otter);
    cmd.arg("run").arg(&file);
    if case.release {
        cmd.arg("--release");
    }
    for (k, v) in &case.env {
        cmd.env(k, v);
    }
    // Time the execution of run/panic cases (compile-error cases never reach
    // execution, so timing is meaningless there).
    if matches!(case.kind, Kind::Run | Kind::Panic) {
        cmd.arg("--time");
    }
    let out = output_with_timeout(&mut cmd, case_timeout());
    let _ = std::fs::remove_file(&file);

    let out = match out {
        Ok(o) => o,
        Err(CaseRunError::Spawn(e)) => {
            return fail(case, format!("could not invoke `otter_fusion`: {e}"));
        }
        Err(CaseRunError::Timeout { after, output }) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return fail(
                case,
                format!(
                    "case timed out after {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    fmt_duration(after),
                    stdout,
                    stderr
                ),
            );
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let exit_code = out.status.code();
    let exec_time = parse_exec_time(&stderr);

    let mut errs = Vec::new();
    match case.kind {
        Kind::Run => {
            if !out.status.success() {
                errs.push(format!(
                    "expected success but exited with {:?}\n--- stderr ---\n{}",
                    exit_code, stderr
                ));
            }
            if let Some(want) = case.exit {
                if exit_code != Some(want) {
                    errs.push(format!("expected exit {want}, got {exit_code:?}"));
                }
            }
            check_stdout(case, &stdout, &mut errs);
        }
        Kind::CompileError => {
            if out.status.success() {
                errs.push("expected a compile error but the program ran successfully".into());
            }
            if !stdout.trim().is_empty() {
                errs.push(format!(
                    "expected no stdout from a compile error, got:\n{stdout}"
                ));
            }
            check_stderr(case, &stderr, &mut errs);
        }
        Kind::Panic => {
            let want = case.exit.unwrap_or(101);
            if exit_code != Some(want) {
                errs.push(format!(
                    "expected panic exit {want}, got {exit_code:?}\n--- stderr ---\n{stderr}"
                ));
            }
            // stdout printed before the panic must still match exactly.
            check_stdout(case, &stdout, &mut errs);
            check_stderr(case, &stderr, &mut errs);
        }
    }

    let meets = errs.is_empty();
    let (status, failure) = match &case.known_bug {
        // A normal case: meeting expectations passes; otherwise it fails.
        None if meets => (Status::Pass, None),
        None => (Status::Fail, Some(errs.join("\n"))),
        // A known-bug case is *expected* to currently miss its expectations.
        Some(note) if !meets => (Status::XFail, Some(note.clone())),
        Some(note) => (
            Status::XPass,
            Some(format!(
                "this `known-bug` case now MEETS its (spec-correct) expectations — \
                 the bug appears fixed. Remove the `//@ known-bug` marker.\nnote: {note}"
            )),
        ),
    };

    Outcome {
        name: case.name.clone(),
        status,
        exec_time,
        failure,
        actual_stdout: stdout,
    }
}

/// Compare actual stdout against the case's `//~` lines (trailing newlines are
/// normalised away so a final `println` newline never causes a spurious diff).
fn check_stdout(case: &Case, actual: &str, errs: &mut Vec<String>) {
    let want = case.stdout_lines.join("\n");
    let got = actual.trim_end_matches('\n');
    let want = want.trim_end_matches('\n');
    if got != want {
        errs.push(format!(
            "stdout mismatch\n--- expected ---\n{want}\n--- actual ---\n{got}\n---------------"
        ));
    }
}

/// Verify every declared `stderr:` substring is present.
fn check_stderr(case: &Case, stderr: &str, errs: &mut Vec<String>) {
    for needle in &case.stderr_contains {
        if !stderr.contains(needle.as_str()) {
            errs.push(format!(
                "stderr missing expected substring `{needle}`\n--- stderr ---\n{stderr}"
            ));
        }
    }
}

fn fail(case: &Case, msg: String) -> Outcome {
    // An infrastructure failure (could not write/spawn) is always a hard fail,
    // even for a known-bug case.
    Outcome {
        name: case.name.clone(),
        status: Status::Fail,
        exec_time: None,
        failure: Some(msg),
        actual_stdout: String::new(),
    }
}

enum CaseRunError {
    Spawn(std::io::Error),
    Timeout { after: Duration, output: Output },
}

fn case_timeout() -> Duration {
    std::env::var("OTTER_TEST_CASE_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(60))
}

fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, CaseRunError> {
    let start = Instant::now();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(CaseRunError::Spawn)?;
    loop {
        match child.try_wait().map_err(CaseRunError::Spawn)? {
            Some(_) => return child.wait_with_output().map_err(CaseRunError::Spawn),
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let output = child.wait_with_output().map_err(CaseRunError::Spawn)?;
                return Err(CaseRunError::Timeout {
                    after: start.elapsed(),
                    output,
                });
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Parse `execution time: … (<N> ns)` out of stderr into a [`Duration`].
fn parse_exec_time(stderr: &str) -> Option<Duration> {
    let line = stderr.lines().find(|l| l.contains("execution time:"))?;
    let open = line.rfind('(')?;
    let close = line.rfind(" ns)")?;
    let ns: u64 = line.get(open + 1..close)?.trim().parse().ok()?;
    Some(Duration::from_nanos(ns))
}

/// Rewrite a passing `run` case's `//~` expected-stdout block to the program's
/// actual stdout (the `OTTER_TEST_BLESS=1` workflow). The `//~` block is
/// regenerated immediately after the last `//@`/`//~` directive line.
pub fn bless(case: &Case, actual_stdout: &str) -> std::io::Result<()> {
    let lines: Vec<&str> = case.source.lines().collect();
    // The maximal leading run of directive lines (`//@` / `//~`) is the header;
    // everything after the first non-directive line is the program body.
    let header_len = lines
        .iter()
        .take_while(|raw| {
            let t = raw.trim_start();
            t.starts_with("//@") || t.starts_with("//~")
        })
        .count();
    let (header, body) = lines.split_at(header_len);

    let mut out: Vec<String> = Vec::new();
    // Preserve every non-stdout directive (kind/description/release/…), in
    // order; the stdout marker + `//~` lines are regenerated from scratch.
    for raw in header {
        let t = raw.trim_start();
        if t.starts_with("//~") {
            continue;
        }
        if t.starts_with("//@") {
            let rest = t.strip_prefix("//@").unwrap().trim();
            let key = rest.split(':').next().unwrap_or("").trim();
            if key == "stdout" {
                continue;
            }
        }
        out.push(raw.to_string());
    }
    // The freshly captured expected stdout.
    out.push("//@ stdout:".to_string());
    let captured = actual_stdout.trim_end_matches('\n');
    if !captured.is_empty() {
        for l in captured.split('\n') {
            if l.is_empty() {
                out.push("//~".to_string());
            } else {
                out.push(format!("//~ {l}"));
            }
        }
    }
    for raw in body {
        out.push(raw.to_string());
    }

    let mut text = out.join("\n");
    if case.source.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(&case.path, text)
}

/// Recursively collect every `.otter` case under `root`, sorted by name.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(root, &mut out);
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("otter") {
            out.push(p);
        }
    }
}

/// A process-unique nonce for temp-file names (pid + a monotonic counter, so
/// parallel cases never collide).
fn unique_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}", std::process::id(), n)
}

/// Format a duration compactly for the timing report.
pub fn fmt_duration(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns >= 1_000_000_000 {
        format!("{:.3}s", d.as_secs_f64())
    } else if ns >= 1_000_000 {
        format!("{:.3}ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.3}µs", ns as f64 / 1_000.0)
    } else {
        format!("{ns}ns")
    }
}
