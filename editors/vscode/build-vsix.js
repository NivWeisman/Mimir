#!/usr/bin/env node
// Offline .vsix builder. vsce can't be fetched (no network), but a .vsix is
// just an OPC ZIP: extension.vsixmanifest + [Content_Types].xml + an
// extension/ tree. This stages those, then shells out to `zip`.
//
// Run from editors/vscode after `npm run compile`.
"use strict";
const fs = require("fs");
const path = require("path");
const cp = require("child_process");

const ROOT = __dirname;
const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, "package.json"), "utf8"));

// Production dependency dirs (from `npm ls --omit=dev`), bundled under
// extension/node_modules so the activated extension can require them.
const PROD_DEPS = [
  "vscode-languageclient",
  "minimatch",
  "semver",
  "vscode-languageserver-protocol",
  "brace-expansion",
  "vscode-jsonrpc",
  "vscode-languageserver-types",
  "balanced-match",
];

// Files/dirs copied verbatim into extension/.
const TOP_LEVEL = [
  "package.json",
  "README.md",
  "language-configuration.json",
  "out",
  "syntaxes",
];

const stage = fs.mkdtempSync(path.join(require("os").tmpdir(), "vsix-"));
const extDir = path.join(stage, "extension");
fs.mkdirSync(extDir, { recursive: true });

// Skip dev/source cruft inside any copied tree (mirrors .vscodeignore intent):
// sourcemaps and compiled test files have no business in a shipped extension.
const SKIP_EXT = new Set([".map"]);
function copy(src, dst) {
  const st = fs.statSync(src);
  if (st.isDirectory()) {
    fs.mkdirSync(dst, { recursive: true });
    for (const e of fs.readdirSync(src)) copy(path.join(src, e), path.join(dst, e));
  } else {
    if (SKIP_EXT.has(path.extname(src)) || src.endsWith(".test.js")) return;
    fs.copyFileSync(src, dst);
  }
}

for (const rel of TOP_LEVEL) {
  const src = path.join(ROOT, rel);
  if (fs.existsSync(src)) copy(src, path.join(extDir, rel));
}
for (const dep of PROD_DEPS) {
  const src = path.join(ROOT, "node_modules", dep);
  if (!fs.existsSync(src)) throw new Error(`missing prod dep: ${dep}`);
  copy(src, path.join(extDir, "node_modules", dep));
}

// --- extension.vsixmanifest ---------------------------------------------
const xmlEsc = (s) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const vscodeEngine = (pkg.engines && pkg.engines.vscode) || "*";
const categories = (pkg.categories || []).map((c) => xmlEsc(c)).join(",");
const manifest = `<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011" xmlns:d="http://schemas.microsoft.com/developer/vsx-schema-design/2011">
  <Metadata>
    <Identity Language="en-US" Id="${xmlEsc(pkg.name)}" Version="${xmlEsc(pkg.version)}" Publisher="${xmlEsc(pkg.publisher)}" />
    <DisplayName>${xmlEsc(pkg.displayName || pkg.name)}</DisplayName>
    <Description xml:space="preserve">${xmlEsc(pkg.description || "")}</Description>
    <Tags>${categories}</Tags>
    <Categories>${categories}</Categories>
    <GalleryFlags>Public</GalleryFlags>
    <Properties>
      <Property Id="Microsoft.VisualStudio.Code.Engine" Value="${xmlEsc(vscodeEngine)}" />
      <Property Id="Microsoft.VisualStudio.Code.ExtensionDependencies" Value="" />
      <Property Id="Microsoft.VisualStudio.Code.ExtensionPack" Value="" />
      <Property Id="Microsoft.VisualStudio.Code.ExtensionKind" Value="workspace" />
      <Property Id="Microsoft.VisualStudio.Code.LocalizedLanguages" Value="" />
    </Properties>
  </Metadata>
  <Installation>
    <InstallationTarget Id="Microsoft.VisualStudio.Code" />
  </Installation>
  <Dependencies/>
  <Assets>
    <Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true" />
    <Asset Type="Microsoft.VisualStudio.Services.Content.Details" Path="extension/README.md" Addressable="true" />
  </Assets>
</PackageManifest>
`;
fs.writeFileSync(path.join(stage, "extension.vsixmanifest"), manifest);

// --- [Content_Types].xml ------------------------------------------------
// OPC requires a content type for every part's extension. Collect every
// distinct extension under the stage; emit a Default for each (the precise
// MIME is not load-bearing for VS Code, only that an entry exists).
const exts = new Set(["vsixmanifest"]);
const extensionless = [];
(function walk(dir) {
  for (const e of fs.readdirSync(dir)) {
    const p = path.join(dir, e);
    if (fs.statSync(p).isDirectory()) walk(p);
    else {
      const ext = path.extname(e).slice(1).toLowerCase();
      if (ext) exts.add(ext);
      else extensionless.push(path.relative(stage, p).split(path.sep).join("/"));
    }
  }
})(stage);
const MIME = {
  json: "application/json", js: "application/javascript", md: "text/markdown",
  ts: "application/typescript", map: "application/json", txt: "text/plain",
  vsixmanifest: "text/xml", xml: "text/xml", svg: "image/svg+xml",
  png: "image/png", node: "application/octet-stream",
};
let ct = '<?xml version="1.0" encoding="utf-8"?>\n<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">\n';
for (const ext of [...exts].sort())
  ct += `  <Default Extension=".${ext}" ContentType="${MIME[ext] || "application/octet-stream"}" />\n`;
for (const part of extensionless)
  ct += `  <Override PartName="/${part}" ContentType="application/octet-stream" />\n`;
ct += "</Types>\n";
fs.writeFileSync(path.join(stage, "[Content_Types].xml"), ct);

// --- zip ----------------------------------------------------------------
const out = path.join(ROOT, `${pkg.name}-${pkg.version}.vsix`);
if (fs.existsSync(out)) fs.unlinkSync(out);
cp.execSync(`zip -r -X -q "${out}" "[Content_Types].xml" extension.vsixmanifest extension`, {
  cwd: stage,
  stdio: "inherit",
});
fs.rmSync(stage, { recursive: true, force: true });
console.log("Wrote " + out);
