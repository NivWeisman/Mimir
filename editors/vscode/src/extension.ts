// VS Code client for Mimir.
//
// Lifecycle:
//   activate()   — VS Code calls this when a .sv/.svh file is opened (see
//                  `activationEvents` in package.json). We spawn the Rust
//                  server binary and create a `LanguageClient` that pipes
//                  LSP messages to/from it over stdio.
//   deactivate() — Cleanly stop the client (which kills the child process).
//
// We deliberately keep this file small: every additional feature in the
// editor client is a feature we have to maintain in *two* languages.
// Server-side features are preferred.

import * as vscode from "vscode";
import {
  Executable,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const config = vscode.workspace.getConfiguration("mimir");
  const serverPath = config.get<string>("server.path", "mimir-server");
  const env = {
    ...process.env,
    ...config.get<Record<string, string>>("server.env", {}),
  };

  // We launch the same binary for both run and debug — there's no separate
  // debug build of the server. (`tower-lsp` doesn't need one.)
  const executable: Executable = {
    command: serverPath,
    transport: TransportKind.stdio,
    options: { env },
  };
  const serverOptions: ServerOptions = {
    run: executable,
    debug: executable,
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "systemverilog" },
      { scheme: "file", language: "verilog" },
    ],
    // Forward `mimir.trace.server` to the LSP machinery so users can flip
    // it on without restarting VS Code.
    traceOutputChannel: vscode.window.createOutputChannel("Mimir LSP Trace"),
  };

  client = new LanguageClient(
    "mimir",
    "Mimir SystemVerilog",
    serverOptions,
    clientOptions,
  );

  // Surface failure clearly: if the binary isn't on PATH we want a real
  // notification, not a silent dead client.
  try {
    await client.start();
  } catch (err) {
    void vscode.window.showErrorMessage(
      `Failed to start mimir-server (${serverPath}): ${err}. ` +
        `Set "mimir.server.path" in settings if the binary lives elsewhere.`,
    );
  }
}

export async function deactivate(): Promise<void> {
  if (client) {
    await client.stop();
    client = undefined;
  }
}
