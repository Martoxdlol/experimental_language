//! `otter_fusion` — the Otter Fusion toolchain driver.
//!
//! A thin orchestrator over `liblangc` (the `compiler` crate) and the Cranelift
//! `backend`, with a Clap-based command surface. This is an early cut covering
//! the subcommands the current pipeline supports end to end:
//!
//! * `otter_fusion check <file>` — lex, parse, and type-check; report diagnostics.
//! * `otter_fusion run <file>`   — the above, then JIT-compile and run `main`.
//! * `otter_fusion build <file>` — the above check + compile, without running.
//!
//! Diagnostics are rendered with a source excerpt and caret, in the spirit of
//! `docs/23` (stable codes and `--message-format` come later).

use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, ExitCode};

use clap::{Parser, Subcommand};

use compiler::ast::{ItemKind, Module, ModuleKind};
use compiler::lexer::lex;
use compiler::parser::parse;
use compiler::sema::symbols::Externals;
use compiler::sema::{analyze_multi, Analysis};
use compiler::span::{SourceMap, Span};

/// The Otter Fusion toolchain.
#[derive(Parser)]
#[command(name = "otter_fusion", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse and type-check a source file; report diagnostics.
    Check {
        /// Path to the `.otter` source file.
        file: PathBuf,
    },
    /// Check, JIT-compile, and run the program's `main`.
    Run {
        /// Path to the `.otter` source file.
        file: PathBuf,
        /// Use the release profile: arithmetic overflow wraps instead of
        /// panicking (`docs/14` §5).
        #[arg(long)]
        release: bool,
    },
    /// Check, compile to a native object, and link a standalone executable.
    Build {
        /// Path to the `.otter` source file.
        file: PathBuf,
        /// Output executable path (defaults to the source file's stem).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Use the release profile: arithmetic overflow wraps instead of
        /// panicking (`docs/14` §5).
        #[arg(long)]
        release: bool,
    },
}

/// What to do after a successful check.
enum Stage {
    Check,
    Build { output: Option<PathBuf> },
    Run,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (file, stage, release) = match cli.command {
        Command::Check { file } => (file, Stage::Check, false),
        Command::Build { file, output, release } => (file, Stage::Build { output }, release),
        Command::Run { file, release } => (file, Stage::Run, release),
    };
    drive(&file, stage, release)
}

fn drive(path: &Path, stage: Stage, release: bool) -> ExitCode {
    let display = path.display();
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read `{display}`: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut map = SourceMap::new();
    let file = map.add_file(display.to_string(), src.clone());

    // 1. Lex.
    let (tokens, lex_errors) = lex(&src, file);
    let mut had_error = false;
    for e in &lex_errors {
        render(&map, e.span, "error", &e.kind.to_string());
        had_error = true;
    }

    // 2. Parse (recovers past errors; AST is still usable for diagnostics).
    let (module, parse_errors) = parse(&src, &tokens);
    for e in &parse_errors {
        render(&map, e.span, "error", &e.kind.to_string());
        had_error = true;
    }

    // 2b. Discover and load file-backed submodules (`mod foo` → `<dir>/<stem>/
    //     foo.otter`), recursively, building the externals map for analysis.
    let mut externals = Externals::new();
    had_error |= load_submodules(&mut map, path, &module, &mut Vec::new(), &mut externals);

    // 3. Semantic analysis over the whole multi-file program.
    let analysis = analyze_multi(&module, &externals);
    for e in &analysis.errors {
        render(&map, e.span, "error", &e.kind.to_string());
        had_error = true;
    }

    if had_error {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }

    if matches!(stage, Stage::Check) {
        println!("ok: no errors in `{display}`");
        return ExitCode::SUCCESS;
    }

    // Select the build profile (release wraps arithmetic; `docs/14` §5).
    backend::set_release_profile(release);

    // 4. Native build: compile to an object and link a standalone executable.
    if let Stage::Build { output } = &stage {
        let exe = output.clone().unwrap_or_else(|| PathBuf::from(path.file_stem().unwrap()));
        return build_executable(&map, &analysis, &exe);
    }

    // 4'. Run: JIT-compile in process.
    let jit = match backend::compile(&analysis) {
        Ok(j) => j,
        Err(e) => {
            render(&map, e.span, "error", &format!("codegen: {}", e.message));
            return ExitCode::FAILURE;
        }
    };

    // 5. Run `main`, with the tracing GC enabled (programs are single-threaded).
    // `run_main` handles both sync and `async function main` (`docs/21` §6):
    // for the latter, the constructed root future is driven by the runtime
    // executor internally — the user never names `block_on`.
    backend::set_gc_enabled(true);
    if unsafe { jit.run_main() } {
        ExitCode::SUCCESS
    } else {
        eprintln!("error: no `main` function to run");
        ExitCode::FAILURE
    }
}

