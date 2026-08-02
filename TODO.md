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
- **NEW, REAL BUG — a callee-local reassigned via `:=` is silently
  dropped at FIRRTL emission; found while fact-checking DESIGN.md's
  "Calling a function from a rule" section during the pass above, not
  by the test suite.** Confirmed pre-existing, not a regression from
  this session's changes (`git stash -u` + rerun on the pre-session
  baseline reproduces byte-for-byte identical output). Repro: a callee
  with `let x = input.Deq[]` followed by `x := x + 1` later in the same
  body — this compiles clean, no error, but the reassignment never
  appears anywhere in the emitted FIRRTL; every read of `x` resolves to
  its FIRST binding as if the `x := x + 1` line were never there. Root
  cause: `enter_rule` (firrtl/writes.rs) is what builds
  `locals_snapshots`, the position-indexed machinery that makes
  rule-level `:=` reassignment resolve each read at its own textual
  position — but it's built from `rule_body`, which returns an empty
  `Vec` for anything that isn't a top-level `Item::Rule`, so a callee
  body reached through inlining never populates it at all. Reads
  instead fall back to the separate, single-binding `self.locals` map
  (`expr.rs`'s Ident arm), which is what silently serves the stale
  first value. NOT yet fixed — deliberately out of scope for the `let`
  feature above (different bug class: silent wrong hardware, not a
  message-quality or ambiguity gap; unrelated machinery). Needs either
  extending `locals_snapshots`-style position tracking into callee
  inlining, or a compile-time rejection of callee-local reassignment
  until that exists (a `:=` reassignment of an already-`let`-bound
  local, when the enclosing item isn't a `rule`).
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

- `logic(...)` boolean-success operator ACHIEVED — see DESIGN.md's
  "Builtins" section for the full write-up (semantics, the guard+write
  v0 restriction and why it mirrors Verse's own `<decides>`/`<transacts>`
  split, examples). Confirmed as a real Verse port (`02_primitives`'s
  `logic{ exp }`), not just a plausible trace-only name.
- `or` fallback chain ACHIEVED (v0: `Deq[]`-only alternatives, depth-1
  fifos, optional infallible default tail) — see DESIGN.md's "`or`:
  fallback chains" section for the full write-up (semantics, v0
  restrictions, examples). Confirmed against Verse's own primary source
  (`08_failure`), not a paraphrase. Call alternatives, `Enq`
  alternatives, depth>1 fifos, and `or` nested in `if`/`while` or a
  callee's own body are all separate, larger gaps, not silently
  accepted. `and` needed no dedicated syntax: sequential bare guards
  already conjoin for free, and `logic(...)` combined with bitwise `&`
  already covers it in expression position.
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
    `?.` safe navigation: still not pursued, no existing analogue to
    reuse (ordinary `.field` access requires the local bound directly
    to a literal, no aliasing) — unrelated to `optional`, would need
    its own design pass if ever wanted.
- **Fallible bindings scoped to a single `if`** (Verse's
  `if (X := Expr, Y > 0):`, where `X` only exists in the `then` branch
  and a failure skips straight past it) — uncertain fit. trace's guard
  model is per-RULE (one composed readiness condition for the whole
  body), not per-statement with real sequential abort/rollback the way
  Verse's `if` is; grafting Verse's fine-grained scoping onto that
  without just reinventing `if`/`else` restructuring needs a real
  design discussion, not an implementation attempt, before committing
  to anything.

Explicitly considered and NOT being ported, so a future session doesn't
re-propose these from a fresh read of the same chapters:

- **`Err()`** (Verse's unrecoverable runtime error) — synthesized
  hardware has no runtime to propagate an error into; the closest
  analogue, an elaboration-TIME fatal check, is a different feature
  already reachable via a plain compile error, not a gap.
- **Comparisons returning their left operand in a failure context**
  (Verse: `X > 0` yields `X` on success) — would blur trace's clean
  condition/value type split (a condition is always `[1]`, a
  compared operand can be any width) for a cosmetic win only; not worth
  the type-system ambiguity. Concrete consequence, surfaced while
  building `or`: this is exactly why `a <> 0 or b` doesn't work today
  (a comparison is a plain always-succeeding `[1]` value, with
  nothing for `or` to discharge — `or`'s alternatives all need real
  fallibility) — `if a <> 0 { a } else { b }` is the spelling instead.
  Revisit only if this specific pattern shows up for real, not
  speculatively; it'd be a much bigger, more disruptive change than
  `logic`/`or` were (every existing bool-returning comparison use would
  need rethinking), not just another operator.
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

## Scheduler / arrays

- v0 arrays are one conflict resource each — no partial disjointness
  (Dahlia-style banking is tier 3, explicitly deferred in DESIGN.md).
- `combines` combinational-loop checking delegates entirely to
  firtool's `CheckCombLoops`, which has a known blind spot around
  multi-top-module designs (CIRCT issue #1138).

## Simulation

- Icarus only; no Verilator path (would need `--public`/`--public-flat-rw`
  wiring for the hierarchical-path testbenches).
- `firtool`/`iverilog` CLI behavior has already drifted once this project
  (firtool 1.147.0 needed `-format=fir` for stdin); no version pin yet.

## Editor tooling (`editors/vscode/`)

- No language server: no go-to-definition, hover, or inline diagnostics —
  only `trace file.tr` on the command line catches real errors.
- Grammar is regex-based (TextMate): highlights `reads`/`writes`/effect
  words unconditionally, even used as plain identifiers outside `<...>`.
- Formatter is a reindenter, not a pretty-printer, by deliberate choice
  (see DESIGN.md's "Tooling" section) — known gap: a multiline signature
  before `refines`/`{` renders flush left (`tests/fmt.rs` pins this).
- Not published to a marketplace; local install only (see the extension's
  own README).
