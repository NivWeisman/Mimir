# Mimir VS Code extension

Thin client that launches the `mimir-server` Rust binary and pipes LSP
traffic over stdio. All real features are implemented server-side.

## Build

```bash
cd editors/vscode
npm install
npm run compile          # produces out/extension.js
```

## Run in development

1. Build the server: `cargo build --release` from the workspace root.
2. Open this `editors/vscode/` folder in VS Code.
3. Press `F5` — VS Code launches an "Extension Development Host" window with
   the extension loaded.
4. In that window, open a `.sv` file. You should see syntax-error squiggles
   if the file is malformed.

If `mimir-server` isn't on `$PATH`, set `mimir.server.path` in settings to
the absolute path of `target/release/mimir-server`.

## Logging

Set in your VS Code settings:

```jsonc
{
  "mimir.server.env": { "RUST_LOG": "mimir=debug" },
  "mimir.trace.server": "verbose"
}
```

Server logs go to the **Output** panel under "Mimir SystemVerilog"; LSP
trace goes to "Mimir LSP Trace".
