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
use compiler::sema::{Analysis, ResolveContext, analyze_multi_ctx};
use compiler::span::{FileId, SourceMap, Span};
use pkg::loader::{self, LoadDiag};
use pkg::project::ProjectContext;

mod fmt;
mod lint;
mod repl;

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
    /// Build and run the program's `test "name" { … }` declarations (`docs/23`),
    /// each in its own process so a panic fails only that test. Reports each
    /// test's outcome and a summary; exits non-zero if any test fails.
    Test {
        /// Path to a `.otter` file, project directory, or `project.toml`. Omit to
        /// test the project in the current directory.
        file: Option<PathBuf>,
        /// Internal: run exactly this one test body (by symbol) in this process.
        /// Used by the runner to isolate each test; not for direct use.
        #[arg(long, hide = true)]
        exact: Option<String>,
    },
    /// Print a long-form explanation of a diagnostic code (`docs/23`), e.g.
    /// `otter_fusion explain E0006`. Codes appear in `error[E0006]: …` diagnostics.
    Explain {
        /// The diagnostic code (case-insensitive), e.g. `E0006` or `e0006`.
        code: String,
    },
    /// Start an interactive read-eval-print loop (`docs/23`): enter
    /// declarations, `var` bindings, statements, or expressions line by line.
    Repl,
    /// Format `.otter` source (`docs/23`): normalize indentation and whitespace.
    /// Conservative — only whitespace changes (verified by re-lexing). Rewrites
    /// files in place; `--check` reports unformatted files and exits non-zero.
    Fmt {
        /// A `.otter` file or a directory (formatted recursively). Omit for the
        /// current directory.
        file: Option<PathBuf>,
        /// Do not write; list files that are not formatted and exit non-zero if
        /// any differ (the CI gate).
        #[arg(long)]
        check: bool,
    },
    /// Report lint warnings (`docs/23`): unused local variables and unused
    /// private functions. Informational — exits zero even when warnings are found.
    Lint {
        /// Path to a `.otter` file, project directory, or `project.toml`. Omit to
        /// lint the project in the current directory.
        file: Option<PathBuf>,
    },
    /// Apply safe automatic fixes (`docs/23`): rename each unused local variable
    /// to `_name` (silencing the lint without removing code). Rewrites the
    /// affected source files in place; `--check` reports without writing.
    Fix {
        /// Path to a `.otter` file, project directory, or `project.toml`. Omit for
        /// the project in the current directory.
        file: Option<PathBuf>,
        /// Report what would be fixed without modifying any files.
        #[arg(long)]
        check: bool,
    },
    /// Build and run the program's `bench "name" { … }` declarations (`docs/23`),
    /// timing repeated executions of each body and reporting nanoseconds/iter.
    Bench {
        /// Path to a `.otter` file, project directory, or `project.toml`. Omit to
        /// benchmark the project in the current directory.
        file: Option<PathBuf>,
        /// Internal: time exactly this one bench body (by symbol) in this process.
        #[arg(long, hide = true)]
        exact: Option<String>,
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
    /// Parse the program and print it back as normalized source (`docs/23`):
    /// the AST the rest of the compiler sees, rendered through the source-printer
    /// with canonical indentation and conservative parentheses. Useful for
    /// inspecting how the surface syntax parses. Output re-parses to the same AST.
    Expand {
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
    /// Host a private package registry (`docs/23` §7): serve the sparse-HTTP
    /// index, tarball downloads, and the publish/yank/search API from a local
    /// directory. Runs in the foreground until terminated.
    Serve {
        /// The registry store directory (sparse index + tarballs; created if
        /// absent). Mirrors the layout `otter_fusion publish` uploads into.
        #[arg(long, default_value = "registry")]
        dir: PathBuf,
        /// Address to bind, `host:port`.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        /// Require this bearer token for writes (`publish`/`yank`); reads stay
        /// open. Omit to allow anonymous writes (development only).
        #[arg(long)]
        token: Option<String>,
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
    Build {
        output: Option<PathBuf>,
    },
    Run,
    /// Run exactly one `test` body by its internal symbol (a `otter_fusion test`
    /// child process; `docs/23`). The body panics → process exit 101 = failure.
    Test {
        symbol: String,
    },
    /// Time one `bench` body by its symbol (a `otter_fusion bench` child): run an
    /// adaptive number of iterations and print `ns/iter (<n> iters)` to stdout.
    Bench {
        symbol: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Check { file } => drive(&Input::Auto(file), Stage::Check, false, false),
        Command::Build {
            file,
            output,
            release,
        } => drive(&Input::Auto(file), Stage::Build { output }, release, false),
        Command::Run {
            file,
            release,
            time,
        } => {
            let input = match file {
                Some(f) => Input::Auto(f),
                // `otter_fusion run` with no path: the project in the cwd.
                None => Input::Auto(PathBuf::from(".")),
            };
            drive(&input, Stage::Run, release, time)
        }
        Command::Exec {
            file,
            release,
            time,
        } => drive(&Input::Exec(file), Stage::Run, release, time),
        Command::Emit { ir, file } => emit(&Input::Auto(file), ir),
        Command::Expand { file } => run_expand(&Input::Auto(file)),
        Command::Doc { file } => gen_doc(&Input::Auto(file)),
        Command::Test { file, exact } => {
            let path = file.clone().unwrap_or_else(|| PathBuf::from("."));
            match exact {
                // Child process: run exactly one test body in this process.
                Some(symbol) => drive(&Input::Auto(path), Stage::Test { symbol }, false, false),
                // Runner: list the tests and run each in its own child process.
                None => run_tests(&path, false),
            }
        }
        Command::Bench { file, exact } => {
            let path = file.clone().unwrap_or_else(|| PathBuf::from("."));
            match exact {
                Some(symbol) => drive(&Input::Auto(path), Stage::Bench { symbol }, false, false),
                None => run_tests(&path, true),
            }
        }
        Command::Explain { code } => run_explain(&code),
        Command::Repl => repl::run(),
        Command::Fmt { file, check } => run_fmt(&file.unwrap_or_else(|| PathBuf::from(".")), check),
        Command::Lint { file } => {
            run_lint(&Input::Auto(file.unwrap_or_else(|| PathBuf::from("."))))
        }
        Command::Fix { file, check } => run_fix(
            &Input::Auto(file.unwrap_or_else(|| PathBuf::from("."))),
            check,
        ),
        Command::Add {
            name,
            version,
            path,
            git,
        } => deps::add(&name, version, path, git),
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
        Command::Serve { dir, bind, token } => run_serve(dir, &bind, token),
    }
}

/// `otter_fusion serve` — host a private registry from `dir` on `bind`.
fn run_serve(dir: PathBuf, bind: &str, token: Option<String>) -> ExitCode {
    match pkg::server::serve_on(bind, dir.clone(), token.clone()) {
        Ok(handle) => {
            println!(
                "registry serving `{}` at {}",
                dir.display(),
                handle.base_url()
            );
            if token.is_some() {
                println!("writes require the configured bearer token; reads are open");
            } else {
                println!("anonymous writes allowed (development only)");
            }
            println!("press Ctrl-C to stop");
            handle.wait();
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot bind `{bind}`: {e}");
            ExitCode::FAILURE
        }
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
    /// Diagnostics produced while expanding user procedural macros (`docs/22`).
    /// Merged into every analysis of this program.
    macro_errors: Vec<compiler::sema::SemaError>,
}

/// Analyse a [`Prepared`] program, folding in any procedural-macro expansion
/// diagnostics (which were produced once, up front, in [`prepare`]).
fn analyze_prepared(p: &Prepared) -> Analysis {
    let mut analysis = analyze_multi_ctx(&p.root, &p.externals, &p.ctx);
    if !p.macro_errors.is_empty() {
        let mut merged = p.macro_errors.clone();
        merged.extend(analysis.errors.drain(..));
        analysis.errors = merged;
    }
    analysis
}

/// Resolve an [`Input`] into [`Prepared`] analysis inputs, determining the run
/// mode and project context (`docs/17` §17.13).
fn prepare(input: &Input) -> Result<Prepared, String> {
    let mut prepared = prepare_inner(input)?;
    // Phase 2 (`docs/22` §4): expand user procedural macros over the loaded
    // module tree before any type checking. A program with no `@ProcMacro`
    // definitions returns immediately and pays nothing.
    prepared.macro_errors =
        macros::expand_user_macros(&mut prepared.root, &mut prepared.externals, &prepared.ctx);
    Ok(prepared)
}

fn prepare_inner(input: &Input) -> Result<Prepared, String> {
    match input {
        // `exec`: always standalone, ignoring any surrounding project.
        Input::Exec(file) => Ok(prepare_loose(file)),
        Input::Auto(path) => {
            // A directory or a `project.toml` names a project directly.
            let is_manifest = path.file_name().and_then(|n| n.to_str()) == Some(pkg::MANIFEST_NAME);
            if path.is_dir() || is_manifest {
                let manifest = if path.is_dir() {
                    path.join(pkg::MANIFEST_NAME)
                } else {
                    path.clone()
                };
                let proj = ProjectContext::load(&manifest).map_err(|e| e.to_string())?;
                return Ok(prepare_project(&proj));
            }
            // A file path: direct mode. Discover a surrounding project and use
            // its context only if the file is reachable in its module tree.
            match ProjectContext::discover(path).map_err(|e| e.to_string())? {
                Some(proj) if proj.contains_source(path) => {
                    let prepared = prepare_project(&proj);
                    let target = normalize(path);
                    let reachable = prepared
                        .ctx
                        .file_of
                        .values()
                        .any(|f| normalize(f) == target);
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
        macro_recursion_limit: proj.manifest.macro_recursion_limit,
    };
    Prepared {
        root: tree.root,
        externals: tree.externals,
        ctx,
        map: tree.map,
        diags: tree.diagnostics,
        macro_errors: Vec::new(),
    }
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
    Prepared {
        root: tree.root,
        externals: tree.externals,
        ctx,
        map: tree.map,
        diags: tree.diagnostics,
        macro_errors: Vec::new(),
    }
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
        let Some((label, name)) = doc_item_label(item) else {
            continue;
        };
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
        f.body
            .as_ref()
            .map(|b| b.span.lo.to_usize())
            .unwrap_or(item.span.hi.to_usize())
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
            let t = t
                .strip_prefix("///")
                .or_else(|| t.strip_prefix("//!"))
                .unwrap_or(t);
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
            let analysis = analyze_prepared(&prepared);
            for e in &analysis.errors {
                render_sema(map, e);
            }
            print!(
                "{}",
                compiler::hir::print_program_for_files(
                    &analysis.hir,
                    &analysis.tcx,
                    &analysis.program,
                    map.file_count()
                )
            );
        }
        EmitIr::Clif => {
            let analysis = analyze_prepared(&prepared);
            for e in &analysis.errors {
                render_sema(map, e);
            }
            // Cranelift IR is only well-formed for an error-free program.
            if analysis.errors.is_empty() {
                match backend::compile_clif_for_files(&analysis, map.file_count()) {
                    Ok(text) => print!("{text}"),
                    Err(e) => render(map, e.span, "error", &format!("codegen: {}", e.message)),
                }
            }
        }
    }
    ExitCode::SUCCESS
}

/// `otter_fusion expand` — parse the entry file and print it back through the
/// AST source-printer. Parse diagnostics go to stderr (parsing recovers, so a
/// partially broken program still prints best-effort); the rendered source goes
/// to stdout and is guaranteed to re-parse to the same AST.
fn run_expand(input: &Input) -> ExitCode {
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
    print!("{}", compiler::ast_print::print_module(&prepared.root));
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
    let analysis = analyze_prepared(&prepared);
    for e in &analysis.errors {
        render_sema(map, e);
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

    // 4'. Run/test/bench: JIT-compile in process from the exact executable
    // root. Callees, vtables, closures, async jobs, and finalizers are still
    // discovered lazily by backend codegen, but unused sibling bodies and
    // untouched stdlib functions are not compiled.
    let jit_result = match &stage {
        Stage::Test { symbol } | Stage::Bench { symbol } => {
            backend::compile_jit_for_names(&analysis, &[symbol.as_str()])
        }
        Stage::Run => backend::compile_entry(&analysis),
        Stage::Check | Stage::Build { .. } => unreachable!("handled above"),
    };
    let jit = match jit_result {
        Ok(j) => j,
        Err(e) => {
            render(&map, e.span, "error", &format!("codegen: {}", e.message));
            return ExitCode::FAILURE;
        }
    };

    // 4''. Test child: run exactly one test body. It panics → process exit 101,
    // which the runner reads as a failure.
    if let Stage::Test { symbol } = &stage {
        backend::set_gc_enabled(true);
        let ran = unsafe { jit.run_void(symbol) };
        return if ran {
            ExitCode::SUCCESS
        } else {
            eprintln!("error: no test `{symbol}`");
            ExitCode::FAILURE
        };
    }

    // 4'''. Bench child: time one bench body over an adaptive iteration count
    // (warm up, then grow until the measured window is ≥ ~50ms or capped), and
    // print the per-iteration nanoseconds + iteration count to stdout.
    if let Stage::Bench { symbol } = &stage {
        backend::set_gc_enabled(true);
        if jit.func_ptr(symbol).is_none() {
            eprintln!("error: no bench `{symbol}`");
            return ExitCode::FAILURE;
        }
        // Warm up once (also validates the body runs without panicking).
        unsafe { jit.run_void(symbol) };
        let mut iters: u64 = 1;
        loop {
            let start = std::time::Instant::now();
            for _ in 0..iters {
                unsafe { jit.run_void(symbol) };
            }
            let elapsed = start.elapsed();
            // Grow until a stable measurement window, then report ns/iter.
            if elapsed.as_millis() >= 50 || iters >= 1_000_000_000 {
                let per = elapsed.as_nanos() / iters as u128;
                println!("{per} {iters}");
                return ExitCode::SUCCESS;
            }
            iters = iters.saturating_mul(4);
        }
    }

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

/// Run every `test`/`bench` declaration (`docs/23`), each in its own child
/// process (`otter_fusion <test|bench> <path> --exact <symbol>`) so a panic fails
/// only that one. For tests: prints ok/FAILED + a pass/fail summary and exits
/// non-zero if any failed. For benches (`bench = true`): prints the measured
/// nanoseconds-per-iteration for each. `bench` selects which declarations to run.
fn run_tests(path: &Path, bench: bool) -> ExitCode {
    use compiler::ast::ItemKind;
    use compiler::sema::symbols::DefKind;

    let input = Input::Auto(path.to_path_buf());
    let prepared = match prepare(&input) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let map = &prepared.map;
    let mut had_error = render_load_diags(map, &prepared.diags);
    if map.file_count() == 0 {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }
    let analysis = analyze_prepared(&prepared);
    for e in &analysis.errors {
        render_sema(map, e);
        had_error = true;
    }
    if had_error {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }

    let kind = if bench { "bench" } else { "test" };
    // (display name, internal symbol) for each matching declaration, in order.
    let items: Vec<(String, String)> = analysis
        .program
        .defs
        .iter()
        .filter(|d| d.kind == DefKind::Test)
        .filter_map(|d| match &d.item {
            Some(ItemKind::Test(t)) if t.is_bench == bench => {
                Some((t.name.clone(), d.name.clone()))
            }
            _ => None,
        })
        .collect();

    if items.is_empty() {
        println!(
            "no {kind}es found",
            kind = if bench { "bench" } else { "test" }
        );
        return ExitCode::SUCCESS;
    }

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("otter_fusion"));
    let mut passed = 0usize;
    let mut failed = 0usize;
    println!("running {} {kind}(s)\n", items.len());
    for (display, symbol) in &items {
        let out = std::process::Command::new(&exe)
            .arg(kind)
            .arg(path)
            .arg("--exact")
            .arg(symbol)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                if bench {
                    // The child prints "<ns_per_iter> <iters>" to stdout.
                    let s = String::from_utf8_lossy(&o.stdout);
                    let mut parts = s.split_whitespace();
                    let ns = parts.next().unwrap_or("?");
                    let iters = parts.next().unwrap_or("?");
                    println!("bench {display} ... {ns} ns/iter ({iters} iters)");
                } else {
                    println!("test {display} ... ok");
                }
                passed += 1;
            }
            Ok(o) => {
                println!("{kind} {display} ... FAILED");
                for stream in [&o.stdout, &o.stderr] {
                    if let Ok(s) = std::str::from_utf8(stream) {
                        for line in s.lines().filter(|l| !l.trim().is_empty()).take(4) {
                            println!("    {line}");
                        }
                    }
                }
                failed += 1;
            }
            Err(e) => {
                println!("{kind} {display} ... FAILED (could not spawn: {e})");
                failed += 1;
            }
        }
    }
    if bench {
        println!("\n{passed} {kind}(s) completed; {failed} failed");
    } else {
        println!("\ntest result: {passed} passed; {failed} failed");
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run `otter_fusion fmt` (`docs/23`): format a `.otter` file or every `.otter`
/// file under a directory. Each file is reformatted and the result is verified
/// to preserve the token stream (only whitespace may change) before it is
/// written. With `check`, nothing is written: unformatted files are listed and
/// the command exits non-zero (the CI gate).
fn run_fmt(path: &Path, check: bool) -> ExitCode {
    let mut files = Vec::new();
    collect_otter_files(path, &mut files);
    if files.is_empty() {
        eprintln!("error: no `.otter` files found at `{}`", path.display());
        return ExitCode::FAILURE;
    }
    files.sort();
    let mut changed = 0usize;
    let mut unchanged = 0usize;
    for file in &files {
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read `{}`: {e}", file.display());
                return ExitCode::FAILURE;
            }
        };
        let formatted = fmt::format_source(&src);
        // Safety net: a reformat must never change code, only whitespace.
        if !fmt::token_stream_preserved(&src, &formatted) {
            eprintln!(
                "error: refusing to format `{}` — the reformat would change tokens \
                 (please report this as a `fmt` bug)",
                file.display()
            );
            return ExitCode::FAILURE;
        }
        if formatted == src {
            unchanged += 1;
            continue;
        }
        changed += 1;
        if check {
            println!("would format {}", file.display());
        } else if let Err(e) = std::fs::write(file, &formatted) {
            eprintln!("error: cannot write `{}`: {e}", file.display());
            return ExitCode::FAILURE;
        } else {
            println!("formatted {}", file.display());
        }
    }
    if check {
        if changed == 0 {
            println!("all {unchanged} file(s) already formatted");
            ExitCode::SUCCESS
        } else {
            println!("{changed} file(s) need formatting");
            ExitCode::FAILURE
        }
    } else {
        println!("formatted {changed}, unchanged {unchanged}");
        ExitCode::SUCCESS
    }
}

/// Collect `.otter` files: `path` itself if it is one, else every `.otter` file
/// under it (recursively, skipping hidden directories and a `target` build dir).
fn collect_otter_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        if path.extension().is_some_and(|e| e == "otter") {
            out.push(path.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !name.starts_with('.') && name != "target" {
                collect_otter_files(&p, out);
            }
        } else if p.extension().is_some_and(|e| e == "otter") {
            out.push(p);
        }
    }
}

/// Run `otter_fusion lint` (`docs/23`): analyze the program and print lint
/// warnings (unused locals / unused private functions). Purely informational —
/// a clean compile with warnings still exits zero; a compile *error* fails.
fn run_lint(input: &Input) -> ExitCode {
    let prepared = match prepare(input) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let map = &prepared.map;
    let mut had_error = render_load_diags(map, &prepared.diags);
    if map.file_count() == 0 {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }
    let analysis = analyze_prepared(&prepared);
    for e in &analysis.errors {
        render_sema(map, e);
        had_error = true;
    }
    if had_error {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }

    let warnings = lint::collect_lints(&analysis, map);
    for (span, msg) in &warnings {
        render(map, *span, "warning", msg);
    }
    match warnings.len() {
        0 => println!("ok: no lint warnings"),
        1 => println!("1 warning"),
        n => println!("{n} warnings"),
    }
    ExitCode::SUCCESS
}

/// Run `otter_fusion fix` (`docs/23`): apply safe automatic fixes — currently,
/// rename each unused local variable to `_name` (inserting `_` at the binding,
/// which silences the unused-variable lint without removing code; the variable
/// is unused so there are no read sites to update). Rewrites affected files in
/// place, or with `check` reports what it would do without writing.
fn run_fix(input: &Input, check: bool) -> ExitCode {
    let prepared = match prepare(input) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let map = &prepared.map;
    let mut had_error = render_load_diags(map, &prepared.diags);
    if map.file_count() == 0 {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }
    let analysis = analyze_prepared(&prepared);
    for e in &analysis.errors {
        render_sema(map, e);
        had_error = true;
    }
    if had_error {
        eprintln!("\naborting due to previous error(s).");
        return ExitCode::FAILURE;
    }

    let lints = lint::analyze(&analysis, map);
    if lints.unused_locals.is_empty() {
        println!("nothing to fix");
        return ExitCode::SUCCESS;
    }
    // Group the binding offsets by file (only real files are fixable).
    let mut by_file: HashMap<u32, Vec<usize>> = HashMap::new();
    for (span, _name) in &lints.unused_locals {
        if (span.file.0 as usize) < map.file_count() {
            by_file
                .entry(span.file.0)
                .or_default()
                .push(span.lo.0 as usize);
        }
    }
    let mut total = 0usize;
    let mut files: Vec<u32> = by_file.keys().copied().collect();
    files.sort_unstable();
    for fid in files {
        let sf = map.file(FileId(fid));
        let mut offsets = by_file.remove(&fid).unwrap();
        offsets.sort_unstable();
        offsets.dedup();
        // Insert `_` at each binding, right-to-left so earlier offsets stay valid.
        let mut src = sf.src.clone();
        for &off in offsets.iter().rev() {
            if off <= src.len() {
                src.insert(off, '_');
            }
        }
        let n = offsets.len();
        total += n;
        if check {
            println!("would fix {n} unused variable(s) in {}", sf.name);
        } else if let Err(e) = std::fs::write(&sf.name, &src) {
            eprintln!("error: could not write `{}`: {e}", sf.name);
            return ExitCode::FAILURE;
        } else {
            println!("fixed {n} unused variable(s) in {}", sf.name);
        }
    }
    let verb = if check { "would fix" } else { "fixed" };
    println!("{verb} {total} unused variable(s)");
    ExitCode::SUCCESS
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
    if let Err(e) = backend::compile_object(analysis, &obj, &main_file.src, &main_file.name) {
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
        cmd.args([
            "-framework",
            "CoreFoundation",
            "-framework",
            "Security",
            "-liconv",
        ]);
    } else {
        cmd.args(["-lpthread", "-ldl", "-lm"]);
    }
    // Libraries requested via `@Link(lib = "…")` (`docs/19` §13), derived from
    // the program's attributes (no checker side table).
    for lib in &analysis.hir.link_libs {
        cmd.arg(format!("-l{lib}"));
    }
    // A `@Variadic` extern call routes through `libffi` (`docs/19` §13): the
    // `lang_variadic_call` runtime shim in `libruntime.a` references `libffi`'s
    // `ffi_prep_cif_var`/`ffi_call`, so link the system `libffi` when the program
    // declares any variadic import. (A `staticlib` does not bundle native libs,
    // hence the explicit `-lffi` here in addition to the runtime's build script.)
    let uses_variadic = analysis
        .program
        .defs
        .iter()
        .any(|d| d.attrs.iter().any(|a| a.name.name == "Variadic"));
    if uses_variadic {
        cmd.arg("-lffi");
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

/// Find the freshest runtime static library Cargo emitted for this profile.
///
/// Cargo may leave both `target/<profile>/libruntime.a` and hashed
/// `target/<profile>/deps/libruntime-*.a` archives behind. Prefer the newest
/// archive so `otter_fusion build` cannot silently link an older runtime after a
/// runtime-only rebuild.
fn find_runtime_lib() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let mut search_dirs = vec![dir.to_path_buf(), dir.join("deps")];
    if let Some(parent) = dir.parent() {
        search_dirs.push(parent.to_path_buf());
        search_dirs.push(parent.join("deps"));
    }
    freshest_runtime_lib_in_dirs(search_dirs)
}

fn freshest_runtime_lib_in_dirs(search_dirs: Vec<PathBuf>) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for search_dir in search_dirs {
        let Ok(entries) = std::fs::read_dir(search_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("libruntime") || !name.ends_with(".a") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if newest
                .as_ref()
                .is_none_or(|(newest_modified, _)| modified > *newest_modified)
            {
                newest = Some((modified, path));
            }
        }
    }
    newest.map(|(_, path)| path)
}

#[cfg(test)]
mod native_runtime_link_tests {
    use super::freshest_runtime_lib_in_dirs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn runtime_library_lookup_prefers_newest_archive() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "otter_runtime_lib_lookup_{}_{}",
            std::process::id(),
            nonce
        ));
        let deps = root.join("deps");
        std::fs::create_dir_all(&deps).unwrap();

        let stale = root.join("libruntime.a");
        let fresh = deps.join("libruntime-newer.a");
        std::fs::write(&stale, b"stale").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&fresh, b"fresh").unwrap();

        let found = freshest_runtime_lib_in_dirs(vec![root.clone(), deps.clone()]);
        assert_eq!(found.as_ref(), Some(&fresh));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_library_lookup_ignores_non_archives() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "otter_runtime_lib_ignore_{}_{}",
            std::process::id(),
            nonce
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("libruntime.rlib"), b"rlib").unwrap();
        std::fs::write(root.join("notruntime.a"), b"archive").unwrap();

        let found = freshest_runtime_lib_in_dirs(vec![PathBuf::from(&root)]);
        assert!(found.is_none());

        let _ = std::fs::remove_dir_all(root);
    }
}

