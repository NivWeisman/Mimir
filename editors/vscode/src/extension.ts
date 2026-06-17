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

import { EXPAND_SCHEME, ExpandMacroResponse, expandMacroAt } from "./expand";

let client: LanguageClient | undefined;

// Per-expand timing output (Output → "Mimir Expand Timing"). Lets us see where
// an expand spends its time in the editor — the server round trip is
// sub-second, so anything slower points at the VS Code side.
let timingChannel: vscode.OutputChannel | undefined;

// Holds the most recent expansion text per virtual-doc URI so the
// TextDocumentContentProvider can serve it. Keyed by the macro name so
// re-expanding the same macro reuses (and refreshes) one tab.
const expansionContents = new Map<string, string>();
const expansionEmitter = new vscode.EventEmitter<vscode.Uri>();

/**
 * Register `mimir.gotoLocation` — the command server CodeLenses invoke to
 * jump to a target declaration. Args: `[uriString, { line, character }]`.
 * Kept tiny on purpose: the server decides *what* to point at; the client
 * just opens it.
 */
function registerGotoLocation(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand(
      "mimir.gotoLocation",
      async (uriStr: string, pos: { line: number; character: number }) => {
        try {
          const uri = vscode.Uri.parse(uriStr);
          const position = new vscode.Position(pos.line, pos.character);
          const doc = await vscode.workspace.openTextDocument(uri);
          const editor = await vscode.window.showTextDocument(doc);
          editor.selection = new vscode.Selection(position, position);
          editor.revealRange(
            new vscode.Range(position, position),
            vscode.TextEditorRevealType.InCenter,
          );
        } catch (err) {
          void vscode.window.showErrorMessage(`Mimir: could not open location: ${err}`);
        }
      },
    ),
  );
}

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
      await expandMacroAt({
        target: {
          uri: editor.document.uri.toString(),
          line: editor.selection.active.line,
          character: editor.selection.active.character,
        },
        sendRequest: (method, params) =>
          client!.sendRequest<ExpandMacroResponse | null>(method, params),
        // Status-bar progress (not a notification toast): expansion now runs
        // on its own sidecar connection, so it no longer blocks behind a
        // background elaborate — a toast that pops and vanishes for a
        // sub-second op just reads as flicker.
        withProgress: (title, task) =>
          vscode.window.withProgress(
            { location: vscode.ProgressLocation.Window, title },
            task,
          ),
        setContent: (uriStr, text) => expansionContents.set(uriStr, text),
        fireChange: (uriStr) => expansionEmitter.fire(vscode.Uri.parse(uriStr)),
        openDoc: (uriStr) =>
          Promise.resolve(vscode.workspace.openTextDocument(vscode.Uri.parse(uriStr))),
        languageIdOf: (doc) => (doc as vscode.TextDocument).languageId,
        setLanguage: async (doc, lang) => {
          await vscode.languages.setTextDocumentLanguage(doc as vscode.TextDocument, lang);
        },
        showDoc: async (doc) => {
          await vscode.window.showTextDocument(doc as vscode.TextDocument, {
            viewColumn: vscode.ViewColumn.Beside,
            preview: true,
            preserveFocus: false,
          });
        },
        info: (msg) => void vscode.window.showInformationMessage(msg),
        warn: (msg) => void vscode.window.showWarningMessage(msg),
        error: (msg) => void vscode.window.showErrorMessage(msg),
        log: (msg) => timingChannel?.appendLine(msg),
        now: () => (typeof performance !== "undefined" ? performance.now() : Date.now()),
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

  timingChannel = vscode.window.createOutputChannel("Mimir Expand Timing");
  context.subscriptions.push(timingChannel);

  // Register commands before the client starts so they're available as soon
  // as the editor loads.
  registerMacroExpansion(context);
  registerGotoLocation(context);

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
