# TODO

## Emission (`src/firrtl/`)

- A call to a user `fn`/`impl` inlines when its body is `let` bindings
  and state writes, then a trailing `return`, or an `if`/`else`
  (mandatory `else`) whose branches both recurse into that same shape —
  no loops, guards/fifo ops, or further calls anywhere in the callee
  (so a called function's own body calling another function, including
  indirect recursion, is rejected outright rather than actually needing
  a recursion check: nothing in the restricted shape can call
  anything). A state write reaches the emitted hardware only when the
  call site is a bare statement or the whole RHS of `:=` — nested any
  deeper (an argument, a `let`, `Bump(a) + 1`) is a clean, explicit
  error, since `call_writes_reg`/`call_writes_port` (firrtl.rs) only
  ever look for a write in those two positions. A callee whose write is
  conditional (`if`/`else`) is only inlinable as a bare statement — if
  its return value is ALSO used, the `if`/`else` must be in TAIL
  position (own separate walk, `compile_callee_body`, stricter shape
  than the write-hunt walk). A call's own state-reaching reads/writes
  (transitively, through the whole call graph) are checked against the
  CALL site's module, not just the callee's declaration site
  (`validate_call` in firrtl.rs, uses `Resolution::def_owner`). Of the
  builtins, `prio` (a fixed-priority encoder, `compile_prio`) and
  `trunc` (the low N bits, `compile_trunc`) are synthesizable, as `mux`/
  `bits` FIRRTL text — neither disqualifies a callee from inlining the
  way a call to another user `fn`/`impl` still does. The rest of the
  builtin vocabulary isn't a "not implemented yet" gap so much as "not
  applicable to a plain combinational rule body at all": `clog2`/`len`
  type as `Ty::Int`, a compile-time-only type (bit-width computation in
  a type position, or a list's length during elaboration) — giving them
  a RUNTIME hardware meaning would mean inventing new semantics with no
  grounding anywhere in the design, unlike `prio`/`trunc`, which had (or
  got) a concrete spec before being implemented. `pack` (concatenation)
  is well-defined but its only documented use (DESIGN.md's `Fetch2`
  example) is inside a `<sequences>`/spawn body — synthesizing it for
  plain rule bodies is possible but wouldn't serve that actual use case
  until spawn/sync/race get a synthesis path of their own (a separate,
  larger gap, see below). `bits`/`wire`/`list`/`any`/`sync`/`race`
  aren't this kind of gap at all — type-position constructs, a
  spec/`chooses`-only construct, or (`sync`/`race`) the same
  FSM/spawn-sequencing gap already rejected in `lower.rs`. A generic
  callee parameter's own width (`bits[N]`) is only resolvable, inside
  the callee's body, for its OWN return value (hint-threaded) or by
  following it through `self.locals` back to a concrete call-site
  expression (`concrete_width_of`, used by `compile_prio`'s argument)
  — anywhere else a callee-body expression's width is needed
  independent of the return value, this same gap can resurface. See
  `examples/call.tr`, `examples/call_branch.tr`, `examples/call_writes.tr`,
  `examples/call_prio.tr`, `examples/call_trunc.tr`.
- Expression surface still excludes: other field access, `/`/`%`,
  dynamic-amount shifts (shift amount must be a literal), computed
  bit-select/slice bounds (must be literal), and logical `!` (only `~`
  and unary `-` are supported). `instance.port` is reads only; writes
  only as a whole statement's LHS. See `examples/alu.tr` for what's
  covered: `*`, `&`/`|`/`^`, `<<`/`>>`, unary `-`/`~`, `x[i]`/`x[hi..lo]`.
- Memory writes can't nest in `if`/`while` (register writes can, via a
  `mux`; mem writes are still top-level-only).
- Fifos are depth-1 only (one data reg + one valid bit); no depth syntax
  exists. No `Enq`+`Deq` of the same fifo in one rule.
- A local reassigned within one emitted rule is rejected (inlining picks
  the wrong binding otherwise — see DESIGN.md's "sharp edge" note).
- `conflict_free` claims are recorded and exempt a pair from the derived
  stall, but the promised simulation assertion that checks the claim
  doesn't exist yet — an unsound claim currently just compiles.

## SUBLEQ / boot sequencing

- `sim/subleq_tb.v` still pokes `mem` via hierarchical paths. Port-based
  memory access works in general (`examples/port_ram.tr`) but only
  per-word, on demand — SUBLEQ needs to bulk-load a whole program before
  `step`/`refill` start firing at reset. Needs a real design decision
  (some kind of boot/load mode), not just an emitter fix.

## Language features with no synthesis path yet

- `spawn`/`sync`/`race`: parse and effect-check, but `lower.rs` and
  `firrtl.rs` both explicitly reject them (v0 restriction, not silent).
- Combinational-only (stateless) modules: `output` is register-backed by
  design (see DESIGN.md's "Module ports"), so a pure function of inputs
  can't be expressed without a cycle of delay.

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
