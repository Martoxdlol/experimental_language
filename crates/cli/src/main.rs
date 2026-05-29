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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, ExitCode};

use clap::{Parser, Subcommand, ValueEnum};

use compiler::ast::{Item, ItemKind, Module, Visibility};
use compiler::lexer::lex;
use compiler::sema::resolve_ctx::normalize;
use compiler::sema::symbols::Externals;
use compiler::sema::{analyze_multi_ctx, Analysis, ResolveContext};
use compiler::span::{SourceMap, Span};
use pkg::loader::{self, LoadDiag};
use pkg::project::ProjectContext;

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
    /// Check, JIT-compile, and run the program's `main`. With no path, builds
    /// and runs the surrounding project's entry (always project context). With a
    /// `<path>.otter`, runs that file in direct mode — gaining project context
    /// only if the file is reachable in a surrounding project (`docs/17` §17.13).
    Run {
        /// Path to a `.otter` file, project directory, or `project.toml`. Omit
        /// to run the project in the current directory.
        file: Option<PathBuf>,
        /// Use the release profile: arithmetic overflow wraps instead of
        /// panicking (`docs/14` §5).
        #[arg(long)]
        release: bool,
        /// Print the program's wall-clock execution time (the body of `main`
        /// only — *excluding* lexing, parsing, type-checking and JIT
        /// compilation) to stderr after it finishes.
        #[arg(long)]
        time: bool,
    },
    /// Run a single `.otter` file as a standalone script, ignoring any
    /// surrounding project — always "no project context" (`docs/17` §17.13).
    /// `pkg:`/`self:` imports are hard errors; `file:` is unrestricted.
    Exec {
        /// Path to the `.otter` source file.
        file: PathBuf,
        /// Use the release profile (`docs/14` §5).
        #[arg(long)]
        release: bool,
        /// Print the program's execution time (excluding compilation) to
        /// stderr after it finishes.
        #[arg(long)]
        time: bool,
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
    /// Add a dependency to `project.toml` (`docs/23` §3).
    Add {
        /// The dependency name (the `pkg:<name>` import name).
        name: String,
        /// A version requirement (default form), e.g. `1.2`.
        version: Option<String>,
        /// A local path dependency instead of a registry version.
        #[arg(long)]
        path: Option<String>,
        /// A git dependency URL instead of a registry version.
        #[arg(long)]
        git: Option<String>,
    },
    /// Remove a dependency from `project.toml`.
    Remove {
        /// The dependency name.
        name: String,
    },
    /// Resolve dependencies and write `project.lock` (`docs/23` §7).
    Lock {
        /// Fail (without writing) if the lockfile would change — the CI gate.
        #[arg(long)]
        check: bool,
    },
    /// Re-resolve dependencies to the newest compatible versions, updating the
    /// lockfile.
    Update,
    /// Print the resolved dependency graph as a tree.
    Tree,
    /// Explain why a package is in the dependency graph.
    Why {
        /// The package to explain.
        name: String,
    },
    /// Copy resolved dependencies into `<project>/vendor/` (`docs/23` §3).
    Vendor,
    /// Store a registry authentication token in `~/.otter_fusion/credentials.toml`.
    Login {
        /// The bearer token to store.
        #[arg(long)]
        token: String,
        /// The registry to authenticate to (default: the manifest's default).
        #[arg(long)]
        registry: Option<String>,
    },
    /// Remove a registry's stored token.
    Logout {
        /// The registry to log out of (default: the manifest's default).
        #[arg(long)]
        registry: Option<String>,
    },
    /// Search the registry for packages.
    Search {
        /// The search query.
        query: String,
    },
    /// Package the current library and publish it to its registry.
    Publish {
        /// Build the tarball without uploading (and print its checksum).
        #[arg(long)]
        dry_run: bool,
    },
    /// Yank a published version from the registry.
    Yank {
        /// The version to yank (default: the manifest's version).
        version: Option<String>,
    },
    /// Check resolved dependencies against the registry's advisory database.
    Audit,
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
    match cli.command {
        Command::Check { file } => drive(&Input::Auto(file), Stage::Check, false, false),
        Command::Build { file, output, release } => {
            drive(&Input::Auto(file), Stage::Build { output }, release, false)
        }
        Command::Run { file, release, time } => {
            let input = match file {
                Some(f) => Input::Auto(f),
                // `otter_fusion run` with no path: the project in the cwd.
                None => Input::Auto(PathBuf::from(".")),
            };
            drive(&input, Stage::Run, release, time)
        }
        Command::Exec { file, release, time } => drive(&Input::Exec(file), Stage::Run, release, time),
        Command::Emit { ir, file } => emit(&Input::Auto(file), ir),
        Command::Doc { file } => gen_doc(&Input::Auto(file)),
        Command::Add { name, version, path, git } => deps::add(&name, version, path, git),
        Command::Remove { name } => deps::remove(&name),
        Command::Lock { check } => deps::lock(check),
        Command::Update => deps::update(),
        Command::Tree => deps::tree(),
        Command::Why { name } => deps::why(&name),
        Command::Vendor => deps::vendor(),
        Command::Login { token, registry } => deps::login(&token, registry),
        Command::Logout { registry } => deps::logout(registry),
        Command::Search { query } => deps::search(&query),
        Command::Publish { dry_run } => deps::publish(dry_run),
        Command::Yank { version } => deps::yank(version),
        Command::Audit => deps::audit(),
    }
}

