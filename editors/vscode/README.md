# trace for VS Code

Syntax highlighting and formatting for `.tr` files (the `trace` HDL — see
the repo root `DESIGN.md`).

- **Syntax highlighting**: a TextMate grammar (`syntaxes/trace.tmLanguage.json`)
  covering keywords, the contextual effect words (`converges`, `suspends`,
  `reads`, ...), builtins (`bits`, `clog2`, `prio`, ...), numbers
  (`0x`/`0b`/decimal, with `_` separators), and `--` line comments.
- **Formatting**: "Format Document" shells out to the `trace` compiler
  itself (`trace - --fmt`, reading the buffer on stdin) rather than
  reimplementing formatting in JavaScript — see `src/fmt.rs` in the repo
  root for what it does and, importantly, does *not* do (it is a simple
  reindenter, not a full pretty-printer; comments are never touched).

## Setup

This extension is not published to the Marketplace. To use it locally:

1. Make sure the `trace` binary is on `PATH` — e.g. `cargo install --path .`
   from the repo root, or launch VS Code from inside `devenv shell`. If it's
   somewhere else, set `trace.formatterPath` in your VS Code settings.
2. Either:
   - **Development host**: open this `editors/vscode` folder in VS Code and
     press F5 (`.vscode/launch.json` is already set up) to launch an
     Extension Development Host with it loaded, or
   - **Local install**: symlink this folder into your VS Code extensions
     directory, e.g.
     `ln -s $(pwd) ~/.vscode/extensions/trace-hdl`, then reload VS Code.

## What's not here

No language server — no go-to-definition, hover, or inline diagnostics.
The grammar is regex-based (TextMate), so highlighting can't distinguish
contexts a real parser would (e.g. `reads`/`writes` as effect-list words vs.
as ordinary identifiers) — good enough for readability, not a substitute
for `trace file.tr` catching real errors.