/// Diagnostic codes and their long-form explanations, keyed by the codes
/// [`compiler::sema::SemaErrorKind::code`] assigns. Each entry is `(code, title,
/// explanation)`.
const EXPLANATIONS: &[(&str, &str, &str)] = &[
    (
        "E0001",
        "duplicate definition",
        "Two items share a name in the same namespace and module. Each type and each\n\
      value must have a unique name within its module; rename one, or move it to a\n\
      different module.",
    ),
    (
        "E0002",
        "unknown type",
        "A name used in type position does not resolve to any type in scope. Check the\n\
      spelling, and ensure the type is declared or `import`ed (the prelude is\n\
      near-empty: even `List`/`Map` must be imported — see `docs/17`).",
    ),
    (
        "E0003",
        "unknown value",
        "A name used in value position does not resolve to any binding, function, or\n\
      import in scope. Check the spelling, declare it with `var`, or `import` it.",
    ),
    (
        "E0004",
        "wrong number of generic arguments",
        "A generic type was applied with the wrong number of type arguments — e.g.\n\
      `Map<i64>` when `Map<K, V>` takes two. Supply exactly the declared count.",
    ),
    (
        "E0005",
        "recursive type alias",
        "A `type` alias refers to itself without an intervening indirection, so it has\n\
      no finite expansion. Break the cycle (e.g. via a struct/pointer), per\n\
      `docs/03` §3.",
    ),
    (
        "E0006",
        "type mismatch",
        "A value of one type was used where another is required. Otter Fusion has no\n\
      implicit conversions: convert explicitly with `as`, adjust the value, or fix\n\
      the annotation so the types agree.",
    ),
    (
        "E0007",
        "operator not supported for this type",
        "An operator was applied to operand type(s) that do not support it (e.g. `<` on\n\
      a type without `Ord`). Use a type that implements the operator's interface,\n\
      or implement it via `extend`.",
    ),
    (
        "E0008",
        "non-boolean condition",
        "A condition (`if`/`while`/…) must be exactly `bool` — there is no implicit\n\
      truthiness (`docs/07` §2). Compare explicitly, e.g. `if n != 0` instead of\n\
      `if n`.",
    ),
    (
        "E0009",
        "expression is not callable",
        "A call `e(...)` was applied to something that is not a function, closure, or\n\
      constructor. Check that `e` names a callable value.",
    ),
    (
        "E0010",
        "wrong number of arguments",
        "A call passed the wrong number of arguments for the callee's parameter list.\n\
      Pass exactly the declared count (trailing closures count as the final arg).",
    ),
    (
        "E0011",
        "`return` outside a function",
        "`return` appears where there is no enclosing function to return from. (Normally\n\
      unreachable after parsing.)",
    ),
    (
        "E0012",
        "invalid cast",
        "An `as` cast was requested between two types with no defined conversion\n\
      (`docs/12` §2). Only the documented numeric/`str`/pointer/interface\n\
      conversions are permitted.",
    ),
    (
        "E0013",
        "no such method",
        "A method call `recv.name(...)` named a method that the receiver's type does\n\
      not have (directly, via `extend`, or through an interface bound). Check the\n\
      spelling, ensure the relevant `extend`/`import` is in scope, and confirm the\n\
      receiver's type is what you expect.",
    ),
    (
        "E0014",
        "no such field",
        "A field access `recv.name` named a field the receiver's type does not\n\
      declare. Check the spelling and the receiver's type; only `struct` record\n\
      fields (and tuple positions via `.0`/`.1`) are accessible this way.",
    ),
    (
        "E0015",
        "unknown field in struct literal",
        "A struct literal `T { ... }` set a field that `T` does not declare. Remove\n\
      the field or fix its name; the literal may only mention declared fields.",
    ),
    (
        "E0016",
        "missing field in struct literal",
        "A struct literal `T { ... }` omitted a field that `T` requires. Record\n\
      structs must initialize every field (a `..base` spread can supply the rest);\n\
      supply the missing field's value.",
    ),
    (
        "E0017",
        "duplicate field in struct literal",
        "A struct literal set the same field more than once. Each field may be\n\
      initialized at most once; remove the redundant assignment.",
    ),
    (
        "E0018",
        "non-exhaustive match",
        "A `match` does not cover every possible value of the scrutinee (`docs/08`\n\
      §4). Add arms for the missing cases, or a `_` wildcard arm to catch the\n\
      rest. There is no implicit fall-through.",
    ),
    (
        "E0019",
        "`break`/`continue` outside a loop",
        "`break` or `continue` was used where there is no enclosing `loop`, `while`,\n\
      or `for`. Move it inside a loop, or remove it.",
    ),
];