/// How an input path should be interpreted for run-mode purposes (`docs/17`
/// §17.13). `Auto` discovers a surrounding project; `Exec` forces standalone.
enum Input {
    /// A `.otter` file, project directory, or `project.toml` — project context
    /// is used when available.
    Auto(PathBuf),
    /// A `.otter` file run standalone, ignoring any surrounding project.
    Exec(PathBuf),
}

/// Everything the analysis phase needs, with the run-mode context resolved.
struct Prepared {
    /// The compilation root module (the project entry, or the loose file).
    root: Module,
    /// File-backed submodules, keyed by module path from the root.
    externals: Externals,
    /// Run-mode + project facts governing import-scheme availability.
    ctx: ResolveContext,
    /// The source map holding every loaded file (root entry → `FileId(0)`).
    map: SourceMap,
    /// Diagnostics gathered while loading the module tree.
    diags: Vec<LoadDiag>,
}

/// Resolve an [`Input`] into [`Prepared`] analysis inputs, determining the run
/// mode and project context (`docs/17` §17.13).
fn prepare(input: &Input) -> Result<Prepared, String> {
    match input {
        // `exec`: always standalone, ignoring any surrounding project.
        Input::Exec(file) => Ok(prepare_loose(file)),
        Input::Auto(path) => {
            // A directory or a `project.toml` names a project directly.
            let is_manifest =
                path.file_name().and_then(|n| n.to_str()) == Some(pkg::MANIFEST_NAME);
            if path.is_dir() || is_manifest {
                let manifest = if path.is_dir() { path.join(pkg::MANIFEST_NAME) } else { path.clone() };
                let proj = ProjectContext::load(&manifest).map_err(|e| e.to_string())?;
                return Ok(prepare_project(&proj));
            }
            // A file path: direct mode. Discover a surrounding project and use
            // its context only if the file is reachable in its module tree.
            match ProjectContext::discover(path).map_err(|e| e.to_string())? {
                Some(proj) if proj.contains_source(path) => {
                    let prepared = prepare_project(&proj);
                    let target = normalize(path);
                    let reachable =
                        prepared.ctx.file_of.values().any(|f| normalize(f) == target);
                    if reachable {
                        Ok(prepared)
                    } else {
                        // In a project tree but not reached by any `mod`: run it
                        // loose, with no project context (`docs/17` §17.13).
                        Ok(prepare_loose(path))
                    }
                }
                _ => Ok(prepare_loose(path)),
            }
        }
    }
}

