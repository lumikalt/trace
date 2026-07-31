# TODO

## Emission (`src/firrtl.rs`)

- No calls to user `fn`/`spec`/`impl` from synthesizable rules (needs
  inlining or instantiation). When this lands: recheck resolve.rs's
  module-boundary state check (`check_module_boundary`) against a
  `fn`/`spec`/`impl` nested inside a module that reads/writes that
  module's own state — today that's moot (calls aren't emitted at all),
  but inlining would need the boundary re-validated at the CALL site's
  module, not just the callee's declaration site.
- Expression surface still excludes: calls, other field access, `/`/`%`,
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
