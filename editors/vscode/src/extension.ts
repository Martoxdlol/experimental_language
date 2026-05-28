import { ExtensionContext, commands, window, workspace } from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

/** Build the client from current settings and start it. */
async function startClient(): Promise<void> {
  const config = workspace.getConfiguration("lang");
  const command = config.get<string>("server.path") || "lang-lsp";

  const serverOptions: ServerOptions = {
    run: { command, transport: TransportKind.stdio },
    debug: { command, transport: TransportKind.stdio },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "lang" }],
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
  );

  await startClient();
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