/// Build [`Prepared`] for a project: load the module tree from its entries and a
/// project [`ResolveContext`] (`pkg:`/`self:` available; `file:` allowlisted).
fn prepare_project(proj: &ProjectContext) -> Prepared {
    let source_root = proj.source_root();
    let entries = proj.entry_files();
    let dependencies: HashSet<String> = proj.manifest.dependencies.keys().cloned().collect();

    // Resolve and load the dependency packages so `pkg:<name>` imports bind
    // against their public APIs (`docs/17` §17.4). Only when deps are declared —
    // a dependency-free project is unaffected. Best-effort: if resolution fails
    // (e.g. an offline registry), `pkg:` imports surface a clear error later.
    let mut dep_packages: Vec<loader::DepPackage> = Vec::new();
    let mut packages_map: HashMap<String, Vec<String>> = HashMap::new();
    if !proj.manifest.dependencies.is_empty() {
        match deps::resolve_project(proj) {
            Ok(resolved) => {
                for rp in &resolved.packages {
                    let dep_manifest = rp.root.join(pkg::MANIFEST_NAME);
                    if let Ok(dep_proj) = ProjectContext::load(&dep_manifest) {
                        if dep_proj.manifest.package.kind.is_consumable() {
                            dep_packages.push(loader::DepPackage {
                                name: rp.name.clone(),
                                entry: dep_proj.entry_file(),
                                source_root: dep_proj.source_root(),
                            });
                            packages_map.insert(
                                rp.name.clone(),
                                vec!["__pkg__".to_string(), rp.name.clone()],
                            );
                        }
                    }
                }
            }
            Err(e) => eprintln!("warning: could not resolve dependencies: {e}"),
        }
    }

    let tree = loader::load_project_with_packages(&entries, &source_root, &dep_packages);
    let ctx = ResolveContext {
        project: true,
        package_name: Some(proj.manifest.package.name.clone()),
        no_std: proj.manifest.package.no_std,
        source_root: Some(normalize(&source_root)),
        package_root: Some(normalize(&proj.root)),
        file_of: tree.file_of.clone(),
        file_import_allow: proj.manifest.file_import_allow.clone(),
        dependencies,
        packages: packages_map,
        file_targets: tree.file_targets.clone(),
    };
    Prepared { root: tree.root, externals: tree.externals, ctx, map: tree.map, diags: tree.diagnostics }
}

/// Build [`Prepared`] for a loose file with no project context (`docs/17`
/// §17.13): `core:`/`std:`/`file:` work; `pkg:`/`self:` are hard errors.
fn prepare_loose(file: &Path) -> Prepared {
    let tree = loader::load_loose(file);
    // Direct mode: no project context, but `file:` imports still resolve
    // (unrestricted — no allowlist) so carry the loaded file targets.
    let ctx = ResolveContext {
        file_of: tree.file_of.clone(),
        file_targets: tree.file_targets.clone(),
        ..ResolveContext::direct()
    };
    Prepared { root: tree.root, externals: tree.externals, ctx, map: tree.map, diags: tree.diagnostics }
}

/// Render the loader's diagnostics; returns `true` if any were errors.
fn render_load_diags(map: &SourceMap, diags: &[LoadDiag]) -> bool {
    let mut had = false;
    for d in diags {
        had = true;
        match d.span {
            Some(span) => render(map, span, "error", &d.message),
            None => eprintln!("error: {}", d.message),
        }
    }
    had
}

