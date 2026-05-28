import { ExtensionContext, Terminal, Uri, commands, window, workspace } from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let runTerminal: Terminal | undefined;

/** Shell-quote a path so spaces and special characters survive `sendText`. */
function shellQuote(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

/** Open (or reuse) the dedicated terminal and dispatch a `lang` subcommand. */
function runInTerminal(args: string[], filePath: string): void {
  const config = workspace.getConfiguration("lang");
  const runner = config.get<string>("runner.path") || "lang";
  if (!runTerminal || runTerminal.exitStatus !== undefined) {
    runTerminal = window.createTerminal({ name: "Otter Fusion" });
  }
  runTerminal.show(true);
  const cmd = [shellQuote(runner), ...args, shellQuote(filePath)].join(" ");
  runTerminal.sendText(cmd);
}

/** Build the client from current settings and start it. */
async function startClient(): Promise<void> {
  const config = workspace.getConfiguration("lang");
  const command = config.get<string>("server.path") || "lang-lsp";

  const serverOptions: ServerOptions = {
    run: { command, transport: TransportKind.stdio },
    debug: { command, transport: TransportKind.stdio },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "otter-fusion" }],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher("**/*.otter"),
    },
    outputChannelName: "Otter Fusion Language Server",
  };

  client = new LanguageClient(
    "lang",
    "Otter Fusion Language Server",
    serverOptions,
    clientOptions,
  );

  try {
    await client.start();
  } catch (err) {
    window.showErrorMessage(
      `Failed to start the Otter Fusion language server ("${command}"). ` +
        `Set "lang.server.path" to the lang-lsp binary. ${err}`,
    );
  }
}

export async function activate(context: ExtensionContext): Promise<void> {
  context.subscriptions.push(
    commands.registerCommand("lang.restartServer", async () => {
      await client?.stop();
      client = undefined;
      await startClient();
      window.showInformationMessage("Otter Fusion language server restarted.");
    }),
    commands.registerCommand(
      "lang.runFile",
      (uri: string, release: boolean = false) => {
        const path = Uri.parse(uri).fsPath;
        const args = release ? ["run", "--release"] : ["run"];
        runInTerminal(args, path);
      },
    ),
    commands.registerCommand("lang.buildFile", (uri: string) => {
      const path = Uri.parse(uri).fsPath;
      runInTerminal(["build"], path);
    }),
  );

  await startClient();
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
