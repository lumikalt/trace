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
  - Field access other than `instance.port` (which is reads only; writes
    only as a whole statement's LHS).
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
- A `let`-bound value that needs to cross a `tick`, and a memory written
  to more than once (unconditionally) in one rule, are both compile-time
  errors now, closing two real silent-miscompile gaps the recent
  fifo/spawn audit found. See DESIGN.md's "`sequences`: multi-cycle
  code" and "Memory, fifo, and submodule declarations" sections.
- The two message-quality gaps queued alongside the above are also
  closed: `let x = m[addr]` (a memory read bound with `let`) now emits a
  real read port and compiles, instead of erroring (`collect_read_sites`
  had no `Stmt::Let` arm, the same Assign-vs-Let parity gap as the
  fifo-op bug); `let h = spawn Foo(args)` now names the real restriction
  (bind with `:=`, since `.result`/`.done` are read on a later cycle)
  instead of being blamed on `race`.
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
  `logic{ exp }`), not just a plausible trace-only name. `or` below
  still needs it as its per-alternative "did this succeed" primitive.
- **`or` fallback operator** — Verse's `X? or Default` / `A or B or C`:
  try the left fallible expression, and if it fails, use the right side
  instead, DISCHARGING the failure rather than propagating it — this is
  the actual "escape hatch" Lumi named when scoping the `fails` gate
  ("the only way to escape needing a fallible context is... with the
  `or` keyword"). Highest-value single item here: closes a real
  expressiveness gap (today a failing sub-expression has no way to
  supply a fallback value short of restructuring into `if`/`else`), and
  gives fallible fifo reads/calls a priority-chain idiom
  (`Deq[fifoA] or Deq[fifoB] or default`) that's a natural fit for
  arbitration hardware. Open questions to settle before writing any
  Rust, ideally by hand-lowering a `fifo_bridge`-style example to raw
  FIRRTL first (this project's established playbook, paid off for
  spawn/sync/race and fifo depth): whether `or` is spelled as the bare
  word (matching Verse, and free of collision with the existing bitwise
  `|`) or as symbolic sugar; whether the muxing reuses `prio`'s
  first-match machinery or needs its own; and how a chain's fail
  condition composes back into the enclosing rule's guard when NONE of
  the alternatives succeed (an `or` chain ending in a non-fallible
  default is always infallible, but `A or B` with no default stays
  fallible — needs the same fold-through-the-chain treatment
  `callee_fail_cond` already does for calls). `logic(...)` (ACHIEVED,
  DESIGN.md's "Builtins" section) is this question's actual answer, not
  just a related feature: `A or B or C` needs "did this alternative
  succeed?" as a boolean before deciding whether to fall through, per
  alternative — that's `logic(...)`'s exact job. `logic(...)` alone
  doesn't finish `or`, though: `or` still needs its own priority-mux/
  exclusivity machinery (which alternative's REAL, effectful op
  actually executes) built on top, the same shape `prio`/`__race_value`
  already establish. `and` isn't listed as a
  companion gap: sequential bare guards already conjoin into one rule's
  readiness for free, and bitwise `&` already covers AND in expression
  position for `bits[1]` operands — there's no missing capability to
  port, just `or`'s missing discharge behavior. The `or` semantics
  above are paraphrased from a fetched summary, not the primary text —
  re-read [08_failure](https://verselang.github.io/book/08_failure/)
  itself before writing any Rust, same as the hand-lowering step.
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
- **Cross-tick fails/rollback safety in `sequences`** — ties directly
  into the existing "Cost model and formal verification" section below:
  Verse's `<transacts>` explicitly notes state changes are provisional
  until the whole context succeeds, rolling back on failure. trace
  already does this for free within one cycle (nothing's committed
  yet), and a `let`-bound value crossing a `tick` is already rejected
  (`compute_captures`'s `let_bound: HashSet<DefId>`, from the earlier
  fifo/spawn audit) — but that's a different question from this one.
  What's unverified: whether a guard/fifo-op/failing call appearing
  AFTER a `tick` inside a `sequences` body is caught by that same
  check, some other existing machinery, or slips through uncaught —
  grep for where `let_bound` is consulted and trace whether a bare
  post-tick guard hits it. Silent wrong behavior (a partial,
  already-committed prior cycle with no way to undo it) would be worse
  than an explicit "not yet supported" compile error. Audit first; only
  build real checkpoint/squash machinery if the audit finds it's
  actually reachable today.

Speculative, bigger, not committed to:

- **Option type** (`?T`, `option{...}` construction, `?.` safe access,
  nested `??T`) — Verse's general mechanism for a value that may be
  absent. Could generalize the valid-bit-plus-payload pattern fifos
  already use into a first-class type, but "absence" has no free
  representation in hardware (every wire has SOME bit pattern) the way
  it does in a language with a heap — would need a real design decision
  (a struct-shaped `{valid: bit, data: T}` under the hood, most likely)
  before this is worth prototyping, not just a syntax port.
  `?.`/nested-option unwrapping would also need TODO's existing "field
  access beyond `instance.port`" gap (see "Expression surface" above)
  closed first, since both are about accessing into a value's shape.
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
  the type-system ambiguity.
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
