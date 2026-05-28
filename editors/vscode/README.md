# Otter Fusion for VS Code

Editor support for Otter Fusion (`.otter`), backed by the `lang-lsp`
language server (`crates/lsp`).

## Features

- **Diagnostics** — lexer, parser, and type-checker errors, live as you type.
- **Hover** — the type of the expression / symbol under the cursor.
- **Go to definition** — jump to a function, method, struct, global, or local.
- **Find references** and **rename** — across the file, for any value symbol.
- **Document symbols** (outline / breadcrumbs) — functions, structs and their
  fields, interfaces and `extend` blocks and their methods.
- **Completion** — keywords, builtins, top-level types/functions, and locals.
- **Semantic highlighting** — token colors driven by the type checker
  (functions vs. methods vs. types vs. parameters vs. fields), refining the
  bundled TextMate grammar.

## Build

The server is part of the Cargo workspace:

```sh
cargo build -p lsp          # produces target/debug/lang-lsp
cargo build -p lsp --release   # target/release/lang-lsp
```

Put `lang-lsp` on your `PATH`, or set the `lang.server.path` setting to its
absolute path (e.g. `${workspaceFolder}/target/debug/lang-lsp`).

## Run the extension

```sh
cd editors/vscode
npm install
npm run compile     # or `npm run watch`
```

Then press <kbd>F5</kbd> in VS Code to launch an Extension Development Host, or
package it with `vsce package` and install the resulting `.vsix`.

## Settings

| Setting             | Default    | Description                               |
| ------------------- | ---------- | ----------------------------------------- |
| `lang.server.path`  | `lang-lsp` | Path to the language-server binary.       |
| `lang.trace.server` | `off`      | Trace LSP traffic (`messages`/`verbose`). |

Command **Otter Fusion: Restart Language Server** reloads the server after a rebuild.
