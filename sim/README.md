# Simulation testbenches

Hand-written Icarus Verilog testbenches that drive the Verilog `firtool`
produces from this compiler's FIRRTL output. See `devenv.nix`'s `simulate`
script, or `tests/sim.rs` for the automated version.

- `accumulator_tb.v` drives `examples/accumulator.tr` through real
  `input`/`output` ports (`dut.inc`, `dut.sum`) — no hierarchical paths,
  no `--disable-opt` (see DESIGN.md's "Module ports" section).
- `port_ram_tb.v` drives `examples/port_ram.tr` (a `mem` fully loaded
  and read back through real `input`/`output` ports — no hierarchical
  paths, no `--disable-opt`; see DESIGN.md's "Port-based memory access"
  section). This is a general capability, not a fix for SUBLEQ: it
  loads one word per cycle on demand, not a whole program before boot.
- `subleq_tb.v` and `fifo_bridge_tb.v` use hierarchical paths (below):
  SUBLEQ needs to bulk-load a whole program before its rules start
  running, and fifos are internal-only — neither has a port-based way
  to reach them from outside.

## Why `subleq_tb.v`/`fifo_bridge_tb.v` use hierarchical paths, not ports

SUBLEQ's `mem m : bits[16][4096]` has no port-based load path (`input`/
`output` cover scalar `bits[w]` signals only, see DESIGN.md). FifoBridge
has no module ports at all — a `fifo` is an internal buffer, there is no
fifo-port concept in the language. Neither module exposes anything
beyond clock/reset, so their testbenches reach in directly — `dut.pc`,
`dut.m_ext.Memory[i]`, `dut.__fifo_input_valid` — which Icarus Verilog
allows with no special compile flags (Verilator needs
`--public`/`--public-flat-rw` for the same thing, which is why Icarus
was picked here). `__fifo_<name>_valid`/`__fifo_<name>_data` are the
internal register names firrtl.rs synthesizes for a depth-1 fifo buffer
(see `src/firrtl.rs`'s `fifo_valid_name`/`fifo_data_name`) — firtool
passes register names through unchanged, so they show up verbatim in
the generated Verilog.

## Why `-DSYNTHESIS` and `--disable-opt`

- `firtool --disable-opt`: needed for `subleq_tb.v`/`fifo_bridge_tb.v`
  only. Without any output ports, a module's entire contents are
  unobservable from outside, so firtool's default optimization passes
  dead-code-eliminate all of it. `--disable-opt` keeps the real logic so
  there's something to simulate. `accumulator_tb.v`/`port_ram_tb.v`
  don't need this: both have a real output port, so firtool already
  knows the logic is observable.
- `iverilog -DSYNTHESIS`: needed for every testbench here, regardless of
  ports. firtool emits a debug-only register/memory
  randomization block gated behind `ifndef SYNTHESIS`, using an
  `automatic`-lifetime variable declaration Icarus doesn't implement
  ("sorry: Overriding the default variable lifetime is not yet
  supported"). Defining `SYNTHESIS` skips that block entirely. This is
  safe: it only removes pre-reset debug randomization, and every
  register here is a `regreset` that's properly synchronously reset
  regardless.
- `firtool -lowering-options=disallowLocalVariables`: the SAME Icarus
  limitation (no `automatic`-lifetime locals), a DIFFERENT source of
  them — first hit by `div_rem_tb.v`. A cross-width `div`/`rem`
  operand (one side zero-extended to match the other before the primop
  runs) can lower to an `automatic logic` declared INSIDE an `always`
  block rather than a top-level `wire`; this flag makes firtool always
  choose the top-level-`wire` form instead. `--disable-opt` alone
  happens to dodge this for `div_rem.tr` specifically (confirmed by
  testing both ways), but the two flags don't conflict and this one is
  cheap insurance against the same class of gap in a future
  differently-shaped example — added to `devenv.nix`'s `simulate`
  script and `tests/sim.rs`'s `firrtl_to_verilog` unconditionally,
  rather than only where it happened to be strictly required.

## `extmodule_tribuf_tb.v` needs a second Verilog source

`examples/extmodule_tribuf.tr` declares `extmodule TriBuf from "tribuf.v"`
— an external module whose real tri-state implementation trace's own
FIRRTL output never references at all (confirmed by hand-lowering one
through firtool: the `.v` path is opaque data, purely a downstream build/
simulation concern). `tribuf.v`, right here in `sim/`, supplies that
implementation; `extmodule_tribuf_tb.v` instantiates the generated
`Top` TWICE with their `bus` ports tied together, alternates which side
drives, and checks the other side senses it — a real bidirectional net.

`devenv.nix`'s `simulate` script does NOT yet know to pass `tribuf.v`
to iverilog alongside the generated design (it only takes one example
name and assumes one generated `.v` plus one testbench) — `devenv shell
-- simulate extmodule_tribuf` fails at iverilog elaboration (verified
directly, not assumed): `Unknown module type: TriBuf ... referenced 2
times`. `tests/sim.rs`'s own
`extmodule_tribuf_runs_a_real_bidirectional_bus` test is the only
currently-working way to run this one; it uses a dedicated
`simulate_with_blackbox` helper instead of the shared `simulate`. See
TODO.md for the unbuilt discovery mechanism this would need (the `.tr`
source only ever says `"tribuf.v"`, not `"sim/tribuf.v"` — no path-
resolution convention has been decided yet).

## If firtool renames `m_ext`/`Memory`

`mem <name>` in a `.tr` file becomes an instantiated `<name>_ext`
submodule (`m_4096x16` for a 4096-entry `bits[16]` array) with an
internal `Memory` register array — those are firtool's own naming
choices, not this project's. If a firtool upgrade changes them, the
testbench's hierarchical paths will fail to resolve at compile time;
regenerate with `firtool --disable-opt` and grep the output for the
new names.
