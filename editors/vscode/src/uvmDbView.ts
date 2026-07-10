// UVM Config/Resource DB tree view.
//
// A thin renderer over the server's custom `mimir/uvmDb` request: the
// server does the workspace scan, the grouping, and even the display
// strings (labels, `detail`, `fileLine`); this file only maps the response
// onto VS Code TreeItems. Leaf clicks reuse the existing
// `mimir.gotoLocation` command (same arg shape the CodeLenses use), so
// there is no navigation logic here either — by design, per the "features
// live in the server" rule at the top of extension.ts.

import * as vscode from "vscode";
import { LanguageClient } from "vscode-languageclient/node";

// ---------------------------------------------------------------------------
// Wire shapes (mirror `UvmDbResponse` & friends in the Rust server)
// ---------------------------------------------------------------------------

interface UvmDbEntry {
  db: string; // "config" | "resource"
  method: string; // "set" | "get" | ...
  access: string; // "write" | "read"
  typeParam: string;
  key?: string;
  instName?: string;
  scope?: string;
  cntxt?: string;
  uri: string;
  range: { start: Position; end: Position };
  callRange: { start: Position; end: Position };
  label: string; // display-ready, e.g. `set  "env.agent"`
  fileLine: string; // display-ready, e.g. `dv_base_test.sv:3`
}

interface Position {
  line: number;
  character: number;
}

interface UvmDbGroup {
  db: string;
  key: string;
  typeParams: string[];
  typeMismatch: boolean;
  unwrittenRead: boolean;
  detail: string; // display-ready group summary
  writers: UvmDbEntry[];
  readers: UvmDbEntry[];
}

interface UvmDbResponse {
  groups: UvmDbGroup[];
  ungrouped: UvmDbEntry[];
  truncated: boolean;
}

// ---------------------------------------------------------------------------
// Tree model
// ---------------------------------------------------------------------------

// The tree has two levels: group nodes (one per (db, key), plus one
// synthetic "Ungrouped" bucket) and entry leaves.
type Node =
  | { kind: "group"; group: UvmDbGroup }
  | { kind: "ungrouped"; entries: UvmDbEntry[] }
  | { kind: "entry"; entry: UvmDbEntry };

class UvmDbProvider implements vscode.TreeDataProvider<Node> {
  private readonly emitter = new vscode.EventEmitter<Node | undefined>();
  readonly onDidChangeTreeData = this.emitter.event;

  // Set by registerUvmDbView so an empty tree can explain itself.
  view: vscode.TreeView<Node> | undefined;

  constructor(private readonly getClient: () => LanguageClient | undefined) {}

  refresh(): void {
    this.emitter.fire(undefined);
  }

  async getChildren(element?: Node): Promise<Node[]> {
    if (!element) {
      return this.fetchRoots();
    }
    if (element.kind === "group") {
      // Writers first (they define the value), then readers.
      return [...element.group.writers, ...element.group.readers].map(
        (entry) => ({ kind: "entry", entry }),
      );
    }
    if (element.kind === "ungrouped") {
      return element.entries.map((entry) => ({ kind: "entry", entry }));
    }
    return [];
  }

  private async fetchRoots(): Promise<Node[]> {
    const client = this.getClient();
    if (!client || !client.isRunning()) {
      if (this.view) {
        this.view.message = "Mimir server is not running.";
      }
      return [];
    }
    let resp: UvmDbResponse;
    try {
      resp = await client.sendRequest<UvmDbResponse>("mimir/uvmDb", {});
    } catch (err) {
      if (this.view) {
        this.view.message = `Mimir: uvmDb request failed: ${err}`;
      }
      return [];
    }
    if (this.view) {
      // The message doubles as an empty-state hint and a truncation notice.
      if (resp.truncated) {
        this.view.message =
          "Result truncated — the workspace has more db calls than the view shows.";
      } else if (resp.groups.length === 0 && resp.ungrouped.length === 0) {
        this.view.message =
          "No uvm_config_db / uvm_resource_db calls found in the workspace.";
      } else {
        this.view.message = undefined;
      }
    }
    const roots: Node[] = resp.groups.map((group) => ({ kind: "group", group }));
    if (resp.ungrouped.length > 0) {
      roots.push({ kind: "ungrouped", entries: resp.ungrouped });
    }
    return roots;
  }

  getTreeItem(node: Node): vscode.TreeItem {
    if (node.kind === "group") {
      const g = node.group;
      const item = new vscode.TreeItem(
        g.key,
        vscode.TreeItemCollapsibleState.Collapsed,
      );
      item.description = g.detail;
      if (g.typeMismatch) {
        item.iconPath = new vscode.ThemeIcon("warning");
        item.tooltip =
          `Type mismatch: this key is accessed with different #(T) parameters ` +
          `(${g.typeParams.join(", ")}) — a get with the wrong type never sees the set.`;
      } else if (g.unwrittenRead) {
        item.iconPath = new vscode.ThemeIcon("warning");
        item.tooltip =
          "No writer found: this key is read but never set anywhere in the scanned workspace.";
      } else {
        item.iconPath = new vscode.ThemeIcon("database");
        item.tooltip = g.detail;
      }
      return item;
    }

    if (node.kind === "ungrouped") {
      const item = new vscode.TreeItem(
        "Ungrouped",
        vscode.TreeItemCollapsibleState.Collapsed,
      );
      item.description = `${node.entries.length} call(s)`;
      item.iconPath = new vscode.ThemeIcon("question");
      item.tooltip =
        "Calls whose key is not a string literal (or read_by_type, which has no name) — they can't be paired.";
      return item;
    }

    const e = node.entry;
    const item = new vscode.TreeItem(e.label, vscode.TreeItemCollapsibleState.None);
    item.description = e.fileLine;
    item.iconPath = new vscode.ThemeIcon(e.access === "write" ? "edit" : "eye");
    item.tooltip = `${e.db}_db #(${e.typeParam}) :: ${e.method} — ${e.fileLine}`;
    // Same arg shape the server's CodeLenses use; registerGotoLocation in
    // extension.ts already knows how to open it.
    item.command = {
      command: "mimir.gotoLocation",
      title: "Open call site",
      arguments: [e.uri, e.range.start],
    };
    return item;
  }
}

/**
 * Register the "UVM Config/Resource DB" explorer view + its refresh
 * command. `getClient` is a thunk because the LanguageClient is created
 * after command registration in activate().
 */
export function registerUvmDbView(
  context: vscode.ExtensionContext,
  getClient: () => LanguageClient | undefined,
): void {
  const provider = new UvmDbProvider(getClient);
  const view = vscode.window.createTreeView("mimirUvmDb", {
    treeDataProvider: provider,
  });
  provider.view = view;
  context.subscriptions.push(view);
  context.subscriptions.push(
    vscode.commands.registerCommand("mimir.uvmDb.refresh", () =>
      provider.refresh(),
    ),
  );
}
