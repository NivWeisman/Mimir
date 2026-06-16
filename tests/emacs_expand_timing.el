;;; emacs_expand_timing.el --- Time mimir/expandMacro through Emacs' LSP client -*- lexical-binding: t -*-
;;
;; Drives mimir-server through Emacs' built-in eglot (the same JSON-RPC path a
;; real Emacs user hits) and times the custom `mimir/expandMacro' request on a
;; real UVM macro. The point is to measure the client->server->client round
;; trip WITHOUT VS Code in the loop, so we can tell whether "expand is slow"
;; is the server/sidecar or the VS Code extension's own handling (opening the
;; read-only expansion tab, markdown rendering, etc.).
;;
;; It builds a throwaway project whose .mimir.toml puts the riscv-dv UVM
;; compilation unit AND UVM-1.2 on the include path (riscv-dv's own
;; .mimir.toml does not ship UVM, so `uvm_* macros aren't reachable there),
;; then opens the real riscv_vector_cfg.sv and expands
;; `uvm_field_queue_int(legal_eew, UVM_DEFAULT).
;;
;; Usage (from the repo root):
;;   MIMIR_BIN=target/release/mimir-server \
;;   MIMIR_SLANG_PATH=slang-sidecar/build/mimir-slang-sidecar \
;;   emacs -Q --batch -l tests/emacs_expand_timing.el
;;
;; Prints one of:
;;   EMACS_EXPAND connect=<s>
;;   EMACS_EXPAND cold=<s> repeats=<s,s,...> name=<macro> lines=<n>
;;   EMACS_EXPAND skip=<reason> | EMACS_EXPAND error=<reason>

(require 'eglot)
(require 'project)
(require 'cl-lib)

(defun mimir--abs (rel repo) (expand-file-name rel repo))

(let* ((repo (expand-file-name
              (concat (file-name-directory (or load-file-name buffer-file-name)) "..")))
       (riscv (mimir--abs "examples/riscv-dv" repo))
       (uvm-src (mimir--abs "examples/uvm-1.2/src" repo))
       (target (mimir--abs "examples/riscv-dv/src/riscv_vector_cfg.sv" repo))
       (bin (mimir--abs (or (getenv "MIMIR_BIN") "target/release/mimir-server") repo))
       (sidecar (mimir--abs (or (getenv "MIMIR_SLANG_PATH")
                                "slang-sidecar/build/mimir-slang-sidecar")
                            repo))
       (cu (list (mimir--abs "examples/riscv-dv/src/riscv_signature_pkg.sv" repo)
                 (mimir--abs "examples/riscv-dv/src/riscv_instr_pkg.sv" repo)
                 (mimir--abs "examples/riscv-dv/test/riscv_instr_test_pkg.sv" repo)
                 (mimir--abs "examples/riscv-dv/test/riscv_instr_gen_tb_top.sv" repo)))
       ;; `uvm_field_queue_int(legal_eew, UVM_DEFAULT) — line 117 (1-based),
       ;; cursor inside the macro name (col 8, 0-based).
       (want-line 116) (want-char 8))
  (condition-case err
      (progn
        (unless (file-exists-p bin) (princ (format "EMACS_EXPAND skip=no-server-binary:%s\n" bin)) (kill-emacs 0))
        (unless (file-exists-p sidecar) (princ (format "EMACS_EXPAND skip=no-sidecar:%s\n" sidecar)) (kill-emacs 0))
        (unless (file-exists-p target) (princ "EMACS_EXPAND skip=riscv-dv-not-cloned\n") (kill-emacs 0))
        (unless (file-exists-p (expand-file-name "uvm_macros.svh" uvm-src))
          (princ "EMACS_EXPAND skip=uvm-1.2-not-present\n") (kill-emacs 0))

        ;; Build a throwaway project that makes the UVM macro reachable.
        (let* ((proot (make-temp-file "mimir-emacs-" t))
               (flist (expand-file-name "files.f" proot)))
          (with-temp-file flist
            (insert (mapconcat #'identity cu "\n") "\n"))
          (with-temp-file (expand-file-name ".mimir.toml" proot)
            (insert "[slang]\n"
                    (format "filelist = %S\n" flist)
                    (format "include_dirs = [%S, %S, %S, %S]\n"
                            (mimir--abs "examples/riscv-dv/src" repo)
                            (mimir--abs "examples/riscv-dv/test" repo)
                            (mimir--abs "examples/riscv-dv/target/rv32imc" repo)
                            uvm-src)))
          (setenv "MIMIR_SLANG_PATH" sidecar)
          ;; Force this temp dir as the workspace root for any file we open.
          (setq project-find-functions (list (lambda (_dir) (cons 'transient proot))))
          (add-to-list 'eglot-server-programs `((verilog-mode) . (,bin)))

          (find-file target)
          (verilog-mode)
          (let* ((proj (project-current nil proot))
                 (t0 (float-time))
                 (server (eglot 'verilog-mode proj 'eglot-lsp-server (list bin) "verilog")))
            (princ (format "EMACS_EXPAND connect=%.3f\n" (- (float-time) t0)))
            (cl-flet ((req ()
                        (let* ((s (float-time))
                               (r (condition-case e
                                      (jsonrpc-request
                                       server :mimir/expandMacro
                                       (list :textDocument (list :uri (concat "file://" target))
                                             :position (list :line want-line :character want-char))
                                       :timeout 120)
                                    (error (list :error (format "%S" e))))))
                          (cons (- (float-time) s) r))))
              ;; Poll until the index has hydrated enough that the macro is
              ;; actually expandable (a real body, lineCount > 0) rather than
              ;; short-circuited as "undefined". The first such call is the
              ;; cold expansion.
              (let ((cold nil) (deadline (+ (float-time) 25.0)))
                (while (and (not cold) (< (float-time) deadline))
                  (let* ((c (req)) (r (cdr c)))
                    (if (and (listp r) (numberp (plist-get r :lineCount))
                             (> (plist-get r :lineCount) 0))
                        (setq cold c)
                      (accept-process-output nil 0.3))))
                (if (not cold)
                    (let ((last (cdr (req))))
                      (princ (format "EMACS_EXPAND error=never-expandable last=%S\n" last)))
                  (let ((repeats (mapcar (lambda (_) (car (req))) (number-sequence 1 5)))
                        (r (cdr cold)))
                    (princ (format "EMACS_EXPAND cold=%.3f repeats=%s name=%s lines=%s\n"
                                   (car cold)
                                   (mapconcat (lambda (x) (format "%.3f" x)) repeats ",")
                                   (plist-get r :name)
                                   (plist-get r :lineCount)))))))
            (ignore-errors (eglot-shutdown server)))
          (ignore-errors (delete-directory proot t)))))
    (error (princ (format "EMACS_EXPAND error=%S\n" err)) (kill-emacs 1)))
  (kill-emacs 0))
