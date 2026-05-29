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

use clap::{Parser, Subcommand, ValueEnum};

use compiler::ast::{Item, ItemKind, Module, ModuleKind, Visibility};
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
    /// Generate Markdown API documentation for a file or project's public items
    /// (`docs/23`), printed to stdout.
    Doc {
        /// Path to the `.otter` source file, project directory, or `project.toml`.
        file: PathBuf,
    },
    /// Pretty-print an intermediate representation to stdout (observability).
    /// Output is stable and deterministic, so it is safe to snapshot-test.
    Emit {
        /// Which representation to print.
        #[arg(value_enum)]
        ir: EmitIr,
        /// Path to the `.otter` source file.
        file: PathBuf,
    },
}

/// The intermediate representations `otter_fusion emit` can print.
#[derive(Clone, Copy, ValueEnum)]
enum EmitIr {
    /// The lexer's token stream (one token per line, with spans).
    Tokens,
    /// The parsed, untyped abstract syntax tree.
    Ast,
    /// The typed, resolved, desugared High-level IR the checker produces.
    Hir,
    /// The generated Cranelift IR (post-codegen, pre-machine-code) per function.
    Clif,
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
        Command::Emit { ir, file } => {
            let entry = match resolve_entry(&file) {
                Ok(e) => e,
                Err(msg) => {
                    eprintln!("error: {msg}");
                    return ExitCode::FAILURE;
                }
            };
            return emit(&entry, ir);
        }
        Command::Doc { file } => {
            let entry = match resolve_entry(&file) {
                Ok(e) => e,
                Err(msg) => {
                    eprintln!("error: {msg}");
                    return ExitCode::FAILURE;
                }
            };
            return gen_doc(&entry);
        }
    };
    // Resolve the entry source file: a `.otter` file is used directly; a
    // directory or `project.toml` is read as a project manifest (`docs/17`).
    let entry = match resolve_entry(&file) {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    drive(&entry, stage, release)
}

/// A parsed `project.toml` manifest (`docs/17` §17.1) — the subset the
/// toolchain currently uses.
struct Manifest {
    /// `kind = "binary" | "library" | "library+bins"` (default `"binary"`).
    kind: String,
    /// Explicit `entry = "..."`, if given.
    entry: Option<String>,
    /// `src = "..."` source root (default `"src"`).
    src: String,
}

/// Resolve the entry `.otter` source file from a CLI path argument: a `.otter`
/// file (or any non-directory, non-manifest path) is used directly; a directory
/// or a `project.toml` path is read as a project manifest and its entry file is
/// returned (relative to the project root).
fn resolve_entry(input: &Path) -> Result<PathBuf, String> {
    if input.extension().and_then(|e| e.to_str()) == Some("otter") {
        return Ok(input.to_path_buf());
    }
    let (manifest_path, root) = if input.is_dir() {
        (input.join("project.toml"), input.to_path_buf())
    } else if input.file_name().and_then(|n| n.to_str()) == Some("project.toml") {
        (input.to_path_buf(), input.parent().unwrap_or(Path::new(".")).to_path_buf())
    } else {
        // Not a `.otter` file, directory, or manifest — treat it as a source path
        // and let the reader report a clear error if it does not exist.
        return Ok(input.to_path_buf());
    };
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read manifest `{}`: {e}", manifest_path.display()))?;
    let m = parse_manifest(&text);
    let entry_rel = match m.entry {
        Some(e) => e,
        None => match m.kind.as_str() {
            "library" | "library+bins" => format!("{}/lib.otter", m.src),
            // "binary" and any unrecognised kind default to a binary entry.
            _ => format!("{}/main.otter", m.src),
        },
    };
    let entry = root.join(entry_rel);
    if !entry.exists() {
        return Err(format!(
            "manifest `{}` points at entry `{}`, which does not exist",
            manifest_path.display(),
            entry.display()
        ));
    }
    Ok(entry)
}

/// Generate Markdown API documentation for `path`'s public items (`docs/23`),
/// printed to stdout. Doc comments (`///`) become prose; each item's signature
/// is sliced from source so it matches the author's exact syntax.
fn gen_doc(path: &Path) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read `{}`: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let mut map = SourceMap::new();
    let file = map.add_file(path.display().to_string(), src.clone());
    let (tokens, _lex_errors) = lex(&src, file);
    let (module, parse_errors) = parse(&src, &tokens);
    for e in &parse_errors {
        render(&map, e.span, "error", &e.kind.to_string());
    }

    println!("# API Documentation\n");
    let mut any = false;
    for item in &module.items {
        if !matches!(item.visibility, Visibility::Public(_)) {
            continue;
        }
        let Some((label, name)) = doc_item_label(item) else { continue };
        any = true;
        println!("## {label} `{name}`\n");
        println!("```otter\n{}\n```\n", doc_signature(&src, item));
        let prose = render_doc_comments(item);
        if !prose.is_empty() {
            println!("{prose}\n");
        }
    }
    if !any {
        println!("_No public items._");
    }
    ExitCode::SUCCESS
}

