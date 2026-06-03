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

// Scheme for the read-only virtual documents that show macro expansions.
const EXPAND_SCHEME = "mimir-expand";

// Shape of the `mimir/expandMacro` custom-request response (mirrors
// `ExpandMacroResponse` in the Rust server).
interface ExpandMacroResponse {
  name: string;
  expansion: string;
  lineCount: number;
}

// Holds the most recent expansion text per virtual-doc URI so the
// TextDocumentContentProvider can serve it. Keyed by the macro name so
// re-expanding the same macro reuses (and refreshes) one tab.
const expansionContents = new Map<string, string>();
const expansionEmitter = new vscode.EventEmitter<vscode.Uri>();

/** Register the "Mimir: Expand Macro" command + its virtual-doc provider. */
function registerMacroExpansion(context: vscode.ExtensionContext): void {
  const provider: vscode.TextDocumentContentProvider = {
    onDidChange: expansionEmitter.event,
    provideTextDocumentContent: (uri) =>
      expansionContents.get(uri.toString()) ?? "// (expansion unavailable)",
  };
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(EXPAND_SCHEME, provider),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("mimir.expandMacro", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor || !client) {
        return;
      }
      let result: ExpandMacroResponse | null;
      try {
        result = await client.sendRequest<ExpandMacroResponse | null>(
          "mimir/expandMacro",
          {
            textDocument: { uri: editor.document.uri.toString() },
            position: {
              line: editor.selection.active.line,
              character: editor.selection.active.character,
            },
          },
        );
      } catch (err) {
        void vscode.window.showErrorMessage(`Mimir: macro expansion failed: ${err}`);
        return;
      }
      if (!result) {
        void vscode.window.showInformationMessage(
          "Mimir: the cursor is not on a macro usage (or slang isn't configured).",
        );
        return;
      }

      const header =
        `// Expansion of \`${result.name} (${result.lineCount} line` +
        `${result.lineCount === 1 ? "" : "s"})\n\n`;
      const docUri = vscode.Uri.parse(`${EXPAND_SCHEME}:${result.name}.expanded.sv`);
      expansionContents.set(docUri.toString(), header + result.expansion);
      expansionEmitter.fire(docUri); // refresh if the tab is already open

      const doc = await vscode.workspace.openTextDocument(docUri);
      await vscode.languages.setTextDocumentLanguage(doc, "systemverilog");
      await vscode.window.showTextDocument(doc, {
        viewColumn: vscode.ViewColumn.Beside,
        preview: true,
        preserveFocus: false,
      });
    }),
  );
}

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

  // Register the macro-expansion command + virtual-doc provider before the
  // client starts so the command is available as soon as the editor loads.
  registerMacroExpansion(context);

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
