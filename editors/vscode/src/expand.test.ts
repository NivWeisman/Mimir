// Offline tests for the Expand Macro command logic. These run under plain
// Node (`node --test`) — no VS Code runtime — because `expandMacroAt` takes
// every side effect as an injected dependency. They lock in the behaviour
// that matters for the "expand is clunky" report:
//   * the per-step timing line is emitted,
//   * the big-expansion re-tokenize is skipped when the doc already classifies
//     as SystemVerilog (the `.expanded.sv` URI does),
//   * null / error responses surface a message and never open a tab.

import { strict as assert } from "node:assert";
import { test } from "node:test";

import { ExpandDeps, ExpandMacroResponse, expandMacroAt, expansionUri } from "./expand";

interface Recorder {
  calls: string[];
  logs: string[];
  infos: string[];
  warns: string[];
  errors: string[];
  opened: string[];
  setLanguageCount: number;
  shown: number;
}

function makeDeps(
  overrides: Partial<ExpandDeps> & { result?: ExpandMacroResponse | null; docLanguageId?: string },
): { deps: ExpandDeps; rec: Recorder } {
  const rec: Recorder = {
    calls: [],
    logs: [],
    infos: [],
    warns: [],
    errors: [],
    opened: [],
    setLanguageCount: 0,
    shown: 0,
  };
  let clock = 0;
  const docLanguageId = overrides.docLanguageId ?? "systemverilog";
  const fakeDoc = { languageId: docLanguageId };

  const deps: ExpandDeps = {
    target: { uri: "file:///w/top.sv", line: 3, character: 5 },
    sendRequest: async (method) => {
      rec.calls.push(`sendRequest:${method}`);
      return overrides.result ?? null;
    },
    withProgress: async (_title, task) => {
      rec.calls.push("withProgress");
      return task();
    },
    setContent: (uriStr) => rec.calls.push(`setContent:${uriStr}`),
    fireChange: (uriStr) => rec.calls.push(`fireChange:${uriStr}`),
    openDoc: async (uriStr) => {
      rec.opened.push(uriStr);
      return fakeDoc;
    },
    languageIdOf: (doc) => (doc as { languageId: string }).languageId,
    setLanguage: async () => {
      rec.setLanguageCount += 1;
    },
    showDoc: async () => {
      rec.shown += 1;
    },
    info: (m) => rec.infos.push(m),
    warn: (m) => rec.warns.push(m),
    error: (m) => rec.errors.push(m),
    log: (m) => rec.logs.push(m),
    now: () => (clock += 10), // each call advances 10ms
    ...stripExtras(overrides),
  };
  return { deps, rec };
}

// Drop the test-only keys so they don't leak into ExpandDeps.
function stripExtras(o: Record<string, unknown>): Partial<ExpandDeps> {
  const { result: _r, docLanguageId: _l, ...rest } = o;
  return rest as Partial<ExpandDeps>;
}

test("happy path opens a tab and logs per-step timings", async () => {
  const { deps, rec } = makeDeps({
    result: { name: "uvm_object_utils_begin", expansion: "x\ny", lineCount: 1006 },
  });
  const out = await expandMacroAt(deps);

  assert.equal(out?.name, "uvm_object_utils_begin");
  const uri = expansionUri("uvm_object_utils_begin");
  assert.ok(rec.calls.includes(`setContent:${uri}`));
  assert.ok(rec.opened.includes(uri), "expansion tab should be opened");
  assert.equal(rec.shown, 1, "tab should be shown once");
  assert.equal(rec.logs.length, 1, "one timing line");
  assert.match(
    rec.logs[0],
    /request=\d+ms open=\d+ms setLang=\d+ms show=\d+ms total=\d+ms/,
    `timing line shape: ${rec.logs[0]}`,
  );
});

test("does not re-set language when the doc is already SystemVerilog", async () => {
  const { deps, rec } = makeDeps({
    result: { name: "A", expansion: "(((k)+1)*2)", lineCount: 1 },
    docLanguageId: "systemverilog",
  });
  await expandMacroAt(deps);
  assert.equal(rec.setLanguageCount, 0, "no needless re-tokenize");
  assert.match(rec.logs[0], /setLang=0ms/);
});

test("sets language only when VS Code guessed something else", async () => {
  const { deps, rec } = makeDeps({
    result: { name: "A", expansion: "x", lineCount: 1 },
    docLanguageId: "plaintext",
  });
  await expandMacroAt(deps);
  assert.equal(rec.setLanguageCount, 1);
});

test("null result shows an info message and opens nothing", async () => {
  const { deps, rec } = makeDeps({ result: null });
  const out = await expandMacroAt(deps);
  assert.equal(out, null);
  assert.equal(rec.infos.length, 1);
  assert.equal(rec.opened.length, 0);
});

test("error result shows a warning and opens nothing", async () => {
  const { deps, rec } = makeDeps({
    result: { name: "uvm_field_int", expansion: "", lineCount: 0, error: "expands to nothing" },
  });
  await expandMacroAt(deps);
  assert.equal(rec.warns.length, 1);
  assert.match(rec.warns[0], /expands to nothing/);
  assert.equal(rec.opened.length, 0);
});

test("no active target is a no-op", async () => {
  const { deps, rec } = makeDeps({ result: { name: "A", expansion: "x", lineCount: 1 } });
  (deps as { target: unknown }).target = undefined;
  const out = await expandMacroAt(deps);
  assert.equal(out, null);
  assert.equal(rec.calls.length, 0);
});
