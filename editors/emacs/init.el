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

;;; ---------- use-package bootstrap -----------------------------------------
;;
;; Ensures use-package is available. Built into Emacs 29+; install it from
;; MELPA on older versions. Place this before any `use-package' call.

(require 'package)
(unless (assoc "melpa" package-archives)
  (add-to-list 'package-archives '("melpa" . "https://melpa.org/packages/") t))
;; Defensive: users with (setq package-enable-at-startup nil) in early-init.el
;; skip Emacs 27+'s implicit init, which would make `package-installed-p' lie.
(package-initialize)
(unless (package-installed-p 'use-package)
  (condition-case err
      (progn (package-refresh-contents)
             (package-install 'use-package))
    (error (message "mimir-init: failed to install use-package: %S" err))))

(require 'use-package)
(setq use-package-always-ensure t)  ; treat :ensure t as the default


;;; ---------- Option 0: evil-mode (Vim keybindings) -------------------------
;;
;; Enabled by default. Comment out the `use-package evil' block to disable.
;; Requires:
;;   M-x package-install RET evil RET
;; Or with use-package:

(use-package evil
  :ensure t
  :init
  (setq evil-want-integration t
        evil-want-keybinding nil)  ; set to nil if pairing with evil-collection
  :config
  (evil-mode 1))

;; Optionally pair with evil-collection for consistent bindings in other modes:
;;   M-x package-install RET evil-collection RET

(use-package evil-collection
  :after evil
  :ensure t
  :config
  (evil-collection-init))


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
