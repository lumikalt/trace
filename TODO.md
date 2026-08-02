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
    (`Ty::Int`), and `bits`/`wire`/`list`/`any`/`sync`/`race` aren't
    applicable to a plain combinational callee body at all.
  - A generic callee parameter's own width (`bits[N]`) is only
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
  `list[bits[N]]`, one-sided slices `xs[..mid]`/`xs[mid..]`). Separate
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
  inferred `reg`/`out` types from one), `bit` sugar for `bits[1]`, `uN`
  (`u8`, `u32`, ...) sugar for `bits[N]`.
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
  local whose width never resolves to a concrete `bits[w]` anywhere in
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
  ordinary `bits[N]` rather than a new type. Revisit only if a real
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
- trace's `not` is confirmed to be a plain `bits[1]` boolean operator
  (`types.rs`'s operand-must-already-be-`bits[1]` rule), not Verse's
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
  - **`??T` (nested Option) — verified, with a real limitation, not
    just "untested."** Compiles and simulates correctly for the two
    states reachable through today's syntax (fully absent via `false`,
    fully present via a bare value coerced through both layers) — see
    DESIGN.md's "Option types" section and
    `nested_option_reaches_only_fully_absent_or_fully_present`
    (tests/firrtl.rs). But the outer and inner `valid` bits are
    provably always equal (no write path can separate them), so `??T`
    is currently indistinguishable from `?T` — `Some(None)` (outer
    present, inner absent) is genuinely inexpressible, not just
    unexercised. Making the layers independent needs a construction
    syntax that doesn't exist yet (there's no way to write "present,
    holding an absent inner value" — `.data` is read-only and a `?T`
    expression can't be written into a `??T` target). Not pursued
    further without a concrete use case; no example added (an example
    file advertises a pattern worth using, and this one currently
    isn't one).
  - **`option{...}` explicit-construction syntax, `?.` safe navigation.**
    Not pursued: implicit coercion already covers construction (`opt :=
    value`/`opt := false`, no wrapper syntax needed), and `?.`'s
    short-circuit-on-empty chaining has no existing analogue to reuse
    (ordinary `.field` access requires the local bound directly to a
    literal, no aliasing) — would need its own design pass if ever
    wanted, not assumed necessary.
- **Struct destructuring** (`let Pair{valid, data} = p`, or a `let
  {valid, data} = p` field-shorthand form — binding several named
  locals from one struct value in a single statement, instead of one
  `p.field` projection per local). Natural ergonomic companion to
  general structs, now that those have landed; not needed for `?T`/
  read-only field access itself.
- **Struct update syntax** (Rust's `Pair{ valid: 1, ..old }` — build a
  new struct value from an existing one, overriding just the named
  fields and copying every other field from `old`). Another ergonomic
  companion to general structs: today every field must be spelled out
  explicitly in every literal, even when only one field of a large
  (possibly nested) struct actually changes. Emission-wise this looks
  straightforward on top of the flattening `struct_field_widths`/
  `compile_field_path_value` already do (a missing field falls back to
  `..old`'s own flat register/field instead of erroring "missing
  field"), but the parser/type-checker side needs its own design pass:
  where `..old` may appear in the field list (trailing only, like
  Rust?), and how it composes with a nested struct field that's itself
  only partially overridden.
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
  condition/value type split (a condition is always `bits[1]`, a
  compared operand can be any width) for a cosmetic win only; not worth
  the type-system ambiguity. Concrete consequence, surfaced while
  building `or`: this is exactly why `a <> 0 or b` doesn't work today
  (a comparison is a plain always-succeeding `bits[1]` value, with
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
