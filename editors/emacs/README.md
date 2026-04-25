# Mimir Emacs setup

Drop the relevant block from [`init.el`](./init.el) into your own Emacs
config. We provide configurations for both `eglot` (built into Emacs 29+)
and `lsp-mode` (richer, third-party).

## Quick start (eglot)

1. Build the server: `cargo build --release` from the workspace root.
2. Make sure `mimir-server` is reachable from Emacs's `exec-path`:
   ```elisp
   (add-to-list 'exec-path (expand-file-name "~/path/to/mimir/target/release"))
   ```
3. Open a `.sv` file. `verilog-mode` auto-starts; `eglot-ensure` connects to
   `mimir-server`.

## Logging

eglot logs to the `*EGLOT events*` buffer. To enable verbose server logging:

```elisp
(setenv "RUST_LOG" "mimir=debug")
```

before starting Emacs (or via `M-x setenv`). Server stderr is shown in the
`*mimir-server stderr*` buffer that eglot creates per server process.
