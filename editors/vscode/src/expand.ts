// Macro-expansion command logic, factored out of `extension.ts` so it can be
// unit-tested WITHOUT the VS Code runtime. Every VS Code API the handler needs
// is injected through `ExpandDeps`; the only `vscode` reference is a
// type-only import (erased at compile time), so the compiled `expand.js` has
// no `require("vscode")` and runs under plain Node in tests.
//
// The handler also records per-step timings (request / open / setLanguage /
// show) so we can see exactly where an expand spends its time in the editor —
// the server round trip is sub-second (measured ~0.26s cold, ~10ms warm via a
// non-VS-Code LSP client), so any remaining "clunkiness" is on the VS Code
// side, and these numbers pinpoint which step.

// Shape of the `mimir/expandMacro` custom-request response (mirrors
// `ExpandMacroResponse` in the Rust server).
export interface ExpandMacroResponse {
  name: string;
  expansion: string;
  lineCount: number;
  // Set when the expansion could not be produced (e.g. the macro has no
  // `define anywhere in the project). The extension shows this message
  // instead of opening an expansion tab.
  error?: string;
}

// Where the cursor is. `undefined` when there's no active editor or no client.
export interface ExpandTarget {
  uri: string;
  line: number;
  character: number;
}

// The virtual-document scheme for read-only expansion tabs.
export const EXPAND_SCHEME = "mimir-expand";

// All the side-effecting capabilities the handler needs, injected so tests can
// supply fakes. `doc` is opaque (`unknown`) so we never depend on the concrete
// `vscode.TextDocument` shape; `languageIdOf` reads what we need from it.
export interface ExpandDeps {
  target: ExpandTarget | undefined;
  sendRequest: (
    method: string,
    params: unknown,
  ) => Promise<ExpandMacroResponse | null>;
  // Wrap `task` in whatever progress UI the caller wants (or none, in tests).
  // Returns `PromiseLike` because `vscode.window.withProgress` yields a
  // `Thenable`, not a `Promise`.
  withProgress: <T>(title: string, task: () => PromiseLike<T>) => PromiseLike<T>;
  setContent: (uriStr: string, text: string) => void;
  fireChange: (uriStr: string) => void;
  openDoc: (uriStr: string) => Promise<unknown>;
  languageIdOf: (doc: unknown) => string;
  setLanguage: (doc: unknown, languageId: string) => Promise<void>;
  showDoc: (doc: unknown) => Promise<void>;
  info: (msg: string) => void;
  warn: (msg: string) => void;
  error: (msg: string) => void;
  log: (msg: string) => void;
  now: () => number;
}

/** Build the read-only virtual-doc URI string for an expanded macro. */
export function expansionUri(name: string): string {
  return `${EXPAND_SCHEME}:${name}.expanded.sv`;
}

/**
 * Run "Expand Macro" at the current cursor: request the expansion, open it in
 * a read-only tab, and log per-step timings. Returns the response (or `null`)
 * so callers/tests can inspect the outcome.
 */
export async function expandMacroAt(
  deps: ExpandDeps,
): Promise<ExpandMacroResponse | null> {
  const target = deps.target;
  if (!target) {
    return null;
  }

  const t0 = deps.now();
  let result: ExpandMacroResponse | null;
  try {
    result = await deps.withProgress("Mimir: expanding macro…", () =>
      deps.sendRequest("mimir/expandMacro", {
        textDocument: { uri: target.uri },
        position: { line: target.line, character: target.character },
      }),
    );
  } catch (err) {
    deps.error(`Mimir: macro expansion failed: ${err}`);
    return null;
  }
  const tReq = deps.now();

  if (!result) {
    deps.info(
      "Mimir: the cursor is not on a macro usage (or slang isn't configured).",
    );
    return null;
  }
  if (result.error) {
    deps.warn(`Mimir: ${result.error}`);
    return result;
  }

  const plural = result.lineCount === 1 ? "" : "s";
  const header = `// Expansion of \`${result.name} (${result.lineCount} line${plural})\n\n`;
  const uriStr = expansionUri(result.name);
  deps.setContent(uriStr, header + result.expansion);
  deps.fireChange(uriStr); // refresh if the tab is already open

  const doc = await deps.openDoc(uriStr);
  const tOpen = deps.now();

  // The `.expanded.sv` URI already classifies as SystemVerilog via the
  // language `extensions` contribution. Only force a reclassify if VS Code
  // guessed something else — calling setTextDocumentLanguage unconditionally
  // re-tokenizes the whole (up to ~1000-line) expansion for nothing.
  let tLang = tOpen;
  if (deps.languageIdOf(doc) !== "systemverilog") {
    await deps.setLanguage(doc, "systemverilog");
    tLang = deps.now();
  }

  await deps.showDoc(doc);
  const tShow = deps.now();

  const ms = (a: number, b: number) => Math.round(b - a);
  deps.log(
    `expand \`${result.name}: ${result.lineCount} lines — ` +
      `request=${ms(t0, tReq)}ms open=${ms(tReq, tOpen)}ms ` +
      `setLang=${ms(tOpen, tLang)}ms show=${ms(tLang, tShow)}ms ` +
      `total=${ms(t0, tShow)}ms`,
  );
  return result;
}
