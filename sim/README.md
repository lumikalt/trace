# Simulation testbenches

Hand-written Verilog testbenches that drive the Verilog `firtool` produces
from this compiler's FIRRTL output, run under either Icarus Verilog (the
default) or Verilator (see "Verilator" below) with no rewrite needed. See
`devenv.nix`'s `simulate` script, or `tests/sim.rs` for the automated
version.

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
allows with no special compile flags. Verilator allows it too, with no
special flags either, in the invocation this project actually uses —
see "Verilator" below for why, and for the correction to what this
paragraph used to claim about `--public`/`--public-flat-rw`.
`__fifo_<name>_valid`/`__fifo_<name>_data` are the
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

`devenv.nix`'s `simulate` script now discovers this automatically:
it greps the example's ORIGINAL source (before `--elaborate`/`--lower`,
which pass an `extmodule` item through untouched but aren't guaranteed
to exist for every example) for every `extmodule ... from "path.v"`
declaration and resolves each `path.v` RELATIVE TO `sim/` — the
convention `tribuf.v`'s own placement here already implied, now made
real. `devenv shell -- simulate extmodule_tribuf` runs correctly as a
result. `tests/sim.rs`'s own
`extmodule_tribuf_runs_a_real_bidirectional_bus` test still uses its
own dedicated `simulate_with_blackbox` helper rather than the shared
`simulate` script (a Rust test can't shell out to a devenv script), but
both now agree on the same "look in `sim/`" resolution.

## If firtool renames `m_ext`/`Memory`

`mem <name>` in a `.tr` file becomes an instantiated `<name>_ext`
submodule (`m_4096x16` for a 4096-entry `bits[16]` array) with an
internal `Memory` register array — those are firtool's own naming
choices, not this project's. If a firtool upgrade changes them, the
testbench's hierarchical paths will fail to resolve at compile time;
regenerate with `firtool --disable-opt` and grep the output for the
new names.

## Verilator

Every testbench here also runs under Verilator, no rewrite needed:
`devenv shell -- simulate <name> verilator` (the `simulate` script's
default backend is still `iverilog`, unchanged — pass `verilator`
explicitly, or `iverilog` explicitly to be exact about it). TODO.md's
"Simulation" section used to say a Verilator path "would need
`--public`/`--public-flat-rw` wiring for the hierarchical-path
testbenches" (`subleq_tb.v`, `fifo_bridge_tb.v`) — that assumption
turned out to be wrong for the invocation that actually matters here,
and is corrected below.

`--public`/`--public-flat-rw` govern access to an internal signal from
OUTSIDE the Verilog design entirely — a C++ or DPI harness peeking/
poking through Verilator's generated API. That's not what any
testbench here does. `devenv.nix`'s `simulate` script (and `tests/
sim.rs`'s `simulate_verilator` helper) instead invoke `verilator
--binary --timing`, which elaborates the hand-written SV testbench
ITSELF as the simulation's own top-level design, DUT included — no C++
wrapper at all, the same "compile straight to a runnable binary" shape
`iverilog`+`vvp` already has. Because the testbench and DUT are one
design under this mode, `dut.pc`/`dut.m_ext.Memory[i]`
(`subleq_tb.v`) and `dut.__fifo_input_valid = ...`
(`fifo_bridge_tb.v`) are ordinary intra-design SystemVerilog
hierarchical references, not a cross-boundary peek/poke — confirmed
directly, not just inferred: both compile and run clean under
`--binary --timing` with no `--public`/`--public-flat-rw` and no
Verilator warning about signal visibility. (A C++/DPI harness reaching
into the same signals from outside would still need those flags — the
correction is scoped to this invocation, not a claim that the flags
are never needed for anything.)

`--timing` is required alongside `--binary`: without it, Verilator (a
synthesis-focused tool by default) rejects most of what these
testbenches do — `#5` delays, `@(posedge clock)`, `initial` blocks —
as unsupported. `-DSYNTHESIS` and firtool's own `--disable-opt`/
`-lowering-options=disallowLocalVariables` are all still applied for
the `verilator` backend too, but purely for PARITY with the `iverilog`
backend's generated Verilog (both backends compile the exact same
`design.v`) — Verilator itself has no trouble with `automatic`-
lifetime locals or firtool's debug-only register-randomization block;
neither flag is a Verilator requirement.

`--top-module` needs the testbench's own module name. Every testbench
here happens to name its module `<name>_tb` to match its filename,
EXCEPT `optional_chain_tb.v`/`optional_rule_tb.v`, both of which just
use `tb` — a real, pre-existing inconsistency in this directory, not
something Verilator support introduced. Both the `simulate` script and
`tests/sim.rs`'s `simulate_verilator` helper grep the testbench file's
own `module <name>` line rather than assume the filename convention,
so this is handled, not worked around by renaming anything.

Test coverage (`tests/sim.rs`) isn't a full second copy of the
`iverilog` suite: three tests, one per genuinely distinct access shape
(`accumulator`: plain module ports; `subleq`: hierarchical peek/poke
including a nested submodule's own memory array, the strongest case in
this repo; `extmodule_tribuf`: a blackbox `.v` source compiled
alongside the design) prove the Verilator INTERFACE works. Functional
correctness of every individual example is already fully proven by the
`iverilog` suite; a full second copy would only be re-proving that,
not testing anything new, at the cost of roughly doubling this test
binary's runtime (each Verilator run pays a real C++ compile, ~15s,
vs. iverilog's near-instant interpretation).
