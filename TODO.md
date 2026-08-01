# TODO

## Emission (`src/firrtl/`)

- Calling a user `fn`/`impl` from a rule inlines the callee at its call
  site; see DESIGN.md's "Calling a function from a rule" for the full
  design history. Remaining real restrictions, not just historical
  color:
  - A callee body must be zero-or-more `let`s + state writes, then a
    trailing `return` or an `if`/`else` (mandatory `else`) recursing
    into that same shape — no loops, guards, or fifo ops anywhere in a
    callee, ever (not scoped to be lifted, just not yet attempted).
  - A nested call composes as a value, or as a state-writing bare
    statement / `:=` RHS, as long as it doesn't form a call cycle
    (static call-graph check) — nested any deeper than those two write
    positions (an argument, a `let`, `Bump(a) + 1`) is a clean, explicit
    error.
  - Of the builtins, only `prio`/`trunc`/`pack` are synthesizable as
    calls; `clog2`/`len` are compile-time-only (`Ty::Int`), and
    `bits`/`wire`/`list`/`any`/`sync`/`race` aren't applicable to a
    plain combinational callee body at all.
  - A generic callee parameter's own width (`bits[N]`) is only
    resolvable for its own return value or by following it through
    `self.locals` back to a concrete call-site expression — used
    independently elsewhere in a generic callee body, this fails
    cleanly ("no concrete width"), not a miscompile, but is still a real
    sharp edge if this area is touched again.

  See `examples/call.tr`, `examples/call_branch.tr`,
  `examples/call_writes.tr`, `examples/call_prio.tr`,
  `examples/call_trunc.tr`, `examples/call_pack.tr`,
  `examples/call_nested.tr`, `examples/call_nested_writes.tr`.
- Expression surface still excludes:
  - Field access other than `instance.port` (which is reads only; writes
    only as a whole statement's LHS).
  - A fully dynamic slice (`x[hi..lo]` with a non-const `hi`/`lo`) —
    its result width would be dynamically sized, which this language
    can't express, so it's a compile-time type error by design, not a
    gap to close. Use `x[base +: width]`/`x[base -: width]` (dynamic
    start, static width) instead.

  Everything else is implemented: arithmetic/bitwise/compare ops, static
  AND dynamic-amount shifts, unary `-`/`~`/`!`, `/`/`%`, static AND
  dynamic bit-select/indexed part-select, Verilog-style sized literals
  (+ inferred `reg`/`output` types from one), `bit` sugar for `bits[1]`.
  See DESIGN.md's "Expression surface" section for the full history and
  width-rule derivations (several confirmed empirically against real
  firtool, not assumed from spec text); `examples/alu.tr`,
  `examples/dynamic_shift.tr`, `examples/div_rem.tr`,
  `examples/sized_literal.tr`, `examples/infer_reg_ty.tr`,
  `examples/dynamic_bit_select.tr` for coverage.
- Fifos are depth-1 only (one data reg + one valid bit); no depth syntax
  exists. A rule may `Enq`+`Deq` the SAME fifo now (a pass-through —
  `Deq` reads the old value, `Enq` writes the new one, `valid` stays 1;
  guard is `valid == 1`, not the always-false AND of each op's own
  individual guard). See DESIGN.md's "FIFO synthesis emission" section,
  `examples/fifo_passthrough.tr` + `sim/fifo_passthrough_tb.v`.
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

- `race`: parses and effect-checks, but `lower.rs` explicitly rejects it
  (v0 restriction, not silent) — needs a loser-cancellation latch design
  first (arbitrating a same-cycle write conflict, which the ordinary
  scheduler already does, isn't the same as "first handle to complete
  wins" across cycles). `spawn`/`sync` themselves are ACHIEVED (see
  DESIGN.md's "`spawn` and `sync`" and "Spawn and sync lowering"
  sections, `examples/fetch2.tr` + `sim/fetch2_tb.v`).
- Combinational-only (stateless) modules: `output` is register-backed by
  design (see DESIGN.md's "Module ports"), so a pure function of inputs
  can't be expressed without a cycle of delay.

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
