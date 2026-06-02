//! `otter_fusion repl` (`docs/23`): a line-oriented read-eval-print loop.
//!
//! Each line is classified and handled so state persists without replaying
//! side effects:
//!
//! * a **declaration** (`function`/`struct`/`interface`/`type`/`extend`/`import`/
//!   `extern`/`test`/`bench`) is accumulated as a top-level item;
//! * a **`var` binding** is accumulated as a persistent local (so later lines see
//!   it — re-run each evaluation, which is fine for ordinary initializers);
//! * anything else is evaluated **once**: a trailing-`;` statement is run for its
//!   effect, and a bare expression is printed via string interpolation.
//!
//! Each evaluation builds a fresh single-file program (auto-imported prelude +
//! accumulated items + a `main` of the accumulated bindings + the current line),
//! analyzes and JIT-runs it. If that fails, the line is reported and **not**
//! accumulated, so the session stays in a good state. `:quit` (or EOF) exits;
//! `:help` lists commands.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use compiler::lexer::lex;
use compiler::parser::parse;
use compiler::span::FileId;

/// A modest auto-imported prelude so common names work without `import`.
const REPL_IMPORTS: &str = "\
import { List, Map, Set, Entry } from \"core:collections\";\n\
import { print, println } from \"std:io\";\n\
import { panic, panic_with } from \"core:prelude\";\n\
import { exit, abort } from \"std:process\";\n";

/// Run the REPL, reading lines from stdin until `:quit`/EOF.
pub fn run() -> ExitCode {
    let mut items: Vec<String> = Vec::new(); // accumulated top-level declarations
    let mut binds: Vec<String> = Vec::new(); // accumulated `var` bindings

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    println!("Otter Fusion REPL — `:help` for commands, `:quit` to exit.");
    loop {
        print!(">>> ");
        let _ = std::io::stdout().flush();
        let Some(Ok(line)) = lines.next() else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            ":quit" | ":q" => break,
            ":help" | ":h" => {
                println!(
                    "commands: :quit  :help  :reset (clear session)\n\
                     enter declarations (function/struct/…), `var` bindings, \
                     statements, or expressions."
                );
                continue;
            }
            ":reset" => {
                items.clear();
                binds.clear();
                println!("(session reset)");
                continue;
            }
            _ => {}
        }

        match classify(line) {
            Input::Item => {
                if let Err(msg) = eval(&items, &binds, Some(Snippet::Item(line))) {
                    eprintln!("{msg}");
                } else {
                    items.push(line.to_string());
                }
            }
            // Bindings/statements are terminated with `;` (added if the user
            // omitted it, so REPL input can be terse).
            Input::Binding => {
                let stmt = ensure_semi(line);
                if let Err(msg) = eval(&items, &binds, Some(Snippet::Binding(&stmt))) {
                    eprintln!("{msg}");
                } else {
                    binds.push(stmt);
                }
            }
            Input::Statement => {
                let stmt = ensure_semi(line);
                if let Err(msg) = eval(&items, &binds, Some(Snippet::Statement(&stmt))) {
                    eprintln!("{msg}");
                }
            }
            Input::Expr => {
                if let Err(msg) = eval(&items, &binds, Some(Snippet::Expr(line))) {
                    eprintln!("{msg}");
                }
            }
        }
    }
    ExitCode::SUCCESS
}

enum Input {
    Item,
    Binding,
    Statement,
    Expr,
}

enum Snippet<'a> {
    Item(&'a str),
    Binding(&'a str),
    Statement(&'a str),
    Expr(&'a str),
}

/// Append a `;` to a statement/binding the user typed without one (a block `}`
/// end needs none).
fn ensure_semi(line: &str) -> String {
    if line.ends_with(';') || line.ends_with('}') {
        line.to_string()
    } else {
        format!("{line};")
    }
}

fn classify(line: &str) -> Input {
    let first = line.split_whitespace().next().unwrap_or("");
    match first {
        "function" | "struct" | "interface" | "type" | "extend" | "import" | "extern" | "test"
        | "bench" | "mod" | "pub" => Input::Item,
        "var" => Input::Binding,
        _ if line.ends_with(';') => Input::Statement,
        _ => Input::Expr,
    }
}

/// Build the synthetic program for the current state plus an optional new
/// `snippet`, analyze + JIT-run it. Returns `Err(message)` on any diagnostic or
/// codegen failure (so the caller can decline to accumulate the snippet).
fn eval(items: &[String], binds: &[String], snippet: Option<Snippet>) -> Result<(), String> {
    let mut src = String::new();
    src.push_str(REPL_IMPORTS);
    for it in items {
        src.push_str(it);
        src.push('\n');
    }
    // A new item declaration goes at top level; everything else goes in `main`.
    let mut body_extra = String::new();
    let mut tail = String::new();
    if let Some(s) = snippet {
        match s {
            Snippet::Item(it) => {
                src.push_str(it);
                src.push('\n');
            }
            Snippet::Binding(b) => body_extra.push_str(b),
            Snippet::Statement(st) => body_extra.push_str(st),
            Snippet::Expr(e) => {
                // Print the expression's value via string interpolation.
                tail = format!("println(\"${{{e}}}\");");
            }
        }
    }
    src.push_str("function main() {\n");
    for b in binds {
        src.push_str(b);
        src.push('\n');
    }
    if !body_extra.is_empty() {
        src.push_str(&body_extra);
        src.push('\n');
    }
    if !tail.is_empty() {
        src.push_str(&tail);
        src.push('\n');
    }
    src.push_str("}\n");

    let (tokens, lex_errs) = lex(&src, FileId(0));
    if let Some(e) = lex_errs.first() {
        return Err(format!("error: {}", e.kind));
    }
    let (module, parse_errs) = parse(&src, &tokens);
    if let Some(e) = parse_errs.first() {
        return Err(format!("error: {}", e.kind));
    }
    let analysis = compiler::sema::analyze(&module);
    if let Some(e) = analysis.errors.first() {
        return Err(format!("error: {}", e.kind));
    }
    let jit = backend::compile(&analysis).map_err(|e| format!("codegen: {}", e.message))?;
    backend::set_gc_enabled(true);
    unsafe { jit.run_main() };
    Ok(())
}