/// The `(kind label, name)` of a documentable top-level item, or `None` for
/// kinds that are not documented as standalone API entries.
fn doc_item_label(item: &Item) -> Option<(&'static str, String)> {
    Some(match &item.kind {
        ItemKind::Function(f) => ("function", f.name.name.clone()),
        ItemKind::Struct(s) => ("struct", s.name.name.clone()),
        ItemKind::Interface(i) => ("interface", i.name.name.clone()),
        ItemKind::TypeAlias(t) => ("type", t.name.name.clone()),
        ItemKind::Var(v) => ("var", v.name.name.clone()),
        _ => return None, // extends/externs/modules/imports: follow-up
    })
}

/// The signature shown for an item: a function's header (up to its body), else
/// the whole item source (struct fields, interface methods, alias body…).
fn doc_signature(src: &str, item: &Item) -> String {
    // Start after the doc comments (rendered separately below); attributes that
    // follow the docs are kept as part of the signature.
    let start = item
        .docs
        .iter()
        .map(|d| d.span.hi.to_usize())
        .max()
        .unwrap_or(item.span.lo.to_usize());
    let end = if let ItemKind::Function(f) = &item.kind {
        f.body.as_ref().map(|b| b.span.lo.to_usize()).unwrap_or(item.span.hi.to_usize())
    } else {
        item.span.hi.to_usize()
    };
    src.get(start..end).unwrap_or("").trim().to_string()
}

/// Join an item's `///` doc comments into Markdown prose (the leading marker and
/// one optional space are stripped from each line).
fn render_doc_comments(item: &Item) -> String {
    item.docs
        .iter()
        .map(|d| {
            let t = d.text.trim_start();
            let t = t.strip_prefix("///").or_else(|| t.strip_prefix("//!")).unwrap_or(t);
            t.strip_prefix(' ').unwrap_or(t).trim_end()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the manifest subset we use: `key = "value"` pairs (sections and other
/// keys are tolerated and ignored). A hand-rolled reader avoids a TOML
/// dependency for the few fields the toolchain currently reads.
fn parse_manifest(text: &str) -> Manifest {
    let mut kind = "binary".to_string();
    let mut entry = None;
    let mut src = "src".to_string();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let Some((key, val)) = line.split_once('=') else { continue };
        let key = key.trim();
        let val = val.trim().trim_matches('"').to_string();
        match key {
            "kind" => kind = val,
            "entry" => entry = Some(val),
            "src" => src = val,
            _ => {}
        }
    }
    Manifest { kind, entry, src }
}

/// Pretty-print one intermediate representation of `path` to stdout. Front-end
/// diagnostics go to stderr; the IR is emitted best-effort so a partially
/// broken program can still be inspected (parsing recovers; HIR lowering is
/// total). Output is deterministic.
fn emit(path: &Path, ir: EmitIr) -> ExitCode {
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

    let (tokens, lex_errors) = lex(&src, file);
    for e in &lex_errors {
        render(&map, e.span, "error", &e.kind.to_string());
    }
    if let EmitIr::Tokens = ir {
        for t in &tokens {
            println!("{:?} @ {}..{}", t.kind, t.span.lo.0, t.span.hi.0);
        }
        return ExitCode::SUCCESS;
    }

    let (module, parse_errors) = parse(&src, &tokens);
    for e in &parse_errors {
        render(&map, e.span, "error", &e.kind.to_string());
    }
    match ir {
        EmitIr::Ast => {
            // The AST derives a deterministic pretty `Debug`; a bespoke printer
            // is a follow-up. (Tokens and HIR have purpose-built printers.)
            println!("{module:#?}");
        }
        EmitIr::Hir => {
            let mut externals = Externals::new();
            load_submodules(&mut map, path, &module, &mut Vec::new(), &mut externals);
            let analysis = analyze_multi(&module, &externals);
            for e in &analysis.errors {
                render(&map, e.span, "error", &e.kind.to_string());
            }
            print!(
                "{}",
                compiler::hir::print_program(&analysis.hir, &analysis.tcx, &analysis.program)
            );
        }
        EmitIr::Clif => {
            let mut externals = Externals::new();
            load_submodules(&mut map, path, &module, &mut Vec::new(), &mut externals);
            let analysis = analyze_multi(&module, &externals);
            for e in &analysis.errors {
                render(&map, e.span, "error", &e.kind.to_string());
            }
            // Cranelift IR is only well-formed for an error-free program.
            if analysis.errors.is_empty() {
                match backend::compile_clif(&analysis) {
                    Ok(text) => print!("{text}"),
                    Err(e) => render(&map, e.span, "error", &format!("codegen: {}", e.message)),
                }
            }
        }
        EmitIr::Tokens => unreachable!("handled above"),
    }
    ExitCode::SUCCESS
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
    // The main source file (FileId 0) backs DWARF line tables.
    let main_file = map.file(compiler::span::FileId(0));
    if let Err(e) =
        backend::compile_object(analysis, &obj, &main_file.src, &main_file.name)
    {
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
    // Libraries requested via `@Link(lib = "…")` (`docs/19` §13), derived from
    // the program's attributes (no checker side table).
    for lib in &analysis.hir.link_libs {
        cmd.arg(format!("-l{lib}"));
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