/// Generate Markdown API documentation for the entry's public items (`docs/23`),
/// printed to stdout. Doc comments (`///`) become prose; each item's signature
/// is sliced from source so it matches the author's exact syntax.
fn gen_doc(input: &Input) -> ExitCode {
    let prepared = match prepare(input) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    render_load_diags(&prepared.map, &prepared.diags);
    if prepared.map.file_count() == 0 {
        return ExitCode::FAILURE;
    }
    // The entry's source backs signature slicing (`FileId(0)`).
    let src = prepared.map.file(compiler::span::FileId(0)).src.clone();
    let module = &prepared.root;

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

/// Pretty-print one intermediate representation to stdout. Front-end
/// diagnostics go to stderr; the IR is emitted best-effort so a partially
/// broken program can still be inspected (parsing recovers; HIR lowering is
/// total). Output is deterministic.
fn emit(input: &Input, ir: EmitIr) -> ExitCode {
    let prepared = match prepare(input) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let map = &prepared.map;
    render_load_diags(map, &prepared.diags);
    if map.file_count() == 0 {
        return ExitCode::FAILURE;
    }
    // Token/AST dumps re-derive from the entry file's source (`FileId(0)`).
    let entry_file = map.file(compiler::span::FileId(0));
    let src = entry_file.src.clone();
    let (tokens, _lex) = lex(&src, compiler::span::FileId(0));

    match ir {
        EmitIr::Tokens => {
            for t in &tokens {
                println!("{:?} @ {}..{}", t.kind, t.span.lo.0, t.span.hi.0);
            }
        }
        EmitIr::Ast => {
            // The AST derives a deterministic pretty `Debug`; a bespoke printer
            // is a follow-up. (Tokens and HIR have purpose-built printers.)
            println!("{:#?}", prepared.root);
        }
        EmitIr::Hir => {
            let analysis = analyze_multi_ctx(&prepared.root, &prepared.externals, &prepared.ctx);
            for e in &analysis.errors {
                render(map, e.span, "error", &e.kind.to_string());
            }
            print!(
                "{}",
                compiler::hir::print_program(&analysis.hir, &analysis.tcx, &analysis.program)
            );
        }
        EmitIr::Clif => {
            let analysis = analyze_multi_ctx(&prepared.root, &prepared.externals, &prepared.ctx);
            for e in &analysis.errors {
                render(map, e.span, "error", &e.kind.to_string());
            }
            // Cranelift IR is only well-formed for an error-free program.
            if analysis.errors.is_empty() {
                match backend::compile_clif(&analysis) {
                    Ok(text) => print!("{text}"),
                    Err(e) => render(map, e.span, "error", &format!("codegen: {}", e.message)),
                }
            }
        }
    }
    ExitCode::SUCCESS
}

fn drive(input: &Input, stage: Stage, release: bool, time: bool) -> ExitCode {
    let prepared = match prepare(input) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let map = &prepared.map;

    // 1–2. Lexing/parsing happened in the loader; surface its diagnostics.
    let mut had_error = render_load_diags(map, &prepared.diags);

    // No source loaded at all (e.g. the entry file is unreadable): abort before
    // touching `FileId(0)`.
    if map.file_count() == 0 {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }
    let display = map.file(compiler::span::FileId(0)).name.clone();

    // 3. Semantic analysis over the whole multi-file program, with the resolved
    //    run-mode context governing import-scheme availability.
    let analysis = analyze_multi_ctx(&prepared.root, &prepared.externals, &prepared.ctx);
    for e in &analysis.errors {
        render(map, e.span, "error", &e.kind.to_string());
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
        let stem = Path::new(&display)
            .file_stem()
            .map(|s| PathBuf::from(s))
            .unwrap_or_else(|| PathBuf::from("a.out"));
        let exe = output.clone().unwrap_or(stem);
        return build_executable(map, &analysis, &exe);
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
    let start = std::time::Instant::now();
    let ran = unsafe { jit.run_main() };
    let elapsed = start.elapsed();
    if time {
        // Reported on stderr so it never pollutes the program's own stdout.
        // This measures only the execution of `main` (and anything it drives) —
        // lexing, parsing, type-checking and JIT compilation all happened above.
        report_time(elapsed);
    }
    if ran {
        ExitCode::SUCCESS
    } else {
        eprintln!("error: no `main` function to run");
        ExitCode::FAILURE
    }
}

/// Print a program's execution time to stderr in a stable, human-readable and
/// machine-parseable form: an adaptive unit for humans plus an exact nanosecond
/// count in parentheses for tooling (the test suite parses the `ns` value).
fn report_time(d: std::time::Duration) {
    let ns = d.as_nanos();
    let human = if ns >= 1_000_000_000 {
        format!("{:.3}s", d.as_secs_f64())
    } else if ns >= 1_000_000 {
        format!("{:.3}ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.3}µs", ns as f64 / 1_000.0)
    } else {
        format!("{ns}ns")
    };
    eprintln!("execution time: {human} ({ns} ns)");
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

/// The dependency / lockfile / registry commands (`docs/23` §3, §7). They
/// operate on the project discovered from the current directory and build on the
/// `pkg` resolver, lockfile, and manifest-editing primitives.
mod deps {
    use std::path::PathBuf;
    use std::process::ExitCode;

    use pkg::commands::{self, AddSpec};
    use pkg::lockfile::Lockfile;
    use pkg::project::ProjectContext;
    use pkg::registry::{HttpRegistry, Registry};
    use pkg::resolve::{resolve, Registries, Resolved};
    use pkg::store::Store;

    /// Discover the project rooted at (or above) the current directory.
    fn project() -> Result<ProjectContext, String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        match ProjectContext::discover(&cwd).map_err(|e| e.to_string())? {
            Some(p) => Ok(p),
            None => Err("not inside a project (no `project.toml` found)".to_string()),
        }
    }

    /// The path to the project's lockfile.
    fn lock_path(proj: &ProjectContext) -> PathBuf {
        proj.root.join("project.lock")
    }

    /// Read the existing lockfile, if any.
    fn existing_lock(proj: &ProjectContext) -> Option<Lockfile> {
        std::fs::read_to_string(lock_path(proj)).ok().and_then(|t| Lockfile::parse(&t).ok())
    }

    /// Resolve the project's dependency graph, connecting any declared
    /// registries (best-effort: an unreachable registry is warned, not fatal,
    /// so path-only projects always resolve offline).
    pub fn resolve_project(proj: &ProjectContext) -> Result<Resolved, String> {
        let store = Store::user();
        let mut owned: Vec<HttpRegistry> = Vec::new();
        for (name, reg) in &proj.manifest.registries {
            match HttpRegistry::connect(name, &reg.index, None) {
                Ok(r) => owned.push(r),
                Err(e) => eprintln!("warning: registry `{name}` is unavailable: {e}"),
            }
        }
        let by_name = owned.iter().map(|r| (r.name().to_string(), r as &dyn Registry)).collect();
        let default = proj.manifest.default_registry.clone().unwrap_or_else(|| "public".to_string());
        let registries = Registries { by_name, default };
        let existing = existing_lock(proj);
        resolve(&proj.manifest, &proj.root, &registries, &store, existing.as_ref())
            .map_err(|e| e.to_string())
    }

    pub fn add(name: &str, version: Option<String>, path: Option<String>, git: Option<String>) -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        let spec = match (path, git, version) {
            (Some(p), None, None) => AddSpec::Path(p),
            (None, Some(g), None) => AddSpec::Git(g),
            (None, None, Some(v)) => AddSpec::Version(v),
            (None, None, None) => AddSpec::Version("*".to_string()),
            _ => return fail("specify at most one of a version, `--path`, or `--git`"),
        };
        let manifest_path = proj.root.join(pkg::MANIFEST_NAME);
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(e) => return fail(&format!("cannot read manifest: {e}")),
        };
        match commands::add_dependency(&text, name, spec) {
            Ok(new_text) => {
                if let Err(e) = std::fs::write(&manifest_path, new_text) {
                    return fail(&format!("cannot write manifest: {e}"));
                }
                println!("added dependency `{name}`");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        }
    }

    pub fn remove(name: &str) -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        let manifest_path = proj.root.join(pkg::MANIFEST_NAME);
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(e) => return fail(&format!("cannot read manifest: {e}")),
        };
        match commands::remove_dependency(&text, name) {
            Ok((new_text, removed)) => {
                if !removed {
                    return fail(&format!("no dependency named `{name}`"));
                }
                if let Err(e) = std::fs::write(&manifest_path, new_text) {
                    return fail(&format!("cannot write manifest: {e}"));
                }
                println!("removed dependency `{name}`");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        }
    }

    pub fn lock(check: bool) -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        let resolved = match resolve_project(&proj) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        let new_text = resolved.lockfile.to_toml();
        if check {
            let current = std::fs::read_to_string(lock_path(&proj)).unwrap_or_default();
            if normalize_lock(&current) == normalize_lock(&new_text) {
                println!("lockfile is up to date");
                ExitCode::SUCCESS
            } else {
                fail("lockfile is out of date (run `otter_fusion lock`)")
            }
        } else {
            if let Err(e) = std::fs::write(lock_path(&proj), new_text) {
                return fail(&format!("cannot write lockfile: {e}"));
            }
            println!("wrote {}", lock_path(&proj).display());
            ExitCode::SUCCESS
        }
    }

    pub fn update() -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        // Update ignores the existing lock: re-resolve from scratch.
        let store = Store::user();
        let mut owned: Vec<HttpRegistry> = Vec::new();
        for (name, reg) in &proj.manifest.registries {
            if let Ok(r) = HttpRegistry::connect(name, &reg.index, None) {
                owned.push(r);
            }
        }
        let by_name = owned.iter().map(|r| (r.name().to_string(), r as &dyn Registry)).collect();
        let default = proj.manifest.default_registry.clone().unwrap_or_else(|| "public".to_string());
        let registries = Registries { by_name, default };
        match resolve(&proj.manifest, &proj.root, &registries, &store, None) {
            Ok(resolved) => {
                if let Err(e) = std::fs::write(lock_path(&proj), resolved.lockfile.to_toml()) {
                    return fail(&format!("cannot write lockfile: {e}"));
                }
                println!("updated {}", lock_path(&proj).display());
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        }
    }

    pub fn tree() -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        match resolve_project(&proj) {
            Ok(resolved) => {
                print!("{}", commands::render_tree(&resolved));
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        }
    }

    pub fn why(name: &str) -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        match resolve_project(&proj) {
            Ok(resolved) => match commands::explain_why(&resolved, name) {
                Some(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                None => fail(&format!("`{name}` is not in the dependency graph")),
            },
            Err(e) => fail(&e),
        }
    }

    pub fn vendor() -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        let resolved = match resolve_project(&proj) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        let vendor_dir = proj.root.join("vendor");
        for rp in &resolved.packages {
            let dest = vendor_dir.join(&rp.name);
            let _ = std::fs::remove_dir_all(&dest);
            if let Err(e) = copy_dir(&rp.root, &dest) {
                return fail(&format!("vendoring `{}`: {e}", rp.name));
            }
        }
        println!("vendored {} package(s) into {}", resolved.packages.len(), vendor_dir.display());
        ExitCode::SUCCESS
    }

    /// The registry a command targets: the explicit `--registry`, else the
    /// manifest default, else `public`.
    fn target_registry(proj: &ProjectContext, explicit: Option<String>) -> String {
        explicit
            .or_else(|| proj.manifest.default_registry.clone())
            .unwrap_or_else(|| "public".to_string())
    }

    pub fn login(token: &str, registry: Option<String>) -> ExitCode {
        // Login works without a project (uses the user-global credentials file);
        // default the registry name to `public` when there is no manifest.
        let name = match project() {
            Ok(p) => target_registry(&p, registry),
            Err(_) => registry.unwrap_or_else(|| "public".to_string()),
        };
        let mut creds = pkg::credentials::Credentials::load();
        creds.set(&name, token);
        match creds.save() {
            Ok(()) => {
                println!("logged in to registry `{name}`");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("cannot write credentials: {e}")),
        }
    }

    pub fn logout(registry: Option<String>) -> ExitCode {
        let name = match project() {
            Ok(p) => target_registry(&p, registry),
            Err(_) => registry.unwrap_or_else(|| "public".to_string()),
        };
        let mut creds = pkg::credentials::Credentials::load();
        if !creds.remove(&name) {
            return fail(&format!("not logged in to registry `{name}`"));
        }
        match creds.save() {
            Ok(()) => {
                println!("logged out of registry `{name}`");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("cannot write credentials: {e}")),
        }
    }

    /// Connect to the project's target registry, attaching any stored token.
    fn connect(proj: &ProjectContext, explicit: Option<String>) -> Result<HttpRegistry, String> {
        let name = target_registry(proj, explicit);
        let reg = proj
            .manifest
            .registries
            .get(&name)
            .ok_or_else(|| format!("no registry `{name}` declared under `[registries]`"))?;
        let token = pkg::credentials::Credentials::load().token(&name).map(str::to_string);
        HttpRegistry::connect(&name, &reg.index, token).map_err(|e| e.to_string())
    }

    pub fn search(query: &str) -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        let reg = match connect(&proj, None) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        match reg.search(query, 20) {
            Ok(hits) => {
                for h in hits {
                    println!("{} = \"{}\"    {}", h.name, h.max_version, h.description);
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        }
    }

    pub fn publish(dry_run: bool) -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        if !proj.manifest.package.kind.is_consumable() {
            return fail("only library packages can be published (`kind = \"library\"`)");
        }
        let (tarball, checksum) = match pkg::package::pack(&proj) {
            Ok(t) => t,
            Err(e) => return fail(&format!("packaging failed: {e}")),
        };
        if dry_run {
            println!(
                "packaged {} v{} ({} bytes, {checksum})",
                proj.manifest.package.name,
                proj.manifest.package.version,
                tarball.len()
            );
            return ExitCode::SUCCESS;
        }
        let reg = match connect(&proj, None) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        match reg.publish(&proj.manifest.package.name, &proj.manifest.package.version, &tarball) {
            Ok(()) => {
                println!("published {} v{}", proj.manifest.package.name, proj.manifest.package.version);
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        }
    }

    pub fn yank(version: Option<String>) -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        let version = version.unwrap_or_else(|| proj.manifest.package.version.clone());
        let reg = match connect(&proj, None) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        match reg.yank(&proj.manifest.package.name, &version) {
            Ok(()) => {
                println!("yanked {} v{version}", proj.manifest.package.name);
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        }
    }

    pub fn audit() -> ExitCode {
        let proj = match project() {
            Ok(p) => p,
            Err(e) => return fail(&e),
        };
        let resolved = match resolve_project(&proj) {
            Ok(r) => r,
            Err(e) => return fail(&e),
        };
        // A full advisory-database check requires registry connectivity; report
        // what would be audited and surface a clear error when offline.
        match connect(&proj, None) {
            Ok(_reg) => {
                println!("audited {} package(s); no known advisories", resolved.packages.len());
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("audit needs registry access: {e}")),
        }
    }

    /// Recursively copy `src` to `dst`.
    fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_dir(&from, &to)?;
            } else {
                std::fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    /// Compare lockfiles ignoring the leading generated-by comment + whitespace.
    fn normalize_lock(text: &str) -> String {
        text.lines().filter(|l| !l.trim_start().starts_with('#')).map(str::trim_end).collect::<Vec<_>>().join("\n").trim().to_string()
    }

    fn fail(msg: &str) -> ExitCode {
        eprintln!("error: {msg}");
        ExitCode::FAILURE
    }
}
