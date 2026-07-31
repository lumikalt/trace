# TODO

## Emission (`src/firrtl/`)

- A call to a user `fn`/`impl` inlines when its body is `let` bindings
  and state writes, then a trailing `return`, or an `if`/`else`
  (mandatory `else`) whose branches both recurse into that same shape —
  no loops or guards/fifo ops anywhere in the callee. A callee's own
  body MAY call another `fn`/`impl` (composition), used as a value — a
  `let`'s init, the return expression, or a state write's own RHS —
  as long as it doesn't form a call cycle (direct or indirect;
  `find_call_cycle` in firrtl/calls.rs is a STATIC graph over which
  functions' own bodies name which others, not a dynamic "currently
  compiling" stack — that approach false-positived on `Avg(Avg(x, y),
  z)`, a rule calling `Avg` twice, once nested as an argument, since
  lazily substituting the argument looks identical to real recursion
  from inside `compile_callee_body`'s own call order). Two things
  still make a nested call an explicit error, not supported yet: (1) a
  nested call that itself writes state — `callee_reg_write`/
  `callee_port_write` (the write-hunt one level into a callee's own
  body) don't yet recurse a SECOND level into a nested call's own body
  to find a write buried behind it; (2) a nested call used as a bare
  statement (its return value discarded) — neither the return-value
  walk (`compile_callee_body`, which already rejects any non-`Let`/
  `Assign` statement before the tail) nor the write-hunt walk can see
  into that shape at all. A state write reaches the emitted hardware
  only when the call site is a bare statement or the whole RHS of `:=`
  — nested any deeper (an argument, a `let`, `Bump(a) + 1`) is a
  clean, explicit error, since `call_writes_reg`/`call_writes_port`
  (firrtl.rs) only ever look for a write in those two positions. A
  callee whose write is conditional (`if`/`else`) is only inlinable as
  a bare statement — if its return value is ALSO used, the `if`/`else`
  must be in TAIL position (own separate walk, `compile_callee_body`,
  stricter shape than the write-hunt walk). A call's own state-reaching
  reads/writes (transitively, through the whole call graph) are
  checked against the CALL site's module, not just the callee's
  declaration site (`validate_call` in firrtl.rs, uses
  `Resolution::def_owner`). Of the builtins, `prio` (a fixed-priority
  encoder, `compile_prio`), `trunc` (the low N bits, `compile_trunc`),
  and `pack` (concatenation, first argument most significant —
  matching FIRRTL's own `cat` primop directly, folding left-to-right
  for 3+ arguments, `compile_pack`) are synthesizable, as
  `mux`/`bits`/`cat` FIRRTL text — none of the three disqualifies a
  callee from inlining the way a cyclic or state-writing nested call
  still does. `pack`'s only DOCUMENTED use (DESIGN.md's `Fetch2`
  example) is inside a `<sequences>`/spawn body, which still has no
  synthesis path of its own (a separate, larger gap, see below) — but
  concatenation is equally well-defined for a plain combinational rule
  body, so it didn't need that surrounding feature to be useful
  standalone. The rest of the builtin vocabulary isn't a "not
  implemented yet" gap so much as "not applicable to a plain
  combinational rule body at all": `clog2`/`len` type as `Ty::Int`, a
  compile-time-only type (bit-width computation in a type position, or
  a list's length during elaboration) — giving them a RUNTIME hardware
  meaning would mean inventing new semantics with no grounding anywhere
  in the design, unlike `prio`/`trunc`/`pack`, each of which had (or
  got) a concrete spec before being implemented. `bits`/`wire`/`list`/
  `any`/`sync`/`race` aren't this kind of gap at all — type-position
  constructs, a spec/`chooses`-only construct, or (`sync`/`race`) the
  same FSM/spawn-sequencing gap already rejected in `lower.rs`. A generic
  callee parameter's own width (`bits[N]`) is only resolvable, inside
  the callee's body, for its OWN return value (hint-threaded) or by
  following it through `self.locals` back to a concrete call-site
  expression (`concrete_width_of`, used by `compile_prio`'s argument)
  — anywhere else a callee-body expression's width is needed
  independent of the return value, this same gap can resurface; a
  generic nested call reached through a binop (`compile_binop`
  recomputes its own hint via `known_width`, ignoring any hint its
  caller threaded down) hits exactly this and fails cleanly with "no
  concrete width", not a miscompile. See `examples/call.tr`,
  `examples/call_branch.tr`, `examples/call_writes.tr`,
  `examples/call_prio.tr`, `examples/call_trunc.tr`,
  `examples/call_pack.tr`, `examples/call_nested.tr`.
- Expression surface still excludes: other field access, `/`/`%`,
  dynamic-amount shifts (shift amount must be a literal), computed
  bit-select/slice bounds (must be literal), and logical `!` (only `~`
  and unary `-` are supported). `instance.port` is reads only; writes
  only as a whole statement's LHS. See `examples/alu.tr` for what's
  covered: `*`, `&`/`|`/`^`, `<<`/`>>`, unary `-`/`~`, `x[i]`/`x[hi..lo]`.
  Verilog-style sized literals (`8'd6`/`8'hFF`/`8'b1010`/`8'6`) are also
  supported, typing directly as `bits[width]` rather than absorbing a
  width from context the way a bare integer literal does — overflow
  against their own declared width is a compile error, not silent
  truncation. See `examples/sized_literal.tr`. `reg`/`output` may omit
  `: ty` when initialized with a sized literal (`reg a = 8'd6`), which
  infers `bits[width]` from the literal — only a literal initializer
  works (`reg a = 8'd6 + 1` or `reg a = 6` still need an explicit type).
  See `examples/infer_reg_ty.tr`.
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
