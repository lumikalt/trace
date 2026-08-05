# TODO

## Emission (`src/firrtl/`)

- Calling a user `fn`/`impl` from a rule inlines the callee at its call
  site; see DESIGN.md's "Calling a function from a rule" for the full
  design history. Remaining real restrictions, not just historical
  color:
  - A callee body must be zero-or-more `let`s + state writes, then a
    trailing `return` or an `if`/`else` (mandatory `else`) recursing
    into that same shape. Bare, top-level guards AND fifo ops are now
    both allowed — folded into the caller's own rule guard
    (`check_fails_is_foldable_guard`, `callee_fail_cond`, `fifo.rs`'s
    `rule_fifo_ops`/`compile_fifo_op_value`) — but only when the
    callee's ENTIRE fail condition reduces to that shape: a guard or
    fifo op nested inside one of the callee's own `if`/`else` branches,
    or a nested call to another failing function (v0 folds one level
    only), still reject outright, explicitly, rather than fold only
    part of the real condition. A callee-local feeding a `Deq` into a
    later `Enq` must be `let`-bound, not `:=` (see DESIGN.md's "Calling
    a function from a rule"). A `while` loop in an ordinary
    (non-`<elaborates>`) callee body remains entirely unimplemented — no
    unroll-and-splice machinery exists for it, not scoped to be lifted,
    just not yet attempted. `<elaborates>` recursion/unrolling (DESIGN.md's
    `AdderTree` example) is separate machinery, ACHIEVED — see below.
  - A nested call composes as a value, or as a state-writing bare
    statement / `:=` RHS, as long as it doesn't form a call cycle
    (static call-graph check) — nested any deeper than those two write
    positions (an argument, a `let`, `Bump(a) + 1`) is a clean, explicit
    error.
  - Of the builtins, only `prio`/`trunc`/`pack`/`logic` are
    synthesizable as calls; `clog2`/`len` are compile-time-only
    (`Ty::Int`), and `wire`/`list`/`any`/`sync`/`race` aren't applicable
    to a plain combinational callee body at all (the old explicit
    `bits[N]` spelling no longer has surface syntax at all — see the
    `[N]` entry below).
  - A generic callee parameter's own width (`[N]`) is only
    resolvable for its own return value or by following it through
    `self.locals` back to a concrete call-site expression — used
    independently elsewhere in a generic callee body, this fails
    cleanly ("no concrete width"), not a miscompile, but is still a real
    sharp edge if this area is touched again.

  See `examples/call.tr`, `examples/call_branch.tr`,
  `examples/call_writes.tr`, `examples/call_prio.tr`,
  `examples/call_trunc.tr`, `examples/call_pack.tr`,
  `examples/call_nested.tr`, `examples/call_nested_writes.tr`,
  `examples/call_guard.tr`, `examples/call_fifo.tr`.
- **RESOLVED — was a fresh-local-declaration footgun, now closed
  language-wide.** A state-"writing" `fn` declared at FILE top level,
  referencing a register by NAME (`log := d`) that happens to match one
  declared inside some module, was previously thought to silently drop
  the write (found by advisor's third probe while verifying struct-
  returning-callee-that-also-writes-state). Root cause, once actually
  traced through resolve.rs: not a miscompile. `x := e` on a name that
  didn't resolve used to declare a FRESH LOCAL, and a top-level fn
  genuinely has no lexical access to a module's internal state (by
  design — modules share no state with each other), so `log := d`
  inside a top-level fn was just declaring an unused local named `log`,
  never touching the module's real `log` reg — no wrong hardware ever
  got built, the fn simply didn't do what its (invalid) source implied.
  Confirmed the EXPLICIT form already caught the genuine mistake
  cleanly (`<writes {log}>` on a top-level fn errors "cannot find state
  `log`" via `check_effect_args`) — it was only the IMPLICIT
  (inferred-signature) path that gave no diagnostic.
  First fix (interim, superseded below): a narrow, fn/impl-scoped check
  in resolve.rs — a local introduced via `let` or a fresh `x := e`
  ANYWHERE in a fn/impl body, never read back anywhere in that same
  body, became a compile error ("assigned but never read... if this was
  meant to write module state, that state isn't in scope here").
  Final fix (Verse-alignment pass, see the `let`-required entry below):
  rather than keep patching narrowly around the ambiguity, `let` became
  the ONLY way to declare a fresh local ANYWHERE in the language — a
  rule body exactly as much as a fn/impl body. `x := e` on an unresolved
  name is now always a clean "cannot find `x`; use `let x = ...` to
  declare a new local" error, which catches `log := d` even earlier and
  more precisely than the fn/impl-scoped unread-local check ever did.
  That unread-local check still exists, scoped to `let`-bound locals in
  fn/impl bodies only (never rule bodies, where a scratch local is a
  legitimate, load-bearing pattern via `let x = ...` then reassignment
  via `x := ...`) — a fn's only outputs are its return value and its
  state writes, so a never-read local there is dead by construction, a
  much tighter invariant than "unused anywhere," deliberately NOT a
  general "unused local" lint. Zero false positives across the full
  existing test suite (every example, every test fixture) — every real
  fn/impl body's locals are already read at least once.
- `<elaborates>` recursion/unrolling (`src/elaborate.rs`) is ACHIEVED —
  DESIGN.md's own `AdderTree` example (compile-time tree recursion over a
  `list[[N]]`, one-sided slices `xs[..mid]`/`xs[mid..]`). Separate
  machinery from ordinary callee inlining above: a real interpreter over
  `Stmt`/`Expr` (sequential execution, real `if`/`return`, real list
  slicing/`len()`) runs as its own text-splice pre-pass — same
  parse/render/re-parse round trip `lower.rs` already uses for
  `<sequences>` — reducing every top-level `<elaborates>` call to plain
  trace source text before the real type-checking/emission passes ever
  see it, so synthesized expressions get ordinary real type checking for
  free. New `--elaborate` CLI flag; `devenv.nix`'s `simulate` script now
  runs elaborate → lower → firrtl. `MAX_DEPTH` (64) is a hard compiler
  backstop against non-terminating recursion — DESIGN.md states
  termination is unchecked in v0 as a language guarantee, but the
  compiler itself still won't hang. `while` inside `<elaborates>` code
  stays unimplemented (v0 restriction: use recursion, `AdderTree` style).
  See `examples/adder_tree.tr` + `sim/adder_tree_tb.v`.
- Expression surface still excludes:
  - A fully dynamic slice (`x[hi..lo]` with a non-const `hi`/`lo`) —
    its result width would be dynamically sized, which this language
    can't express, so it's a compile-time type error by design, not a
    gap to close. Use `x[base +: width]`/`x[base -: width]` (dynamic
    start, static width) instead.

  Everything else is implemented: arithmetic/bitwise/compare ops, static
  AND dynamic-amount shifts (logical `<<`/`>>` and arithmetic
  sign-extending `>>>`), unary `-`/`~`/`not`, `/`/`%`, static AND dynamic
  bit-select/indexed part-select, Verilog-style sized literals (+
  inferred `reg`/`out` types from one), the `[N]` bit-vector type (`bit`/
  `uN` sugar retired in favor of it — see below).
  See DESIGN.md's "Expression surface" section for the full history and
  width-rule derivations (several confirmed empirically against real
  firtool, not assumed from spec text); `examples/alu.tr`,
  `examples/dynamic_shift.tr`, `examples/arith_shift.tr`,
  `examples/div_rem.tr`, `examples/sized_literal.tr`,
  `examples/infer_reg_ty.tr`, `examples/dynamic_bit_select.tr` for
  coverage.
- A local reassigned at a rule's top level now resolves each reference
  at its own textual position (still rejected inside `if`/`else` — no
  example needs that, and it's a materially different, larger change).
  One narrow remaining case still rejects reassignment explicitly: a
  local whose width never resolves to a concrete `[w]` anywhere in
  the rule (e.g. used only as a mem-read index). See DESIGN.md's
  "Locals" section, `examples/reassigned_local.tr` +
  `sim/reassigned_local_tb.v`.
- A memory written to more than once (unconditionally) in one rule is a
  compile-time error, closing a real silent-miscompile gap the recent
  fifo/spawn audit found. See DESIGN.md's "Memory, fifo, and submodule
  declarations" section. (Its erstwhile companion gap — a `let`-bound
  value crossing a `tick` being rejected — is gone entirely now; see
  below.)
- The two message-quality gaps queued alongside the above are also
  closed: `let x = m[addr]` (a memory read bound with `let`) now emits a
  real read port and compiles, instead of erroring (`collect_read_sites`
  had no `Stmt::Let` arm, the same Assign-vs-Let parity gap as the
  fifo-op bug); `let h = spawn Foo(args)` and `let value = race[...]`
  are now both the ORDINARY forms (see below), no longer restricted to
  `:=`.
- **`let` required everywhere; `let` can now cross a `tick` (Verse-
  alignment pass).** Closing the `x := e`/`let` fresh-local-declaration
  ambiguity (see the RESOLVED entry above) surfaced a real architectural
  conflict: `<sequences>`/`spawn`/`race` previously required a bare
  `x := value` specifically for anything crossing a `tick`, because only
  a `:=`-bound name could be promoted to a save register and still parse
  correctly once spliced verbatim into the generated segment rule
  (`x := value` stays an ordinary register write once `x` becomes a
  `reg`; `let x = value` would always shadow-bind a fresh local
  instead). Rather than carve out a `<sequences>`-scoped exception to
  "`let` is required" (considered and explicitly rejected via
  AskUserQuestion — the more invasive, more principled option was
  chosen instead), `let` itself was taught to cross a `tick`:
  `CapturedLocal` now records a `let_prefix_span` (the declaring
  statement's own `let x = ` prefix, from the statement's start through
  the init expression's own start) for a `let`-bound capture, and
  `render_rule`/`render_spawn_segments` rewrite that prefix to `x := `
  before splicing the rest of the statement verbatim — the same trick a
  `:=`-bound capture already used, just with one rewritten prefix first.
  A spawn handle (`let h = spawn Foo(args)`) and a race destination
  (`let value = race[...]`) are a simpler case: `spawn_trigger_shape`/
  `race_value_shape` now recognize `Stmt::Let` directly and synthesize
  brand-new register-write lines from the extracted handle, no
  splice/rewrite needed at all. Net effect: only "define vs. mutate"
  distinguishes `let` from `:=` now, not "gets a capture register vs.
  doesn't" — that second axis is gone.
  One new restriction, not possible before this change (previously ANY
  `let` crossing a tick was rejected outright, so it couldn't arise):
  two textually-distinct `let x = ...` bindings that both cross a tick
  within the same rule (shadowing — different `DefId`s, same name)
  would otherwise both become captures sharing one save-register name;
  `compute_captures` now rejects this directly ("shadows another
  captured value of the same name") instead of emitting two colliding
  `reg x` lines that would fail to re-resolve downstream. Found via
  advisor review before commit, not by the test suite.
  Two more bugs self-caught during implementation, both via dedicated
  discriminating probes before the mechanical test migration ever ran:
  an `unreachable!()` panic in `find_unsupported_construct` (assumed
  spawn-trigger/race-value statements were always `Stmt::Assign`-shaped;
  fixed by using `stmt_exprs(...).last()` instead, which already handles
  both shapes uniformly), and `render_rule`'s race-value arm always
  synthesizing `{name} := __race_value(...)` regardless of whether the
  destination was `let`-declared (needing `let name = ...`) or
  `:=`-declared (needing `name := ...`), producing lowered output with
  an undeclared `winner` that failed to re-resolve at FIRRTL-emission
  stage. See DESIGN.md's "Locals", "`sequences`: multi-cycle code",
  "`spawn`, `sync`, and `race`", and "Sequences lowering" sections.
- **RESOLVED — a callee-local reassigned via `:=` was silently dropped
  at FIRRTL emission; found while fact-checking DESIGN.md's "Calling a
  function from a rule" section in an earlier pass, not by the test
  suite.** Confirmed pre-existing at the time, not a regression from
  that session's changes (`git stash -u` + rerun on the pre-session
  baseline reproduced byte-for-byte identical output). Repro: a callee
  with `let x = input.Deq[]` followed by `x := x + 1` later in the same
  body — this used to compile clean, no error, but the reassignment
  never appeared anywhere in the emitted FIRRTL; every read of `x`
  resolved to its FIRST binding as if the `x := x + 1` line were never
  there.
  Root cause, once probed through every inlining entry point (not just
  the originally-reported return-value path): `self.locals`
  (firrtl/expr.rs, calls.rs, writes.rs) substitutes each callee-local's
  ORIGINAL binding EXPRESSION at every use site — an `ExprId`,
  re-resolved fresh at each read — rather than a snapshot of its value
  at bind time, unlike `enter_rule`'s `locals_snapshots` (writes.rs),
  which gives RULE-level `:=` reassignment its correct position-indexed
  behavior by eagerly compiling each local to TEXT the moment it's
  bound. A naive fix (just start tracking `Stmt::Assign`-to-Local in
  `self.locals`, the same way `Stmt::Let` already is, at each of the
  four callee-body-walking sites) was tried and REJECTED before
  shipping, based on a probe that found it would trade one silent bug
  for another: `let z = x; x := x + 1; return z` currently (correctly,
  if accidentally) returns `x`'s value AT BIND TIME for `z`, since the
  reassignment is dropped entirely today; naively threading the
  reassignment through the lazy `ExprId` map would make `z` resolve to
  `x`'s NEW value instead, since `self.locals[z]` stores a live
  reference to `x`, not a copy of what `x` was at the time. A SEPARATE
  probe also found a branch-nested variant of the original bug
  (`callee_reg_write`/`callee_port_write` recurse into `if`/`else`
  branches directly, unlike the return-value path, which restricts
  `if`/`else` to the body's own trailing position) that a naive
  sequential fix couldn't have addressed correctly anyway — would need
  real mux-threading of an arbitrarily-named local across branches, the
  same class of machinery `while`'s own accumulator restriction defers.
  **Fixed by rejecting outright, not by threading the fix through.**
  One new check, `check_no_reassigned_locals_in_callee_body`
  (firrtl/checks.rs), run once at `validate_call` (calls.rs) — the
  single choke point every inlining entry point (`compile_call`,
  `callee_fail_cond`, `call_writes_reg`/`call_writes_port`,
  `compile_call_field_value`) already goes through — rather than four
  separate patches at `compile_callee_body`/`compile_callee_body_field`/
  `callee_reg_write`/`callee_port_write`, each of which would still
  have been wrong for the `let z = x` shape above even once patched.
  Recurses into `if`/`if let`/`while`/`while let` bodies too, so the
  branch-nested variant is caught the same way. Verified against all
  five probed shapes (return value, state write, `<fails>` guard,
  branch-nested state write, the `let z = x` chain); the full existing
  test suite (`all_examples_parse`/`all_examples_resolve` plus every
  per-example emission test) staying green confirms no existing `.tr`
  file hits the new restriction. Pinned by
  `a_callee_local_reassignment_is_rejected_not_silently_dropped` and
  `a_callee_local_reassignment_is_rejected_through_every_inlining_path`
  (tests/firrtl.rs).
  **The REAL fix this rejection defers, not attempted here:** extending
  `locals_snapshots`-style eager-text, position-indexed local resolution
  into callee inlining — `self.locals` would need to stop being a plain
  `HashMap<DefId, ExprId>` for scalar locals specifically, while STAYING
  an `ExprId` map for struct/`?T`-typed params (`compile_struct_field_
  read`'s param chase-through resolves those structurally, not by
  scalar value) — two interacting mechanisms, not a type swap. A future
  session picking this up should start from the `let z = x` finding
  above, not from the originally-reported case alone, since that's the
  shape a partial fix keeps getting wrong.
- **`bits[N]` respelled `[N]`; fifo depth respelled `{depth}elem_ty`
  (Lumi's call, an explicit syntax simplification, not a bug fix).**
  `bits`/`bit`/`uN` (`u8`, `u32`, ...) sugar all retired outright — no
  dual-spelling transition period, matching this session's other
  full-replacement migrations. `x := input.Deq[]` on the OLD explicit
  `bits[8]` spelling is now a clean parser-level "no longer valid
  syntax; use `[N]`" error (`parser.rs`'s `Some(Ident)` primary arm),
  not a silent accept alongside the new spelling.
  The real design question was disambiguating `[N]` from an EXISTING
  bare-bracket primary meaning: `[a, b, c]`, a `list[T]` literal.
  Resolved by CONTENT, not position: a bracket holding exactly one item
  with no trailing comma is the `bits[N]` shorthand; empty or 2+
  comma-separated items stays a list literal (`parse_expr`'s
  `Some(LBracket)` arm). This is unambiguous and needs no separate
  type-grammar production, no position tracking, and no special-casing
  of `list`/`wire`'s own callee name, because a nested type position
  (`list[[8]]`) reaches the identical primary rule through the ordinary
  postfix-bracket-args path — confirmed via real firtool on
  `AdderTree`'s own `list[[32]]` signature after respelling it. A
  genuine one-element list VALUE isn't actually lost: a trailing comma
  is the escape hatch (`[a,]`), the same role Rust's own one-element
  tuple syntax gives a trailing comma — `tests/elaborate.rs`'s own
  `AdderTree`-recursion-base-case tests use exactly this spelling
  (`One([a,])`). Omitting the comma (`Foo([x])`) fails with a clear
  type error (a `[N]` type where a value was expected), not a silent
  miscompile — advisor caught an earlier draft of this bullet WRONGLY
  claiming no spelling existed at all, before commit.
  The AST shape is unchanged throughout (`Bracket { Ident("bits"),
  [N] }`, exactly what a `bits[N]` literal always produced) — the
  intent going in was "parser-and-text-only, zero changes downstream,"
  and that held for resolve/effects/types/emission's own LOGIC. It did
  NOT hold for every place that SYNTHESIZES source text to be
  re-parsed: `Ty`'s own `Display` impl (`types.rs`) is reused by
  `lower.rs`'s text-splice-and-reparse pipeline to print a captured
  local's/spawn's save-register type back into generated `<sequences>`
  segment text, and three more call sites in `lower.rs` hardcode
  `"bits[{}]"` directly for continuation-register declarations — all
  four had to move to `[{}]` or the respliced text would fail to
  re-parse against the compiler's OWN new rule. Caught by real test
  failures (`rmw_emits_and_compiles`, `subleq_emits_and_compiles`,
  a fifo-op-after-tick test), not anticipated by the design pass — the
  general lesson, worth remembering for any future AST-shape-preserving
  syntax change: grep every hardcoded format string that emits the OLD
  spelling, not just the AST/parser, since anything on a splice-reparse
  path (`<sequences>`/`spawn`/`<elaborates>` lowering) will silently
  regenerate stale surface syntax otherwise. Several hardcoded
  `bits[...]`-in-error-message strings in `types.rs` (`does not fit
  in`, `condition must be`, `` `not` needs a ``, `` `list` takes ``,
  `a mem needs``) needed the same sweep, verified via `tests/types.rs`.
  Fifo depth's leading-bracket prefix (`[depth]elem_ty`) moved to
  curly braces (`{depth}elem_ty`) specifically to stay unambiguous from
  the new `[N]` primary — `fifo f : [4][8]` would otherwise misparse
  as a nested-list type, not depth-4-of-`[8]`. Verified end to end via
  real firtool + Icarus sim on a representative spread (`fifo_depth`,
  `adder_tree`, `subleq`, `race_value`, `option`, `call_fifo`,
  `struct_pair`, `fetch2`), not just the unit-test suite.
- **`list[T]`/`wire[T]`'s own single argument sugars a bare width
  (Lumi's follow-up call after the `[N]` respelling above: even the new
  `[N]` shorthand still doubled up inside `list[[N]]`).** `list[8]`,
  `list[N]`, `list[clog2(N)]` are all now shorthand for `list[[N]]`
  (that is, `list[bits[N]]`); `list[Pair]` (a list of some OTHER type,
  e.g. a declared struct) is unaffected, since only a `[N]`-shaped
  element had anything left to abbreviate. New `eval_elem_ty` helper
  (types.rs), used by the `list` arm of `eval_ty`: an argument that's
  ALREADY type-shaped (a `Bracket`, an `OptionTy`, or an `Ident` naming
  a declared struct) evaluates as a type normally; anything else is
  treated as the width `[...]` would have wrapped, mirroring the `bits`
  arm's own Known/Unknown `const_eval` fallback. Needed zero parser or
  resolve.rs changes: resolve.rs's free-identifier-as-implicit-param
  collection (`resolve_expr`'s `in_type` recursion) already walks every
  sub-expression of a signature type uniformly, regardless of bracket
  nesting depth, so a bare `list[N]`'s `N` was already becoming an
  implicit param correctly before this change — only its TYPE
  evaluation needed to stop erroring ("expected a type here").
  `wire[T]` was never actually touched by this change, but was already
  accepting a bare width for a different reason: `wire[N]` is a `Bracket`
  whose callee (`wire`) itself evaluates to `Ty::Unknown` (the
  `DefKind::Builtin` arm), and `eval_ty`'s generic-memory-type fallthrough
  short-circuits to `Ty::Unknown` the moment its callee is `Unknown` —
  before ever looking at `args`, let alone calling `eval_elem_ty`. So
  `wire[N]` and `wire[[N]]` were already indistinguishable before this
  commit, confirmed by probing both spellings through `--elaborate` and
  seeing identical reduced output; `list`'s new sugar doesn't extend to
  `wire`, it just happened to already not need it.
  Confirmed a sharper edge, too: `eval_elem_ty`'s "already type-shaped"
  check only special-cases an `Ident` that resolves to a declared
  `Struct`; a misspelled struct name (or any other free identifier,
  e.g. `list[SomeModule]`) is indistinguishable from a genuine implicit
  width param, so it's silently accepted as `list[bits[Unknown]]` rather
  than erroring "expected a type here" the way it did before this change.
  Documented as a known sharp edge in DESIGN.md rather than fixed — a
  bare identifier is a genuine ambiguity the sugar can't resolve without
  losing the implicit-param case it exists to support.
  One real methodology trap, caught before commit: a first attempt at
  testing the implicit-param case (`list[N]`) directly against
  `types::check` failed ("expected `list[[?]]`, got `list[[16]]`") —
  not a real bug, but a test written against an unrealistic pipeline
  ordering. `types::check` alone can't validate a call against an
  unresolved generic list param; in the REAL `--firrtl`/`--lower`
  pipeline, `elaborate.rs` always reduces every such call away FIRST
  (`--elaborate` returns before `types::check` ever runs — see
  `main.rs`), so ordinary type-checking never actually sees an
  unreduced generic-list call. Moved to a `tests/elaborate.rs`-level
  test (the representative venue, matching how every other `<elaborates>`
  test in that file already validates through `plan`/`render`, not
  `types::check` directly) instead of chasing a type-checker fix for a
  scenario that can't arise for real.
- **`optional <expr>`: an explicit one-layer "present" constructor, making
  `??T`'s `Some(None)` (outer present, inner absent) expressible for the
  first time (Lumi's actual goal — a follow-up correction after an initial
  implementation missed it, see below).** Bare-value coercion (`opt := 5`,
  `opt := false`) fills EVERY remaining `?` layer at once, so it can only
  ever reach `??T`'s two fully-agreeing states; `optional false`/`optional
  (optional 5'd3)` force exactly one layer present per keyword, reaching
  the third, previously-inexpressible state too. DESIGN.md's "Option
  types"/`??T` sections have the full read/write semantics and worked
  examples.
  Went through two implementations in one session. The FIRST (since
  reverted) took `reg opt : ?[5] = optional 5'3` reading clearly as the
  whole goal, and — since that already worked via plain bare-value
  coercion before `optional` existed at all (confirmed via `--firrtl`:
  `reg flag : ?[1] = 1'd0` already reset to `flag_valid=1, flag_data=0`)
  — implemented `optional` as pure notation, elided entirely at parse
  time (no new AST node). Lumi then clarified the actual goal was `??T`
  construction, which a transparent/elided `optional` cannot do BY
  DESIGN: eliding it means nothing downstream can ever tell `optional e`
  apart from bare `e`, so there is no way to independently drive two
  Option layers. Reverted the elision, added a real `Expr::Optional`
  node (ast.rs) instead, forced through by the compiler at every
  exhaustive `Expr` match — resolve.rs, effects.rs (x2), elaborate.rs,
  lower.rs's `sub_exprs`, firrtl/calls.rs's `collect_calls`, firrtl/
  fifo.rs's `collect_fifo_ops` — each just recurses into the wrapped
  expression like `Guard`/`Spawn` already do, except `elaborate.rs`
  (`optional` has no elaboration-time meaning, same treatment `Absent`/
  `OptionTy` get: a clean "no meaning in `<elaborates>` code" error).
  Typing (types.rs) mirrors `false`'s own `Ty::AbsentLit` sentinel rather
  than eagerly computing `Ty::Option(inner_ty)`: `optional e` has no
  standalone type, only `Ty::Optional(ExprId)`, which carries `e`'s id
  and recurses through `check_assignable` again on demand, against the
  TARGET's own inner — this was a correction mid-implementation too (the
  advisor's first pass suggested eager `Ty::Option(inner_ty)`, which
  turned out to require loosening `check_assignable`'s existing
  Option-into-Option aliasing guard, itself a real correctness invariant
  protecting params/returns/struct-fields/list-elements everywhere else;
  the lazy sentinel needs no such loosening, `check_assignable`'s
  existing arms are untouched). One genuine silent-miscompile caught by
  hand-probing before considering this done: `oo := optional opt1`
  (`opt1` an EXISTING `?T`-typed reg, not a fresh value) type-checked
  clean and compiled with `oo_valid` driven but `oo_data_valid`/
  `oo_data_data` silently undriven (holding stale/reset values) —
  `compile_field_path_value`'s pre-existing aliasing guard caught it and
  returned `None`, but nothing on the WRITE side escalates a `None` to a
  diagnostic (only the READ side, `compile_struct_field_read`, does).
  Fixed at the type-checking level instead of chasing the emission gap:
  `check_assignable`'s new `Ty::Optional`/`Ty::Option` arm now rejects
  `optional <alias>` outright when `<alias>`'s own type is already
  `Ty::Option` and it isn't a `Expr::Call` (calls decompose per-leaf via
  `compile_call_field_value`, not aliasing, so stay exempt — the same
  carve-out `type_write`'s sibling check already has). Also caught:
  `reg x : [8] = optional false` (a non-Option target) silently passed
  with no error, since `x`'s reg-init routing condition only diverted a
  literal `Expr::Absent` init through the `check_assignable` path, not
  `Expr::Optional` — `check_literal_fits`'s `const_eval` silently returns
  `None` for anything it doesn't recognize (a no-op, not a check),
  identical to the ORIGINAL gap `false` itself needed the same routing
  fix for. Emission side: `option_lit_field_const` (mod.rs, reg/output
  reset consts) and `compile_field_path_value`'s `Ty::Option` arm
  (writes.rs, runtime rule-body writes) both got a matching `Expr::
  Optional` branch, checked ahead of their existing "is it literally
  `Expr::Absent`" logic — forces `valid=1` at that layer, then recurses
  on the WRAPPED sub-expression (not `expr` itself) for `data`, which is
  the one place presence genuinely DOES add a layer to peel off (every
  other case in both functions deliberately does NOT, since bare-value
  coercion has no wrapper to peel).
- **`let {field, field: bind, ...} = source` struct/`?T` destructuring
  (Lumi's pick off the "what's next" list — closes the "Struct
  destructuring" bullet that used to live under "Verse alignment"
  below).** Bare-brace, not Rust's `let StructName{...} = source`:
  deliberately NOT supporting a struct-name prefix, since the parser has
  no type information to validate it against `source`'s actual type, and
  advisor flagged an unchecked "looks like an assertion" name as exactly
  the sharp edge this codebase avoids elsewhere (`list[Piar]`'s own
  bare-identifier ambiguity was JUST documented as a reluctant one, not
  a pattern to add to on purpose). Implemented as PURE parser sugar —
  `parse_let_destructure` expands one destructuring statement into N
  ordinary `Stmt::Let { name: bind, init: Expr::Field { base:
  Expr::Ident(source), name: field } }` nodes at parse time, the same
  "one source statement, several AST statements" `Vec<StmtId>` shape
  `tick <expr>` already returns — no new AST node, and no other pass
  (resolve/effects/types/firrtl) needed a single line changed, since
  each projection is indistinguishable from a hand-written `let bind =
  source.field`. Confirmed this "inherits every restriction for free"
  claim empirically, not just by inspection: a typo'd field name
  surfaces the exact same "struct `Pair` has no field `vlaid`" a
  hand-written projection gives (span points at just the bad field
  token, not the whole statement — each projection gets its own fresh
  `Expr::Ident`/`Expr::Field` pair, never one shared base `ExprId`
  reused across items, keeping the AST a tree the way every existing
  walker assumes), and destructuring a LOCAL that itself aliases another
  struct/Option value hits the pre-existing "not aliased from another
  value" rejection with zero new code
  (`let_destructure_of_an_aliased_option_local_is_rejected`,
  tests/firrtl.rs).
  `source` is restricted to a bare identifier — advisor's call, verified
  before deciding rather than assumed: a call source would desugar to
  one re-evaluation of the call PER destructured field (`Make().a`,
  `Make().b`), silently duplicating whatever the callee's body does
  instead of binding one shared result. Rather than auditing which call
  shapes are actually safe to duplicate, the restriction is enforced
  syntactically (a dedicated parser check, not the generic
  `expect_terminator` message, so `let {a,b} = Make()` reports "must be
  a plain reference... not a call/field access/other expression" instead
  of a confusing "expected end of statement" pointing at `(`).
  Field-shorthand alone (`let {valid, data} = p`) was the FIRST design —
  advisor caught a real gap before implementation: `valid`/`data` are
  the two field names every `?T` has, so destructuring TWO `?T` values
  in one rule (`let {valid} = p` then `let {valid} = q`) would silently
  shadow the first pair rather than error (`let_shadowing_is_allowed`,
  tests/resolve.rs, confirms shadowing is legal, not a diagnostic) —
  exactly the realistic use case this feature exists for. Added
  `field: bind` renaming to close that gap before shipping, not as a
  follow-up. No nested destructuring — single-level field projection
  only, matching the scope Lumi actually asked for.
  **Follow-up: exhaustive by default, `..` to opt out (Lumi's call,
  after asking how to destructure a struct into FEWER locals than it
  has fields — the answer, "just omit the field, no error," wasn't the
  behavior wanted).** Naming only some fields with no trailing `..` is
  now a compile-time error ("missing field(s): b — name them, or add
  `..` to discard the rest"), mirroring `Expr::StructLit`'s own
  missing-field check on the construction side. Unlike the rest of
  destructuring, this piece can't stay pure sugar — validating
  exhaustiveness needs `source`'s resolved type, which the parser
  doesn't have. Rather than promoting destructuring to a real `Stmt`
  variant (the struct-update playbook, and the more "architecturally
  consistent" option per advisor), went with a lighter side channel: a
  new `Ast.destructures: Vec<Destructure>` list the parser populates
  alongside the existing N `Stmt::Let` nodes (unchanged), read only by
  types.rs's new `check_destructures` at the very end of `check()`.
  Justified precisely BECAUSE it's the opposite risk profile from
  struct update's `base` field: forgetting to thread `base` into a
  walker silently miscompiled hardware (a real bug this session hit,
  `infer_expr` missing a read); forgetting to consult this side channel
  anywhere but types.rs only means a missing diagnostic, never wrong
  emission — no other pass needs to know a run of `Stmt::Let`s came
  from a destructuring pattern, they're ordinary lets over `Expr::Field`
  either way. `source`'s type is read back out of `expr_tys` using one
  item's already-type-checked `Expr::Field.base` (`Destructure::
  source_field_base`) rather than re-typing anything — skipped entirely
  for a zero-item pattern (`let {..} = s` / `let {} = s`, no per-item
  base to hang the lookup off, and "bind/discard nothing" can't miss a
  field either way). Advisor flagged a double-error trap before
  implementation, confirmed by the existing `let {vlaid, data} = p`
  typo test: naive exhaustiveness-checking would ALSO report "missing
  field(s): valid" alongside the typo, since `vlaid` doesn't count as
  naming `valid`. Fixed by skipping the exhaustiveness error entirely
  whenever any named field fails to resolve against the declared list —
  that field's own `.field` projection already reports the typo, one
  error not two (pinned:
  `let_destructure_field_typo_reports_the_struct_has_no_such_field`,
  tests/types.rs, now also asserts the missing-field error does NOT
  fire). `?T`'s two synthetic field names were about to get a second
  hand-written `["valid", "data"]` copy (the exhaustiveness side needs
  the same list the `.field`-read arm already matches against) — pulled
  both into one `OPTION_FIELDS` const instead so a future change to
  `?T`'s shape has one site to update, not two.
- **`Name{ field: value, ..., ..base }` struct update — closes the
  "Struct update syntax" bullet that used to live under "Verse
  alignment" below (Lumi's next pick off the same "what's next" list
  destructuring came from).** `..base` fills every field this literal
  doesn't name from `base`'s own same-named field, resolved via
  `compile_struct_field_read` (expr.rs) — the SAME machinery an
  ordinary `.field` read off a plain reference already uses, not new
  emission logic of its own.
  UNLIKE destructuring, this one is NOT pure parser sugar — it's a
  real `base: Option<ExprId>` field on `Expr::StructLit` threaded
  through typing (missing-field validation, base-type check) and
  emission (the missing-field fallback itself). Confirmed empirically
  before implementing (advisor-prompted, given this session's `optional`
  history of getting the sugar-vs-real-node call wrong once already):
  a new FIELD on an existing variant, unlike a new variant, is NOT
  enumerated by the compiler — only sites that destructure `StructLit`
  fully (`{ name, fields }`, no `..`) get forced to handle `base`; sites
  using `{ fields, .. }` compile silently unaware of it. 6 of 10
  `StructLit`-matching sites were compiler-forced (ast.rs, parser.rs,
  resolve.rs, types.rs, lower.rs, firrtl/calls.rs, firrtl/fifo.rs); 4
  were NOT and needed a manual grep-for-`..`-after-adding-the-field
  pass: effects.rs's two walkers (`infer_expr`, `check_expr`),
  firrtl/writes.rs's `compile_field_path_value`, firrtl/mod.rs's
  `struct_lit_field_const`.
  One of the four silent sites was a real correctness bug, not just a
  missing feature: `infer_expr` (effects.rs) not walking `base` would
  mean `q := Pair{ data: 5, ..p }` never recorded a READ of `p` in the
  rule's effect signature — the scheduler wouldn't know the rule reads
  `p` at all, and could legally schedule it concurrently with another
  rule writing `p`, a real hazard invisible without checking the
  schedule (no naturally-written "does `..old` copy the right fields"
  test would catch it, since the VALUES would still be correct on
  whichever cycle it happened to run). Fixed before shipping, verified
  by isolating the read from any write to `p` in the same rule
  (`struct_update_spread_registers_as_a_read_for_scheduling`,
  tests/firrtl.rs) — confirms a second rule writing `p` gets scheduled
  mutually exclusive with the `..p`-only rule, not just that `..p`'s
  VALUES come out right.
  `base` is restricted to a bare identifier — same reasoning and same
  enforcement style as destructuring's `source` restriction (a
  dedicated parser error, not the generic "expected `}`"): a call there
  would need re-evaluating once per field `..base` supplies, silently
  duplicating whatever the callee does. `..` may only be the LAST item
  (Rust's own rule, parser-enforced) and never recurses into a nested
  struct field that's itself only partially given — matches Rust's own
  `..` semantics exactly (never a recursive merge), and sidesteps the
  open design question the original TODO bullet raised about how it
  "composes with a nested struct field that's itself only partially
  overridden" by simply not composing with one at all.
  Not supported in a reg/output INIT (a compile-time-constant position):
  `base`'s own flat fields generally aren't known until runtime. Caught
  the silent-miscompile shape here too before shipping — `struct_lit_
  field_const`'s existing `fields.iter().find(...)?` returning `None`
  for a `..base`-supplied field is routed by its caller (module.rs)
  through `.unwrap_or(0)`, a silent zero reset with no error, the exact
  same bug class `optional opt1`'s aliasing rejection closed for `?T`
  earlier this session. Closed with a dedicated recursive walk
  (`contains_struct_update`, types.rs, built on the now-`base`-aware
  `lower::sub_exprs`) over the WHOLE init tree, not a top-level-only
  check — `Outer{ inner: Inner{ ..old }, x: 1 }` nests a `..` one level
  inside an explicitly-given field's own literal, still unreachable
  from a const-eval and still caught.
  Verified end to end through real firtool, not just `--firrtl` text:
  `p := Pair{ data: 5, ..p }`'s untouched field emits a self-connect
  (`connect p_valid, p_valid`), confirmed firtool accepts it cleanly —
  a register reading its own pre-edge value combinationally, the same
  transactional semantics `pc := pc + 3` already relies on.
  `examples/struct_pair.tr` gained a `counter`/`bump` reg-and-rule pair
  incrementing one field of its own current value every cycle while
  carrying the other forward unchanged, simulated over several real
  cycles (`sim/struct_pair_tb.v`) — not just checked once.
- Audited every `_ => {}` wildcard match over `Stmt`/`Expr`/`Item`/etc.
  across `src/` (30 sites) for more Assign-vs-Let-shaped silent gaps.
  29 are legitimately safe (most route the semantically-important part
  through `lower.rs`'s `sub_exprs`/`stmt_exprs`, which are fully
  exhaustive with no wildcard at all; the rest are narrowly scoped to a
  write shape — mem/reg/port writes — that can only ever be
  `Stmt::Assign`, never `Stmt::Let`, so excluding `Let` there isn't a
  gap). One real, new, silent bug turned up: `return <expr>` at a
  rule's top level (not a `fn`) used to compile clean and silently drop
  the entire statement — nothing downstream had a case for
  `Stmt::Return` outside a callee body. Now a compile-time error in
  `effects.rs`'s `check_stmt` (a `rule` has no return value). See
  DESIGN.md's "Calling a function from a rule" section.
- **RESOLVED — an over-wide literal combined with a `Bits` value via a
  binary operator silently absorbed, uncaught.** Found directly: editing
  `examples/call_nested_writes.tr` to pass `a + 100000000` (`a : [8]`)
  as a call argument was expected to error and didn't — `type_binop`'s
  `(Ty::Bits(w), Ty::Int)`/`(Ty::Int, Ty::Bits(w))` arms just absorbed
  the literal's width from its sibling and moved on, unlike every
  assignment-shaped coercion site (a state write, a port default, a
  destructure), which already call `check_literal_fits`.
  `check_assignable`'s own comment even promised this case was "range-
  checked at coercion" — it wasn't, because a binary operand was never
  itself a coercion site the same way an assignment is. Now it is: each
  arm calls `check_literal_fits` against the correct child `ExprId`
  (whichever side is the literal), so `a + 100000000` is a clean
  `100000000 does not fit in [8]` naming the literal's own span, the
  same diagnostic an over-wide state write already gets. Applies to
  comparisons too (`a = 300` on an `[8]` `a` can never be true — same
  bug class). Deliberately EXCLUDED from THIS check: a shift's amount
  operand (`x >> 300`) — a shift count isn't a value bounded by the
  shifted operand's own width domain, so "does 300 fit in [8]" is the
  wrong question (would give a nonsensical "300 does not fit in [8]").
  Not left unchecked, though — see the next entry, a separate,
  differently-shaped check added right after this one landed. Verified
  no regression two ways: every one of the 54 shipped `examples/*.tr`
  files (read from their clean, committed content, not whatever's
  currently on disk) still compiles with zero errors, and 6 new tests
  in `tests/types.rs` cover the literal on either side of `+`, the
  comparison case, a fitting literal staying accepted, and the shift
  exclusion specifically (so a future change can't silently reintroduce
  the false positive this fix's own first draft had).
- **RESOLVED — a constant shift amount `>= w` on a `[w]` operand went
  uncaught**, following directly from the entry above: excluding shifts
  from `check_literal_fits` closed a false-positive gap but left the
  real, DIFFERENT bug (a shift amount large enough to discard the whole
  value — `x >> 300`, or a swapped-operand typo) with no check at all.
  New `check_shift_amount` (`src/types.rs`) fires when a shift's amount
  is a compile-time constant `>= w`: `Shr`/`AShr` always land on
  all-zero/all-sign there, and `Shl` shifts every original bit out past
  the top (the result STAYS `[w]` wide, never grows to `[w + amount]`),
  so this is a genuine, statically-knowable total discard, not a
  data-dependent judgment call. Wired into both the bare-literal shift
  amount case (`x >> 300`, `type_binop`'s `(Bits, Int)` arm) AND a
  SIZED-literal one (`x >> 8'd8`, a real `Ty::Bits` of its own — a
  different match arm, `(Bits, Bits)`, that a narrower fix would have
  missed). A shift amount one less than the width (`x >> 7` on `[8]`,
  the largest amount that isn't a total discard) stays accepted, and a
  genuinely dynamic (non-constant) amount is silently skipped — nothing
  for `const_eval` to evaluate, the same restriction `check_literal_
  fits` itself already has. Deliberately narrower than "any lossy
  shift": `x << 7` on `[8]` loses seven of eight bits and is NOT
  flagged — that's data-dependent partial loss, not a statically-known
  total discard, and catching it structurally would mean growing `<<`'s
  result width (like `*` already does, `Bits(a + b)`) instead of
  keeping it at the left operand's own — a real semantic change to
  shift's width rule, not a diagnostic addition, and its own separate
  design conversation if wanted later. Verified no regression against
  all 54 shipped examples (clean, committed content) and the full
  `--firrtl`/sim test suites (229 + 49/50 passing, the one failure
  being the still-open `call_nested_writes.tr` edit from the entry
  above, unrelated to this change). One new test in `tests/types.rs`
  covers 7 cases: bare literal, sized literal, exactly `w`, `<<`,
  `>>>`, `w - 1` staying accepted, and a dynamic amount staying
  unchecked.
- **RESOLVED — no way to silence the two checks above on a specific,
  intentional operator application, and `trunc` always required naming
  its width even when the target already implies one.** Two additions,
  requested together: a `.!` suffix on any binary operator (`a +.! 100000000`,
  `x >>.! 300`) marks that one `Expr::Binary` node in a new side table
  (`Ast.lossy: HashSet<ExprId>`, lexed as its own token so it beats a bare
  `.`/`..` the same way `<>` beats `<`) that `type_binop` checks before
  calling `check_literal_fits`/`check_shift_amount`, so an unmarked sibling
  expression using the same values still errors normally — a genuine
  per-application suppression, not a global toggle (verified directly:
  `if a =.! 300 { ... }` compiles clean, removing the `.!` reproduces the
  `300 does not fit in [8]` error). `.!` does NOT reach `check_assignable`;
  `x := a +.! b` still needs `trunc` if `a + b` is wider than `x`. Second,
  `trunc(value)` (1 argument, width omitted) infers its width from the
  write target instead of forcing every call site to repeat a width the
  target already states — typed `Bits(Width::Unknown)` in `types.rs`
  (bottom-up inference has no target-width visibility at the call site)
  and resolved top-down in FIRRTL emission by reusing the existing `hint`
  mechanism (`compile_expr_hinted`) that a write target's width already
  threads through emission for every other expression shape; also resolves
  through a `let`-bound local for free, since `writes.rs`'s lazy,
  read-site-hinted fallback (used whenever a local's type isn't a concrete
  `Bits(Width::Known(_))`) was already exactly the right mechanism. A
  1-argument `trunc` with no reachable hint (e.g. nested inside an
  arithmetic operand rather than a write target) is a clear compile error
  naming `trunc(value, width)` as the fix, not a fallthrough to a generic
  "can't determine width" message. Verified against all 54 shipped
  examples (clean, committed content) and the full gate
  (`cargo build`/`clippy`/`fmt --check`/`test`, all green except the
  still-open, unrelated `call_nested_writes.tr` edit from two entries
  above); new tests in `tests/lexer.rs`, `tests/parser.rs` (2, confirming
  `.!` marks the existing `Expr::Binary` id rather than creating a new AST
  shape), `tests/types.rs` (1 test, 7 sub-cases including the unmarked-
  sibling-still-errors check), and `tests/firrtl.rs` (4, one of which
  needed the compiler actually run once to confirm the correct emitted
  FIRRTL — `pad(shr(a, 300), 8)` for a constant shift amount, not the
  initially-guessed `dshr(...)`).

## Language features with no synthesis path yet

- Combinational-only (stateless) modules: `out` is register-backed by
  design (see DESIGN.md's "Module ports"), so a pure function of inputs
  can't be expressed without a cycle of delay.
- A real signed type (proper sign extension/preservation through
  arithmetic, comparisons, truncation, and casts — a second dimension
  cutting across the whole type system, not a small addition).
  Deliberately deferred in favor of the narrower `>>>` (arithmetic,
  sign-extending shift) operator, added as a per-operator choice on
  ordinary `[N]` rather than a new type. Revisit only if a real
  design needs more than a shift — signed compare/add/mul, or
  sign-aware truncation/widening.

## Verse alignment: more of its failure system and operators (design-level, not scheduled)

`fails` gating (commit `e97795e`) ported Verse's `<decides>`-requires-a-
context rule. Verse's book has more in this vein — surveyed
[08_failure](https://verselang.github.io/book/08_failure/),
[04_operators](https://verselang.github.io/book/04_operators/), and
[13_effects](https://verselang.github.io/book/13_effects/) for what else
might translate to an HDL. Triaged below; nothing here is scheduled or
committed to.

Worth building:

- `logic <expr>` boolean-success operator ACHIEVED — see DESIGN.md's
  "Builtins" section for the full write-up (semantics, the guard+write
  v0 restriction and why it mirrors Verse's own `<decides>`/`<transacts>`
  split, examples). Confirmed as a real Verse port (`02_primitives`'s
  `logic{ exp }`), not just a plausible trace-only name. **Follow-up:
  reworked from `logic(...)` call syntax into a real prefix OPERATOR
  (Lumi's call, step one of building on the comparisons-as-fallible
  decision above — `logic` needs to read a bare operand, including
  eventually a bare fallible comparison, not force parens around
  everything).** A genuine AST node now (`Expr::Logic`, ast.rs), a
  dedicated lexer keyword, parsed at the same precedence tier as `not`/
  `optional`/`spawn` — not resolved as an identifier against the
  `BUILTINS` list the way `prio`/`trunc`/`pack` still are. `logic(e)`
  still parses (parens are just grouping, absorbed by the operand
  parse), but every example/test now uses the bare form as the
  canonical spelling.
  **Second follow-up: lowered `logic`'s operand parse from `PREFIX_BP`
  to `0` (Lumi's call — "logic a>b looks neater" than `logic (a > b)`)
  — a full expression, same as a parenthesized group's inner parse, not
  the tight `not`/`optional`/`spawn` tier anymore.** `logic a > b` now
  reads as `logic (a > b)` with no parens needed, the actual point of
  asking. Traded away deliberately, not a surprise found later: this
  language's bitwise operators (`&`/`|`/`^`) bind TIGHTER than
  comparisons (Rust-style, `precedence_matches_rust_not_c`), so no
  single threshold can swallow a bare comparison without ALSO
  swallowing `&`/`|`/`^` — flagged this exact conflict before touching
  anything (`logic A & logic B`, the `and`-combination idiom just
  below, would reparse as one `logic` wrapping the whole `&`
  expression) and let Lumi pick which side keeps the parens requirement
  rather than choosing unilaterally. `logic A & logic B` now needs
  explicit parens on each side — `(logic A) & (logic B)` — to keep its
  old two-separately-discharged-values meaning; the bare form instead
  wraps the whole `&` expression in one `logic`, which `check_logic_
  args` (firrtl/checks.rs) cleanly rejects (bitwise `&` can't take a
  still-fallible left operand) rather than silently doing something
  else. The one existing example/test using this idiom (`tests/
  firrtl.rs`'s `logic_wrapped_call_is_allowed_inside_an_if_condition`)
  updated to the now-required parenthesized form.
- `or` fallback chain ACHIEVED (v0: `Deq[]`-only alternatives, depth-1
  fifos, optional infallible default tail) — see DESIGN.md's "`or`:
  fallback chains" section for the full write-up (semantics, v0
  restrictions, examples). Confirmed against Verse's own primary source
  (`08_failure`), not a paraphrase. Call alternatives, `Enq`
  alternatives, depth>1 fifos, and `or` nested in `if`/`while` or a
  callee's own body are all separate, larger gaps, not silently
  accepted. Sequential bare guards already conjoin for free, needing no
  dedicated `and` syntax for THAT — but combining two fallibles inside a
  single larger expression (an `if` condition, especially) did want one;
  see the next bullet.
  **Follow-up: `A and B` ACHIEVED (Lumi's call, "bring `and` instead of
  relying on that" — replacing the `(logic A) & (logic B)` idiom's
  required-by-hand parens).** Pure parse-time sugar, not a new AST node:
  the parser desugars `A and B [and C ...]` directly into that same
  `Binary(BitAnd, Logic, Logic)` shape, one `Logic` wrap per operand
  folded left-to-right (`parser.rs`'s new `TokenKind::And` arm, right
  after `or`'s). Every downstream pass (types/effects/firrtl checks and
  emission) needed zero changes — they already handle the hand-written
  idiom, and sugar with no AST trace of its own already has precedent
  here (`tick sync[...]`'s desugar). The one real design question was
  precedence, since copying `or`'s own mechanism (looser than every
  real operator, checked outside `infix_bp`'s table entirely) verbatim
  would have made `A or B and C` parse as one flat, ambiguous chain with
  no `and`/`or` distinction — resolved by giving `and` its own threshold
  (`2`, rhs at `3`): looser than every real operator (comparisons are
  the loosest at `(3, 4)`) but tighter than `or`'s `(0, 1)`, so `A or B
  and C` reads as `A or (B and C)` like every real language, confirmed
  by tracing the actual Pratt loop rather than assumed. Advisor-flagged
  before shipping and fixed rather than left as a footgun: (1) the
  synthesized `Logic` nodes made `check_logic_args_in`'s rejection
  message say `` `logic` needs... `` for an operand the user wrote via
  `and`, naming a keyword never typed — fixed with a new `ast.and_sugar`
  side set (mirroring `ast.lossy`) marking which `Logic` operands came
  from the desugar, so the error names `and` instead; (2) `and` does
  NOT mutate the way `or` does — `f.Deq[] and g.Deq[]` reads both fifos'
  occupancy and dequeues neither (`logic`'s existing pure-test emission,
  inherited as-is) — documented directly in DESIGN.md's `and` section
  rather than left as a silent asymmetry between two sibling-looking
  operators. See DESIGN.md's "`and`: boolean combination sugar".
- **Comparisons returning their left operand in a failure context ACHIEVED**
  (Verse: `X > 0` yields `X` on success, fails otherwise, `04_operators`)
  — see DESIGN.md's "Comparisons: fallible by default" for the full
  write-up (semantics, examples). Built as a fourth member of trace's
  existing closed family of fallible expressions (fifo `Deq[]`, `opt?`,
  a failing call) rather than a type-system rework: `type_binop`'s
  comparison arm (types.rs) now returns the LEFT operand's own type
  instead of a flat `Ty::Bits(Width::Known(1))` — no new `Ty` sentinel,
  fallibility is tracked entirely by AST shape, mirroring how a fifo
  `Deq[]`'s own type is just its element type. `logic`'s existing
  two-way allowlist (`check_logic_args_in`, firrtl/checks.rs) took a
  third arm for a comparison exactly as predicted — simpler than the
  fifo-op/call cases, since a comparison has no side effect to guard
  against silently discarding.

  **The predicted if-guard dependency turned out to be avoidable.**
  This bullet used to say comparisons-as-fallible couldn't ship ahead
  of the (still-unbuilt) if-guard branch-scoped-fallibility design,
  since every `if`/`while` conditioned on a bare comparison would
  break with no discharge path. That was true, but the FIX didn't
  need branch-scoping at all: `logic <expr>` (already built, and by
  this point already reworked into a prefix operator with a
  deliberately loose precedence — see the `logic` bullet above) is a
  complete, working discharge path for `if`/`while` on its own.
  `if logic a > b { ... }` reads cleanly BECAUSE of that precedence
  change, which is why Lumi asked for it right before this decision,
  not a coincidence. If-guard's branch-scoped `if`-with-`else` design
  remained a separate, larger, still-unbuilt feature at the time —
  it would let a BARE comparison (no `logic`) sit directly in an
  `if`-with-`else` condition; that was a real ergonomic gap this
  migration left open, not something this work quietly solved.
  **Update: built in a later session — see "`if`: branch-scoped
  fallible conditions ACHIEVED" below.**

  **Migration measured and completed, not just predicted.** 7 examples
  and the ~50 test snippets both bullets estimated needed `logic`-
  wrapping their `if`/`while` conditions; all migrated, full suite
  green, firtool + Icarus reconfirmed on every touched example.
  `adder_tree.tr` — DESIGN.md's own `<elaborates>` showcase — was the
  one real readability cost: `if logic len(xs) = 1 { ... }`, not the
  original bare `if len(xs) = 1 { ... }`, because Lumi chose uniform
  application over carving `<elaborates>` bodies out (see below), and
  `logic` was the only working discharge path available even there.
  **Update: reverted to the bare form once the if-guard feature below
  shipped — `<elaborates>` code needed no changes of its own to accept
  it (elaborate.rs's interpreter already evaluated a bare comparison
  correctly; only `check_cond`'s `[1]`-requirement, shared by every
  `if`, was ever in the way), so this readability cost is fully closed,
  not just reduced.**

  **`<elaborates>` decision: uniform, not carved out (Lumi's call,
  against the recommendation).** Offered a carve-out (comparisons stay
  plain `[1]` inside `<elaborates>` bodies, zero migration cost there)
  versus uniform application (needs `elaborate.rs` to ALSO gain a
  discharge mechanism, a second feature). Lumi picked uniform. Built:
  `elaborate.rs`'s own interpreter never consulted `Ty` for a
  comparison anyway (`eval_elab_int_binop`'s `Eq`/`Ne`/`Lt`/... arms
  already folded straight to `Int(0)`/`Int(1)`, confirmed by reading
  before assuming), so the ONLY gap was `Expr::Logic` itself, which
  used to fall into the same "no meaning in `<elaborates>` code"
  bucket as `Guard`/`Spawn`/`Optional`. Fixed with one arm:
  `Expr::Logic(inner) => self.eval_elab_expr(inner, ...)` — discharge
  is a pure no-op at elaboration time, since every value there is
  already a resolved compile-time constant with nothing to gate.  A
  BARE (undischarged) comparison inside `<elaborates>` is still
  correctly an error, unchanged — it's exactly the SAME "implicit
  guard, cannot fail at elaboration time" mechanism a bare fifo op/
  `Guard` already had (`is_guard_like`'s catch-all already covered a
  comparison before this feature existed at all, so this needed no new
  code, just confirming the existing tests still pass unchanged —
  they did:
  `guards_forbidden_at_elaboration_time`/`implicit_guards_forbidden_
  at_elaboration_time_too`, tests/effects.rs).

  **Nesting: no dedicated position restriction shipped — the original
  recommendation (below) turned out to be based on the wrong
  precedent.** Advisor caught this before implementation: a fifo op/
  failing call's "whole statement only" restriction exists because a
  MISPLACED one would be SILENTLY missed by `compile_guard`'s fold
  (which only scans specific top-level shapes) — wrong hardware, not
  just a worse error. A comparison has no side effect, so instead of
  restricting WHERE it can appear, `compile_guard`'s fold was made to
  genuinely SEARCH for one anywhere within a statement's expression
  tree (`comparison_conds`, firrtl/writes.rs, recursing via
  `lower::sub_exprs` the same way `logic_arg_exprs`'s own comparison-
  finding does) — `x := a + (a > b)` folds `a > b`'s condition into
  the guard correctly, not just `x := a > b` alone. This was
  UNDER-BUILT on the first pass and self-caught by direct probe before
  it shipped: the naive version (checking only whether a statement's
  own top-level RHS/init directly WAS a comparison) compiled `x := a +
  (a > b)` clean with `fires_r = UInt<1>(1)`, never gating on `a > b`
  at all despite effects.rs's `sig.fails` already correctly being
  `true` for it — silently wrong whenever `a > b` didn't hold, pinned
  by `a_comparison_nested_inside_a_larger_value_still_folds_into_the_
  guard` (tests/firrtl.rs). One restriction DOES still apply, for a
  different reason than "silent miss": a comparison nested inside an
  `if`/`while`'s own BODY (not its condition) is rejected outright,
  matching the EXISTING guard/fifo-op/failing-call restriction there —
  folding it into the whole rule's guard would be wrong when the
  branch might not even be taken, confirmed by the identical class of
  self-caught bug one level deeper
  (`a_comparison_nested_in_if_is_an_error_not_a_dropped_guard`).

  **All six operators shipped, per the original recommendation.**
  `BinOp::is_comparison()` (ast.rs) is the single shared predicate
  every site (types.rs/effects.rs/firrtl) keys off of, matching
  `resolve::is_guard_like`'s own "one predicate, not independent
  re-derivations" rationale.

  **A third self-caught bug, one level deeper still: `logic` itself
  wasn't discharging.** `comparison_conds`'s recursive search (added
  for the bug above) and `contains_comparison`'s if/while-nesting walk
  both recurse via `lower::sub_exprs`, which already includes
  `Expr::Logic(inner) => vec![inner]` (added when `logic` became a
  prefix operator) — so both walkers happily descended straight
  through a `logic` wrapper into the comparison it was supposed to be
  discharging. `ok := logic a > b` compiled clean but with `fires_r =
  gt(a, b)` instead of the correct `UInt<1>(1)`: `logic` was
  contributing NO discharge at all, just silently re-adding the same
  guard term compile_logic's own boolean already represents —
  advisor-caught (not self-caught this time) before commit, by probing
  the exact snippet the "silent-miss risk" reasoning above should have
  been re-checked against once `comparison_conds` existed but wasn't.
  Fixed both walkers to stop specifically at `Expr::Logic(inner)`
  where `inner` IS the comparison itself, not at every `Expr::Logic`
  — a `logic`-wrapped COMPARISON is fully discharged by `logic`, but a
  `logic`-wrapped CALL only discharges the call's own fail cond, not
  an independent comparison nested in its arguments. The first pass at
  this fix stopped at every `Expr::Logic` unconditionally and was
  itself caught by a second advisor probe before commit: `logic
  Check(a > b)` compiled clean with `fires_r = UInt<1>(1)`, silently
  dropping `a > b`'s own guard entirely. Pinned by
  `logic_of_a_comparison_reads_its_ordinary_boolean_value`'s new
  `fires_r = UInt<1>(1)` assertion, plus three new tests
  (tests/firrtl.rs): `logic_wrapped_comparison_inside_an_if_body_is_
  allowed_and_discharged`, `two_logic_wrapped_comparisons_combine_
  with_amp`, and `logic_wrapped_call_with_a_comparison_argument_
  still_gates_on_it` (the call-argument case specifically).

  **A fourth advisor-caught bug, at the fn-boundary layer this time:**
  `effects.rs`'s `infer_expr` had the identical isolation mistake as the
  firrtl walkers above, one layer up the pipeline. `logic <call>`'s
  effect-inference isolated the WHOLE wrapped call (to capture the
  callee's own `reads`), discarding `fails`/`writes` entirely — correct
  for the callee's OWN fail condition (that IS what `logic` discharges),
  but wrong for an independent comparison inside the call's ARGUMENTS,
  which is an ordinary caller-side expression `logic` never touches.
  `Wrap(a, b) : [8] <combines> { return logic Check(a > b) }` used to
  type-check as `<combines>` (no `<fails>` needed) even though `a > b`
  is a live, undischarged failure — exactly the inconsistency `e97795e`
  ("Require `<fails>` to be declared wherever it's computed true")
  exists to catch, silently bypassed. Fixed by handling the call shape
  like an ordinary `Expr::Call` (callee's `reads` merged, args inferred
  straight into the enclosing sig) while specifically omitting the
  callee's own `fails`/`writes` — only THAT part is what `logic`
  discharges. Pinned by
  `a_comparison_inside_a_logic_wrapped_calls_argument_still_needs_
  fails_declared` (tests/effects.rs).

  **RESOLVED in a follow-up session — was mischaracterized as
  diagnostic-only; it was actually a real false-positive bug.**
  `check_guard_placement`'s `Stmt::Assign`/`Stmt::Let` arms used to test
  `rhs_is_comparison` shallowly (top-level shape only), unlike
  `comparison_conds`'s (writes.rs) recursive fold — so `x := a + (a >
  b)` after a state write did NOT get the "comparison after a state
  write" error a bare `x := a > b` would, which is the diagnostic-only
  framing this bullet originally used. But the SAME shallow check also
  fed `contributes_guard` (the flag deciding whether THIS write itself
  already gates on its own embedded comparison, and so should NOT close
  the guard window) — so `x := a + (a > b)` wrongly left `contributes_
  guard` false, `seen_write` got set true, and a LATER, perfectly legal
  top-level comparison in the same rule (`y := c > d`) then got a
  false-positive "comparison after a state write" rejection, even
  though `compile_guard` itself already correctly gates both writes
  together (`and(gt(a, b), gt(c, d))`). Confirmed live: isolating the
  first statement alone showed it was already correctly gated
  (`fires_r = gt(a, b)`) before this fix, proving the SECOND
  statement's rejection was the bug, not a missing diagnostic on the
  first. Fixed by extracting `contains_comparison`'s (checks.rs) private
  nested `expr_has_comparison` helper to module scope and reusing it at
  both `check_guard_placement` call sites (the `rhs_is_comparison`/
  `init`-comparison checks), so the SAME recursive predicate now backs
  the diagnostic, the `contributes_guard` decision, and the if/while-
  nesting check uniformly. Pinned by `a_write_whose_rhs_embeds_a_
  comparison_does_not_falsely_close_the_guard_window` (the false-
  positive fix) and `a_comparison_embedded_in_a_larger_value_after_an_
  unrelated_write_is_still_rejected` (confirming the genuinely-bad shape
  is still caught, tests/firrtl.rs).

  **RESOLVED in a follow-up session.** `effects.rs`'s `Expr::Logic`
  rewrite branched on whether the operand was a `Call`; the firrtl
  walkers branched on whether the operand IS the comparison. Those
  agreed for the comparison and Call shapes, but the third legal
  `logic` operand shape — a fifo op — wasn't specially handled in
  effects.rs at all, still fully isolated (that part was, and remains,
  existing intentional behavior: `logic f.Deq[]`/`logic f.Enq[x]` is a
  pure occupancy TEST, `compile_logic` never emits the actual mutation,
  so suppressing `writes` there is correct, not a gap, and this fix
  doesn't touch it). The narrow edge that WAS a real gap: `logic
  f.Enq[a > b]`'s `a > b` argument is discarded by `compile_logic`
  exactly like the rest of `Enq`'s data argument (never emitted at
  all), but its `fails` used to be silently isolated away too —
  reachable only when this whole pattern sits inside a fn boundary
  (`Wrap(...) : [1] <combines> { return logic f.Enq[a > b] }` used to
  be wrongly accepted without `<fails>`, confirmed live before fixing);
  a bare rule-body use already folded `a > b` correctly into the guard
  via the (correctly-fixed) firrtl walkers, so this was specifically an
  effects.rs-only gap. Fixed by giving `Expr::Logic`'s `Expr::Bracket`
  fifo-op shape the identical special-casing its `Expr::Call` sibling
  already had: the fifo's own occupancy resource is still read (`sig.
  reads.insert(fifo)`, matching the plain non-`logic` fifo-op arm), but
  the ARGUMENT is inferred straight into the enclosing `sig` rather
  than isolated, so an independent embedded comparison's `fails`
  propagates correctly — the fifo op's OWN `fails`/`writes` stay
  excluded, unaffected, since that's still exactly what `logic`
  discharges. Pinned by `a_comparison_inside_a_logic_wrapped_fifo_ops_
  argument_still_needs_fails_declared` (tests/effects.rs), which also
  confirms the plain-argument and no-argument (`Deq[]`) shapes are
  unaffected — still no `<fails>` required for either.
- trace's `not` is confirmed to be a plain `[1]` boolean operator
  (`types.rs`'s operand-must-already-be-`[1]` rule), not Verse's
  "test success/failure without committing" operator — `17f21e9` was a
  respelling, not a semantics port, and that's fine as-is: the two
  positions where Verse's discharge semantics would matter (`not`
  wrapping a failing call or a bare `expr?` guard) are already
  hard-rejected with a clean compile error. A fifo op (`Enq`/`Deq`) is
  only recognized in the exact structural positions `fifo_op_stmt`
  matches (a bare statement, the whole RHS of `:=`, a `let` init) —
  anywhere else (arithmetic, an `if`/`while` condition, wrapped in
  `not`, a call argument) it's now a clean "not yet supported" error
  (`check_fifo_op_positions`/`collect_fifo_ops`, mirroring calls' own
  `check_failing_call_positions`/`collect_calls`) instead of the silent
  miscompile it used to be (no occupancy guard, no state transition, the
  fifo's raw register read as if valid). Rule bodies only — a callee's
  own body already independently rejects any shape this permissive
  (`check_fails_is_foldable_guard`), confirmed separately.
- Cross-tick fails/rollback safety in `sequences` AUDITED, no gap found:
  a guard/fifo-op/failing-call after a `tick` gets the ordinary
  per-rule guard-placement treatment, not some special-cased or missing
  check — `sequences` lowering splits each segment into its OWN `rule
  {name}_s{N}` (`render_rule`, lower.rs), gated by `(cont = N)?`, so
  post-tick code is simply a fresh rule's own top-level statement by
  the time `check_guard_placement`/`compile_guard` ever see it. No
  checkpoint/squash machinery needed; confirmed against real firtool,
  not just reasoned through (`a_fifo_op_after_a_tick_gets_the_ordinary_
  per_segment_guard_fold`, tests/firrtl.rs). The audit's OWN probing
  did surface a real, GENERAL (not tick-specific) gap along the way:
  `check_guard_placement`'s `Stmt::Assign` arm chained the fifo-op/
  failing-call checks behind an `else if is_state_write(lhs)`, so "fifo
  op after a write" was only ever caught when the op's own lhs was a
  plain local, never when the lhs was ALSO state (`r0 := f.Deq[]`, an
  entirely ordinary pattern) — reproduced with no `sequences` involved
  at all. Not a wrong-hardware bug (`compile_guard` already folds
  regardless of position), a validation-completeness one, same else-if-
  behind-`is_state_write` class the `or` guard-placement check was
  fixed for earlier this session. Fixed by checking independently and
  only closing the guard window on a PLAIN write (`contributes_guard`);
  a first version of the fix set the flag unconditionally and broke a
  real pattern (two independent fifo-op-driven writes) — advisor-
  caught before commit, both directions now pinned by tests.
  Message-quality note, not a gap: a `<sequences>` rule that spawns
  something but has no top-level `tick`/`sync` of its own (`rule go
  <sequences> { h := spawn Body() }`) is never touched by `lower::
  plan` (skips any `<sequences>` rule with no top-level tick) and
  correctly errors — "still a `<sequences>` rule with `tick`; run
  sequences lowering first" — but that message is misleading here since
  lowering DID run, it just had nothing to do; could be sharpened to
  name the real issue (a spawning rule needs its own tick/sync to
  observe the result) if this trips someone up in practice.

Speculative, bigger, not committed to:

- **Option type core (`?T`) — ACHIEVED.** `?T` sugar over a compiler-
  synthesized `{valid: bit, data: T}` struct, reusing struct's own
  flattening/read/write machinery end to end (see DESIGN.md's "Option
  types"/"Option emission" sections, `examples/option.tr`). Verse's own
  answer settled the open design question this bullet used to raise:
  absence is the literal `false` (tying into Verse's logic-programming
  failure model), not a `none` keyword; unwrap reuses the *existing* `?`
  guard operator, generalized to fold a `?T` value's absence into the
  rule's guard the same way a fifo `Deq[]` already does (including
  through a `let` init, not just a bare statement/`:=` RHS, and from
  inside a callee body's own `callee_fail_cond` fold too); `.valid`/
  `.data` stay directly readable for a non-failing presence check, but
  don't compose with `?` in one chain (`opt?.valid` is rejected, same
  as any other nested-guard position). `T` may itself be a struct
  (`reg o : ?Pair`) or a struct's own field, both directions confirmed
  through real firtool + one full Icarus simulation
  (`examples/option.tr`'s `relayed` output exercises a `?T`-typed
  OUTPUT port's own separate write-threading bookkeeping directly, not
  just the reg case). What's left, not attempted this pass:
  - **Struct- and `?T`-typed fn/rule PARAMS — ACHIEVED.** Scoped via
    AskUserQuestion (params-only first, Lumi's pick — returns followed
    in a same-day follow-up, see below). An argument may be a struct
    literal/`false`/a
    plain value of `T`, or a reg/output/input/another same-typed
    param, resolved by CHASING through the alias to that value's own
    flat fields (`compile_struct_field_read`'s param-only chase-
    through, expr.rs — see DESIGN.md's "Calling a function from a
    rule"/"Calling a function: inlining" sections,
    `examples/call_struct_param.tr`) — not just the literal-argument
    case, which is what "lifting the type-check gate alone" would have
    given. Chains through nested calls correctly (`Outer(p) {
    return Inner(p) }` resolves back to the original reg through TWO
    levels of param binding). Building this surfaced a real pre-
    existing miscompile, found and fixed BEFORE writing any of the
    params machinery: `let o = opt` (`opt` itself `?T`, no call
    involved at all) typed fine via ordinary inference and reached
    `compile_field_path_value`'s `Ty::Option` arm with its "not itself
    Option-typed" invariant already broken (that invariant is normally
    enforced by `type_write`'s Option-to-Option rejection, which only
    runs for a state WRITE with a known target type — a plain `let`
    has none), silently hardcoding `o.valid`/`o.data` to wrong
    constants instead of reading `opt`'s actual register. Fixed by
    rejecting the alias at the arm itself (matching struct's own
    existing local-aliasing restriction) — a separate commit, before
    params, per advisor's explicit "verify these two before writing
    the test" pre-commit review. A plain rule-level `let` merely
    re-binding a param/aliasing another `?T` value STILL rejects
    (deliberately narrower than "params work" — general alias
    resolution for `let` wasn't asked for and wasn't built).
  - **Struct- and `?T`-typed RETURNS — ACHIEVED.** Same-day follow-up
    to params, per Lumi's "continue the optional stuff." A new
    field-path-aware sibling, `compile_callee_body_field` (calls.rs),
    extracts ONE leaf field's value per call, re-walking/re-binding the
    callee's body independently per leaf (the exact `Avg(Avg(x, y), z)`
    reentrancy discipline params already established, now on the
    return side); `compile_field_path_value` (writes.rs) gained a new
    `Expr::Call` case dispatching into it — see DESIGN.md's "Calling a
    function from a rule"/"Calling a function: inlining" sections,
    `examples/call_struct_return.tr`. Supports a fresh literal return,
    chaining through a nested struct/Option-returning call, and a
    callee returning one of its OWN params unchanged (`Passthrough(p) {
    return p }`) — but NOT a callee-local that merely re-binds a param
    and returns THAT (`let x = p; return x`), the identical restriction
    the param side already has one level in. An advisor-recommended
    probe (compile with the type-check gate disabled, see WHERE it
    breaks before designing further) surfaced a second self-caught
    silent-drop bug of the SAME shape the `?T`-aliasing bug earlier
    this session had: with no `Expr::Call` case, `struct_field_value_
    in_stmts` treated a matching-but-unresolvable `Assign` as "doesn't
    write this field" rather than an error — fixed by having the new
    dispatch always emit a real error on genuine failure. Getting the
    param-passthrough case right took two more self-caught fixes before
    it shipped: a first attempt put the `Expr::Ident` chase-through
    generically inside `compile_field_path_value`, which is ALSO
    reached from `compile_struct_field_read`'s pre-existing Local-arm
    fallback — silently relegalizing a callee-local aliasing a param,
    caught by an EXISTING regression test failing, not by inspection;
    moved instead into `compile_callee_body_field`'s own `Return` arm,
    checked directly against `ret_expr` only. That version then needed
    its own exact-type-match gate (mirroring the param side's) to avoid
    misfiring on an ordinary `T`-into-`?T` present-coercion return — a
    dedicated guard-fold-intersection probe (per advisor's flagged
    priority) caught this one before it shipped, not after.
  - **`??T` (nested Option), independent layers — ACHIEVED.** The
    concrete use case this bullet used to wait on turned out to be
    Lumi asking for it directly: `optional <expr>` (below) is exactly
    the "construction syntax that doesn't exist yet" this bullet named
    as the blocker. `Some(None)` (outer present, inner absent) is now
    expressible and simulated (`examples/option.tr`'s `nested` reg,
    `sim/option_tb.v`) — see the `optional` bullet in "Emission" above
    for the full write-up. `nested_option_reaches_only_fully_absent_or_
    fully_present` (tests/firrtl.rs) still correctly pins that BARE
    coercion alone (no `optional`) only ever reaches the two
    fully-agreeing states — that restriction didn't change, `optional`
    is a strictly new capability layered on top of it, not a
    loosening.
  - **`option{...}` explicit-construction syntax — partially covered by
    `optional`, not fully.** Implicit coercion still handles ordinary
    construction (`opt := value`/`opt := false`) with no wrapper syntax
    needed; `optional <expr>` (above) additionally covers the ONE case
    coercion can't — forcing a specific `?` layer present independently
    of the value beneath it. Neither is a general struct-literal-style
    `option{ valid: ..., data: ... }` constructor; not pursued, no
    concrete use case for one beyond what `optional` already closes.
    `?.` safe navigation: now designed against Verse's actual primary
    source (`08_failure`, fetched directly, not paraphrased) rather
    than a guess — see its own bullet below, split out of this one
    once `if let` made clear the two are related but separately
    scoped features, not one.
- **`if`: branch-scoped fallible conditions ACHIEVED** — a BARE
  comparison (no `logic`) directly as an `if`'s own condition, Verse-
  faithful branch-scoping applied uniformly to the with-else and
  no-else shapes alike (Lumi's call via `AskUserQuestion`, resolving
  the one open question below that had no argued default: failure
  skips only the `then` branch and the REST of the rule still commits,
  even with no `else` — a real behavior change from today's top-level
  bare comparison/`(cond)?`, which would abort the whole cycle instead).
  See DESIGN.md's "`if`: branch-scoped fallible conditions" for the
  full write-up (semantics, examples, why the value side needed almost
  no new code). `while`'s own condition stays out of scope, unaffected
  — resolves the "while with a fallible condition" open question below
  by taking its already-argued default. **UPDATE, much later: this call
  got REVERSED — see the four open questions' own "while with a
  fallible condition" entry below for why (Verse turned out to have no
  native `while` at all) and DESIGN.md's "`while`: multi-cycle loops"
  for the shipped feature.** A fifo op or failing call as an
  `if`'s condition remains a type error, same as ever — this ships only
  the comparison case; the harder branch-scoped-fifo/failing-call
  question the open-questions list below still owes stays fully open,
  not narrowed by this work.

  **Two more pre-existing silent-miscompile bugs, found the same way
  the comparisons-as-fallible bugs were (direct probing before
  considering this done), neither specific to `if`.** A comparison's
  own TYPE is its left operand's type (`type_binop`), so whenever that
  operand is exactly 1 bit wide, combining it with `&`/`|`/`^` produces
  an entirely ordinary `[1]` type — indistinguishable from a genuine
  boolean by width alone, and reachable at `a01683f` already, before
  this feature touched anything. `if (a > b) & c` (`a`/`b`/`c` all
  1-bit) compiled clean to `mux(and(a, c), ...)`, silently using `a`'s
  own passthrough value instead of `gt(a, b)`; the identical shape hit
  a bare rule-body guard STATEMENT too (`((a > b) & c)?` folding to
  `fires_r = and(a, c)`, dropping `a > b`'s guard entirely); and
  `while x <> 0` with a 1-bit `x` slipped straight past `check_cond`'s
  width check, silently accepting a bare comparison `while` was never
  supposed to allow. `compile_guard_unwrap_cond` (writes.rs) itself is
  UNCHANGED — still exactly as shallow as before, checking only whether
  a condition's own IMMEDIATE shape is a comparison — fixed instead at
  the one gate all three shapes must pass through first: `check_cond`
  (types.rs)'s new `expr_has_undischarged_comparison` walks a
  condition's full subexpression tree (`sub_exprs`, lower.rs) and
  rejects outright, before emission ever runs, when an undischarged
  comparison is reachable anywhere that isn't either the whole
  condition (this feature's new `if`-only exemption) or already
  `logic`-wrapped — closing all three call sites (`if`, `while`, and
  the bare-statement guard fold) at once, since all three route through
  `check_cond` before anything reaches `compile_guard_unwrap_cond`. A
  future guard-fold call site that reached `compile_guard_unwrap_cond`
  WITHOUT going through `check_cond` first would reintroduce this exact
  gap with no test catching it.

  **Verified the way this session's other features were: direct `.tr`
  probes through `--firrtl` for every distinct write-threading walk
  before considering this done**, not just the register case — a
  memory write's explicit write-enable (`mem_write_in_stmts`), a
  submodule instance port (`inst_port_value_in_stmts`), a callee
  writing a caller's register as a bare statement
  (`callee_reg_write` — a different code path from a callee's own
  RETURN value), and a reassigned local referenced inside an
  if-condition comparison (resolves at its own textual position,
  `set_pos`/`enter_rule`, not a later reassignment) — all confirmed
  correct and pinned with regression tests (tests/firrtl.rs), alongside
  the type-level rejections (tests/types.rs) and the fn-boundary
  `<fails>`-discharge behavior (tests/effects.rs). `examples/
  adder_tree.tr` reverted to its original bare `if len(xs) = 1 { ... }`
  now that `logic` is no longer needed there either (elaborate.rs's
  interpreter already evaluated the bare form correctly all along —
  only `check_cond` was ever in the way), closing the one readability
  cost the earlier comparisons-as-fallible migration left behind.

- **`if let`: branch-scoped Option-presence binding ACHIEVED** — Verse's
  general failure-context binding form (`if (X := Expr, Y > 0):`,
  `08_failure`), narrowed to the Option-only slice: `if let NAME = opt?
  { then_body } [else { else_body }]` binds `NAME` to `opt`'s own
  unwrapped value, visible ONLY within `then_body`, branch-scoped and
  discharged exactly like the bare-comparison `if` feature above
  (uniform with-else/no-else semantics, no rule-level guard). See
  DESIGN.md's "`if let`: branch-scoped Option-presence binding" for the
  full write-up (semantics, v0 scope, the emission machinery reused vs.
  the one genuinely new piece — resolving `NAME` to `opt.data`).
  Scoped via `AskUserQuestion` (Lumi's call): binding sugar only, no
  general multi-clause comma-chain the way Verse's own form has (not
  needed — a bare `opt?` already reads cleanly as the whole right-hand
  side), and `?.` safe navigation (Verse's own multi-hop chained
  unwrap-and-field-access, `opt?.next?.value`) deferred entirely as a
  separate, larger feature — see its own bullet below. `NAME` may be
  used as a whole value inside `then_body` but not chased through a
  further `.field` access (struct-typed `T`) — cleanly rejected with
  the same message an ordinary `let p = opt?; p.field` already gets,
  not a gap this feature opens. `if let` inside a `<sequences>`/spawn-
  callee body (crossing a `tick`) isn't supported — a `tick` nested
  inside `if let`'s own body is cleanly rejected ("must be at the top
  level, not nested in if/while", the same message `if`/`while` get)
  since the `while`-lowering pass below fixed `find_nested_tick`/`find_
  tick_anywhere` (and three siblings) to recognize `Stmt::IfLet`, which
  they'd been silently missing since this bullet shipped — before that
  fix the SAME source hit a confusing "a spawned fn's last segment must
  end with `return`" instead. Still a known v0 gap (a captured local
  crossing a `tick` from inside `if let`'s body isn't built), just with
  the right error now.

  **Three more non-exhaustive-match gaps self-caught the same way the
  previous `if` feature's bugs were — direct probing before considering
  this done, not code review.** None of these are compile errors (Rust's
  exhaustiveness check only fires on an EXHAUSTIVE match; each of these
  three used a `_ => None`/`_ => {}` fallback instead, so adding `Stmt::
  IfLet` to the AST silently compiled clean without them): `find_mem_
  write` (writes.rs) — a memory write buried inside an `if let` was
  invisible to the "does this rule write this mem at all" gate, so the
  mem never got a writer port declared at all, silently dropping the
  write (guard, address, data, all of it) rather than miscompiling one
  piece of it; `collect_read_sites` (module.rs) — the identical gap for
  a mem READ address referenced inside an `if let`; `stmt_contains`
  (writes.rs, used by `set_pos`) — a REASSIGNED local referenced inside
  an `if let`'s own body would have silently resolved to the rule's
  FINAL locals snapshot instead of the position-correct one, the exact
  reassigned-locals miscompile class `examples/reassigned_local.tr`
  exists to guard against. All three fixed and pinned by regression
  tests (tests/firrtl.rs) before this was considered done.

  **What's STILL open, unchanged by this bullet:** the harder half of
  the original design — a fifo op or failing call as a branch-scoped
  `if`'s condition (not just an Option's presence) — remains fully
  unbuilt; see the four open questions below, still standing except
  where noted resolved for the comparison/Option cases specifically.

  **UPDATE — the fifo-Deq slice of this harder half is now ACHIEVED,
  as `if let` (not bare `if`).** `if let x = fifo.Deq[] { ... }` — bare,
  never `?`-wrapped, `Deq` only (no `Enq`, no value to bind), a rule's
  own top level only. Turned out to need almost no new machinery:
  `checks.rs`'s fifo-op position checks already had `Stmt::IfLet`/
  `WhileLet` exemptions in place (dead code until this landed), and the
  emission side reuses the `or`-chain `select` mechanism verbatim
  (`RuleFifoOp::select`, `compile_guard`'s existing "skip ops with
  `select.is_some()`" fold, `module.rs`'s existing `.select`-branching
  Deq emission). See DESIGN.md's new "`if let`: a fifo op's presence"
  section for the full implementation write-up, `examples/
  if_let_fifo.tr` + `sim/if_let_fifo_tb.v` for a real firtool+Icarus+
  Verilator-proven example. A failing call as `if let`'s rhs is STILL
  explicitly out of scope (`TypeChecker` has no `fx` access) — see that
  same DESIGN.md section for why. Bare `if` (not `if let`) with a fifo
  op/failing call as its OWN condition — the ORIGINAL literal ask this
  whole sub-item names — remains unbuilt; the four open questions below
  were written against that shape specifically and stay open for it,
  though `if let`'s new fifo-Deq answers may now inform them (in
  particular, the dequeue-enable question below turned out to be
  exactly `select`, not a new mechanism).

  **UPDATE 2 — the failing-call slice is now ACHIEVED too, closing the
  "STILL explicitly out of scope" gap the previous update named.**
  `if let x = Classify(a) { ... }` — bare, never `?`-wrapped, same
  reasoning as `Deq[]`. Needed `TypeChecker` to gain `fx` access
  (`types::check`'s signature grew a third `&Effects` parameter,
  threaded through every call site) plus two small, deliberately
  narrow `checks.rs` changes (a `contains_failing_call` init-exemption
  mirroring the fifo case's pre-existing one, and a NEW `allow_if_let`
  parameter kept separate from `allow_let` so `check_writing_call_
  positions_in` doesn't also loosen). A writing-and-failing callee used
  as `if let`'s init is still cleanly rejected — by that same untouched
  write-position check, not new logic. Self-caught a real sign bug
  while verifying against real FIRRTL (not just adding code and
  trusting it): `calls.rs`'s `callee_fail_cond`, despite its name,
  already returns the callee's SUCCESS condition, not its fail
  condition — an early version wrapped it in `not(...)`, silently
  inverting the mux, caught by reading the actual emitted `mux(...)`
  text before writing any test. See DESIGN.md's new "`if let`: a
  failing call's own presence" section, `examples/if_let_failing_call.tr`
  + `sim/if_let_failing_call_tb.v` for a real firtool+Icarus+Verilator-
  proven example. Bare `if` (not `if let`) with a fifo op/failing call
  as its OWN condition remains the one piece of the original ask still
  unbuilt — see the four open questions below, unchanged by this update.

- **`while`: multi-cycle loops ACHIEVED — found to be a bigger
  prerequisite gap than asked for, then built as its own unit.** Lumi
  asked for `while let` (the loop-shaped sibling of `if let`, "scoped
  the same way"); direct probing through the real `emit_from_source`
  harness found `while` itself had NO working FIRRTL emission path at
  all — `firrtl::emit` unconditionally rejects any `<sequences>`-tagged
  rule (module.rs), and `lower::plan` only lowered a rule with a
  top-level `tick` (a `while`-only rule has none, `tick` can never nest
  inside `while` either, so there was no way to reach a working state).
  `while` type-checked and effect-checked (the `logic`-discharge
  restriction above, "one iteration per cycle") but was never wired to
  synthesize the loop itself. Scoped via `AskUserQuestion`: build
  `while`'s own lowering first, `while let` after. See DESIGN.md's
  "`while`: multi-cycle loops" (feature-level) and "`while` lowering"
  (implementation) sections for the full write-up.

  A top-level `while COND { body }` cuts its own self-looping segment
  the same way `tick` cuts a straight-line one, rendered as `if COND {
  <body>; cont := SELF } else { cont := NEXT }` — needing **zero**
  `firrtl.rs` changes, since the existing `Stmt::If` write-threading
  already produces exactly this mux for any reg/mem/instance-port/
  callee write. The entire new surface is in `lower.rs`: segment-
  cutting (`Segment::while_cond`), `find_nested_while` (top-level-only,
  mirroring `find_nested_tick`/`find_nested_spawn`), and `plan()`'s own
  gate widened from "top-level `Tick` present" to "`Tick` OR `While`
  ANYWHERE" (self-caught: the narrower version let a nested `while`
  skip `find_nested_while`'s own rejection entirely, silently passing
  through unchecked).

  **A `while` loop may only write module state directly, not a local**
  — an accumulator, or any local surviving past the loop, is rejected
  with a message naming the real cause instead of the generic write-
  once capture text. This is the one deliberately deferred half:
  `compute_captures`'s write-once/read-only-in-later-segments invariant
  would need teaching to reason about loop-carried dataflow (a same-
  segment self-referential read is semantically sound for an
  accumulator — a register genuinely sees last cycle's value — but
  indistinguishable from the read-before-write hazard those checks
  exist to catch, without new machinery this pass doesn't attempt). A
  local computed BEFORE the loop and only READ inside it (never
  reassigned there) already works today, unaffected — confirmed by
  direct probe plus a firtool-checked regression test.

  **Six pre-existing recursive scans in lower.rs were self-caught
  missing a `Stmt::IfLet` arm** while extending this exact function
  family for `find_nested_while` — `find_returns`, `find_nested_tick`/
  `find_tick_anywhere`, `find_nested_spawn`/`find_spawn_anywhere`,
  `find_unsupported_construct` all matched only `Stmt::If`/`Stmt::
  While`, a gap dating to when `if let` first shipped (see that bullet
  above, now corrected). Confirmed live by direct probe: a `tick`
  nested inside `if let`'s body used to surface as "a spawned fn's last
  segment must end with `return`" instead of the clear "not nested in
  if/while" every other nested-tick shape gets. Fixed all six.

  Proven through the full pipeline, not just structural FIRRTL: examples/
  while_countdown.tr + sim/while_countdown_tb.v drives a real Icarus
  simulation across many real clock cycles (firtool + iverilog), holding
  `x` at 5, 0, then 12 in turn and checking the loop's own iteration
  count settles exactly right each time — `x=0` pins the zero-iteration
  edge case (the loop's own condition already false the first time it's
  checked). Also self-caught along the way: `emit_from_source` (tests/
  firrtl.rs's own full-pipeline test harness) only asserted `lower::
  plan`'s errors were empty INSIDE the branch where something had
  already lowered successfully — a rule whose ONLY lowering candidate
  failed outright (`lowered` empty, `lower_errors` not) silently fell
  through to re-running the ORIGINAL, unlowered source instead of
  surfacing the real error, exactly the shape `while`'s own accumulator
  rejection has. Fixed; one existing test (`errors_on_unlowered_
  sequences_rule`) had been unknowingly relying on this exact gap to
  reach a DIFFERENT downstream error and needed rewriting to reach its
  actual target (`firrtl::emit`'s own "still a `<sequences>` rule"
  check) a cleaner way.

  **`while let` itself — the original ask — built as a follow-up, see its
  own bullet below.**

- **`while let`: looping over Option presence ACHIEVED** — the original
  ask this whole `while` detour started from, built once `while`'s own
  lowering (bullet above) existed to build on. `while let NAME = opt? {
  body }` renders its own segment as literal `if let` source text (`if
  let NAME = EXPR { <body>; cont := SELF } else { cont := NEXT }`),
  reusing `if let`'s ENTIRE existing emission machinery — needing zero
  new emission code, on top of the zero plain `while` already needed.
  See DESIGN.md's "`while let`: looping over Option presence" (feature)
  and "`while let` lowering" (implementation) for the full write-up.

  Inherits `if let`'s v0 restrictions verbatim (Option-only `init`, no
  `.field` chase-through) and plain `while`'s own (module state only
  inside the loop body, no locally-captured accumulator) — neither is a
  new gap this feature opens.

  Two more pre-existing gaps self-caught while writing this feature's
  own worked example (`examples/while_let_drain.tr`), NOT while building
  the segment-cutting/rendering machinery itself: six lower.rs recursive
  scans (`find_returns`, `find_nested_tick`/`find_tick_anywhere`, `find_
  nested_spawn`/`find_spawn_anywhere`, `find_unsupported_construct`)
  were already missing a `Stmt::IfLet` arm, dating to `if let`'s own
  landing — a `tick` nested inside `if let` used to surface as "a
  spawned fn's last segment must end with `return`" instead of the
  clear "not nested in if/while" message; and `collect_renames` (the
  spawn callee-body text-rewrite pass) also had no `Stmt::IfLet` arm, so
  a captured param referenced inside `if let`'s own branches in a
  spawned callee never got renamed, a hard resolve-error failure on the
  second pass. Both fixed, with matching `Stmt::WhileLet` arms added at
  the same time.

  A THIRD, OLDER gap was found but not fixed in this pass, since it was a
  materially different, pre-existing problem (branch-body local tracking
  generally) than anything `while let` itself needed: a `let`-bound local
  declared inside a plain `if`'s branch failed to resolve on EVERY read,
  not just a second one — a same-branch local read even exactly once
  already failed (`enter_rule` only walks a rule's own top-level
  statements; none of the branch-recursing write-threading walks had a
  `Stmt::Let` arm either, so a branch-local's binding was never
  registered anywhere ANY read could find it). Confirmed with a PLAIN
  `if`, no `if let` involved — predates every feature on this page.
  **RESOLVED in a follow-up session:** each of the four branch-recursing
  write-threading walks
  (`reg_value_in_stmts`, `mem_write_in_stmts`, `struct_field_value_
  in_stmts`, `inst_port_value_in_stmts`) now binds a branch-local `let`
  into `self.locals` (saved/restored around each recursive branch call)
  the moment it's encountered — sound for this shape specifically because
  a branch-local is bound exactly once and never reassigned via `:=`
  afterward, unlike the callee-local-reassignment gap above, which this
  is deliberately NOT the same fix as. `examples/while_let_drain.tr`
  reverted to the natural `let new_cnt = cnt - 1`, read twice, now that
  the recompute-workaround is unnecessary — reconfirmed through the full
  `<sequences>`/`while let` pipeline via real Icarus simulation, not just
  a plain `if`. Pinned by `a_branch_local_read_more_than_once_resolves_
  both_reads` (tests/firrtl.rs, renamed from its former known-gap name).

  Proven through a real Icarus simulation (examples/while_let_drain.tr +
  sim/while_let_drain_tb.v), the identical x=5/0/12 shape `while_
  countdown` pins for plain `while`, gated on a real `opt_valid` register
  instead of a comparison.

- **`?.` safe navigation — RESOLVED, built (option (b) from the three
  sizes below, via `AskUserQuestion`: full Verse-faithful multi-hop
  chaining, not the smaller one-hop-only slice).** See DESIGN.md's
  "`?.` safe navigation" for the full write-up. In one sentence: `opt?.
  field?.next` already PARSED and TYPE-CHECKED correctly before this —
  `?`/`.field` are ordinary postfix operators, no new AST shape, and
  `Expr::Guard`'s existing `?T`-to-`T` peel plus `Expr::Field`'s
  existing generic field lookup already compose correctly through
  arbitrary depth. The actual gaps were narrower than the original
  survey below predicted: not "PLUS type-checking that threads
  correctly through a chain of nested `?U` fields" (already worked,
  confirmed by tracing, not assumed) — just two structural/emission
  gaps. `lower::guard_chain_spine` (every `Expr::Guard` reachable by
  descending only through `Guard`/`Field` nodes) is shared by both:
  `checks.rs`'s `guards_outside_allowed_positions` now permits a WHOLE
  spine at the three legal positions instead of only a single exact-
  match `Expr::Guard`; `writes.rs`'s `compile_guard` gained `guard_
  chain_conds`, folding EVERY hop's condition (not just the outermost)
  into the rule's guard; `struct_field_path` (expr.rs) gained one new
  arm treating a `Guard` on the spine as "push `data`, recurse" — the
  identical rule `compile_expr_hinted`'s pre-existing single-hop Guard-
  value arm already used, just generalized to compose at any depth
  (that arm itself needed zero changes, already delegating to `struct_
  field_path`). Confirmed empirically before writing any test, not
  just reasoned through: a two-hop chain's `fires_r` is `and(a_data_b_
  valid, a_valid)` — BOTH hops' conditions, not one — and the flattened
  register path (`a_data_b_data_c`) matches prediction exactly.

  **`if let`/`while let` deliberately did NOT get multi-hop chaining —
  a real correctness trap caught before it shipped, not an oversight
  found later.** Their own mux-select machinery (writes.rs/calls.rs,
  roughly a dozen call sites) reads `init`'s immediate `inner` alone as
  the presence check, never the whole chain — `if let x = a?.b?` would
  have silently read `a.data.b.valid` without also gating on `a.valid`,
  wrong hardware passing a structural FIRRTL check with nothing to
  catch it short of running it. `guards_outside_allowed_positions`
  keeps `IfLet`/`WhileLet` restricted to a single bare `Expr::Guard`,
  exactly as before this feature; a chained init is rejected with the
  same message a misplaced guard gets. Fixing every one of those
  call sites (folding the whole spine into each) is real, separate
  work, not attempted here.

  **Proven end to end, not just structurally:** `examples/optional_
  chain.tr` + `sim/optional_chain_tb.v` (`tests/sim.rs`'s `optional_
  chain_sugar_runs_through_a_genuinely_absent_intermediate_hop`) drives
  the ONE state that actually discriminates a correct multi-hop fold
  from a naive one — `a` present, the intermediate `b` absent — through
  real firtool + Icarus. The OTHER direction (`b`'s own bit stale-`1`
  while `a` itself is absent) turns out to be unreachable in this
  language today: every write to `a` (a struct-literal or `false`)
  writes every flattened leaf together, confirmed by probing `a :=
  false`'s own emitted FIRRTL before assuming it — so only one half of
  the theoretical hazard is empirically exercisable, and that's the
  half that's tested.

  Original survey (kept for context on how the actual gaps compared to
  what was predicted): Verse's own primary source (`08_failure`,
  fetched directly, not guessed at) is explicit that `?.` is MULTI-HOP,
  each `?.` its own independent unwrap-or-fail — `Head?.Next?.Value`
  chains through however many `?Node` layers `.Next` itself is, not a
  single unwrap followed by ordinary field reads. That ruled out
  treating this as a small extension of `if let`'s own one-hop
  resolution (`if_let_binds`, DESIGN.md). Confirmed the underlying
  blocker was real before scoping further, not assumed: `let p = opt?;
  p.field` (T a struct) already failed with "a struct-typed local must
  be bound directly to a struct literal, not aliased" — a DIFFERENT
  chase-through gap than the one `?.` actually needed (that one is
  about a LOCAL aliasing another struct value; `?.` chains through the
  EXPRESSION tree directly, never through a local alias, so it never
  actually hit this path). Three sizes were on the table, smallest to
  largest: (a) one-hop only (`opt?.field`, honestly a DIFFERENT,
  smaller feature than `?.` and shouldn't be called that in DESIGN.md
  if built); (b) full Verse-faithful multi-hop chaining, the real `?.`
  — **this is what got built**; (c) doing nothing further, `if let` +
  `.data`/`.valid` already covering the ergonomic gap asked for so far.

  **The no-else/with-else split is what makes this tractable at all.**
  An `if` whose fallible condition has NO `else` is already exactly
  today's supported top-level guard: failure means the rule doesn't
  fire this cycle, full stop — no new semantics needed, it's just
  `if cond? { body }` instead of `cond?` followed by `body` unnested.
  The genuinely new half is `if`-WITH-`else`: failure must take the
  `else` branch (or fall through, with no `else`) while the REST of the
  rule still runs and still commits — trace has never had a construct
  where one part of a rule can fail without the whole rule failing.

  **The value side is already solved; only side-effecting ops are the
  gap.** Probed directly: `if opt.valid { result := opt.data } else
  { result := 2 }` already compiles today, unconditionally (no rule-
  level guard at all), to a plain `mux(opt_valid, opt_data, 2)` —
  ordinary conditional-write muxing, which `writes.rs` already does
  correctly for any `if`/`else`-nested write. An Option's presence check
  is a pure combinational predicate with nothing to make atomic, so
  `opt?` as an if-condition is arguably just SCOPING sugar over this
  (binding `opt.data` to a name visible only in `then`) — trace has no
  branch-scoped bindings anywhere else (`let` is body-scoped,
  shadowing is legal, see `let_shadowing_is_allowed`), so even the
  Option-only case would be a new scoping rule, not free. **Built —
  see the `if let` ACHIEVED bullet above: the scoping rule turned out
  to need no new resolve.rs machinery at all, just declaring `NAME`
  one scope-push deeper than `Stmt::If`'s existing then/else push/pop
  already goes.**
  The real gap is a FIFO op / failing call as an if's condition: a
  `Deq[]` is a genuine side effect (the fifo's occupancy register
  actually decrements this cycle), and `compile_guard` (writes.rs) is
  structurally top-level-only — `for stmt in &body`, no recursion into
  `if`/`while` at all. Today EVERY guard/fifo-op/failing-call in a rule
  is assumed unconditional, folding straight into one whole-rule `AND`
  (`check_guard_placement`'s "nested in if/while is not yet supported"
  restriction is this assumption enforced, not an arbitrary limitation).
  Branch-scoping a fifo op means its dequeue-enable signal has to
  become `<AND of every enclosing branch's take-condition> AND
  fires_rule` instead of `fires_rule` alone — likely an extension of
  the per-branch conditional-write machinery `writes.rs` already has
  for ordinary state writes, not a wholly separate mechanism, but this
  hasn't been confirmed against the actual fifo emission code in
  `firrtl/fifo.rs`/`module.rs`.

  **`or`-with-default is the existence proof, not a desugaring
  target.** `x := f1.Deq[] or f2.Deq[] or 0` already has EXACTLY this
  shape today: a fallible op tries an alternative, falls back on
  failure, and contributes NOTHING to the rule's guard when a default
  is present — "fallible thing whose failure takes an alternate path
  without failing the rule" already exists and is hand-lowered +
  Icarus-confirmed. That's precedent that the semantics are soundly
  buildable in this execution model, not a claim that `if` should
  desugar to `or`: `or`'s alternatives are `Deq[]`-only on depth-1
  fifos with no writes of their own, while an `if`'s `then`/`else` need
  to be arbitrary statement blocks with real writes in both branches —
  a materially bigger surface than `or` covers.

  Four open questions this design still owes before implementation:
  - **Two `Deq[]`s on the same fifo, one in `then` and one in `else`.**
    `check_fifo_op_counts` rejects a second `Deq[]` on the same fifo
    per rule TODAY, unconditionally — but two `Deq[]`s in mutually
    exclusive branches are provably not a double-dequeue. Does this
    check become branch-aware (track mutual exclusion through the
    if/else tree), or does the blanket restriction stay and this
    pattern remains rejected even once branch-scoping otherwise works?
    This is the sharpest concrete sub-question of the four. **STILL
    OPEN** — the ACHIEVED bullet below builds a bare `if`'s condition
    as the ONE fifo op (not a second, independent op inside a branch
    body), so this exact question — a Deq in `then` AND a DIFFERENT
    Deq in `else` — never had to be answered; `check_fifo_op_counts`
    stays exactly as unconditional/blanket as before.

    **UPDATE — ACHIEVED, for the narrowest provable case.** Lumi's
    call: "we need some static analysis to prove it, but I do want to
    allow branching on it" — so `check_fifo_op_counts` DOES become
    branch-aware, but only for the single syntactic shape that's
    provably exclusive by AST construction alone, not a general
    dataflow analysis. Two prerequisite pieces, in order: (1) a fifo
    `Deq[]` may now sit directly as an ordinary statement inside an
    `if`'s `then_body`/`else_body` at all (previously ANY fifo op
    nested in if/while was rejected outright, `contains_fifo_op`) —
    `rule_fifo_ops` (fifo.rs) gained a new case mirroring its `if let`/
    bare-`if`-condition cases, but this time the `select` can't be a
    pre-built string (an arbitrary enclosing condition needs real
    expression compilation, which needs `enter_rule`/`set_pos` context
    `rule_fifo_ops` doesn't have), so `RuleFifoOp::select` became an
    enum (`FifoSelect::Cond(String)` for the three pre-existing
    producers, `FifoSelect::Branch(ExprId, bool)` for this one,
    compiled lazily at the actual module.rs emission site). Enq stays
    excluded (no `select`-gating exists for it), and this is v0-
    restricted to depth-1 fifos (`check_branch_fifo_op_depth`, mirroring
    `check_or_shape`'s identical depth restriction on `or` — `emit_fifo_
    depth_n` has no gating logic to extend) and exactly one level of
    nesting (a fifo op nested inside a FURTHER if/while within the
    branch still isn't supported). (2) `check_fifo_op_counts` gained
    `mutually_exclusive_branch_pair`: exactly two ops on the same fifo,
    both `FifoSelect::Branch`-selected off the IDENTICAL enclosing `if`
    (`ExprId` equality IS same-`if`-statement identity — two lexically
    distinct `if`s always get distinct `cond` `ExprId`s) with opposite
    `is_then`, are allowed; anything else (three or more touches, an
    unconditional touch mixed in, two `if`s instead of one) still
    collides exactly as before. `module.rs`'s per-fifo touch tracking
    had a REAL latent bug this surfaced: `enq`/`deq` were `Option<
    RuleFifoOp>` per rule, silently overwritten by a second touch — two
    mutually-exclusive Deqs would have dropped one on the floor with no
    error. Fixed by widening the Deq slot to `Vec<RuleFifoOp>` (the Enq
    slot and the depth>1 path stay `Option`-shaped; both are still
    provably ≤1 by construction). One v0 boundary flagged by the
    external second-opinion review (`advisor`) before implementation
    and built in from the start: a nested op is REJECTED when the
    enclosing `if`'s own condition is itself a fifo op or failing call
    — composing "did the branch fire" with "was ITS OWN dequeue/call
    also successful" hasn't been reasoned about, so `rule_fifo_ops`
    excludes it explicitly and `checks.rs`'s matching exemption
    (`contains_disallowed_branch_fifo_op`) stays in sync with the exact
    same eligibility test, rather than letting checks.rs allow something
    `rule_fifo_ops` doesn't recognize (which would silently drop the
    dequeue from emission). Hand-verified end to end against real
    firtool+Icarus simulation before any test was written — see
    `examples/branch_fifo_deq.tr` + `sim/branch_fifo_deq_tb.v`.
    **Still open beyond this narrowest case**: proving exclusivity
    across a longer if/else-if chain, across two unrelated `if`s, or
    through nesting deeper than one level — all deliberately out of
    scope for now, not attempted.
  - **Interaction with "a guard must appear before any state write."**
    That restriction (`check_guard_placement`) is rule-wide today. Does
    it become per-branch (a state write before the branch's OWN
    fallible condition is still restricted, but a write in a SIBLING
    branch or after the whole `if` is fine), or does introducing any
    fallible condition inside an `if` still close the guard window for
    the rest of the rule the same way a top-level one does? **Moot for
    the ACHIEVED bullet below**, same reasoning `if let` already
    established: a bare `if`'s own fifo-op/failing-call CONDITION
    never participates in `check_guard_placement`'s seen-write tracking
    at all (that block treats the whole `If`/`IfLet`/`While`/`WhileLet`
    statement as a single unit, unconditionally, regardless of what's
    inside) — there was nothing new to decide here.
  - **`while` with a fallible condition** — out of scope unless named
    in. Verse's own construct is `if`-shaped only; nothing here argues
    for extending to `while`, so treat this as staying restricted
    (today's "guard nested in if/while" error) unless a concrete need
    for a fallible loop condition shows up. **Resolved for all three
    shapes now** (comparison, fifo op, failing call — see the ACHIEVED
    bullet below): took exactly this already-argued default in every
    case, `while` stays restricted, `logic` still the discharge for
    comparisons.

    **UPDATE — REVERSED, on a corrected premise.** The "Verse's own
    construct is `if`-shaped only" premise this whole resolution rested
    on was checked directly against Verse's own docs (the "Book of
    Verse" this repo already cites for `or`'s semantics, `verselang.
    github.io/book/`) when Lumi pushed back on it, and turned out to be
    wrong in a more specific way than assumed: Verse has no native
    `while` AT ALL, not an `if`-shaped-only one — only `loop`
    (unconditional, exited via an explicit `break`) and `for` (each
    iteration gets its OWN failure context; a failed filter clause skips
    to the next item rather than ending the loop, per `08_failure`).
    Lumi's call once that was confirmed: "I still want while to work
    like if, so it takes a fallible as a guard and breaks on a break
    keyword" — two separate asks, landed as two commits:
    1. `while`'s own condition now takes the identical `allow_bare_
       comparison`/`is_fifo_deq`/`is_failing_call` exemption `if`'s
       condition already had (`types.rs`'s `check_cond`, `effects.rs`'s
       `Stmt::While` arm mirroring `Stmt::If`'s three-way discharge) —
       `logic COND` still works, just no longer required. Needed almost
       no new machinery: `while COND { body }` already lowers by
       rendering literal `if COND { ...; cont := 1 } else { cont := 2 }`
       SOURCE TEXT and re-running the WHOLE pipeline on it (lower.rs),
       so once the ORIGINAL `Stmt::While`'s `cond` survives `check_cond`
       once, the RENDERED text hits `if`'s already-built discharge on
       re-entry for free. Confirmed by direct probe against real
       emitted FIRRTL (does the bare guard leak into `fires_r_sN`,
       wrongly gating the loop's own re-firing? — no) and a real
       firtool+Icarus simulation reusing the EXISTING `while_countdown_
       tb.v` testbench unchanged against a bare-comparison twin source
       file. See DESIGN.md's "`while`: multi-cycle loops" for the full
       write-up.
    2. A standalone `break` keyword, usable inside a loop body for early
       exit independent of the loop's own guard — the actual mechanism
       Verse's `loop`+`if`+`break` idiom uses, which trace's `while` is
       now built to match structurally, not just semantically.
       **ACHIEVED — see this section's own `break` bullet below.**

- **`break`: exiting a loop early ACHIEVED.** New `Stmt::Break` AST node
  (a new keyword, not sugar for anything pre-existing), v0-restricted to
  TAIL position: the last statement of a `while`/`while let`'s own body,
  or of a `then`/`else` branch of an `if`/`if let` that is ITSELF in that
  tail position — nested as deep as the user likes, as long as every
  enclosing level stays in tail position (no artificial one-level cap,
  unlike several other v0-scoped constructs in this codebase — the SAME
  recursive rendering applies identically at every depth, so there was
  no extra risk per level to cap against). An `if` with no `else` and a
  tail `break` in `then` gets its missing `else` synthesized (keep
  looping) at render time, so `if cond { break }` alone is legal —
  trace's spelling of Verse's own `loop: if (Cond[]) { ... } else {
  break }` idiom (confirmed against the "Book of Verse" the same way the
  `while`-reversal above was), minus the need to spell an explicit
  `else` when it would just be "keep going" anyway.

  Every exhaustive `Stmt` match across the compiler needed a `Stmt::
  Break` arm (found via the Rust compiler's own exhaustiveness checking,
  not a memory sweep) — almost all a pure no-op leaf mirroring `Stmt::
  Tick`'s treatment (`break` has no sub-expressions/reads/writes/calls of
  its own). Three sites hold the real logic: `effects.rs`'s `check_stmt`
  requires `<sequences>` on the enclosing item (mirroring `tick`'s
  identical check, deliberately not extended to `<elaborates>` — that
  interpreter already rejects `while` itself outright, so `break` there
  is moot, but this check catches it explicitly and earlier); `lower.rs`
  gained `find_break_misplaced` (the placement check, mirroring `find_
  nested_tick`'s shape but the opposite question) and `find_break_
  anywhere` (mirrors `find_tick_anywhere`/`find_while_anywhere`'s role in
  `plan()`'s own top-level gate, so a stray misplaced `break` with no
  `tick`/`while` alongside it still routes through the placement check
  rather than silently reaching firrtl.rs as an unrejected no-op);
  `lower.rs`'s `render_loop_body` replaces the old "splice every
  statement verbatim, then unconditionally append `cont := stay`" with a
  function that recurses into a tail `if`/`if let` ONLY when a `break` is
  actually reachable inside it — when there's none, it reproduces the
  OLD rendering byte for byte, so every pre-existing `while`/`while let`
  test kept its exact original FIRRTL assertions, not a restructured-
  but-equivalent one that would have needed updating for no functional
  reason. `find_break_misplaced` and `render_loop_body` are deliberately
  kept in sync on what counts as a legal position.

  A real, documented (not glossed-over) semantic point flagged by
  `advisor`'s review before implementation: statements before a `break`
  in its own branch still commit (`break` means "advance past the loop
  starting NEXT cycle," not "nothing this iteration happened") — and
  `break`'s own condition, like any other read in the loop body, sees a
  register's OLD (pre-edge) value, so a threshold-based break lands one
  iteration "later" than a naive read suggests (worked out precisely,
  cycle by cycle, in `sim/while_break_tb.v`'s own doc comment, after a
  first testbench draft got exactly this wrong and had to be corrected
  before the simulation could pass).

  Hand-verified end to end against real firtool + Icarus AND Verilator
  simulation before any structural test was written: `examples/while_
  break.tr` + `sim/while_break_tb.v` (`tests/sim.rs`'s `while_break_
  exits_the_loop_early_through_real_cycles`) covers a mid-loop break, an
  immediate break on the very first iteration, and the loop never even
  being entered. A separate probe confirmed two-level nested `if`-break
  (both in tail position) settles correctly and holds, not just compiles
  — structurally captured in `tests/firrtl.rs`'s `break_nested_two_
  levels_deep_in_tail_ifs_still_renders_correctly`. See DESIGN.md's
  "`break`: exiting a loop early" for the full write-up.
  - **What `fires_rule` becomes for an `if`-WITH-else fallible
    condition.** Per the no-else/with-else split above, the with-else
    case should contribute NOTHING to the whole-rule guard (matching
    `or`-with-default) — confirm this is actually achievable for a
    fifo-occupancy-gated branch, not just an Option-presence one, once
    the dequeue-enable question above is answered. **RESOLVED — but not
    the way this question assumed.** Confirmed by direct probe before
    writing any code (see the ACHIEVED bullet below): the no-else/with-
    else split this question presupposes DOESN'T EXIST for a fifo op or
    failing call at all — `compile_guard`'s statement loop never looks
    inside `Stmt::If`, with or without an `else`, REGARDLESS of what
    the condition is, so a fifo/call condition contributes nothing to
    `fires_rule` in BOTH shapes, not just with-else. The no-else-gates/
    with-else-doesn't ASYMMETRY only exists for comparisons, and it's
    `comparison_conds`'s own artifact (a position-blind scan with no
    fifo/call equivalent), not a language-level convention every
    fallible condition shares — a real, documented semantic difference
    between `if <comparison>` and `if <fifo-op-or-failing-call>` now
    (DESIGN.md's "`if`: a fifo op's own bare condition").

  **UPDATE — ACHIEVED, both remaining shapes (fifo op, failing call).**
  `if f.Deq[] { ... } [else]` / `if Classify(a) { ... } [else]` — bare,
  no bound name, built directly on top of `if let`'s own fifo-Deq/
  failing-call features above rather than as a separate mechanism:
  `Stmt::If`'s existing mux-select machinery already called `compile_
  guard_unwrap_cond(cond)` for the ACHIEVED comparison case, and that
  function's fifo-Deq/Call branches (built for `if let`) needed zero
  changes to also serve a bare `if`'s condition. The only genuinely new
  code: `types.rs`'s `check_cond` gained the `is_fifo_deq`/`is_failing_
  call` exemption (`if`-only); `fifo.rs`'s `rule_fifo_ops` gained a
  `Stmt::If` case mirroring its `Stmt::IfLet` one (`checks.rs`'s
  position exemption for THIS shape was genuine new work, unlike `if
  let`'s — no pre-existing dead-code groundwork this time); `effects.
  rs`/`checks.rs` gained matching `Stmt::If` arms mirroring their
  `Stmt::IfLet` ones. See DESIGN.md's "`if`: a fifo op's own bare
  condition" and "`if`: a failing call's own bare condition" for the
  full write-up, `examples/if_bare_fifo.tr` + `sim/if_bare_fifo_tb.v`
  and `examples/if_bare_failing_call.tr` + `sim/if_bare_failing_call_tb.v`
  for real firtool+Icarus+Verilator-proven examples. This closes the
  ORIGINAL literal ask this whole page section started from.

  **UPDATE — the predicted dead code went live, exactly as this entry
  said it would.** An explicit `<expr>?` is now legal directly as an
  `if`/`while`'s own WHOLE condition (`guards_outside_allowed_positions`,
  firrtl/checks.rs, gained `Stmt::If`/`Stmt::While` arms mirroring `Stmt::
  IfLet`/`Stmt::WhileLet`'s single-hop exemption; `compile_guard_unwrap_
  cond`, firrtl/writes.rs, gained a leading-`Guard`-unwrap-and-recurse
  arm, since every OTHER call site pre-unwraps by hand before calling
  but a bare `if`/`while` hands `cond` straight through with the `Guard`
  node still on top). Driven by a SEPARATE, larger decision this made
  necessary, not by this note alone: a plain `[1]` value is no longer
  accepted bare as an if/while condition in a RULE body at all (Lumi's
  call) — see DESIGN.md's "An if/while condition must itself be
  fallible" for the full rule (the `logic`-bare-condition footgun this
  closes, the `(logic A) & (logic B)` carve-out, and why callee bodies
  keep the old lenient behavior). `check_cond`'s two `Expr::Guard`
  exemptions (Option-inner, comparison-inner) were already live before
  this — only the POSITION check was the blocker this note originally
  flagged.

Explicitly considered and NOT being ported, so a future session doesn't
re-propose these from a fresh read of the same chapters:

- **`Err()`** (Verse's unrecoverable runtime error) — synthesized
  hardware has no runtime to propagate an error into; the closest
  analogue, an elaboration-TIME fatal check, is a different feature
  already reachable via a plain compile error, not a gap.
- **`first`/`for` runtime failure-filtering** — the synthesizable case
  (pick the first of several candidates that's actually valid) is
  already covered by the `prio` builtin; no new control-flow syntax
  needed.
- **Compound assignment** (`+=`/`-=`/etc.) — purely cosmetic sugar over
  existing `:=`/`Assign`; low value, skip unless a real example makes
  the spelled-out form genuinely painful.
- **Classes/structs/interfaces/persistence/module-path generality**
  from Verse's later chapters (09–12, 14–18) — general-purpose OOP/heap/
  persistence machinery with no hardware analogue; trace's `spec`/
  `impl`/`refines` already covers the HDL-relevant slice of "an
  interface," see DESIGN.md's "`chooses`: specification, not synthesis."
- **Verse's effect-subtyping rules** (fewer effects ⊆ more effects,
  join-on-conditional-selection) — already effectively matched: trace's
  standing overstate-ok/understate-error policy for `reads`/`writes`/
  `fails` gives "fewer effects is a safe substitute for more" without a
  separate subtyping mechanism.

## Cost model and formal verification (design-level, not scheduled)

- Multi-cycle `<sequences>` transactions (`tick`/`spawn`/`sync`) hide the
  real cost of what they express: rollback within one cycle is free
  (nothing has committed yet), but rolling back a transaction that has
  already crossed a `tick` needs real checkpoint/squash machinery — the
  same thing an out-of-order core builds, and it's expensive. The
  language currently lets a five-cycle atomic transaction be written as
  casually as a single-cycle write, with the effect system tracking
  correctness but not cost — the standard "cost opacity" criticism of
  HLS tools generally, and there's no reason to expect trace is immune
  to it once designs get larger.
- Closest research precedents, if this is ever addressed structurally
  rather than by convention: Filament (Cornell CAPRA) — "timeline
  types" encode which cycle each signal is valid in directly in the
  type system, rather than as a side effect-system check the way
  trace's `mutually_exclusive`/`conflict_free` directives do now; Dahlia
  — affine types for predictable memory banking, relevant to v0 arrays'
  current one-conflict-resource-per-array limit (see "Scheduler /
  arrays" below); Kôika — a Coq-verified one-rule-at-a-time Bluespec
  descendant, the closest existing precedent for formally proving this
  project's own scheduling model correct rather than trusting it by
  testing alone.
- Formal verification of the scheduler/conflict semantics specifically
  (not the whole compiler) looks like the highest-value next step if
  this is ever picked up: a small satellite Lean/Coq model of just
  `schedule.rs`'s `mutually_exclusive`/`conflict_free` arbitration,
  validated against the Rust implementation via generated test vectors
  — not a full rewrite, and not an attempt to encode cycle-validity in
  Rust's own type system, which (unlike Lean's) isn't dependently typed
  and can't express "valid in cycle n" natively.
- Not scheduled, no plan to act on this now — trace's actual discipline
  today (real firtool/Icarus runs, discriminating negative tests) is
  substituting empirical verification for formal proof, case by case
  rather than once and for all. If this is ever worth addressing head
  on, porting just the scheduler/cost-model layer to a language built
  for it wouldn't be a large loss — the conflict matrix and urgency
  logic already live in one fairly self-contained module,
  `schedule.rs`.

## Rules: optional/enable sugar — RESOLVED, built

**`rule foo? { body }` — sugar for an implicit, rising-edge-triggered enable
port sharing the rule's own name — built and proven end to end, not just
"parses and firtool accepts it."** See DESIGN.md's "Optional rule sugar"
for the full design writeup (the desugaring template, the three axes
Lumi picked, and why one-shot + `in`-port together cost real new
machinery, not free sugar) — not re-derived here.

**Implementation, in one sentence: five items synthesized as real AST
nodes directly at parse time** (`parser.rs`'s `parse_rule`/
`desugar_optional_rule`), spliced into the enclosing module's item list
exactly as if hand-written — the implicit `in` port, a `__prev_{rule}`
shadow register, an always-firing `__edge_{rule}` rule updating it, the
original rule with a rising-edge guard prepended, and a synthesized
`conflict_free { __edge_{rule}, {rule} }` exemption (an ordinary
read/write conflict otherwise, since both touch the shadow register —
sound to exempt because the guard reads the shadow register's
pre-this-cycle value regardless of any same-cycle write to it). This
needed `parse_item`/`parse_module`'s item collection to go from
`Option<ItemId>` to `Vec<ItemId>` throughout the parser, the one real
plumbing change — the namespace prerequisite from the section above is
what makes `foo` able to name both the port and the rule with zero
collision.

**The reset-edge question is decided: a port already held high AT RESET
does NOT count as a rising edge** (Lumi's call, via `AskUserQuestion`,
over the alternative — resetting `__prev_foo` to `0`, which would read a
tied-high port as a genuine `0→1` pulse and spuriously fire the rule once
on cycle 0). Implemented by resetting `__prev_foo` to `1`, not `0` — zero
extra machinery beyond the reset literal itself, since `not(__prev_foo)`
then reads `0` on the very first post-reset cycle regardless of what
`foo` itself reads as, while every genuine edge afterward is still
detected correctly (confirmed by direct cycle-by-cycle trace before
writing the permanent testbench: held high through reset stays
suppressed, a real drop-then-rise fires exactly once, holding level
afterward doesn't refire).

**Proven end to end:** `examples/optional_rule.tr` + `sim/optional_rule_tb.v`
(`tests/sim.rs`'s `optional_rule_sugar_runs_through_real_reset_and_edges`)
drives real reset behavior AND two genuine edges through firtool +
Icarus, not just a held-high level — the one behavior a naive
level-sensitive read would get wrong. `tests/parser.rs`'s
`optional_rule_sugar_desugars_to_five_items` pins the exact desugared
shape via `Ast::dump()`; `tests/resolve.rs`'s
`optional_rule_sugar_resolves_with_no_namespace_collision` and
`tests/firrtl.rs`'s
`optional_rule_sugar_emits_a_reset_to_one_shadow_register_and_no_derived_stall`
cover resolve/emission directly.

**One real, documented restriction, found before it shipped as a silent
footgun, not after: `rule foo? <sequences>` is rejected at parse time.**
Every synthesized node shares `foo`'s own span; `lower.rs`'s
`<sequences>` splicing reconstructs a rule's segments FROM spans, and
the collision panics (`lower.rs`'s "overlapping lowering edits" assert)
rather than emitting silently-wrong hardware — confirmed by hand
(`rule step? <sequences> { tick \n ... }` through `--lower`) before
adding the check, not guessed at. `tests/parser.rs`'s
`optional_rule_sugar_rejects_sequences` pins the clean parse-time error
in place of that panic. The manual `in foo : [1]` + `foo?` pattern is
unaffected — only the sugar itself is restricted, and only under
`<sequences>`.

**Left as-is, not decided against, just not done here:** whether the
hand-written `trigger`/`trigger?` pattern in the `spawn`/`race` examples
should migrate to this sugar. Left alone — a manually-named, still-legal,
more general pattern the sugar doesn't replace (a level-sensitive or
`reg`-backed enable still needs the manual form).

## `io` ports + `extmodule`: RESOLVED, both tiers built

**`io name : ty` (a third port kind alongside `in`/`out`) and `extmodule
Name from "path.v" { ports }` (an external Verilog module's interface) —
both built, both proven against real firtool AND real Icarus simulation,
not just "firtool accepts the text."**

**`io` + `attach`.** `io name : ty` lowers to FIRRTL's `Analog<N>`. The
only legal statement touching one is `attach a, b` (a module-level item,
alongside `reg`/`inst`, not inside a rule — unconditional net wiring has
no clock/cycle semantics), where each operand is a bare `io` name or
`instance.port`, both sides must be `io`-kind and the same width. Since
`Analog`'s only operation is `attach` (net-to-net, confirmed directly:
`attach(bus, data)` mixing an `Analog` and a `UInt` operand is rejected,
`operand #1 must be variadic of analog type`), `io` needs no effects-row/
`sig.reads`/`sig.writes` integration at all: resolve.rs rejects any bare
read/write outright (`DefKind::Io`), types.rs rejects the `instance.port`
form the same way an `in`/`out` direction mismatch already is
(`find_port`'s `DefKind::Io` arm).

**`extmodule`.** Ports reuse the exact same `in`/`out`/`io` vocabulary and
`find_port`/`inst.port` machinery ordinary module ports already have — an
extmodule's own `input`/`output` (its own declaration's perspective) map
onto exactly the same `DefKind::Input`/`Output`/`Io` an ordinary module's
ports produce, so read/write legality needed zero new logic. The only
real new machinery: `Item::ExtModule` is collected and declared
separately from ordinary modules (`all_extmodules`/`item_of_extmodule_def`
in `firrtl/mod.rs`, never a candidate for "the top", never walked the way
`emit_module` walks a real module's body — see DESIGN.md's "Extmodule
emission"), and an instance of one gets no `connect t.clock`/`connect
t.reset` (v0 restriction: an extmodule has no such port at all; a
blackbox needing a clock declares one as an ordinary `in` port instead).
The `.v` path itself (`from "tribuf.v"`) is opaque data trace's compiler
pipeline never reads or validates — confirmed by hand-lowering an
extmodule through firtool before writing any trace code: FIRRTL text has
zero linkage to the implementation file, entirely a downstream build/
simulation concern.

**Proven end to end, not just "compiles":** `examples/extmodule_tribuf.tr`
+ `sim/tribuf.v` (a real combinational tri-state buffer) +
`sim/extmodule_tribuf_tb.v` (two instances of the generated `Top` with
their `bus` ports tied together) — `tests/sim.rs`'s
`extmodule_tribuf_runs_a_real_bidirectional_bus` alternates which side
drives and checks the other side senses it, a genuine bidirectional net
with neither direction fixed at compile time.

**One real, documented gap left, not silently dropped:** `devenv.nix`'s
`simulate` script doesn't know to pass an extmodule's `.v` file to
iverilog alongside the generated design — it assumes one example name
maps to exactly one generated `.v` plus one testbench.
`extmodule_tribuf_runs_a_real_bidirectional_bus` uses its own dedicated
`simulate_with_blackbox` helper in `tests/sim.rs`, not the shared
`simulate` every other sim test uses. Fixing this needs a real design
decision not made here: how would `simulate` (or trace itself) DISCOVER
which `.v` files an example's extmodules need and WHERE to find them
relative to the invocation — the `.tr` source only ever says `"tribuf.v"`,
a bare filename with no directory, and trace's compiler deliberately
never resolves it against any base directory (keeping the pipeline pure
text-in/text-out, no filesystem awareness added to lexer/parser/resolve/
types/firrtl passes that have none today). See sim/README.md for where
this is documented for a human running `devenv shell -- simulate`
manually in the meantime.

## Scheduler / arrays

- **RESOLVED (scoped v1+v2) — a read/write pair whose mem indices are
  either both compile-time-constant integers, or an affine expression of
  the SAME base register/input with the mem's own depth a power of two,
  is now auto-proven disjoint and dropped from the conflict matrix, no
  `conflict_free` annotation needed** (`schedule.rs`'s `Exemption::
  Disjoint`, `examples/mem_disjoint_rw.tr` (v1, constants) and `examples/
  mem_disjoint_affine.tr` (v2, `m[i]` vs `m[i+1]`)). v2's own two real
  restrictions, not oversights: the base is never chased through a
  rule-local (reopens the reassigned-local/`Avg(Avg(x,y),z)` bug class),
  and the power-of-two-depth requirement is load-bearing — v0 has no
  bounds check on an index against a non-power-of-two depth at all (an
  out-of-range index is undefined, left to firtool), so the modular
  disjointness argument simply never depends on that undefined behavior.
  Still fully conservative for: two DIFFERENT bases with no shared
  power-of-two multiplier (`m[i]` vs `m[j]`, distinct registers — their
  runtime values could coincide, and proving otherwise IN GENERAL needs
  real range tracking, not a syntactic check — this is the genuinely
  remaining tier-3 gap, see v3 below for the scoped exception); a mem
  sharing even one unrecognized index; and every write/write pair
  regardless of index shape (v0 has one shared, priority-muxed write
  port per mem — see DESIGN.md's "Arrays: one resource each" — so two
  "disjoint" writers would still race on it). Explicitly NOT
  dependent/refinement types: a syntactic affine-offset check in
  schedule.rs, no new types or propositions.
- **RESOLVED (v3, scoped) — `m[2*i]` vs `m[2*j+1]` for TWO DIFFERENT
  bases is also now auto-proven disjoint, without any range tracking**
  (`schedule.rs`'s `IndexForm` gained a multiplier field,
  `examples/mem_disjoint_banked.tr`). NOT a scoped version of Dahlia-
  style banking/range tracking — the argument doesn't need one: `M*x` is
  congruent to 0 mod `M` for ANY x, so base identity drops out entirely,
  unlike the v2 same-base case. Recon before implementing found no
  existing design needs this (`subleq.tr`'s `m[b]` vs `m[pc]`, the one
  real different-base candidate, is empirically unprovable regardless —
  `b` is loaded out of the mem itself, arbitrary program data, exactly
  the shape the checked `conflict_free` assertion handles dynamically
  instead); this is a new capability with its own new example, not a
  gap an existing design was blocked on. Sound only up to `k = log2(M)`
  bits — a compiled `M*base [+ r]` expression's own natural width
  empirically collapses to the base's OWN declared width rather than
  growing to accommodate the multiply (confirmed via the CLI, not
  assumed), so the `base_width(..) >= k` guard on BOTH sides is
  load-bearing, not caution for its own sake. The genuinely general
  case — two arbitrary, unrelated, unscaled bases — remains exactly as
  unprovable as before; Dahlia-style banked/affine array types (real
  range tracking) are still tier 3, explicitly deferred in DESIGN.md.
- **RESOLVED (v4, refactor, not a new proof) — the index recognizer
  itself is now a single compositional representation, not a growing
  pile of hardcoded shapes** (`schedule.rs`'s `IndexForm` went from an
  enum with one variant per shape to a struct `{base, multiplier,
  offset}` with `add`/`sub`/`mul` composition methods; `index_form`
  recurses through `+`/`-`/`*` generically instead of pattern-matching
  `base+k`/`M*base`/`M*base+k` as separate cases). Direct response to
  Lumi flagging the v1/v2/v3 pattern itself as the problem ("add the
  type system now, instead of proving just different disjoint cases")
  — advisor's reframe: the actual complaint was `forms_differ`/
  `index_form` growing one arm per shape, not a request for dependent
  types in the abstract, and a symbolic composable representation
  fixes that without new syntax, new annotations, or any whole-program
  analysis. Zero behavior change for anything already provable
  (confirmed via a normalized `--explain-schedule` diff across every
  example both before and after, not just the unit/firrtl/sim test
  suite passing unchanged) — this is a pure generalization: a NEW shape
  that reduces to the same linear form (`(i+1)*2`, nested arithmetic)
  is now recognized automatically, confirmed via `git stash` to have
  fallen back to a derived stall before this commit and proven disjoint
  after, with no new arm added for it. Does NOT touch the two
  disjointness ARGUMENTS themselves (same-base-same-multiplier,
  same-power-of-two-multiplier) — those are two fixed mathematical
  cases, not an open-ended list, and stay as `forms_differ`'s own `||`.
  Does NOT attempt the genuinely general different-base case either;
  that's still exactly as unprovable, and still tier 3.
- **RESOLVED (v5) — `reg i : [w] where i < K` STATICALLY PROVES `i`'s
  value bound by induction over every write site, closing the actual
  gap all three prior proofs sidestepped rather than fixed: v1/v2/v3
  all require the mem's depth to be an exact power of two, purely to
  avoid depending on undefined out-of-range-index behavior** (new
  module `src/bounds.rs`; `schedule.rs`'s `forms_differ` gains a
  FOURTH, independent argument — same base/multiplier, both index
  forms confirmed under the mem's REAL depth via the proven bound, no
  power-of-two requirement at all; `examples/mem_disjoint_bounded.tr`,
  depth 10, not a power of two). Reached after a second AskUserQuestion
  round (purpose: bounds-checking, not disjointness — the real
  motivation, since disjointness itself had run out of use cases;
  tier: statically proven, not trusted, not a runtime assertion) and a
  full plan-mode design pass. v0 restrictions, all deliberate: `reg`
  only (`in` has no write site to prove anything over — a bound there
  would be a TRUSTED contract, a different feature; `out` is provable
  in principle but unmotivated); a single strict upper bound against a
  compile-time constant, no `<=`/lower bounds/arbitrary expressions;
  `Add`-only RHS composition (`Sub`/`Mul` fail closed); no interaction
  with the v3 banking argument (different, modular soundness argument
  a value bound doesn't slot into). Register reads are FROZEN for a
  whole item's walk (pre-edge-read invariant — a write earlier in the
  same body is never visible to a later read in it), unlike a
  `Stmt::Let` local's ordinary forward-flow tracking; a single forward
  walk suffices, no fixpoint, since a bound is a per-program-point
  fact (narrower here, wider there by an `if` guard) rather than
  width-inference's whole-body-unified property. `IndexForm`'s own
  wrapping arithmetic is NOT reused for the new argument's range math
  (`m[i-1]` under a proven bound must not be treated as safe just
  because `IndexForm` stores its offset as a wrapped `u64::MAX`) — a
  separate, independent `real_upper_bound` walk recognizes only the
  same restricted shape `bounds.rs` itself proves. Confirmed via `git
  stash` end to end: the driving example's `write`/`read` pair falls
  back to a derived stall without the `where` clause (`read_count`
  stuck at 0 forever in real simulation, `write_count` climbing) and
  fires unconditionally with it. Still does NOT close the general
  tier-3 case (`m[i]` vs `m[j]`, two arbitrary unannotated bases) —
  explicitly a narrow, induction-checked annotation, not general range
  inference.
- **RESOLVED (v6) — `where` gained an optional LOWER end (`where L <=
  i < K`), letting two DIFFERENT registers each be independently
  proven to occupy a non-overlapping sub-range, closing one further,
  narrow slice of the general tier-3 gap v5 explicitly left open**
  (`schedule.rs`'s `forms_differ` gains a FIFTH, independent argument
  — tried FIRST, sound regardless of base identity, since two
  provably non-overlapping real ranges can never coincide in value;
  `examples/mem_disjoint_ranges.tr`, `i`'s range `[0,5)` vs `j`'s
  `[5,10)`). Motivation checked directly before scoping, same
  discipline v3 used: every existing mem access with two different
  bases is either module inputs (already answered by the checked
  `conflict_free` assertion) or data loaded out of the mem itself
  (`subleq.tr`'s `m[a]`/`m[b]`, unprovable regardless) — no existing
  design needed this, so a new example was written for it, same basis
  v3 shipped on (confirmed to Lumi via AskUserQuestion before building
  anyway). `bounds.rs`'s `BoundedReg`/`Bounds` now track a `(lower,
  upper)` pair throughout (`Bounds.upper` renamed to `Bounds.ranges`);
  every write site checks BOTH ends against the full declared range.
  Deliberately NOT extended: `narrow_for_condition` still only narrows
  the UPPER end via `if <reg> < <const>` — a `>=`/`>`-shaped guard
  narrowing the LOWER end isn't recognized, traced by hand against the
  driving example to confirm it isn't needed (every write is checked
  directly against the full frozen declared range on both ends, not a
  further-narrowed one). The fifth argument also gives a small freebie
  for free: a bare constant vs. a bounded different-base index (`m[3]`
  vs `m[j]` where `j`'s range is `[5,10)`) is now provable too, since
  that combination previously always failed closed. `schedule.rs`'s
  `real_upper_bound` (v5) is renamed `real_range`, returning the full
  pair instead of just the upper end. Confirmed via a targeted
  discriminating check (disabling just the new argument, not a full
  `git stash`, since bounds.rs's `Bounds` struct shape changed too):
  the driving example's `write`/`read` pair falls back to a derived
  stall without it, proven disjoint with it, at both the
  `--explain-schedule` and real firtool+Icarus sim level. Still does
  NOT close the general tier-3 case: two arbitrary, unrelated,
  UNANNOTATED bases (no declared range at all) stay exactly as
  unprovable as before — this only ever compares ranges that were
  each independently declared and proven, it infers nothing from
  nothing.
- **RESOLVED (v7) — `bounds.rs`'s guard narrowing gained `Gt`/`Ge` arms
  (narrowing a bounded reg's LOWER end, symmetric to the existing `Lt`
  arm) and `expr_bound` gained `Sub` composition, closing a FALSE
  REJECTION rather than adding a new provable case** (unlike v3/v5/v6):
  `reg cnt : [8] where cnt < 100 = 0` with `if cnt > 0 { cnt := cnt - 1
  }` was previously rejected outright ("unsupported expression shape")
  even though the guard makes it provably safe —
  `examples/countdown_bounded.tr` is the driving example, confirmed to
  fail on the pre-feature code before implementing (the same
  discriminating-baseline discipline as every prior step, just checked
  before writing code instead of via `git stash` after, since this pass
  touches no scheduling logic to stash around). Traced by hand before
  scoping (self-caught, then confirmed with Lumi via AskUserQuestion):
  Sub as originally proposed alone would have been INERT — the only
  real Sub pattern (`while_countdown.tr`'s down-counter) needs `cnt`'s
  frozen LOWER bound narrowed first, or `0 - 1` always fails the
  underflow check regardless of any guard, since narrowing only
  recognized `<` before this. `Sub`'s soundness check reuses the
  existing `checked_*`-fails-closed idiom exactly: `b_max =
  b_hi.checked_sub(1)`, `lo = a_lo.checked_sub(b_max)`, `hi =
  a_hi.checked_sub(b_lo)` — if the smallest possible minuend can't be
  proven to dominate the largest possible subtrahend, `checked_sub`
  itself returns `None`, the same "unprovable, fails closed" signal
  `Add`'s `checked_add` already gives on overflow, needing no separate
  error path. `Mul` and `<>`-shaped guard narrowing were both
  considered and DROPPED: `Mul` has no motivating example (nothing in
  the repo multiplies into a state write) and would need two more
  overflow paths for zero known consumers; `<>` narrowing is only sound
  when the excluded constant equals the CURRENT frozen bound exactly
  (otherwise it splits the range into two disjoint pieces a single
  interval can't express) — a genuinely different, more special-cased
  argument than a plain inequality, confirmed to still stay rejected by
  its own dedicated regression test. A normalized `--explain-schedule`
  diff across every existing example came back byte-identical (this
  pass touches no scheduling logic at all, only `bounds.rs`'s own
  write-site proof) — the first RESOLVED entry in this whole arc with
  zero `schedule.rs` involvement.
- **RESOLVED (v8) — `where` extended to `out` ports** (`examples/
  output_bounded.tr`). A pure plumbing extension, not a new proof: `out`
  is register-backed and written via the exact same `Stmt::Assign` shape
  a `reg` is (DESIGN.md: `out` "behaves like a plain `reg` inside a
  rule"), so `bounds.rs`'s whole induction argument — freeze the
  declared range, narrow per guard, check every write site — never cared
  which kind of state def it was checking; the only reason it didn't
  already work was `parser.rs` explicitly rejecting `where` on anything
  but `reg`, and no AST/resolve/types plumbing threading a bound through
  `Item::Output` at all. Checked motivation before scoping, same
  discipline as v3/v6: no existing example writes an `out` via
  self-referencing arithmetic near an interesting bound or uses one as a
  mem index — same "no existing consumer, would need an invented
  example" situation as v6, disclosed to Lumi up front before they
  picked this thread, so proceeding on the same informed basis v3/v6
  did. `bounds.rs`'s `BoundedReg`/`collect_bounded_regs` renamed
  `BoundedDef`/`collect_bounded_defs` (both no longer reg-specific);
  every other function in `bounds.rs` needed ZERO changes, being already
  fully generic over `DefId`. Discriminating baseline confirmed BEFORE
  implementing (same variant on the discipline v7 used): `where` on an
  `out` parse-errored on the pre-feature code, confirmed to parse/
  resolve/type-check/prove cleanly after. A normalized
  `--explain-schedule` diff across every existing example came back
  byte-identical again — the second RESOLVED entry in this arc (after
  v7) with zero `schedule.rs` involvement.
- **RESOLVED (v9) — `bounds.rs`'s guard narrowing gained a `Ne` (`<>`)
  arm, narrowing EITHER end but only at an exact edge** (`examples/
  output_bounded_ne.tr`). `if <reg> <> <const>` narrows the LOWER end up
  by one when `<const>` equals the current frozen floor, and the UPPER
  end down by one when `<const>` equals the current max — excluding any
  OTHER value would split the range into two disjoint pieces a single
  `(lo, hi)` interval can't express, so that case stays a no-op exactly
  as v7's own doc comment predicted. Exact equality only (`k == lo` /
  `k + 1 == hi`), not a widened `<=`/`>=` comparison — the latter would
  be unsound, narrowing past a `k` that isn't actually the current edge.
  Same "new capability, not a gap an existing design is blocked on"
  situation as v3/v6/v8: `while_countdown.tr` is the one file in the
  repo using `<>` (`while cnt <> 0 { cnt := cnt - 1 }`), but it stays
  unprovable regardless of this feature — `cnt := x` reads an unbounded
  `in` port every cycle and carries no `where` bound at all, so the
  driving example (`output_bounded_ne.tr`) had to be invented, disclosed
  up front rather than built around silently. Advisor flagged the one
  real test-design risk before implementing: the obvious mid-range
  negative test (`if cnt <> 50 { cnt := cnt + 1 }`) doesn't discriminate
  a buggy generalized narrow from a correct no-op, since both accept the
  write — needed two narrower tests instead, picked so an unsound
  generalization (`k >= lo` / `k < hi` instead of exact equality) flips
  each to a false accept; both verified to actually catch that exact bug
  by temporarily reintroducing it. Two more non-goals stated explicitly
  at the time (the commuted form was left unrecognized, closed by v10
  below): the `else` branch of an `if <>` (a provable singleton) is
  left unnarrowed — conservative, blocks nothing a real example needs. A
  normalized `--explain-schedule` diff across every existing example
  came back byte-identical — the third RESOLVED entry in this arc (after
  v7, v8) with zero `schedule.rs` involvement.
- **RESOLVED (v10) — `Ne`'s commuted guard form (`<const> <> <reg>`,
  constant on the LEFT) is now recognized too** (`ne_commuted_lower_
  edge_subtraction_is_proven`, `tests/bounds.rs`). A ten-line operand-
  order extraction, not a new soundness argument: `narrow_for_condition`
  only ever treats a guard as a pass/fail predicate deciding which
  branch to check, never a comparison's own RETURNED value (which IS
  order-sensitive in this language — `type_binop`'s "comparisons return
  their left operand" rule means `x <> k` and `k <> x` are genuinely
  different expressions wherever that value is consumed) — so `x != k`
  and `k != x`, the same fact about the same two values, narrow
  identically. Extracted the shared `(def, k)` lookup into
  `ident_const_operands`, tried in both operand orders but ONLY when
  `op == Ne`; `Lt`/`Gt`/`Ge` stay single-order since `k < x`/`x < k` are
  different claims even as bare predicates — commuting those would mean
  recognizing a different operator in the flipped position, a separate
  feature nobody asked for. Advisor's explicit guidance going in: treat
  this as its own small commit, NOT the same increment as mid-range `<>`
  exclusion (a materially bigger change — see below) — the two were
  raised together but are a different complexity class. Two tests
  verified by bug-reintroduction: the flipped `ne_commuted_..._is_
  proven` test (was `ne_commuted_form_is_not_recognized`, pinning the
  old behavior) and a new `commuted_lt_is_not_recognized`, confirming
  the `Ne`-only gate doesn't leak into `Lt` — verified by temporarily
  hoisting the commuted fallback out of its `Ne`-only guard and
  observing `commuted_lt_is_not_recognized` fail exactly as predicted.
  Mid-conversation, Lumi caught an overclaim in the doc-comment wording
  ("`<>` is genuinely symmetric") — correct only for the narrow pass/
  fail-predicate purpose this function uses it for, NOT as a claim about
  `<>` in general in this Verse-inspired language, where a comparison's
  own returned value is asymmetric by design; reworded across both
  `bounds.rs` doc sites and DESIGN.md to state the narrower claim
  explicitly rather than the misleading broad one. A normalized
  `--explain-schedule` diff across every existing example came back
  byte-identical.
- **RESOLVED (v11) — `Mul` composition in `expr_bound`** (`examples/
  scaled_counter.tr`). Lumi asked to build the full interval-set domain
  for mid-range `<>` exclusion (v10's own deferred item); before writing
  any code, checked whether it would buy anything and found it would
  be PROVABLY INERT — every check in `bounds.rs` (width-clamp, upper,
  lower) reads only the extreme `lo`/`hi` of a tracked range, and both
  `Add`/`Sub` compose purely from the operands' own extremes, so
  excluding one interior point (as opposed to an edge, which v9 already
  handles) changes neither the min nor the max of anything downstream —
  the domain would cost real complexity (cross-product composition, a
  piece cap, every write-site check iterating pieces) for zero
  additional proving power, given `Add`/`Sub`/bare-ident/literal are the
  only compositions that exist. Confirmed with advisor before reporting
  it back (same shape as v7's own "Sub would be inert" catch), then
  surfaced the finding to Lumi rather than silently building an inert
  feature or silently downscoping the ask. Lumi picked "build `Mul`
  first, then revisit the domain" — `Mul` is the concrete operation
  whose result actually depends on which values are reachable, not just
  the extremes, so it's the domain's real prerequisite, the same
  ordering relationship v7 found between lower-narrowing and `Sub`.
  `Mul` composes cleanly because both operands are non-negative
  (`bits[N]`): a product's minimum is exactly `a_lo * b_lo` and its
  maximum is exactly `a_max * b_max` (no sign-corner-case reasoning
  needed, unlike general signed interval multiplication), using the
  same `checked_*`-fails-closed idiom `Add`/`Sub` already established.
  Discriminating baseline confirmed before implementing (the driving
  example fails to compile on pre-feature code); the existing
  `multiplication_on_the_rhs_is_rejected_as_unknown` test flipped to
  provable exactly as v7's `Sub`-related tests once did, replaced with
  `multiplication_by_a_literal_is_proven`. Five new/flipped tests, two
  verified by bug-reintroduction: an UNSOUND bug (computing the ceiling
  from the operands' own `lo` instead of `max`) flips
  `unguarded_multiplication_that_could_exceed_the_bound_is_rejected`
  from 1 error to a wrongly-accepted 0; an over-conservative bug (using
  `a_hi`/`b_hi`, the exclusive upper bound, instead of `a_max`/`b_max`)
  flips `multiplication_uses_the_operands_own_max_not_their_exclusive_
  upper_bound` from provable to a false rejection — both confirmed by
  temporarily reintroducing the exact bug. A normalized
  `--explain-schedule` diff across every existing example came back
  byte-identical. Explicitly still NOT done: whether the interval-set
  domain is worth building now that `Mul` exists to make it non-inert
  remains open, not yet decided as of this entry — resolved by v15,
  several entries below: still provably inert (for a broader reason
  than just "Mul specifically"), with `else`-branch negated-condition
  narrowing built instead as the actual non-inert capture of the same
  underlying idea.
- **RESOLVED (v12) — cross-boundary bound propagation: a `fn`/`impl`
  PARAMETER can carry a `where` bound, checked as an obligation at
  every CALL site** (`examples/param_bound_check.tr`). Lumi asked to
  build "the type system" — the recurring broader ambition (v4's
  "should we add the type system now," raised again here). Checked
  whether `bounds.rs`'s own arm space still justified a v4-style
  generalization before designing anything: it doesn't — `IndexForm`'s
  arm space (v4) was syntactic SHAPES, genuinely open-ended;
  `bounds.rs`'s arm space is OPERATORS, and it's now closed (all three
  arithmetic ops, all four comparisons this language has are
  implemented). Surfaced this distinction to Lumi directly rather than
  assuming the v4 precedent transfers, and asked what a real type
  system would need to buy that `bounds.rs` structurally can't: bounds
  crossing a CALL boundary. `bounds.rs` only ever walked write sites
  within one item's own body — no way to reach a callee's signature or
  check a caller upholds it. Lumi picked this as the concrete
  increment.

  Does NOT need a new `Ty` variant: a param bound is checked the same
  way a write site already is (compute the argument's provable range,
  verify against the callee's declared `[lower, upper)`) — a boundary
  CHECK, not a value flowing through unification, mirroring how
  `check_assignable` already checks argument WIDTH at each call site.
  `Param` (`ast.rs`, shared with `Item::Struct`'s own fields) gained
  `bound`/`lower` fields, mirroring `Item::Reg`'s; `parse_where_bound`
  extracted from `parse_state_decl`'s inline block into a shared helper,
  called from both (reg/out keeps its restriction; a param has none —
  always meaningful). `resolve.rs`'s `check_bound_self_reference`
  generalized to take a `DefId` directly (a param has no `ItemId` of
  its own, but `declare()` already returns its fresh `DefId` directly,
  no span-scan needed unlike `types/collect.rs`'s own param-type
  workaround). `bounds.rs`'s `base_width` extended to fall back to
  `Types::local_tys` (where a param's own declared type actually lives
  — `state_tys` is reg/mem/fifo only). A bounded param's `DefId` is
  collected into the SAME `self.bounded` map a reg/out populates, so a
  callee's own body trusts its param's declared range as the base case
  of ITS OWN induction, zero new logic there — the actual new work is
  `expr_bound`'s new `Expr::Call` arm (signature widened to `&mut self`,
  since checking arguments is a real side effect), checking each
  argument against the callee's declared param bound via a shared
  `check_against_bound` helper extracted from the existing `Stmt::
  Assign` arm (avoiding duplicating the 4-branch error logic, and
  generalizing "the reg's own declared width" wording since a param
  isn't a reg).

  **Scope correction found empirically, not planned:** the plan
  scoped checking to `Stmt::Assign`'s RHS and nested arithmetic only,
  deliberately excluding bare call statements — but the driving example
  itself (`Bump(x)`, a VOID fn called purely for its `writes` effect,
  the realistic shape for ANY effectful call) turned out to BE a bare
  `Stmt::Expr` call statement, which the original scope didn't cover at
  all. Running the driving example against the just-implemented code
  produced 0 errors instead of the expected 1 — caught by testing
  against the example immediately, not assumed correct. Fixed by
  routing `Stmt::Expr` through `expr_bound` for its side effect
  (discarding the meaningless return value) — the realistic shape for
  this feature, not an edge case, so this is a scope WIDENING from the
  plan, not a workaround. `Stmt::Return`'s expr and `if`/`while`
  conditions remain genuinely out of scope (need a generic expression-
  tree walk, a separable follow-up).

  Driving example (invented, same disclosure as v6/v8/v9/v10/v11): `Bump(i
  : [8] where i < 10) { cnt := i + 1 }` called as `Bump(x)`/`Bump(y)`
  from an `if`/`else` (both branches walked regardless of the runtime
  condition, avoiding any scheduling complexity) — `x`'s declared range
  exactly matches, proven; `y`'s declared range (`[0,50)`) doesn't,
  rejected. Without the call-site check, `Bump`'s own proof (sound only
  if callers respect `i < 10`) would be silently unenforced system-wide
  — the actual soundness gap this closes, not just a new diagnostic
  surface. 8 new/updated tests across `tests/parser.rs`/`tests/
  resolve.rs`/`tests/bounds.rs` (parse, self-reference resolve/reject,
  argument accept/reject, unbounded-param no-op, two-sided param bound);
  the call-check logic verified by bug-reintroduction (swapping which
  `DefId` is looked up in `self.bounded` flips the rejection test to a
  wrongly-accepted pass, confirmed then reverted). A `--firrtl` sanity
  check on the driving example confirmed zero codegen impact (`Param`'s
  new fields are consumed only by `bounds.rs`). A normalized
  `--explain-schedule` diff across every existing example came back
  byte-identical. Explicitly still NOT done: `Stmt::Return`/condition
  positions, return-bound propagation, and whether the interval-set
  domain (v11's own open item) is worth building all remain undecided.
- **RESOLVED (v13) — return-bound propagation: a fn/impl's return type
  can carry a `where _ < N` postcondition, checked against every
  `Stmt::Return` in the fn's own body and trusted at every CALL site so
  a caller can compose with the call's own result**
  (`examples/return_bound_check.tr`). Lumi picked this over widening
  v12's checked positions (no new capability) and over revisiting the
  v11-deferred interval-set domain (still assessed as inert). This is
  v12's own mirror in the OTHER direction: v12 checks an argument
  against the callee's declared precondition; this checks the callee's
  own return value against its declared postcondition, then lets that
  provable range flow OUT to the caller (`Expr::Call` returns
  `Some((lower, upper))` instead of unconditionally `None` when the
  callee opted in) — e.g. `total := Bump(3) + Bump(4)` composes cleanly
  when `Bump`'s declared `_ < 20` matches `total`'s own bound,
  impossible before this feature. (The self-reference placeholder was
  the literal identifier `result` through v17, retrofitted to `_` in
  v18 for consistency with mem-element and struct-field bounds.)

  No synthetic `DefId` needed for "the return value": unlike a reg/out/
  param's self-reference (compared by `DefId` equality against an
  existing declaration), a return value is never a named binding
  anywhere in scope. Instead the bound's self-reference position must
  be the literal placeholder identifier `result` (already idiomatic in
  this codebase — a spawned fn's own handle exposes its return value
  via `.result`), checked by TEXT in a new `check_ret_bound_shape`
  (resolve.rs), not passed through ordinary `resolve_expr` (which would
  otherwise error "cannot find `result`"). `bounds.rs` keys the
  postcondition in a SEPARATE map (`fn_ret_bound: HashMap<DefId,
  BoundedDef>`, by the fn's own `DefId`) rather than folding it into
  `self.bounded` (which a return value has no `DefId` to key by); a new
  `ret_width` free fn reads the `[N]`-desugared `bits[N]` shape of the
  `ret` type expression directly (mirroring `const_fold`'s own self-
  contained approach) since there's no `DefId` to look a width up
  through `base_width`. A new `current_ret_bound` field, set once per
  item at the top of `check_item`, lets `Stmt::Return`'s own arm check
  each return site independently — the same way every write site is
  independently checked against a reg's bound. Opt-in, not blanket
  inference: a fn with no declared postcondition still composes to
  `None`, confirmed by a dedicated test.

  **A real soundness hole, caught by an advisor pass before committing,
  not by any test written up front.** The first implementation created
  a `fn_ret_bound` entry purely from the DECLARATION at collection
  time, with no coupling to whether `check_stmt` ever actually reached
  a `Stmt::Return` to check it against — a fn with a declared
  postcondition and an EMPTY body (no `return` at all, which nothing
  upstream requires) compiled clean, and its undischarged postcondition
  was trusted at every call site with zero obligations ever verified.
  Confirmed empirically (a scratch module matching exactly this shape
  produced 0 errors) before being closed by a new `found_returns:
  HashSet<DefId>` + `check_return_site_exhaustiveness`, the same
  defense-in-depth shape `found_writes`/`check_write_site_
  exhaustiveness` already established for regs — except this one is a
  real user-facing compile error, not an internal-invariant panic,
  since there's no independent oracle (unlike `effects.rs` confirming a
  write site exists) proving a return site should be there. The
  reusable lesson: any future declared-contract feature in this arc
  should ask "can this table gain an entry whose corresponding check
  site never actually executes?" before considering itself done.
  Separately confirmed (not assumed) that `Stmt::Return` inside an `if
  let`/`while let` body IS reached by the existing `check_body`
  recursion — no gap there.

  The advisor pass also found the ORIGINAL bug-reintroduction test
  insufficient: it only proved the composition logic was WIRED (swap to
  the wrong map), not that its fencepost was right. An accept-direction
  test structurally can't catch an under-reporting (narrowing) bug,
  since narrowing a propagated range only ever makes an `Add`
  composition easier to satisfy, never harder — a REJECT-direction test
  is needed instead, one whose correct outcome is the error. Added
  `return_bound_composition_uses_the_full_declared_width_not_narrower`
  (tight enough that subtracting 1 from the propagated upper flips it
  from a correct 1-error rejection to a wrongly-accepted 0), and
  confirmed the ORIGINAL composition test's own tight boundary already
  caught the opposite, more dangerous over-widening direction (adding 1
  to the propagated upper flips IT from correct acceptance to rejection)
  — both fencepost directions now pinned, not just one.

  11 new/updated tests across `tests/parser.rs`/`tests/resolve.rs`/
  `tests/bounds.rs` (parse, placeholder resolve/reject, return-value
  accept/reject, composition accept, the fencepost-sensitive reject
  above, the no-return-statement reject above, unbounded-fn no-
  propagation, two-sided return bound). A `--firrtl` sanity check on a
  passing scratch variant confirmed zero codegen impact (`ret_bound`/
  `ret_lower` are consumed only by `bounds.rs`). A normalized
  `--explain-schedule` diff across every existing example came back
  byte-identical. Explicitly still NOT done: a call inside an
  `if`/`while` condition or as a bare argument to another call remains
  unchecked as an argument position (v12's own remaining scope gap,
  untouched by this pass), and whether the interval-set domain is worth
  building remains undecided.
- **RESOLVED (v14) — widened checked positions: a call inside an
  `if`/`while`'s own CONDITION, or an `if let`/`while let`'s own
  `init`, is now checked** (`examples/cond_call_check.tr`). Lumi picked
  this over revisiting the v11-deferred interval-set domain. This is
  pure coverage widening, not a new capability — the last position
  v12/v13's own docs explicitly left open: `narrow_for_condition` only
  ever pattern-matches `cond`'s shape to narrow ranges, never routes it
  through `expr_bound` at all, and `Stmt::IfLet`/`Stmt::WhileLet`'s own
  arms destructured `init` with `..` and never touched it — confirmed
  by direct read, not assumed. New `check_calls_in` walks `cond`/`init`
  via `crate::lower::sub_exprs` (the same generic one-level-children
  helper `elaborate.rs`/`types/stmt.rs`/`firrtl/*.rs` already reuse),
  finds every "outermost" `Call` (stopping the descent the instant one
  is found — `expr_bound`'s own `Call` arm already recurses into ITS
  OWN args, so continuing further would double-check and double-report
  the same site), and checks it via `expr_bound` purely for the side
  effect, the same idiom a bare `Stmt::Expr` call statement already
  uses. Four one-line call sites (`Stmt::If`/`While`/`IfLet`/
  `WhileLet`), no restructuring of the surrounding narrowing/branch
  logic.

  A SECOND, adjacent gap was found empirically while writing this
  feature's OWN tests, not assumed away by the plan (which had
  expected "a call as an argument to another call" to already work for
  free): `Outer(Bump(50))` — a call nested as ANOTHER call's own
  argument — was only reached when the OUTER param (`Outer`'s own) had
  a declared bound to check the argument against; with none, the old
  `Expr::Call` arm's loop `continue`d before ever calling `expr_bound`
  on that argument, silently never checking `Bump(50)`'s own violation.
  Fixed by calling `expr_bound` on every argument unconditionally; the
  bound CHECK itself still only fires when the corresponding param
  actually declares one. Caught by writing
  `call_argument_as_an_argument_to_another_call_is_still_checked` as a
  CONFIRMATION test per the plan, and discovering it wasn't confirming
  anything — the reusable lesson: a plan's "this should already work"
  claim is exactly the kind of thing a dedicated test needs to verify,
  not just assert.

  6 new tests in `tests/bounds.rs` (if-condition, while-condition,
  if-let-init, while-let-init, a deeply-nested-under-an-extra-operator
  condition, the argument-to-another-call fix above), bringing the
  file's own total to 50. Bug-reintroduction on all three fixes:
  temporarily made `check_calls_in` a no-op (all four condition/init
  tests correctly flip to silently accepting), temporarily restored the
  old `continue`-before-`expr_bound` gating (the argument-to-another-
  call test correctly flips), and separately reverted JUST the
  `Stmt::WhileLet` call site alone (its own dedicated test flips too,
  confirming that ONE site specifically isn't dead wiring) — all three
  confirmed then reverted.

  **A second, later advisor pass (after the ones below) found the
  argument-to-another-call gap wasn't an isolated instance — it was one
  case of a reusable shape: gate the recursive `expr_bound` descent on
  whether there's a bound to check against, instead of always
  descending and gating only the CHECK.** Rather than fix each
  occurrence as a separate round, grepped the file for every other
  instance of that shape and found THREE more, all real, all confirmed
  by a scratch file before fixing: `Stmt::Assign`'s own early returns (a
  write to an UNBOUNDED reg, `plain := Bump(50)`, used to `return`
  before `rhs` was ever evaluated), `Stmt::Return`'s own `current_ret_
  bound` gate (a `return Bump(50)` inside a fn with NO declared
  postcondition used to skip its own expr entirely), and `Add`/`Sub`/
  `Mul`'s own chained `self.expr_bound(*lhs, ...)?` into
  `self.expr_bound(*rhs, ...)?` (an unprovable LHS, e.g. an unbounded
  `in` port, short-circuited via `?` before the RHS was ever evaluated
  — `unbounded + Bump(50)` never checked `Bump`'s own argument). All
  four fixed identically: compute the descent UNCONDITIONALLY, gate
  only the eventual check/arithmetic on whether a bound actually
  exists. 3 more tests added (bringing the file's own total to 53), each
  independently bug-reintroduction-verified (temporarily restoring the
  old gated code and confirming the relevant test flips, then
  reverting). The reusable lesson, generalizing past this one feature:
  when a gating-the-descent bug is found, grep the whole file for `if
  let Some(...) = ... { ... expr_bound(...) }` (or an early `?`/`return`
  immediately before an `expr_bound` call) rather than fixing instances
  one at a time as they surface across separate advisor rounds.

  **A THIRD advisor pass, explicitly asked for one more targeted probe
  past the file-wide grep sweep above, found a FIFTH instance the grep
  itself couldn't surface** — because it isn't a gated `expr_bound`
  call at all, it's a position that was never passed to `expr_bound`
  in the first place: `Stmt::Assign`'s own `lhs`. The existing code
  only ever resolved `lhs` when it was a bare `Expr::Ident` (to look up
  a `def`/`bounded` pair); a mem write's own index expression (`m[Bump
  (50)] := 1`, an `Expr::Bracket`) is neither that Ident case nor part
  of `rhs`, so `Bump(50)`'s own argument violation there was silently
  accepted with 0 errors — confirmed via a dedicated scratch file
  before fixing. Fixed by sweeping `lhs` itself through `check_calls_in`
  unconditionally, the same walker already used for `if`/`while`
  conditions and `if let`/`while let` inits — reusing the existing
  "outermost Call, stop descending" mechanism rather than writing a new
  one. 1 more test (`call_argument_in_a_mem_write_index_is_still_
  checked`, bringing the file's own total to 54), bug-reintroduction-
  verified (temporarily removed just that one `check_calls_in(lhs, ...)`
  call, confirmed the test flips from 1 error to 0, then restored).
  The lesson this adds past the "grep for the shape" one above: a
  grep sweep only finds gated calls that already exist — it can't find
  a position that was never wired to `expr_bound` at all. Worth a
  distinct, deliberate "which expression positions in this file are
  NEVER reached by any `expr_bound`/`check_calls_in` call, gated or
  not" pass, not just a grep for one specific gating shape.

  An advisor pass surfaced two more things before committing, both
  resolved by direct empirical checks rather than assumed: (1) whether
  `while`'s own lowering (`lower/render.rs`'s `while_loop_header`,
  which re-renders a `while` as an `if`-shaped structure for FIRRTL
  emission) could cause `check_calls_in` to run TWICE on the same
  condition, double-reporting an error — traced to `main.rs`'s own
  pipeline order (`bounds::check` runs exactly once, on the original
  AST, well before any lowering/rendering happens) and confirmed
  empirically (`while Bump(50) < 5 { ... }` under `--firrtl` reports
  exactly ONE error, not two). (2) `Stmt::WhileLet`'s own new call site
  had ZERO test coverage initially — none of the other bug-
  reintroductions happened to flip through it — closed by the dedicated
  while-let test and its own targeted bug-reintroduction above; along
  the way, confirmed `WhileLet`'s `init` (unlike `IfLet`'s, which also
  accepts a bare fifo op or failing call) is genuinely restricted to
  `Expr::Guard(inner)` over `Ty::Option(T)` — but `inner` itself CAN be
  a `?T`-returning fn's own call (`examples/call_struct_return.tr`'s
  `Wrap` shape), which `check_calls_in`'s recursion through `Guard`'s
  own child already reaches correctly. A `--firrtl` sanity check on a
  passing scratch variant confirmed zero codegen impact (this pass only
  adds new error sites to bounds.rs, touching no AST fields and no
  codegen). A normalized
  `--explain-schedule` diff across every existing example came back
  byte-identical (rerun after all five gating/coverage fixes landed).
  This closes out the position explicitly named in v12/v13's own
  "still NOT done" language (a call inside an `if`/`while` condition);
  the five gating-the-descent/never-reached fixes above were a
  separate class of gap, found empirically rather than pre-existing in
  any doc's "still NOT done" list — not asserted to be exhaustive over
  every possible instance of that shape (the mem-write-index gap alone
  proves a grep for the gating shape isn't exhaustive by construction),
  just every one this file's grep sweep plus one further advisor probe
  surfaced. Whether the interval-set domain is worth building remains
  open — resolved next, by v15 below.
- **RESOLVED (v15) — `else`-branch negated-condition narrowing, in
  place of the interval-set domain** (`examples/
  else_branch_narrowing.tr`). Before writing any code, checked (this
  arc's own standing discipline since v11) whether the v11/v13/v14-
  deferred interval-set domain was finally worth building now that
  `Mul` exists — found it's PROVABLY inert, and MORE broadly than v11's
  own finding: every check in `bounds.rs` (write-bound, width-clamp,
  `Sub`'s own fail-closed condition) reads only a range's EXTREMES, and
  `Add`/`Sub`/`Mul` are all monotonic given unsigned operands, so
  punching an interior hole in a range can never move a downstream
  min/max — true for ANY monotonic composition, not just the ones that
  happen to exist today. A second, independent reason it's inert:
  `schedule.rs`'s own mem-disjointness proofs (the consumer `bounds.rs`'s
  own module doc names as the whole point of a proven bound) only ever
  read a def's FLAT, whole-program declared range (`Bounds.ranges`,
  populated straight from the declared `where` clause) — no per-branch
  narrowing, edge or hole, ever reaches that consumer at all, so an
  interval-set wouldn't change what `schedule.rs` sees either, without a
  separate (much bigger) feature to export narrowed per-site ranges in
  the first place. Confirmed with advisor before presenting it back,
  same shape as v11's own "is this inert" check.

  Found instead: `if`'s own `else` branch inherited the RAW, unnarrowed
  entry state through v14 — never narrowed on the NEGATED condition,
  even though that's just as real a proven fact as the condition
  itself. `narrow_for_else` mirrors `narrow_for_condition`'s `Lt`/`Ge`/
  `Gt` formulas in the opposite direction (`else` of `i < k` is `i >=
  k`, etc. — `Gt`'s own mirror needed a genuinely new formula, since
  this language's `Le` isn't recognized by `narrow_for_condition` at
  all); `Ne`'s own negation is the one truly NEW capability — `else` of
  `i <> k` is the EXACT singleton `i == k`, sound for ANY `k`, mid-range
  included, unlike `narrow_for_condition`'s own `Ne` arm (edge-only,
  v9's documented no-op for an interior exclusion). A singleton needs
  no interval-set at all — it's an ordinary one-piece interval — so
  this is the actual non-inert capture of the same underlying idea the
  interval-set domain was chasing, built entirely inside the
  representation already in place. Guarded so the excluded `k` must lie
  within the CURRENT `[lo,hi)`: outside it, the `else` branch is
  unreachable dead code (the excluded value could never have been the
  real one), so leaving it unnarrowed there stays sound rather than
  risking a bogus out-of-declared-range interval.

  An advisor pass caught a broader version of that same hazard before
  committing: THREE of the four arms, not just `Ne`, could produce a
  degenerate range — `Lt`/`Gt`/`Ge` all degrade to an EMPTY range
  (`lo >= hi`) whenever the condition is always true for the def's
  current bound (`cnt >= 0` on an unsigned `cnt` computes the empty
  `(0, 0)`), and inserting that unclamped let a write with no real
  relationship to the def's actual range slip through as a coincidental
  VACUOUS accept in two of the three cases, rather than the deliberate
  "leave dead code on the raw entry state" fallback the `Ne` guard
  already modeled. Fixed by replacing the `Ne`-only guard with one
  uniform rule covering all four arms: every raw result is CLAMPED
  against the def's own current `(lo, hi)` first, then only inserted if
  that clamp is non-empty — `Lt`/`Gt`/`Ge`'s raw results already derive
  from `lo`/`hi` via `max`/`min` so the clamp is a no-op for them
  (only the emptiness check bites), while `Ne`'s raw `(k, k+1)` doesn't
  derive from `lo`/`hi` at all, so the SAME clamp is what keeps a
  wildly out-of-range `k` from ever being inserted — subsuming the
  original ad hoc guard instead of needing a second one. (The advisor's
  first-pass suggestion — just check `new_lo < new_hi` with no clamp —
  was insufficient on inspection: `Ne`'s raw `(k, k+1)` is non-empty for
  ANY `k`, so that alone wouldn't have closed the out-of-range hazard at
  all; the clamp-then-check version was needed to cover both.)

  7 new tests (`tests/bounds.rs`, bringing the file's own total to 61):
  one per mirrored operator (`Lt`/`Gt`/`Ge`), the `Ne` singleton
  (deliberately using a genuinely MID-range `k`, not an edge, to
  discriminate from `narrow_for_condition`'s own existing edge-only
  handling), its own commuted form (`<const> <> <reg>`, a live path
  through the `Ne`-only `or_else` fallback that had zero coverage until
  a final advisor pass named it — the same "found a real call site with
  no test" shape as v14's own `Stmt::WhileLet` gap), a dedicated "k
  outside the current range stays unnarrowed" case, and (added after
  the SECOND advisor pass) a dedicated "always-true
  condition stays conservatively unnarrowed, not vacuously accepted"
  case for the empty-range hazard. Bug-reintroduction on all of them:
  temporarily reverted the whole `else`-branch call to `state.clone()`
  (the old v14-and-earlier behavior) — flips 4 of the first 5 tests
  from passing to a spurious rejection; separately reverted just the
  clamp-and-emptiness-check to an unconditional insert of the raw,
  unclamped result — flips BOTH the out-of-range-`k` test and the
  always-true-condition test at once, confirming the single unified fix
  covers both hazards together, not as two independent guards. Driving
  example and its own discriminating baseline confirmed before
  implementing (`examples/else_branch_narrowing.tr`'s write only
  compiles given the else-narrowed range; 1 error on pre-v15 code, 0
  after). `cargo fmt`/`clippy`/`test` all clean. A normalized
  `--explain-schedule` diff across every existing example came back
  byte-identical except the new driving example, and a `--firrtl`
  sanity check on the driving example confirmed clean codegen. This
  closes the last open item from the whole bounds.rs arc (v9 through
  v14's own "still NOT done" language) — not by building the
  interval-set domain Lumi originally asked about, but by finding and
  building the concrete, non-inert capability sitting next to it once
  the inertness was actually checked rather than assumed either way.
- **RESOLVED — `Expr::Bracket` (a mem/fifo access) now recurses into
  `callee`/`args` for the side effect of checking any nested `Call`**
  (`examples/mem_read_call_check.tr`). Found empirically while
  designing v16 below (exporting this pass's own per-site facts to
  `schedule.rs`): `Bracket` had no arm in `expr_bound` at all through
  v15, falling to
  the catch-all `_ => None` with ZERO recursion — so a mem access used
  as a VALUE, not an assignment target (`y := m[Bump(50)]`, `return m
  [Bump(50)]`, `Outer(m[Bump(50)])` as a call argument), silently
  skipped a nested call's own argument obligations entirely, since
  every one of those positions routes through this same shallow
  `expr_bound` call. Confirmed via a driving scratch file (0 errors on
  all three sibling positions) before fixing, per advisor's explicit
  direction to ship this as its own small commit rather than bundle it
  into the larger feature it was found while designing (the same call
  made at v10 for the `Ne`-commuted-order fix). Fix: the arm still
  composes to `None` (a mem access's own value has no provable bound,
  unchanged) but now calls `check_calls_in` — the established "find
  every outermost Call, check it, discard the bound" idiom already used
  elsewhere in this file — on `callee` and every arg first. 3 new tests
  (`tests/bounds.rs`, file's own total 64), each bug-reintroduction-
  verified (reverting the arm to `Expr::Bracket { .. } => None` flips
  all three from passing to a spurious accept).
- **RESOLVED (v16) — per-site proven ranges exported to `schedule.rs`**
  (`examples/mem_site_narrowing.tr`). Lumi asked "time for the type
  system proper?" — surveyed what's actually left rather than guessing:
  every prior "the type system" ask (v4, v12, v13, v15) resolved to a
  specific, scoped gap once investigated, never the big rewrite, and
  the concrete gaps were nearly exhausted except one. `bounds.rs`'s own
  `Bounds.ranges` only ever exports a def's FLAT, whole-program
  DECLARED range; `schedule.rs`'s mem-disjointness proof (`real_range`)
  had zero visibility into any branch-local narrowing `bounds.rs` proves
  internally during its own forward walk (`if i < 10 { m[i] := x }`
  proves a tighter fact for THIS site than `i`'s raw declared bound, but
  `schedule.rs` only ever saw the latter, however wide). This is the
  first capability in the whole arc a bolted-on post-pass structurally
  cannot express — needs an actual per-`ExprId` fact table populated
  during the existing forward walk, not a per-def one. Lumi picked this
  over `where` on struct fields/mem elements via `AskUserQuestion`.
  Verified as a non-issue, not assumed: promoting `bounds.rs`'s own
  refinements into a real `Ty::Refined` variant in `types.rs` was
  already offered at v12 and declined — no proving power over what
  exists, just a much larger implementation shape. Not part of this.

  Found while investigating and shipped separately first (see the
  `Expr::Bracket` entry directly above, per advisor's explicit "ship the
  bug apart from the feature" direction, the same call made at v10).

  Design: new `Bounds.site_ranges: HashMap<ExprId, (u64, u64)>`.
  `check_calls_in` widened from stopping only at `Call` to stopping at
  EVERY shape `expr_bound` has a dedicated arm for (`Int`, `SizedInt`,
  `Ident`, `Add`/`Sub`/`Mul`, `Call`, `Bracket`), and given a real return
  value (previously discarded `()`) — the exact same "don't double-check
  a shape `expr_bound` already fully recurses through" reasoning that
  justified stopping at `Call` alone now applies uniformly to the wider
  list. `expr_bound`'s own `Bracket` arm captures that value and exports
  it, keyed by the index's own `ExprId`, only when `callee` resolves to
  a `mem` (a fifo shares this exact `Bracket` shape but has no consumer
  — `effects.rs`'s `mem_read_idx`/`mem_write_idx`, the only reader of a
  per-site mem-index fact, are keyed by mem `DefId` specifically). No
  other call site needed to change: every existing checked position
  (`If`/`While` cond, `IfLet`/`WhileLet` init, `Assign`'s lhs AND rhs,
  `Return`'s expr, a call's own arg loop, `Add`/`Sub`/`Mul`'s own operand
  recursion) already routes through one of these two functions, both now
  handling `Bracket` uniformly, so a mem access anywhere gains the
  export automatically. `schedule.rs`'s `real_range` consults `bounds.
  site_ranges` first, falling back to its own independent walk
  unchanged — consulted-then-fallback, never replaced, keeping the
  whole proof fail-closed for any `ExprId` this pass never visited. Its
  own stale doc comment (claiming it recognizes "the same restricted
  shape `expr_bound` does") was corrected — `expr_bound` gained `Sub` at
  v7 and `Mul` at v11, `real_range`'s own independent walk still hasn't.

  A design fork surfaced mid-investigation: naively calling `expr_bound`
  alone on a mem index (to get both the export AND the call-check in one
  call, avoiding a double-check) would have silently dropped `check_
  calls_in`'s own thorough recursion through shapes `expr_bound` doesn't
  reach at all (`Field`, `Guard`, `StructLit`, ...) — confirmed
  empirically, not assumed, that this is a REAL reachable shape
  (`m[SomeStructCall().data]` parses and type-checks as a legal mem
  index). Resolved by widening `check_calls_in`'s OWN stop-list instead
  of introducing a second, parallel traversal: since `expr_bound`
  already recurses fully through every shape now in that widened list,
  `check_calls_in` dispatches to it once per top-level recognized shape
  and still falls through generically for everything else, so a call
  hidden under `Field`/`Guard`/etc. is still found via that generic
  path. An earlier advisor-suggested simplification (a bare `new_lo <
  new_hi` check with no clamp, from the UNRELATED v15 conversation) was
  explicitly NOT reused here without re-deriving it for this shape —
  the two problems aren't the same.

  Soundness: cited the SPECIFIC invariant, not a generic "this site
  proves it" claim — schedule.rs's own pre-edge-read argument (every
  rule sees a shared reg's IDENTICAL frozen value within one cycle,
  regardless of which rule fires) is what makes a per-site fact proven
  in ONE rule's own body a valid fact for a CROSS-RULE pairwise
  comparison, not just "true within that one rule." No-collision
  verified structurally, not assumed: no `ExprId` is ever visited by
  this pass's forward walk more than once under a different state
  (`collect_bodied_items` yields each Rule/Fn item exactly once; each
  item's body is walked once; `if`/`while` branches are disjoint
  subtrees walked once each; no fn is ever inlined per call site) — a
  plain `insert` is correct, no merge-on-conflict needed.

  `examples/mem_site_narrowing.tr`: two regs `i`/`j`, each merely
  declared `< 20` (individually insufficient — identical, fully-
  overlapping declared ranges), each narrowed by a DIFFERENT `if` guard
  in ITS OWN accessing rule to a disjoint half (`if i < 10` in `write`,
  `if j >= 10` in `read`). Discriminating baseline confirmed before
  implementing: NOT a compile error either way (there's no
  `conflict_free` annotation on this file at all, so there's no trusted
  fallback to fall back to) — pre-fix, the scheduler derives a real
  STALL between the two rules (`--explain-schedule` shows `read` waits
  on `write`); post-fix, it proves disjointness automatically
  (`--explain-schedule` shows "index sites proven disjoint... no stall
  derived") and removes it, with zero annotation needed. (First attempt
  at this example used an explicit `conflict_free { write, read }`
  annotation, which turned out to short-circuit the auto-detection path
  entirely — `schedule.rs`'s own auto-proof is only ever attempted when
  there's NO user annotation at all; caught by checking the actual
  scheduling output rather than assuming the annotation was harmless.)

  5 new tests: 4 in `tests/bounds.rs` (`site_ranges` populated with the
  NARROWED, not flat-declared, value at a write, a read, a return, and
  a call-argument position — file's own total 69 with the regression
  test below), 1 in `tests/schedule.rs` (the driving example's own
  schedule has exactly one conflict, exempted via `Exemption::Disjoint`)
  — plus a dedicated regression test pinning the `Field`-hidden-call
  design-fork resolution above. Every test bug-reintroduction-verified,
  including two JOINT reintroductions that each flip multiple tests at
  once: disabling `real_range`'s own `site_ranges` lookup flips the
  schedule test; skipping the `site_ranges.insert` in `expr_bound`'s
  `Bracket` arm flips all 4 `site_ranges` tests AND the schedule test
  together; reverting `check_calls_in`'s stop-list to `Call`-only flips
  the same 4 `site_ranges` tests (confirming the widening itself, not
  just the export line, is load-bearing) with no impact on the other 68
  pre-existing tests (confirming the widening doesn't regress anything).

  The stated `--explain-schedule` prediction (byte-identical across
  every EXISTING example, checked file-by-file before running the diff,
  not guessed after seeing a surprise): `checksum`/`fifo2`/`port_ram`/
  `rmw`/`subleq*` use literal or unconditional/undeclared-bound indices;
  `conflict_free_mem`/`mem_write_branch` index via untracked `in` ports;
  `mem_disjoint_affine`/`banked` are `IndexForm`/`pow2_addr_width`-
  driven, never touching `Bounds` at all; `mem_disjoint_bounded`/
  `ranges` are already satisfied by the FLAT declared bound alone (no
  narrowing wraps the actual mem access in either file's own accessing
  rule bodies). Confirmed exactly as predicted — only the new driving
  example differed. `cargo fmt`/`clippy`/`test` all clean; a `--firrtl`
  sanity check on the driving example confirmed clean codegen (a
  separate mem read port, no stall-mux serialization needed). Plan mode
  was used for this one (the first since v5) given it genuinely touches
  two subsystems' own contract, not just `bounds.rs` alone.
- **RESOLVED (v17) — `where` on mem elements, WRITE-side only: a mem can
  now declare a statically-checked bound on every value ever WRITTEN to
  it** (`examples/mem_elem_bounded.tr`). At v16's own decision point,
  "where on struct fields / mem elements" was the alternative NOT
  picked; picked up directly here — Lumi asked to keep pushing on "the
  type system," offered struct-field `where` (believed at the time to
  be smaller, pure plumbing, the same shape as v8's `out` extension —
  `Param`, shared by fn params AND struct fields, already carries
  `bound`/`lower` in the AST) against mem-element `where` (believed the
  genuinely new capability) via `AskUserQuestion`, and picked mem
  elements. (v18's own investigation later found this backwards: struct
  fields turned out to support sound READ composition too, which mem
  elements' lack of init/reset ruled out below — see v18's own entry.)

  Grammar: `mem m : [8][20] where _ < K` (optionally `where L <= _
  < K`), reusing `parse_where_bound` completely unchanged — it already
  parses the `where <expr> [<= <expr>] < <expr>` shape structurally with
  no awareness of what the self-reference resolves to. The self-
  reference is `_` (`Expr::Wildcard`; the literal identifier `elem`
  through v17, before v18's retrofit), not the mem's own
  declared name — mirrors the fn return bound's own placeholder (v13)
  exactly, and for the same reason: a mem element has no scoped `DefId`
  of its own to compare against (unlike a reg/out/param's bound, a real
  binding already in scope). Checked by SHAPE in a new `resolve.rs`
  function, `check_mem_bound_shape`, mirroring `check_ret_bound_shape`
  verbatim in shape rather than generalizing it to take a placeholder
  string — matches this file's own stated convention (`bounds.rs`'s
  `collect_one_ret_bound` doc comment: share a helper only across near-
  identical branches WITHIN one function, not force one across two
  conceptually separate top-level checks).

  `Item::Mem` gained `bound`/`lower` fields identical in shape to
  `Item::Reg`/`Item::Output`'s own; every existing exhaustive match on
  `Item::Mem` in the codebase already destructured with `..` (confirmed
  by grep before touching the AST), so no other file needed to change to
  keep compiling. `bounds.rs`: a new `mem_bounds: HashMap<DefId,
  BoundedDef>`, kept SEPARATE from the map a reg/out/param populates —
  a mem's bound is a flat, whole-array fact checked at every write site,
  never narrowed per-branch the way that map's per-item `state` clone
  is, the same reason `fn_ret_bound` is kept separate. Its own width
  comes from a new `mem_elem_width` helper, reading one layer deeper
  than `base_width` does (`Ty::Mem { elem, .. }` in `state_tys`, not a
  bare `Ty::Bits` directly) — kept as its own function rather than
  unifying with `base_width`/`ret_width`, same reasoning: the width
  SOURCE genuinely differs each time. The write-site check (`Stmt::
  Assign`'s arm) gained a new branch: when `lhs` is `Expr::Bracket
  {callee, ..}` and `callee` resolves to a `mem_bounds` entry (the
  identical mem-recognition idiom v16's own `Bracket` arm already
  established), check `rhs`'s already-computed bound against it and
  record the write into `found_writes` — the same set a reg/out write
  already populates.

  **`check_item`'s own early-return guard had to widen from a two-way to
  a THREE-way check** (bounded-def/return-bound/mem-bound maps all
  empty) — flagged explicitly during planning, before any code was
  written, as the same "gated the descent on the wrong condition" class
  of bug v14 found four times in a row: a program with ONLY a bounded
  mem and no bounded reg/out/param/return anywhere would otherwise skip
  the whole body walk, silently letting every mem write through
  unchecked. A dedicated driving example with NO other bounded def
  anywhere in the module (`examples/mem_elem_bounded_only.tr`) was
  written specifically to discriminate this guard, not just the write
  check in general.

  **A soundness hole in the read side, found by an advisor pass right
  before this was going to be committed — the most significant finding
  of this feature.** The first implementation ALSO handed the mem's
  declared bound back at read sites (`expr_bound`'s `Bracket` arm
  returning `Some((lower, upper))` on a hit instead of `None`), composing
  through `Add`/`Sub`/`Mul`/further write checks for free. This is
  UNSOUND: the write-site induction proves "every value ever WRITTEN
  satisfies the bound," which is not "every value a READ returns is
  in-range" — unlike a reg/out, a mem has no `init`/reset at all, so a
  read at an address never written (or in an early cycle before the
  corresponding write happens) returns uninitialized data the write-site
  proof never touched. Concretely demonstrated against the feature's OWN
  shipped example: `write-latency => 1`/`read-under-write => old` in the
  emitted FIRRTL means `total := m[i]` at cycle 0 reads garbage, not
  something `bounds.rs` "proved" anything about. Worse, the unproven
  value could reach `let a = m[pc]`, then `m[a]`, exporting a FABRICATED
  range into `Bounds.site_ranges` — `schedule.rs`'s own disjointness
  proof (v16) would trust it, exactly `subleq.tr`'s own shape (an index
  loaded out of the mem itself), turning a false compile-time proof into
  a real hardware aliasing bug. This is NEWLY introduced by this feature,
  not pre-existing: through v16 a mem read composed to `None`
  unconditionally, so the compiler made no claim at all there.

  Of the two fixes the advisor offered that fit repo precedent (reframe
  as a trusted-at-read contract, matching how `where` on `in` is already
  rejected as "a different feature entirely"; or emit a checked
  simulation assertion at read sites, matching `conflict_free_mem`'s own
  precedent), NEITHER was taken: a trusted contract is exactly what this
  module's whole existence argues against (proof, not trust), and a
  simulation-only assertion still wouldn't stop the fabricated
  `site_ranges` entry from reaching a REAL, synthesized-hardware
  disjointness proof with no runtime check at all in silicon. Instead,
  read-side propagation was DROPPED entirely: `expr_bound`'s `Bracket`
  arm's own value composes to `None` again, exactly as before this
  feature existed — `total := m[i]` still fails as an unsupported
  expression shape. Confirmed directly, not just reasoned about: re-ran
  the advisor's own counterexample shape (`let a = m[pc]; total := m[a]`
  with `m` written elsewhere) through `--explain-schedule` post-fix and
  confirmed it falls back to a derived stall, never a fabricated proof.
  This narrows the feature's final scope from what was first
  implemented and documented: a genuinely useful, SOUND write-side proof
  ("catches an out-of-range value being written to this mem"), and
  nothing about reads.

  A declared mem bound with zero write sites anywhere in the program is
  still dead, misleading metadata even with reads no longer trusting it
  — narrower in reach than v13's own return-bound version (not closing a
  read-trust hole, since none remains), but still worth a real error:
  closed with a new `check_mem_bound_is_proven` (mirrors `check_return_
  site_exhaustiveness`) — NOT redundant with the existing `check_write_
  site_exhaustiveness` (a panic, an internal-consistency check against
  `effects.rs`'s own independent oracle), which was ALSO widened to
  cover `mem_bounds`'s own keys, since a mem write is already `sig.
  writes.insert(mem_def)` there exactly like a reg/out's own write.

  Bug-reintroduction verification, all independent: disabling the
  write-site check flips the 2 out-of-range-write tests; removing the
  `check_mem_bound_is_proven` call flips the never-written test;
  reverting the three-way guard flips the no-other-bounded-def test; and
  (before the fix) re-enabling read-side propagation flips the dedicated
  regression test added specifically to pin its absence. **Found
  empirically, not planned: disabling either the write-site check OR the
  three-way guard trips the internal PANIC check first** (`effects.rs`
  still says the mem was written somewhere, this pass's own walk now
  disagrees) — a stronger safety net than the plan anticipated, since it
  turns what would otherwise be a silent false-accept into a loud
  internal-error crash instead.

  5 new tests in `tests/bounds.rs` (write-within-bound, write-exceeding-
  bound, a regression guard pinning that a mem read still does NOT
  compose — so a future change can't silently reintroduce the soundness
  hole — never-written-is-rejected, and the no-other-bounded-def guard
  test), 2 in `tests/resolve.rs` (the `elem`-placeholder shape check,
  mirroring `result`'s own coverage exactly). Also 1 new test in
  `tests/schedule.rs`, `mem_read_derived_value_does_not_feed_the_
  disjointness_proof`, built around `let a = m[pc]; total := m[a]` —
  confirmed passing on the fixed code (0 errors, `Exemption::None`), but
  honestly NOT bug-reintroduction-verified via this exact scenario: a
  `Local` (`let a = ...`) is never recognized as a valid `IndexForm`
  base by `state_base` (`DefKind::is_state()` excludes `Local`), so this
  particular shape structurally can't reach `forms_differ`'s disjointness
  argument regardless of the read-propagation bug. Investigating why led
  to a MORE severe, more directly exploitable variant: a bounded REG used
  as the intermediate value instead of a `Local` (e.g. `reg cached : [8]
  where 10 <= cached < 20 = 10; cached := m[pc]`) — a reg IS a state def,
  so `state_base` DOES recognize it, and `cached`'s own pre-existing
  declared range (a v5-era mechanism, independent of `site_ranges`) would
  be poisoned by an unproven mem-read value. Confirmed this variant
  reproduces the historical bug when the read-propagation fix is
  temporarily reverted, and confirmed the single shipped fix (`expr_
  bound`'s `Bracket` arm returning `None` unconditionally) closes both
  variants at their common source, since neither a `Local` nor a `reg`
  can ever receive a "proven" value out of a mem read anymore. The
  `tests/bounds.rs` regression test (`mem_elem_read_value_is_
  deliberately_not_composed`) is the one that actually discriminates the
  fix via bug-reintroduction; the `tests/schedule.rs` test is kept as a
  scheduler-level sanity check on the same scenario, not as this
  feature's primary regression guard.

  `cargo fmt`/`clippy`/`test` all clean across every binary, re-run after
  the soundness fix, not just before it. A normalized `--explain-
  schedule` diff across every existing example came back byte-identical,
  both before and after the fix (this feature touches no scheduling
  logic at all — pure `parser.rs`/`resolve.rs`/`bounds.rs` plumbing plus
  one new AST field, the same class of change as v8/v9's zero-`schedule.
  rs` entries). A `--firrtl` sanity check confirmed clean codegen through
  both the compiler's own emitter and real `firtool` — the new AST
  fields are consumed only by `bounds.rs`, never codegen. This closes
  the mem-element half of the alternative left open at v16's own
  decision point; struct-field `where` remains the other half, still
  unbuilt and undecided.
- **RESOLVED — a `conflict_free` mem read/write pair the disjointness
  proof above can't close now gets a checked runtime assertion, not just
  a trusted claim** (`firrtl/module.rs`'s `conflict_free_mem_check_N`,
  `examples/conflict_free_mem.tr`). This is not the tier-3 range-tracking
  gap above closing — it can't be, since the driving case
  (`write_addr`/`read_addr`, module inputs) has no provable value set at
  all for a static proof to ever bound — it's a DIFFERENT, complementary
  answer for exactly that shape: the two addresses are real compiled
  port signals, so the claim's own precondition (they differ) is
  checkable in simulation even though it isn't provable at compile time.
  Scoped to read sites that are unconditional top-level statements in
  their rule (`read_addrs` in `module.rs`) — a branch-nested read is
  silently excluded rather than risking a false-positive assertion on a
  correct design, since read ports are wired up unconditionally
  regardless of which branch a cycle actually takes. Non-mem shared
  state in a `conflict_free` pair stays fully trusted/unchecked, as
  before.
- `combines` combinational-loop checking delegates entirely to firtool's
  `CheckCombLoops`, run only by `devenv.nix`'s `simulate` script and
  `tests/sim.rs` — never by `trace` itself, and its diagnostics are never
  remapped to `.tr` source spans.
- **CORRECTED — this doc previously cited the wrong CIRCT issue for
  `CheckCombLoops`'s known blind spot.** It's #7435 (open: a blind spot
  when a circuit has more than one `public module`), not #1138 (closed —
  an unrelated, already-fixed single-module bug). Not currently reachable
  through `trace`'s own output either way: `firrtl::emit` hard-errors
  unless a circuit has exactly one top module, and only that module is
  ever emitted `public module` — see DESIGN.md's "Combinational loops:
  what the checker does" for the full correction.

## Simulation

- **RESOLVED — Icarus only; no Verilator path.** The stated blocker
  ("would need `--public`/`--public-flat-rw` wiring for the
  hierarchical-path testbenches") didn't hold for the invocation that
  actually matters, the same shape of correction as the CheckCombLoops
  citation two entries above: `--public`/`--public-flat-rw` govern a
  C++/DPI harness reaching into an internal signal from OUTSIDE the
  Verilog design, and nothing here does that. `verilator --binary
  --timing` instead elaborates the hand-written SV testbench itself as
  the simulation's own top, DUT included, so `subleq_tb.v`'s/
  `fifo_bridge_tb.v`'s hierarchical peeks and pokes are ordinary intra-
  design SystemVerilog references — confirmed empirically, not just
  inferred (both run clean, no `--public` flags, no Verilator warning
  about signal visibility). `devenv.nix`'s `simulate` script gained an
  optional second positional arg (`simulate <name> [iverilog|
  verilator]`, default `iverilog`, unchanged), version-pinned
  (`verilator 5.050`) the same way firtool/iverilog already are;
  `packages` gained `pkgs.verilator` and `pkgs.python3` (the latter
  purely because Verilator's own `--binary` build step shells out to
  it). 3 new tests in `tests/sim.rs` (not a full second copy of the
  46-example `iverilog` suite — functional correctness is already fully
  proven there; these three prove the Verilator INTERFACE works, one
  per genuinely distinct access shape: `accumulator` for plain module
  ports, `subleq` for hierarchical peek/poke including a nested
  submodule's own memory array, `extmodule_tribuf` for a blackbox `.v`
  source compiled alongside the design). See sim/README.md's new
  "Verilator" section for the full mechanism and the exact correction.
- **RESOLVED — no way to write testbenches in trace itself.**
  `examples/tb_accumulator.tr` proves BOTH halves need no new language
  surface. Stimulus: its `Top` `inst`s `Accumulator` and drives it across
  real cycles via a `<sequences>` rule, ordinary instance-port writes,
  `tick`-separated segments — the same machinery `sequences`/`tick`
  already exist for. Checking the result: the rule's own last statement
  is a bare comparison (`dut.sum = 26`) — fallible by default, the same
  mechanism `?`/fifo ops/failing calls already fold into a rule's guard
  via `writes.rs`'s `comparison_conds`. Folded into a lowered
  `<sequences>` rule's LAST segment, that comparison becomes a checkpoint
  for free: holds → the segment fires and `__cont_drive` wraps back to
  0; fails → the segment never fires and `__cont_drive` sticks there
  forever, a directly-observable stall rather than a silent wrong answer.
  Initially assumed this needed a new `assert` construct (a statement-
  position version needing enable-gating derived from `writes.rs`'s
  guard-folding machinery PLUS a new `Stmt` variant threaded through
  ~30 exhaustive match sites — this codebase's own known silent-
  miscompile shape, see this doc's `Stmt::Return` entry) — turned out to
  be unnecessary: the guard-fold ALREADY does exactly this for a bare
  comparison in any rule, `<sequences>`-lowered or not, so a checkpoint
  is just a comparison in the right position, zero new syntax. Verified
  both directions empirically before committing to the design: a
  deliberately-wrong checkpoint (`examples/tb_accumulator_failing.tr`,
  `dut.sum = 99`) sticks at segment 6 under both firtool+iverilog and
  firtool+Verilator (`tests/sim.rs`'s
  `tb_accumulator_checkpoint_failure_stalls_and_is_observable`), and the
  real one runs to completion (`tb_accumulator_drives_a_dut_through_
  real_cycles`, plus its Verilator twin). `Top` now has no output ports
  at all — `sim/tb_accumulator_tb.v`'s job is clock/reset generation and
  a hierarchical peek at `dut.__cont_drive` (the same access pattern
  `fifo_bridge_tb.v` already uses for a port-less DUT), reporting which
  segment a stall happened at. `devenv.nix`'s `simulate` script needed
  ZERO changes to run either example under either backend — both plug
  into the exact same infrastructure any other example does. See
  DESIGN.md's "Testbenches written in trace" section for the full story.
- **RESOLVED — `firtool`/`iverilog` version pin.** `devenv.nix`'s
  `simulate` script now asserts the exact versions this project's
  scripts/tests are written against (`firtool-1.147.0`, iverilog `13.0`)
  before doing any real work, failing with a clear message naming what
  was found instead — the CLI behavior HAD already drifted once
  silently (firtool started needing `-format=fir` for stdin, with no
  signal until something else broke confusingly downstream); this turns
  a future drift into an immediate, actionable failure instead. Doesn't
  touch `devenv.yaml`'s own nixpkgs pin (still tracks the `rolling`
  branch, re-resolved on `devenv update`) — this check is a second,
  independent line of defense, not a replacement for pinning nixpkgs
  itself, deliberately: it also catches a differently-provisioned
  environment (system-installed tools, a different channel) that never
  goes through this project's own lockfile at all.
- **RESOLVED — `simulate`'s `extmodule` `.v`-discovery gap.** Was:
  `devenv shell -- simulate extmodule_tribuf` failed at iverilog
  elaboration (`Unknown module type: TriBuf ... referenced 2 times`) —
  the script only ever passed one generated `.v` plus one testbench to
  iverilog, with no way to also supply an `extmodule`'s own real
  implementation. Fixed by having `simulate` grep the ORIGINAL example
  source (not the elaborated/lowered intermediates — an `extmodule`
  item passes through both passes untouched, but the original is the
  one guaranteed to exist regardless) for every `extmodule ... from
  "path.v"` declaration, resolving each `path.v` relative to `sim/` —
  the convention `tribuf.v`'s own placement already established, now
  made real rather than just documented — and passing each resolved
  file to iverilog alongside the generated design. A referenced `.v`
  file that doesn't exist under `sim/` is a clear, immediate error
  naming the missing path, not a confusing iverilog elaboration
  failure. Confirmed directly: `devenv shell -- simulate
  extmodule_tribuf` now runs and reports `SIMULATION PASSED`.

## Editor tooling (`editors/vscode/`)

- No language server. **RESOLVED.** `trace --lsp` (`src/lsp.rs`) now backs
  diagnostics, go-to-definition, and hover, driving the exact same `lex ->
  parse -> resolve -> effects -> types` pipeline `main.rs` uses for the
  CLI — no second, drifting implementation. Full-document sync (every
  `didChange` recompiles the whole buffer — cheap at these file sizes),
  UTF-16 code-unit positions (`LineIndex` in `src/lsp.rs` converts
  to/from this compiler's own byte-offset `Span`s) — tried the cheaper
  `positionEncoding: "utf-8"` first (spec-legal since LSP 3.17, and this
  compiler's `Span` is already byte-based, so it needed zero conversion),
  but `vscode-languageclient` hardcodes `positionEncodings: ['utf-16']`
  in what it advertises and rejects anything else outright — caught
  immediately on first real VS Code use ("Unsupported position encoding
  (utf-8)"), not by any test here, since the JSON-RPC smoke test client
  never negotiated capabilities as strictly as a real client does. And
  diagnostics/definition/hover all degrade in step with `main.rs`'s own
  early-return-on-error chain (a parse error means no `Resolution` exists
  yet, so definition/hover answer nothing until it's fixed). Verified
  end-to-end with a hand-rolled JSON-RPC client script driving real stdio
  framing through `initialize`/`didOpen`/`hover`/`definition`/`shutdown`/
  `exit` — this
  caught a real deadlock on shutdown (`run()` held the `Connection` alive
  across `io_threads.join()`, so the writer thread's channel never closed;
  fixed by moving `Connection` into `main_loop` so it drops first) — and
  fuzzed against every `examples/*.tr` file truncated at 5 cut points plus
  several hand-picked adversarial prefixes to confirm no pipeline phase
  panics on malformed input (a panic kills the whole server, not just one
  request). The VS Code extension wires it up via `vscode-languageclient`
  (`editors/vscode/extension.js`), spawning `trace --lsp` — the client
  side of the LSP wiring couldn't be verified in this environment (no way
  to drive a real VS Code Extension Development Host headlessly); the
  server side is the part verified above.
- **RESOLVED — the known gap from the entry above: go-to-definition/hover
  only resolved identifier *uses*, not declaration sites themselves**
  (hovering the `counter` in `reg counter : [8]` found nothing; a later
  `counter := ...` did). Root cause: `ident_at` only scans `Expr::Ident`
  spans, and a declaration's own name (`reg`/`in`/`out`/`fn`/`rule`/...) is
  never an `Expr::Ident` — it's plain data on an `Item`, resolved once at
  def-creation time, not re-parsed as an expression. Fixed with a new
  `thing_at` (`src/lsp.rs`) that falls back to a direct scan over every
  `resolve::Def`'s own `span` (already recorded — `goto_definition`
  already jumped there, just never matched a request arriving AT it)
  whenever `ident_at` finds nothing, so a declaration's own name now
  resolves directly to its `DefId` with no `ExprId` in hand. Hovering a
  declaration site has no per-expression type to read from `expr_tys`
  (there's no `ExprId`), so it falls back to the def-keyed
  `local_tys`/`state_tys` maps instead — between the two, every def with a
  scalar type is covered; a `rule`/`fn`/`module`/... declaration (neither
  map has an entry) shows just its kind, the same graceful fallback an
  untyped use site already had. Go-to-definition on a declaration's own
  name now resolves to itself rather than answering nothing — a harmless,
  expected no-op jump, not new behavior added for its own sake. Verified
  with 5 new tests in `src/lsp.rs` itself (`#[cfg(test)] mod tests`,
  matching `main.rs`'s own precedent for testing private helpers in-crate)
  that call `hover`/`goto_definition` directly with constructed
  `HoverParams`/`GotoDefinitionParams` — no stdio framing needed, since
  both already take a plain `&HashMap` of open documents — covering: a use
  site still resolving (regression check), a declaration site now
  resolving for both hover and go-to-definition, a declaration with no
  scalar type falling back to just its kind, and a position on neither an
  identifier nor a declaration's own name span (a bare keyword) still
  correctly resolving to nothing.
- **RESOLVED — hovering a function showed only `` `Outer` — a function ``,
  no signature.** A fn/spec/impl def has no single scalar `Ty` (its "type"
  is params + return + effects, not one value), so it fell all the way
  through to the generic `name — kind` fallback with nothing useful to
  show. Now `fn_signature` (`src/lsp.rs`) renders a proper Markdown hover
  for these three kinds specifically: a fenced ` ```trace ` code block
  holding the signature exactly as written (`Outer(x : [8]) : [8]
  <combines>`), followed by a placeholder description line (`"A
  function."`/`"A spec."`/`"An impl."`) since real doc comments don't
  exist yet. Built by slicing each param/return type annotation's own span
  directly out of the source rather than re-deriving and reformatting a
  `Ty` — simpler, and exact (no `Ty::Unknown`/generic-display edge case to
  handle). `FnKind::Fn` renders with NO leading keyword, matching real
  source syntax (`parser.rs` dispatches a plain function on a bare `Ident`
  at item position — only `spec`/`impl` consume an actual keyword token;
  `ast.rs`'s own debug dump prints a synthetic `fn ` prefix for its own
  readability, which would have been a wrong, unparseable signature to
  echo back in a hover). Works from both a call site (`thing_at`'s
  existing `Expr::Ident`-use path) and the declaration site itself (the
  previous entry's `Def::span` fallback path) — same signature either way,
  since both resolve to the same `DefId`. Verified with 2 new tests in
  `src/lsp.rs` against a function shaped exactly like
  `examples/call_nested_writes.tr`'s own `Outer`, one hovering the
  declaration and one hovering a call site, both asserting the exact
  rendered Markdown string.
- **RESOLVED — hovering an effect keyword (`reads`/`writes`/`combines`/
  `sequences`/`elaborates`/`fails`/`chooses`) answered nothing.** An
  effect's `Name` is plain syntax on `Item::Rule`/`Item::Fn`
  (`ast.rs`'s `Effect` struct) — never an `Expr::Ident`, and (unlike every
  other hover target so far) never given a `resolve::Def` either, since
  nothing ever references an effect the way a call references a fn or a
  read references a reg. Neither `ident_at` nor `thing_at`'s `Def::span`
  fallback could ever find one, so this needed a genuinely third,
  independent lookup path rather than extending either existing one. New
  `effect_hover` (`src/lsp.rs`) scans `ast.items` (a flat arena — a
  module's nesting is `ItemId` cross-references, not real tree nesting, so
  one pass already reaches every rule/fn) for whichever `Effect::name` span
  contains the cursor, and `effect_doc` renders a small Markdown hover for
  it: a description plus a runnable example, one per keyword, hand-
  transcribed from DESIGN.md's own "Effects" section (the same content, no
  new prose invented) — the closest this untyped-doc-comment language can
  get to "hover a keyword, see its docs," matching Rust/rust-analyzer's own
  hover for a language keyword. Checked before `res`/`thing_at` are even
  required, so it works on a file with resolve errors too, matching how far
  diagnostics themselves degrade (only a parse error blocks everything).
  Deliberately narrow: only an effect's own name matches, never `reads {pc,
  mem}`'s bracketed row arguments — `pc`/`mem` there are real state names,
  and giving THEM a proper hover needs actual name resolution, not a doc
  lookup (see the next entry). Verified with 4 new tests in `src/lsp.rs`:
  hovering `combines` in a fn's own effect list, hovering `reads`/`writes`
  in a rule's (confirming they get genuinely different text, not a shared
  generic blob), and a negative test confirming a bracketed row argument
  (`a` inside `reads {a}`) does NOT accidentally match — a looser
  "anywhere inside the effect" span check would have produced a wrong,
  misleading hover there instead of correctly finding nothing (later
  updated, not removed, once the next entry gave row arguments a real,
  different hover to correctly find instead).
- **RESOLVED — a `reads {a, b}`/`writes {a}` row argument itself
  (`pc`/`mem` in `reads {pc, mem}`) had no hover or go-to-definition at
  all**, the gap the entry above deliberately left open. Unlike every
  other hover target so far, the fix wasn't new lookup logic in `lsp.rs`
  alone — `resolve.rs`'s own `check_effect_args` already looks each
  argument name up (to validate it names real state) and simply discarded
  the answer once validated. New `Resolution::effect_arg_defs: HashMap<Span,
  DefId>` keeps it instead, keyed by the argument `Name`'s own span (a row
  argument is a plain `Name`, never an `ExprId`, so it can't join
  `expr_defs` the way a real use does), populated for every name that
  resolves to SOME def — even one the surrounding match then rejects as
  the wrong kind (a rule named in `reads`, say) — since the def is still
  real and still worth hovering regardless of whether using it there is
  legal. `thing_at` (`src/lsp.rs`) gained a new `effect_arg_at` helper
  (mirroring `ident_at`'s own linear scan) tried as its second case,
  between a live `Expr::Ident` use and the `Def::span` declaration
  fallback. Its return type changed from `(Option<ExprId>, DefId)` to
  `(Span, Option<ExprId>, DefId)` to carry this through cleanly: the
  `Span` is now always the actual site under the cursor, computed once
  inside `thing_at` rather than re-derived by each caller from whichever
  branch matched. This wasn't just a refactor for its own sake — reusing
  the OLD two-way `None`-means-declaration-site convention unchanged for
  this third, meaningfully different kind of `None` would have highlighted
  the wrong span entirely (the state's own faraway declaration, not the
  row argument actually under the cursor). Verified with 2 new tests in
  `src/lsp.rs` covering both `reads`' and `writes`' own arguments, for
  both hover (confirming the exact `` `name: ty` — kind `` text, same
  format any other state reference already gets) and go-to-definition
  (confirming the jump lands on the right declaration line, `in`/`reg`
  respectively) — plus the previous entry's negative test was updated,
  not deleted, to assert the row argument now resolves to ITS OWN hover
  rather than accidentally matching the effect keyword's doc.
- **RESOLVED — hovering an output port (or any other write target: a
  `reg`, a `fifo`, a reassigned local) at its own `x := ...` write site
  showed no type, just its kind** (`` `v` — an output port ``, not `` `v:
  [8]` — an output port ``). Root cause: a write target IS a live
  `Expr::Ident` use — `resolve.rs` already gives it an `expr_defs` entry
  like any read — but `types.rs`'s `type_write` (called from
  `Stmt::Assign` instead of the ordinary `type_expr`, since a write target
  has no "value" to compute a `Ty` FROM) never itself called `type_expr`,
  and nothing else populates `Types::expr_tys` besides `type_expr`'s own
  wrapper — so a write target's entry there simply never existed, for ANY
  state kind, not something specific to `out`. Fixed by having
  `type_write` insert into `Types::expr_tys` itself at the two points it
  already resolves a write target's type: the `state_tys` lookup (used
  verbatim) and the `Local` branch (the just-widened `merged` type — the
  correct type as of that specific write, not necessarily the local's
  final fixed-point type across the whole body). Verified with 2 new
  tests in `src/lsp.rs`: hovering the write-target `counter` in the
  existing `counter := counter + 1` fixture (its OWN LHS span, 8..15 —
  distinct from the RHS read span the pre-existing use-site test
  deliberately targets instead, sidestepping this exact gap without
  realizing it at the time), and a dedicated `out` port write-site test
  matching the original report. Both assert the exact `` `name: ty` —
  kind `` string, not just a substring match, so a future regression back
  to the type-less fallback text would fail loudly.
- Grammar is regex-based (TextMate), still pattern matching, not semantic
  analysis. **RESOLVED — the specific "highlights unconditionally, even
  as plain identifiers" gap.** `reads`/`writes`/`combines`/`sequences`/
  `elaborates`/`fails`/`chooses` are now scoped to inside a `<...>`
  effects list (`#effects-list`, anchored on `<` immediately followed
  by one of those words — the only viable disambiguator against `<` the
  less-than operator, since this language has no generics; capped at
  end-of-line as a defensive fallback for the one adversarial case, `x <
  reads` against a variable actually named `reads`, so a misfire can't
  cascade past that one line). `urgency`/`mutually_exclusive`/
  `conflict_free` are scoped to inside `schedule { ... }`
  (`#schedule-block`, anchored on the real lexer keyword `schedule`
  itself — no ambiguity risk there — with a recursive `#schedule-body`
  handling `mutually_exclusive`/`conflict_free`'s own nested `{ name,
  name }` lists so the scope spans the whole block, not just up to the
  first `}`). Confirmed against the real VS Code tokenizer
  (`vscode-textmate`/`vscode-oniguruma`, not just the regexes read in
  isolation): a reg/rule named `reads`/`mutually_exclusive` now
  highlights as a plain identifier, and a raw grep count of every
  non-comment occurrence of these words across every `examples/*.tr`
  file matches the tokenizer's own highlighted count exactly (16/16 for
  the schedule directives) — no regression on real content.
- Formatter is a reindenter, not a pretty-printer, by deliberate choice
  (see DESIGN.md's "Tooling" section). **RESOLVED — the one known gap
  this deliberately narrow design had**: a multiline `impl ... refines
  Spec` signature used to render `refines` flush left instead of hand-
  indented (no bracket depth to hang an indent off). Fixed narrowly,
  not by adding real statement awareness: a `Refines` token starting a
  line is the ONE construct the parser's own grammar (`parse_fn`'s
  `skip_newlines()` before `Refines`) allows to continue a signature
  outside any bracket, so `fmt.rs` special-cases exactly that token,
  giving its line one extra indent level relative to whatever depth its
  signature sits at (confirmed nested, not just top-level).
  `examples/arbiter.tr` (the one shipped example with this shape) is
  now idempotent under the formatter like every other example — no
  longer needs its own skip in `tests/fmt.rs`'s idempotency sweep.
- **RESOLVED — a closing bracket stranded on its block's last content
  line** (`return x + 20    }` instead of `}` on its own line — a
  deleted newline, an editor merge gone wrong) used to render
  unchanged: plain brace-counting only reindents EXISTING lines, never
  moves tokens across them. `fmt.rs`'s new `split_stray_closers` pass
  runs before the reindent pass and inserts a newline before any
  closing bracket whose matching OPENER is on an earlier line (an
  unambiguous multi-line-block marker — a deliberate single-line block
  always has both ends on the same line instead, so this can never
  misfire against intentional style like `if x = 1 { y := 1 }`).
  Bracket-agnostic (`}`/`)`/`]` alike), and a stacked run of closers
  (`}))`) splits out together as one unit, not one per line, matching
  how the run already renders once correctly placed. Six new tests in
  `tests/fmt.rs` cover the reported case, `)`/`]`, single-line blocks
  staying untouched, stacked runs (both already-correct and
  stray-then-split), a trailing `--` comment riding along with a split
  closer, and — the one this whole design's two-pass (split, re-lex,
  reindent) structure actually depends on — that formatting is
  idempotent on its own split output, not just on already-good input.
- Not published to a marketplace; local install only (see the extension's
  own README).
