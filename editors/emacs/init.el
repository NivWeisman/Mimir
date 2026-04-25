;;; init.el --- Emacs config snippet for the Mimir SystemVerilog LSP -*- lexical-binding: t -*-
;;
;; Drop the relevant block of this file into your own `init.el`. We provide
;; configurations for both `eglot` (built into Emacs 29+) and `lsp-mode`
;; (richer, third-party). Pick one — don't enable both for the same buffer.
;;
;; ---------------------------------------------------------------------------
;; PREREQUISITE: build mimir-server with `cargo build --release` and ensure
;; the binary is reachable via `exec-path`. The simplest way:
;;
;;   (add-to-list 'exec-path (expand-file-name "~/path/to/mimir/target/release"))
;;
;; ---------------------------------------------------------------------------

;;; ---------- Option 1: eglot (built-in, minimal) ---------------------------

(with-eval-after-load 'eglot
  ;; Tell eglot which command to launch when entering a SystemVerilog buffer.
  ;; The car of the list is matched against `major-mode'; the cdr is the
  ;; command line. We hook both `verilog-mode' (built into Emacs) and
  ;; `verilog-ts-mode' (tree-sitter, Emacs 29+).
  (add-to-list 'eglot-server-programs
               '((verilog-mode verilog-ts-mode) . ("mimir-server"))))

;; Auto-start eglot when opening .sv / .svh / .v / .vh files.
(add-hook 'verilog-mode-hook    #'eglot-ensure)
(add-hook 'verilog-ts-mode-hook #'eglot-ensure)


;;; ---------- Option 2: lsp-mode (richer, requires the package) -------------
;;
;; Uncomment if you prefer lsp-mode over eglot. Requires:
;;   M-x package-install RET lsp-mode RET

;; (with-eval-after-load 'lsp-mode
;;   (add-to-list 'lsp-language-id-configuration
;;                '(verilog-mode . "systemverilog"))
;;   (lsp-register-client
;;    (make-lsp-client
;;     :new-connection (lsp-stdio-connection "mimir-server")
;;     :major-modes '(verilog-mode verilog-ts-mode)
;;     :server-id 'mimir
;;     :environment-fn (lambda ()
;;                       ;; Crank up logging in the server. Watch with:
;;                       ;;   M-x lsp-workspace-show-log
;;                       '(("RUST_LOG" . "mimir=debug"))))))
;;
;; (add-hook 'verilog-mode-hook    #'lsp-deferred)
;; (add-hook 'verilog-ts-mode-hook #'lsp-deferred)


;;; ---------- File extensions ----------------------------------------------

;; Make sure .sv and .svh open in a Verilog-aware mode. Emacs's built-in
;; verilog-mode already handles .v/.vh; we extend it for SystemVerilog.
(add-to-list 'auto-mode-alist '("\\.sv\\'"  . verilog-mode))
(add-to-list 'auto-mode-alist '("\\.svh\\'" . verilog-mode))

(provide 'mimir-init)
;;; init.el ends here