/// Discover the file-backed submodules declared in `module` (whose source file
/// is `file_path`), load and parse each, and recurse. `mod_path` is the current
/// module's path from the crate root; loaded bodies are inserted into
/// `externals` keyed by their full path. Returns `true` if any error occurred.
///
/// Resolution mirrors `docs/17` §2: a `mod foo` in `dir/parent.otter` lives at
/// `dir/parent/foo.otter` — submodule files sit in a directory named for the
/// parent file's stem.
fn load_submodules(
    map: &mut SourceMap,
    file_path: &Path,
    module: &Module,
    mod_path: &mut Vec<String>,
    externals: &mut Externals,
) -> bool {
    let mut had_error = false;
    let dir = file_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(file_path.file_stem().unwrap_or_default());
    for item in &module.items {
        let ItemKind::Module(m) = &item.kind else { continue };
        if !matches!(m.kind, ModuleKind::External) {
            continue;
        }
        let child_path = dir.join(format!("{}.otter", m.name.name));
        let src = match std::fs::read_to_string(&child_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read module `{}`: {e}", child_path.display());
                had_error = true;
                continue;
            }
        };
        let file = map.add_file(child_path.display().to_string(), src.clone());
        let (tokens, lex_errors) = lex(&src, file);
        for er in &lex_errors {
            render(map, er.span, "error", &er.kind.to_string());
            had_error = true;
        }
        let (child_module, parse_errors) = parse(&src, &tokens);
        for er in &parse_errors {
            render(map, er.span, "error", &er.kind.to_string());
            had_error = true;
        }
        mod_path.push(m.name.name.clone());
        had_error |= load_submodules(map, &child_path, &child_module, mod_path, externals);
        externals.insert(mod_path.clone(), child_module);
        mod_path.pop();
    }
    had_error
}

/// Compile `analysis` to a native object and link it against the runtime
/// static library (`libruntime.a`) into a standalone executable at `exe`.
fn build_executable(map: &SourceMap, analysis: &Analysis, exe: &Path) -> ExitCode {
    // Emit the relocatable object next to the executable.
    let obj = exe.with_extension("o");
    if let Err(e) = backend::compile_object(analysis, &obj) {
        render(map, e.span, "error", &format!("codegen: {}", e.message));
        return ExitCode::FAILURE;
    }

    let runtime_lib = match find_runtime_lib() {
        Some(p) => p,
        None => {
            eprintln!("error: cannot locate `libruntime.a` next to the `otter_fusion` executable");
            return ExitCode::FAILURE;
        }
    };

    // Link with the system C toolchain driver, which supplies crt startup and
    // libc; the Rust static library carries std and its dependencies. On macOS
    // the runtime's std needs a few system frameworks/libraries.
    let mut cmd = ProcCommand::new("cc");
    cmd.arg(&obj).arg(&runtime_lib).arg("-o").arg(exe);
    if cfg!(target_os = "macos") {
        cmd.args(["-framework", "CoreFoundation", "-framework", "Security", "-liconv"]);
    } else {
        cmd.args(["-lpthread", "-ldl", "-lm"]);
    }

    match cmd.status() {
        Ok(s) if s.success() => {
            let _ = std::fs::remove_file(&obj);
            println!("ok: linked `{}`", exe.display());
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("error: linker `cc` failed with {s}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: could not invoke linker `cc`: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Find `libruntime.a` — the runtime static library cargo emits alongside the
/// `otter_fusion` binary (`target/<profile>/`), checking the executable's own directory
/// and its `deps/` sibling.
fn find_runtime_lib() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidates = [
        dir.join("libruntime.a"),
        dir.join("deps").join("libruntime.a"),
        dir.parent().map(|d| d.join("libruntime.a")).unwrap_or_default(),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Render one diagnostic with a source excerpt and caret underline.
fn render(map: &SourceMap, span: Span, severity: &str, message: &str) {
    // Synthesised code (e.g. `@Derive` expansions) carries spans in a virtual
    // file with no source text; report the message without an excerpt.
    if span.file.0 as usize >= map.file_count() {
        eprintln!("<generated>: {severity}: {message}");
        return;
    }
    let sf = map.file(span.file);
    let start = sf.line_col(span.lo);
    eprintln!("{}:{}:{}: {severity}: {message}", sf.name, start.line, start.col);

    // Show the offending line and underline the span (single-line spans only).
    let line_idx = (start.line - 1) as usize;
    if let Some(line_text) = sf.src.lines().nth(line_idx) {
        let gutter = format!("{} | ", start.line);
        eprintln!("{gutter}{line_text}");
        let pad = " ".repeat(gutter.len() + (start.col as usize - 1));
        let width = span.len().max(1) as usize;
        eprintln!("{pad}{}", "^".repeat(width));
    }
}
