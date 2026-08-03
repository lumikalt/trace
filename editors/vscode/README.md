# trace for VS Code

Syntax highlighting and formatting for `.tr` files (the `trace` HDL — see
the repo root `DESIGN.md`).

- **Syntax highlighting**: a TextMate grammar (`syntaxes/trace.tmLanguage.json`)
  covering keywords, the contextual effect words (`combines`, `sequences`,
  `reads`, ...), builtins (`bits`, `clog2`, `prio`, ...), numbers
  (`0x`/`0b`/decimal, with `_` separators), and `--` line comments.
- **Formatting**: "Format Document" shells out to the `trace` compiler
  itself (`trace - --fmt`, reading the buffer on stdin) rather than
  reimplementing formatting in JavaScript — see `src/fmt.rs` in the repo
  root for what it does and, importantly, does *not* do (it is a simple
  reindenter, not a full pretty-printer; comments are never touched).
- **Language server**: diagnostics, go-to-definition, and hover are backed
  by a real language server (`trace --lsp`, `src/lsp.rs` in the repo root)
  running the exact same `lex -> parse -> resolve -> effects -> types`
  pipeline the CLI itself drives — not a second, drifting reimplementation.
  It recompiles the whole (in-editor, unsaved-included) buffer on every
  edit and republishes diagnostics; go-to-definition and hover both need
  at least a clean `resolve` pass to work (see "What's not here" below).

## Setup

This extension is not published to the Marketplace. To use it locally:

1. Make sure the `trace` binary is on `PATH` — e.g. `cargo install --path .`
   from the repo root, or launch VS Code from inside `devenv shell`. If it's
   somewhere else, set `trace.formatterPath`/`trace.serverPath` in your VS
   Code settings (both point at the same binary by default).
2. Install the extension's own JS dependency (`vscode-languageclient`, the
   client half of the language server wiring): `npm install` in this
   directory. Not checked in (`node_modules` is gitignored), so this is a
   one-time step after cloning.
3. Either:
   - **Development host**: open this `editors/vscode` folder in VS Code and
     press F5 (`.vscode/launch.json` is already set up) to launch an
     Extension Development Host with it loaded, or
   - **Local install**: symlink this folder into your VS Code extensions
     directory, e.g.
     `ln -s $(pwd) ~/.vscode/extensions/trace-hdl`, then reload VS Code.

## What's not here

Go-to-definition and hover only resolve identifier *uses* (a name where
it's referenced), not the declaration site itself (hovering the `counter`
in `reg counter : [8]` returns nothing — only a later `counter := ...`
does). Diagnostics stop at the first pipeline phase with errors (matching
`main.rs`'s own early-return chain) — a file with a parse error shows
exactly those parse errors and nothing from `resolve`/`effects`/`types`,
since those passes never ran against a broken AST; fix the earliest error
first to see what's next. The grammar is still regex-based (TextMate), not
a real parser — it scopes `reads`/`writes`/etc. to inside `<...>` effect
lists and `urgency`/`mutually_exclusive`/`conflict_free` to inside
`schedule { ... }` blocks (so the same words used as ordinary identifiers
elsewhere highlight as plain identifiers, not keywords), but this is still
pattern matching, not semantic analysis — good enough for readability, not
a substitute for the language server's own diagnostics.
