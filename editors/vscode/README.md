# Otter Fusion for VS Code

Editor support for Otter Fusion (`.otter`), backed by the `otter_fusion_lsp`
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
- **Run / Build code lenses** — `▶ Run`, `▶ Run (release)`, and `🔨 Build`
  appear above every `function main`. Clicking them opens an integrated
  terminal and invokes the `otter_fusion` CLI on the current file.

## Build

The server is part of the Cargo workspace:

```sh
cargo build -p lsp          # produces target/debug/otter_fusion_lsp
cargo build -p lsp --release   # target/release/otter_fusion_lsp
```

Put `otter_fusion_lsp` on your `PATH`, or set the `otter-fusion.server.path` setting to its
absolute path (e.g. `${workspaceFolder}/target/debug/otter_fusion_lsp`).

## Run the extension

```sh
cd editors/vscode
npm install
npm run compile     # or `npm run watch`
```

Then press <kbd>F5</kbd> in VS Code to launch an Extension Development Host.

## Package & install

```sh
cd editors/vscode
npm install                  # one-time, also installs @vscode/vsce
npm run package              # writes otter-fusion-<version>.vsix
npm run install-extension    # installs that .vsix into VS Code
```

`npm run package` runs `vsce package` (via the bundled `@vscode/vsce` dev
dependency) and names the artifact after the current `version` in
`package.json`. `npm run install-extension` then loads it into the local
VS Code via the `code` CLI.

## Settings

| Setting             | Default    | Description                                                |
| ------------------- | ---------- | ---------------------------------------------------------- |
| `otter-fusion.server.path`  | `otter_fusion_lsp` | Path to the language-server binary.                        |
| `otter-fusion.runner.path`  | `otter_fusion`     | Path to the `otter_fusion` CLI used by the Run / Build code lenses. |
| `otter-fusion.trace.server` | `off`      | Trace LSP traffic (`messages`/`verbose`).                  |

Command **Otter Fusion: Restart Language Server** reloads the server after a rebuild.