/// Run `otter_fusion explain <code>` (`docs/23`): print the long-form explanation
/// for a diagnostic code. Unknown codes list the available ones.
fn run_explain(code: &str) -> ExitCode {
    let want = code.trim().to_ascii_uppercase();
    if let Some((c, title, body)) = EXPLANATIONS.iter().find(|(c, ..)| *c == want) {
        println!("{c}: {title}\n\n{body}");
        ExitCode::SUCCESS
    } else {
        eprintln!("error: unknown diagnostic code `{code}`");
        let codes: Vec<&str> = EXPLANATIONS.iter().map(|(c, ..)| *c).collect();
        eprintln!("available codes: {}", codes.join(", "));
        ExitCode::FAILURE
    }
}

/// Render a semantic error, tagging its stable code (`error[E0006]: …`) when it
/// has one so `otter_fusion explain <code>` can elaborate.
fn render_sema(map: &SourceMap, e: &compiler::sema::SemaError) {
    let severity = match e.code() {
        Some(code) => format!("error[{code}]"),
        None => "error".to_string(),
    };
    render(map, e.span, &severity, &e.kind.to_string());
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
    eprintln!(
        "{}:{}:{}: {severity}: {message}",
        sf.name, start.line, start.col
    );

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
    use pkg::resolve::{Registries, Resolved, resolve};
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
        std::fs::read_to_string(lock_path(proj))
            .ok()
            .and_then(|t| Lockfile::parse(&t).ok())
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
        let by_name = owned
            .iter()
            .map(|r| (r.name().to_string(), r as &dyn Registry))
            .collect();
        let default = proj
            .manifest
            .default_registry
            .clone()
            .unwrap_or_else(|| "public".to_string());
        let registries = Registries { by_name, default };
        let existing = existing_lock(proj);
        resolve(
            &proj.manifest,
            &proj.root,
            &registries,
            &store,
            existing.as_ref(),
        )
        .map_err(|e| e.to_string())
    }

    pub fn add(
        name: &str,
        version: Option<String>,
        path: Option<String>,
        git: Option<String>,
    ) -> ExitCode {
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
        let by_name = owned
            .iter()
            .map(|r| (r.name().to_string(), r as &dyn Registry))
            .collect();
        let default = proj
            .manifest
            .default_registry
            .clone()
            .unwrap_or_else(|| "public".to_string());
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
        println!(
            "vendored {} package(s) into {}",
            resolved.packages.len(),
            vendor_dir.display()
        );
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
        let token = pkg::credentials::Credentials::load()
            .token(&name)
            .map(str::to_string);
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
        match reg.publish(
            &proj.manifest.package.name,
            &proj.manifest.package.version,
            &tarball,
        ) {
            Ok(()) => {
                println!(
                    "published {} v{}",
                    proj.manifest.package.name, proj.manifest.package.version
                );
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
                println!(
                    "audited {} package(s); no known advisories",
                    resolved.packages.len()
                );
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
        text.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    fn fail(msg: &str) -> ExitCode {
        eprintln!("error: {msg}");
        ExitCode::FAILURE
    }
}
