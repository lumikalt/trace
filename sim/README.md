# Simulation testbenches

Hand-written Icarus Verilog testbenches that drive the Verilog `firtool`
produces from this compiler's FIRRTL output. See `devenv.nix`'s `simulate`
script, or `tests/sim.rs` for the automated version.

## Why hierarchical paths, not ports

The language has no `input`/`output` port concept yet (see DESIGN.md's
SUBLEQ milestone section). An emitted module exposes only `clock` and
`reset`, so a testbench can't load memory or observe registers through
ordinary ports. These testbenches reach in directly instead —
`dut.pc`, `dut.m_ext.Memory[i]` — which Icarus Verilog allows with no
special compile flags (Verilator needs `--public`/`--public-flat-rw`
for the same thing, which is why Icarus was picked here).

## Why `-DSYNTHESIS` and `--disable-opt`

- `firtool --disable-opt`: without any output ports, a module's entire
  contents are unobservable from outside, so firtool's default
  optimization passes dead-code-eliminate all of it. `--disable-opt`
  keeps the real logic so there's something to simulate.
- `iverilog -DSYNTHESIS`: firtool emits a debug-only register/memory
  randomization block gated behind `ifndef SYNTHESIS`, using an
  `automatic`-lifetime variable declaration Icarus doesn't implement
  ("sorry: Overriding the default variable lifetime is not yet
  supported"). Defining `SYNTHESIS` skips that block entirely. This is
  safe: it only removes pre-reset debug randomization, and every
  register here is a `regreset` that's properly synchronously reset
  regardless.

## If firtool renames `m_ext`/`Memory`

`mem <name>` in a `.tr` file becomes an instantiated `<name>_ext`
submodule (`m_4096x16` for a 4096-entry `bits[16]` array) with an
internal `Memory` register array — those are firtool's own naming
choices, not this project's. If a firtool upgrade changes them, the
testbench's hierarchical paths will fail to resolve at compile time;
regenerate with `firtool --disable-opt` and grep the output for the
new names.
