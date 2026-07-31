# trace: an effect-based HDL with transactional semantics

Design document. This version supersedes the first handoff draft. It folds in the
resolutions from the 2026-07-30 design review. The prose follows Simplified Technical
English where possible: short sentences, active voice, one idea per sentence.

## The core idea

A clock cycle is a transaction. Register writes are speculative until the clock edge.
Rollback within a cycle is free: do not assert the register enable. The language builds
on this insight. It borrows the semantic core of Verse: failure as control flow, an
effect system, and a choice operator for specification.

Hardware is a set of **guarded atomic rules**, as in Bluespec. Each rule either commits
fully at the cycle boundary or has no effect. The compiler derives all handshaking from
failure. The user never writes a `ready` signal.

```
module FifoBridge {
    fifo input  : bits[8]
    fifo output : bits[8]

    rule transfer {
        x := input.Deq[]      -- fails when input is empty
        output.Enq[x]         -- fails when output is full
    }
}
```

Square brackets mark a fallible operation. If any operation in a rule fails, the whole
rule aborts for this cycle. The rule retries next cycle. The compiler turns the two
failure conditions above into ready/valid handshake logic. No stall logic appears in the
source.

A bare `?` tests a condition. Failure of the test aborts the rule.

```
rule drain {
    (mode == Draining)?       -- guard: rule only fires in Draining mode
    x := input.Deq[]
    count := count - 1
}
```

## The effect system

Effects give each piece of hardware a static "color". The type checker enforces the
colors. Effect annotations sit in angle brackets after the signature.

The effect *names* below are hardware-descriptive (`combines`, `sequences`,
`elaborates`, `fails`, `chooses`), not Verse's own vocabulary (`converges`,
`suspends`, `allocates`, `decides`, `choice`). The semantics are still Verse's; only
the surface spelling changed, so the words read naturally in a hardware context
instead of assuming familiarity with Verse.

| Effect                 | Meaning                       | Hardware                         |
| ----------------------- | ----------------------------- | -------------------------------- |
| `combines`              | total, pure, terminates       | combinational logic              |
| `sequences`             | crosses cycle boundaries      | FSM + registers (see lowering)   |
| `elaborates`            | runs at elaboration time only | no hardware; builds the circuit  |
| `fails`                 | can fail (inferred)           | guard inputs to the handshake    |
| `chooses`               | nondeterminism, spec-only     | none; model-checker free vars    |
| `reads R` / `writes W`  | state access rows             | input to the scheduler           |

The policy for inferred effects (`fails` and the rows) is uniform: the compiler
computes them; a stated effect is an interface assertion. Overstating is legal, because
a conservative claim is sound. Understating is an error.

### `combines`: combinational logic

A `combines` function cannot recurse, loop on circuit values, or suspend. It always
lowers to a pure combinational expression.

```
Parity(x : bits[8]) : bits[1] <combines> {
    return x[0] ^ x[1] ^ x[2] ^ x[3] ^ x[4] ^ x[5] ^ x[6] ^ x[7]
}
```

The checker rejects circuit-value loops in `combines` code:

```
Bad(x : bits[8]) : bits[8] <combines> {
    while x != 0 { x := x >> 1 }
    -- error[E012]: loop bound depends on a circuit value.
    -- Loops over circuit values need `<sequences>` (one iteration per cycle)
    -- or an elaboration-time bound under `<elaborates>`.
}
```

### `elaborates`: elaboration time

`elaborates` code runs once, before synthesis. It builds the circuit. Recursion and
dynamic allocation are legal here and only here. This makes the Chisel confusion between
elaboration time and circuit time (`if` vs `when`, Scala `var` in generator loops) a
type error instead of a silent bug.

```
AdderTree(xs : list[wire[bits[32]]]) : wire[bits[32]] <elaborates> {
    if len(xs) == 1 { return xs[0] }        -- `if` on an elab value: unrolls
    mid := len(xs) / 2
    return Add(AdderTree(xs[..mid]), AdderTree(xs[mid..]))   -- recursion: legal
}
```

An `if` on a circuit value inside `combines` code is a mux. An `if` on an elaboration
value inside `elaborates` code selects what to build. The effect of the scrutinee decides.
The user does not choose a keyword; the checker rejects mixtures that do not lower.

### `reads` / `writes` rows

Every rule gets a read set and a write set. The compiler infers them; users can state
them to assert an interface. The scheduler consumes these rows (see Scheduling).

```
rule refill <reads {pc, mem}, writes {ir}> {
    ir := mem[pc]
}
```

### `fails`: fallibility

Adapted from Verse's `<decides>`, renamed `fails` here to read as ordinary hardware
vocabulary. Code that can fail carries `fails`: a guard `?`, a fifo operation, or a
call to failing code. The compiler infers it bottom-up through the call graph. This
tracking is what makes handshake derivation compositional: a rule's derived ready
logic is the conjunction of guards from every failing call in its body, however deep.

```
Classify(x : bits[8]) : bits[2] <combines, fails> {
    (x != 0)?                 -- fallible: aborts the calling rule's cycle
    return clog2(x)
}

rule step {
    class := Classify(acc)    -- rule stalls until Classify succeeds
}
```

Context rules:

- A rule body is always a failure context. Failure aborts the cycle and retries.
  Rules never need to declare `fails`.
- Elaboration positions are never failure contexts. There is no transaction to
  abort. Guards and fifo ops in `elaborates` bodies, state types, and initializers
  are errors.

### Verse effects not carried over

Three other Verse effects were considered and folded away:

- `transacts` — every rule is a transaction by construction. The effect is ambient.
- `varies` (non-deterministic reads) — subsumed by a nonempty `reads` row.
- `diverges` — circuit code cannot diverge inside a cycle by construction.
  Termination of `elaborates` recursion is unchecked in v0; accept this.

## `sequences`: multi-cycle code without multi-cycle rollback

**Resolution of the main open question from the first draft.** A `sequences` block is
sugar. It lowers to a continuation register plus one single-cycle rule per segment.
There is no cross-cycle rollback, no checkpoint hardware, ever. The transaction is
always one cycle. "Cycle = transaction" survives intact.

The `tick` statement marks a cycle boundary. It cuts the block into segments.

```
Rmw(addr : bits[8]) <sequences, reads {mem}, writes {mem}> {
    v := mem[addr]
    tick
    mem[addr] := v + 1
}
```

The compiler produces this (shown as source; the real lowering is internal):

```
reg cont   : {S0, S1} = S0
reg v_save : bits[8]

rule rmw_s0 {
    (cont == S0)?
    v_save := mem[addr]
    cont := S1
}

rule rmw_s1 {
    (cont == S1)?
    mem[addr] := v_save + 1
    cont := S0
}
```

Each segment is an ordinary rule. Two kinds of step exist:

- **Progress step.** The rule fires, commits, and writes the next continuation value.
- **Blocked step.** A guard inside the segment fails. The rule aborts for this cycle.
  The continuation register keeps its value. The segment retries next cycle. This
  reuses the same failure = backpressure mechanism as `Deq[]`. It is not a new concept.

Values that cross a `tick` (like `v` above) get save registers automatically. The
compiler reports the cost: how many segments, how many saved bits.

### `sync`, `race`, `spawn`

These three constructs compose sequenced code. All three lower to rules and registers.

**`spawn`** starts a parallel FSM. It allocates a new continuation register. Spawn
counts must be static, because dynamic allocation needs `elaborates`. A spawn inside a
circuit-value loop is a type error.

**`sync`** joins parallel FSMs. It lowers to a rule guarded on the conjunction of the
joined continuations reaching their end states.

**`race`** takes the first of several sequenced computations to complete. The
competing continuations write the same result register. They become conflicting rules.
Arbitration falls out of the ordinary scheduler; there is no separate arbiter construct.

```
Fetch2(pc : bits[16]) <sequences> {
    h1 := spawn ReadBank(bank0, pc)
    h2 := spawn ReadBank(bank1, pc + 1)
    sync(h1, h2)                        -- both reads have landed
    ir := pack(h1.result, h2.result)
}
```

## `chooses`: specification, not synthesis

The choice operator `|` and the `any` form are **not synthesizable**. They exist for
specification and verification. Nondeterministic choice becomes a free variable in a
model checker, like SVA `$anyseq`. Implementations are checked as refinements of specs
that declare `chooses`.

```
spec AnyGrant(reqs : bits[N]) : bits[clog2(N)] <combines, chooses> {
    i := any(0..N-1)          -- free variable: the checker picks
    reqs[i]?                  -- constrained: the pick must be a requester
    return i
}

impl RoundRobin(reqs : bits[N]) : bits[clog2(N)] <combines>
    refines AnyGrant
{
    -- deterministic logic; checked against the spec
}
```

The `chooses` effect marks spec-only code. Only a `spec` may declare it. Using `any`
without it is a type error. Synthesizing code with it is a type error.

`|` is contextual: in an item without `chooses` it is bitwise or; in a `chooses` item it
is the choice operator. One token, disambiguated by the effect, never by the parser.

## Scheduling

**Resolution of review point 2.** The scheduler does not disappear; it becomes a
user-facing surface. The design treats schedule quality as a first-class UX problem.

The compiler builds a conflict matrix from the read/write rows. Two rules conflict when
one writes what the other reads or writes. Conflicting rules cannot fire in the same
cycle. An urgency order picks the winner.

Three tiers, in order of preference:

**Tier 1: diagnostics (v0).** The compiler always explains its schedule. This is where
Bluespec users spend their effort, so make the default legible.

```
$ trace build cpu.tr --explain-schedule
rule step_s1 conflicts with rule refill:
    both write {mem}          (mem is one resource in v0; see Arrays)
urgency: step_s1 > refill     (declaration order; no annotation given)
derived stall: refill fires only when step_s1 is blocked or idle
```

**Tier 2: annotations (v0).** When the default is wrong, the user shapes it. The
compiler checks `conflict_free` claims with inserted simulation assertions. It does not
trust them blindly.

```
schedule {
    urgency step_s1 > refill
    conflict_free { read_port, write_port }   -- checked in simulation
}
```

**Tier 3: provable disjointness (later, not v0).** Dahlia-style banked and affine array
types would let the compiler prove two accesses disjoint and drop the conflict. This is
a real type-system feature on its own. It is explicitly out of scope for v0 so that the
scheduler work stays bounded.

## Arrays and aliasing

**Resolution of review point 3.** In v0, a whole array is one resource. Any two
accesses to the same array conflict unless both are reads. There is no partial
disjointness proof in v0.

```
rule a { x := m[i] }      -- reads {m}
rule b { m[j] := y }      -- writes {m}
-- v0: a conflicts with b, even if i != j always holds.
```

Consequence: designs with one memory serialize on it, one access per cycle. That is
honest behavior for a single unbanked, single-port memory. Banking is the tier-3
extension above. The read/write-row shape does not change when banking arrives; only
the granularity of resources does.

## Inference: two solvers, not one

**Resolution of review point 4.** "Constraint solving" in the first draft named two
different algorithms. Keep them separate.

**Pass 1: parameter and type inference.** Ordinary unification. Runs at elaboration
time, where recursion is legal.

```
Fifo(depth : int, T : type) { ... }

f := Fifo(_, bits[8])
f.Enq[x]                  -- x : bits[8] unifies; depth still free
g := connect(f, deep16)   -- deep16 : Fifo(16, _) → depth := 16, T := bits[8]
```

**Pass 2: width inference.** A monotone fixed-point over an interval lattice. Runs
after elaboration, on the concrete circuit. Widths only grow; the pass terminates at
the fixed point.

```
let sum = a + b           -- |sum| = max(|a|, |b|); modular, Chisel-style
let prod = a * b          -- |prod| = |a| + |b|
let idx = pc + 3          -- an int literal absorbs the other width: bits[16]
sum := trunc(sum, 8)      -- explicit narrowing; silent truncation is an error
```

`+` and `-` are modular: the result keeps the max operand width, so
`pc := pc + 3` is legal. A carry-preserving grow-add (`+&`, width max+1) can come
later as a distinct operator. The first draft gave plain `+` the max+1 rule; that
contradicted this document's own SUBLEQ example, so modular won.

Do not build one solver for both jobs. The first draft implied one mechanism; that
was wrong.

## Combinational loops

**Resolution of review point 5.** The `combines` effect
guarantees acyclicity only inside one scope, via a no-forward-reference rule. It does
not guarantee acyclicity across module boundaries. Two internally-acyclic modules wired
output-to-input in a cycle still form a real combinational loop.

v0 position:

- Enforce no-forward-reference inside `combines` scopes. Local cycles are
  inexpressible.
- Do not build a whole-program cycle checker. The backend is firtool (CIRCT), and its
  `CheckCombLoops` pass already detects loops. Map its diagnostics back to source
  spans.
- Known blind spot: `CheckCombLoops` has historical gaps around multi-top-module
  designs (CIRCT issue #1138). Accept this in v0. Record it.

The airtight fix is Filament-style timeline types on ports, which make cross-module
feedback inexpressible by construction. That is a large feature. It is not v0.

## Architecture

Front end only. **Do not write a backend.** Emit an existing IR as text.

- Primary target: **FIRRTL** text (`.fir`) → `firtool` (CIRCT) → Verilog.
- Alternative to evaluate: **Calyx**. It is semantically closer to the rule/control
  world. Filament compiles to it.

Pipeline:

```
logos lexer
  → hand-written recursive-descent parser (Pratt core for expressions)
  → index-based AST (arena, NodeId(u32), side tables)
  → semantic passes: effect check → unification → width fixed-point → scheduling
  → simulator + FIRRTL/Calyx emission
```

## Implementation decisions (Rust)

- **Host language: Rust.** Chosen for parser-writing familiarity. Scala 3 deep
  embedding was the considered alternative.
- **Hand-written recursive descent**, not a parser library. Syntax will churn.
  Recursive descent is what rustc does. It gives the best error messages.
- **Pratt parser for expressions.** The language is expression-oriented with many
  custom operators (`|`, `?`, `:=`, ranges). Pratt makes each operator tier one
  binding-power table entry. Reference: matklad, "Simple but Powerful Pratt Parsing".
- **logos** for the lexer. **ariadne** or **codespan-reporting** for diagnostics.
- **Braces and newlines in v0**, not indentation sensitivity. All snippets in this
  document use braces. Indentation later is a lexer-only change: an indent stack that
  emits synthetic INDENT/DEDENT/NEWLINE tokens, Python-style. The parser never knows.
- **Index-based AST.** Nodes in per-kind arenas. Children referenced by `NodeId(u32)`.
  Analysis results in side tables (`HashMap<NodeId, EffectRow>`). This avoids
  borrow-checker fights in annotation passes. Skip rowan and lossless trees in v0.
- If a parser library is ever wanted: **chumsky**. lalrpop is useful later only as an
  ambiguity check on the grammar.

## First milestone: SUBLEQ

**Achieved 2026-07-30.** SUBLEQ's read-modify-write-branch step is one natural
transaction. Target: parse → check → schedule → emit FIRRTL → firtool → simulate.
The port must demonstrate derived stall logic that the Chisel version wrote by hand.

Every stage runs on `examples/subleq.tr` today, ending in a real simulation, not just a
compile: `devenv shell -- simulate subleq` (or `tests/sim.rs`) lowers the file, emits
FIRRTL, runs it through `firtool`, and simulates the resulting Verilog with a hand-written
Icarus Verilog testbench (`sim/subleq_tb.v`) against a small SUBLEQ program (`mem[11] -=
mem[10]`, then an unconditional jump, then halt). It passes: the CPU computes `8 - 3 = 5`
and halts at the right address, not a wrong one a mis-taken branch would reach.

One honest gap the testbench works around: at the time this milestone landed, the
language had no `input`/`output` port concept, so the emitted `Subleq` module exposed
only `clock`/`reset` — nothing else was observable or drivable from outside. The
testbench reaches in with hierarchical paths (`dut.pc`, `dut.m_ext.Memory[i]`) instead
of real ports, which Icarus allows with no special flags (Verilator would need
`--public`). Real ports landed afterward (see "Module ports" below) and retire this gap
for scalar designs, but SUBLEQ itself still uses hierarchical paths: there is still no
way to load a `mem` through a scalar port, so `sim/subleq_tb.v` is unchanged.

Sketch, honest about v0 array rules (single-port memory → one access per tick):

```
module Subleq {
    reg pc : bits[16] = 0
    mem m  : bits[16][4096]

    rule step <sequences> {
        a := m[pc]
        tick
        b := m[pc + 1]
        tick
        c := m[pc + 2]
        tick
        va := m[a]
        tick
        r := m[b] - va       -- second operand read; one array access per tick
        tick
        m[b] := r
        if r <= 0 { pc := c } else { pc := pc + 3 }
    }
}
```

The lowering makes the cost visible: the compiler reports the segment count and the
saved-register bits for `a`, `b`, `c`, `va`, `r`. A banked memory (tier 3, later)
would collapse the fetch ticks. That improvement lands without changing this source
shape, only its schedule.

## Module ports

**Achieved 2026-07-30.** Two new declarations, alongside `reg`/`mem`/`fifo`:

```
input inc : bits[8]           -- external combinational signal, read-only
output sum : bits[8] = 0      -- register-backed, exposed as a port
```

`input` is a pure wire driven from outside the module; reading one inside a rule reads
this cycle's value, and writing one is a resolve-time error ("input ports are
read-only"). `output` looks like a plain `reg` from inside a rule — same `:=` write,
same effect-row treatment, same scheduling — but is also exposed as a module port.

The interesting decision is that `output` is **register-backed, never combinational**.
A combinational (Mealy) output would expose a rule's value mid-cycle, before the clock
edge — but this whole design's core invariant is that a rule's writes are speculative
until the clock edge (that is what makes same-cycle rollback free; see "The core
idea"). A combinational output would leak the speculative value out of the module,
which breaks that invariant for anyone watching from outside. So `output x` compiles to
an ordinary internal register plus one port, connected unconditionally
(`connect x, <internal register>`); rules read and write the register, and the outside
world sees the committed value one cycle after it is computed. One real consequence:
`output sum = a + b` is not expressible as a pure combinational function of two inputs —
it is one cycle delayed, like every other piece of state in this language. Purely
combinational modules (no state at all) are not a target for v0.

```
module Accumulator {
    input inc : bits[8]
    output sum : bits[8] = 0

    rule accumulate {
        sum := sum + inc
    }
}
```

This example (`examples/accumulator.tr`) is also the first design simulated through
*real* Verilog ports (`sim/accumulator_tb.v`) rather than hierarchical peek/poke, and
the first to compile with plain `firtool` — no `--disable-opt` — since an observable
output port is enough to stop dead-code elimination from erasing the design. (SUBLEQ
still needs `--disable-opt`, since loading its `mem` still has no port-based path; see
the milestone section above.)

## FIFO synthesis

**Achieved 2026-07-30.** `FifoBridge`, this document's opening example, now emits real
hardware:

```
module FifoBridge {
    fifo input  : bits[8]
    fifo output : bits[8]

    rule transfer {
        x := input.Deq[]      -- fails when input is empty
        output.Enq[x]         -- fails when output is full
    }
}
```

v0 fifos are **depth-1 buffers**: one data register plus one valid bit. `Deq[]`
succeeds only while valid; `Enq[x]` succeeds only while *not* valid (there is no room
for a second element). Both failure conditions fold into the rule's guard exactly like
an explicit `?` — the compiler ANDs them into the same `fires` signal that already
carries every other guard, so "derive ready/valid handshaking from failure" (this
document's opening claim) now covers fifos, not just guards. One consequence stated
honestly, not hidden: a rule cannot both `Enq` and `Deq` the *same* fifo in one cycle —
that would require its valid bit to be 1 (for `Deq`) and 0 (for `Enq`) at once, an
always-false guard, so the compiler rejects it outright rather than silently
synthesizing permanently dead hardware. `Enq`/`Deq` calls must stay at a rule's top
level, same restriction as an explicit guard, and for the same reason (a guard nested
in `if`/`while` isn't threaded through the `fires` computation yet).

Compiling `x := input.Deq[]` then `output.Enq[x]` surfaced a real, previously-latent
gap: `x` is a local, and this is the first example where a local's value is *used*
later in the same emitted rule, rather than only ever appearing on one side of an
assignment. FIRRTL has no notion of a `let`-bound name — locals are wires, not
declarations — so referencing one must *inline* its binding rather than emit an
undeclared identifier. The emitter now tracks each rule's local bindings and resolves a
local reference by recursively compiling whatever it was bound to (`x` compiles to
`input`'s data register directly, `__fifo_input_data`), the same way a Verilog reader
would mentally substitute a `let` before reading the hardware it describes. One sharp
edge this inlining creates: a local reassigned within one rule has only one binding in
the emitter's table, so a read between the two assignments would silently inline the
*wrong* (later) one — a genuine miscompile, and one no current example happened to
exercise. Reassigning a local inside an emitted rule is therefore an explicit v0 error,
not a silent trap.

`sim/fifo_bridge_tb.v` proves both halves of the claim through real simulation, not
just a passing compile: a value placed in `input` reaches `output` unchanged one cycle
later (the forward path), and a rule that would overflow `output` correctly stalls
(does not fire, does not drop or overwrite `input`) until `output` drains. `FifoBridge`
has no module ports — fifos are an internal-only construct, there is no fifo-port
concept — so, like SUBLEQ, the testbench reaches in with hierarchical paths
(`dut.__fifo_input_valid`, `dut.__fifo_input_data`, ...) rather than real ports.

## Port-based memory access

**Achieved 2026-07-30.** Loading and observing a `mem` through ordinary module ports
needed **no new compiler machinery at all** — it falls out of `input`/`output` (already
supported) plus an ordinary guarded write:

```
module PortRam {
    mem m : bits[16][256]

    input addr : bits[8]
    input write_data : bits[16]
    input write_en : bits[1]
    output read_data : bits[16] = 0

    rule write {
        (write_en == 1)?
        m[addr] := write_data
    }

    rule read {
        read_data := m[addr]
    }

    schedule {
        urgency write > read
    }
}
```

`write`'s guard (`write_en == 1`) is an ordinary explicit `?`, already fully supported;
`m[addr] := write_data` is an ordinary top-level memory write, already fully supported;
`read_data := m[addr]` is an ordinary output write whose right side happens to be a
memory read, and output writes already accept any expression the emitter can compile.
Nothing needed to change in `firrtl.rs` to make this compile and simulate — the
capability was already there, just never assembled into an example. `write` outranking
`read` (an explicit `schedule` directive, not an accident of declaration order) means a
cycle that writes never races a same-cycle read: `read_data` correctly holds its old
value on a write cycle rather than reading a half-committed word. `sim/port_ram_tb.v`
proves it through real ports: two distinct addresses, written and read back without
aliasing.

**This closes the general capability, not SUBLEQ's specific gap.** SUBLEQ needs to load
an entire *program* — a whole memory's worth of words — before the CPU's own rules
start running, and its rules (`step`, `refill`) start firing the instant `reset`
clears, racing any port-driven load sequence. `PortRam`'s pattern works per-word, on
demand, with no notion of "not yet loaded" — good enough for a RAM, not for booting a
CPU from a cold `mem`. Making that work needs a real design decision this document does
not make yet: some kind of boot/load mode that holds `step`/`refill` off until loading
finishes (an extra `input` gating their guards would work structurally, but *deciding*
that shape, and whether it generalizes past SUBLEQ, is undone design work, not an
emitter gap). `sim/subleq_tb.v` still pokes `dut.m_ext.Memory[i]` directly and is not
changed by this section.

## Submodule instantiation

**Achieved 2026-07-30.** Modules compose by name, not lexical nesting — a `module` is
just another item, and `inst name : Module` names one to instantiate:

```
module Adder {
    input a : bits[8]
    input b : bits[8]
    output sum : bits[8] = 0

    rule add {
        sum := a + b
    }
}

module Top {
    inst adder : Adder

    input x : bits[8]
    input y : bits[8]
    output result : bits[8] = 0

    rule wire {
        adder.a := x
        adder.b := y
        result := adder.sum
    }
}
```

`inst name : Module` declares a child instance, parsed by the same `name : type-expr`
grammar as `reg`/`mem`/`fifo`/`input`/`output` — `Module` is just an identifier in that
slot instead of a `bits[...]` shape. A port is accessed as `instance.port`, reusing the
existing `.field` expression the parser already had (previously only used for fifo's
`Enq`/`Deq`): `adder.a := x` writes a child's input port, `result := adder.sum` reads a
child's output port. Writing an output port or reading an input port is a resolve-time
type error, not a wiring mistake that only shows up as broken hardware.

Two design choices carried over deliberately from elsewhere in this document:

- **Conflict model.** Unlike a v0 array ("Arrays and aliasing" above, still one whole-array
  resource), each instance port is its own conflict resource (**achieved 2026-07-31**):
  two rules writing different ports of the same instance don't conflict, and can fire the
  same cycle. This is a much easier case than array-index disjointness — a port name is
  static and lexical, known at resolve time, so telling two ports apart needs no runtime
  proof (Dahlia-style banking, tier 3) at all. resolve.rs synthesizes one resource `DefId`
  per `(inst, port)` pair the first time it's referenced (memoized, so every `c.a` in the
  file shares one), and effects.rs's read/write-row inference for `inst.port` uses that
  resource instead of the instance's own `DefId` — the scheduler itself is unchanged,
  since a conflict is still just "two rules' rows share a `DefId`."
- **Nested writes.** A port write may live inside `if`/`else`, threaded through a `mux`
  exactly like a register write (**achieved 2026-07-31**) — an unwritten path falls back
  to the port's unconditional `UInt(0)` default rather than holding a stale value, since
  a port (unlike a register) has no state of its own. A memory write still can't nest:
  that restriction remains.
- **Lexical nesting.** A `module` may itself be declared inside another module's body
  (**achieved 2026-07-31**) — purely a naming convenience, not a hardware relationship:

  ```
  module Top {
      module Adder {
          input a : bits[8]
          input b : bits[8]
          output sum : bits[8] = 0
          rule add {
              sum := a + b
          }
      }
      inst adder : Adder
      ...
  }
  ```

  `Adder` is visible only within `Top` (resolve.rs pushes a fresh scope per module,
  popped on exit — the same mechanism that already kept a `reg`/`rule` name invisible
  outside its own module) — a sibling module cannot `inst` it. FIRRTL itself has no
  nested-module concept, so this changes nothing about emission: a nested module still
  becomes its own top-level FIRRTL block, found by walking the whole AST for `Item::Module`
  regardless of depth instead of only `ast.roots`. Parsing, resolution's own per-module
  scoping, effect inference, and scheduling already worked at arbitrary nesting depth
  with no changes at all — FIRRTL emission was the one place still hard-assuming every
  module a file-level root.

  Nesting surfaced a real gap worth stating plainly: **modules share no state with each
  other, however they're lexically arranged.** Before nesting existed, this was true by
  construction (sibling modules' scopes never overlapped on the resolver's scope stack).
  Once a module's body can resolve names from an enclosing scope, a nested module's rule
  could accidentally reference its *parent's* own `reg` — resolving successfully in
  resolve.rs (the name genuinely is in scope, lexically), but referencing a register that
  doesn't exist in the nested module's own emitted FIRRTL text, since each module is still
  emitted as an independent block. Previously this reached `firtool` as a raw "use of
  unknown declaration" error with no link back to the `.tr` source. Fixed by tracking each
  definition's owning module (`None` for one declared outside any module) and rejecting,
  at resolve time, any reference to state whose owner isn't the innermost enclosing
  module — an `inst` target name is deliberately exempt (that lookup crossing a module
  boundary is the entire point of nesting), resolved through a separate path that skips
  the check.

The interesting part was emission, not the front end. Resolution, effect inference, and
scheduling needed only small, structurally obvious additions (a new `DefKind::Inst`, a
`Field`-write case alongside the existing `Bracket`-write case) because a "module" was
already just an item like any other — a flat file with several top-level modules and no
`inst` between them typed and scheduled correctly *before* this section existed, one
`GroupSchedule` per module already, since `schedule.rs`'s module-scoping recursion
predates this work. FIRRTL emission is the one place a module was still hard-assumed
singular. It now works in two passes: first, find *the top* — the one top-level module
nobody else instantiates (ambiguous otherwise: zero candidates means a cycle, more than
one means unrelated designs sharing a file, both explicit errors) — then walk the `inst`
graph out from it, emitting every reachable module once, with a cycle in that graph
(a module instantiating itself, even indirectly, which has no hardware meaning) caught
before it can recurse forever. Each module keeps its own `Emitter`, independent of any
other module being emitted alongside it.

The one genuinely new wiring rule: a FIRRTL instance's `clock`/`reset` are input ports
like any other, and FIRRTL requires every instance input driven on every path — so they
connect unconditionally (`connect adder.clock, clock`), not gated by whichever rule
happens to be driving the instance's other ports this cycle. Every other input port
defaults to 0, then the (at most one) firing rule that writes it overrides via
last-connect, the same priority-mux pattern a mem writer already uses. Reading an output
port needs no wiring at all: `adder.sum` compiles straight to the FIRRTL reference
`adder.sum` — the child drives it unconditionally, so there is nothing to gate.

`sim/submodule_tb.v` proves it through real simulation and makes the latency honest: a
child's `output` is one cycle behind its inputs (same as any `output`), and the parent's
own `output result := adder.sum` is a *second* register hop behind that — `result`
reflects `x`/`y` two cycles after they are driven, not one. This is the same "state is
never combinational" rule as "Module ports" above, just compounding once per hop through
the hierarchy: a real, honestly-modeled consequence of composition, not a hidden
surprise.

## Expression surface

**Widened 2026-07-30.** FIRRTL emission's expression surface used to be idents,
integer literals, `+`/`-`, comparisons, and memory reads — enough for SUBLEQ and
Rmw, nothing more. It now also covers multiply, the three bitwise ops, static
(literal-amount) shifts, unary negate/complement, and bit-select/slice:

```
module Alu {
    input a : bits[8]
    input b : bits[8]

    output prod : bits[16] = 0   -- a * b
    output shl3 : bits[8] = 0    -- a << 3
    output shr3 : bits[8] = 0    -- a >> 3
    output lo4 : bits[4] = 0     -- a[3..0]

    rule compute {
        prod := a * b
        shl3 := a << 3
        shr3 := a >> 3
        lo4 := a[3..0]
    }
}
```

`examples/alu.tr` exercises the whole set; `sim/alu_tb.v` picks operand values whose
high and low bits differ, so a shl/shr mix-up or a truncated (instead of widening)
multiply shows up as a wrong output value, not just a design firtool happens to
accept.

Two width rules already existed in the type checker (`types.rs`) and just needed a
FIRRTL primop that matched them:

- **Multiply doesn't truncate when both sides are `bits`** — the checker sums their
  widths (`a * b` on two `bits[8]`s is `bits[16]`), and FIRRTL's `mul` primop already
  produces exactly that sum, so `prod := a * b` compiles straight to `mul(a, b)`, no
  `tail` needed. Multiplying by a bare literal is different: the checker keeps the
  *other* operand's width instead of summing (`x * 3` on `bits[8]` stays `bits[8]`),
  but `mul` itself still sums both compiled widths — so that case needs a `tail` to
  drop back down, the same idea as `add`/`sub`'s carry-bit truncation but by a
  variable amount instead of a constant 1.
- **Shifts keep the left operand's width**, matching Verilog's fixed-width `<<`/`>>`
  rather than FIRRTL's own `shl`/`shr` (which grow/shrink the width so no bits are
  lost). `shl3 := a << 3` compiles to `tail(shl(a, 3), 3)` — shift up, then drop the
  high bits that fell off; `shr3 := a >> 3` compiles to `pad(shr(a, 3), 8)` — shift
  down (discarding the low bits), then zero-pad back up to the original width. Only a
  literal shift amount is supported (v0 restriction): FIRRTL's `shl`/`shr` need a
  static amount, and a dynamic-amount `dshl`/`dshr` isn't wired up yet.

Bit-select and slice (`x[i]`, `x[hi..lo]`) compile to FIRRTL's `bits(x, hi, lo)`
primop, which also needs static bounds — so, like shifts, only literal-integer
indices are supported; a computed bound is a v0 restriction, not something the type
checker would otherwise reject (it happily types a dynamic single-bit select as
`bits[1]`). Unary `-` compiles to `tail(sub(UInt<w>(0), x), 1)` (two's-complement
negate, wrapping within the operand's own width, same convention as `+`/`-`); unary
`~` compiles straight to FIRRTL's `not`. Logical `!` was deliberately left out this
pass (added later — see "Logical `!`" below).

**Verilog-style sized literals, added 2026-07-31**: `<width>'<radix?><value>` — e.g.
`8'd6`, `8'hFF`, `8'b1010`, `8'o17`, or `8'6` (no radix letter, defaulting to decimal
like `'d`). Lexed as its own token (`SizedInt`), longer than the plain `Int`
alternative for the same input, so logos's longest-match rule always prefers it — no
ambiguity with a bare integer immediately followed by a field access or anything else
starting with `'`.

This is a SECOND, deliberately different literal-typing model, not a variant of the
existing one. A bare `Expr::Int` has no width of its own: it types as the coercible
`Ty::Int`, absorbs a width from whatever context it's used in (`check_assignable`'s
`(Ty::Int, Ty::Bits(_))` arm, `type_binop`'s mixed-operand arm), and is only
overflow-checked later, at that coercion site. A sized literal has a definite width
BY CONSTRUCTION: it types directly as `Ty::Bits(Width::Known(width))` and is
range-checked immediately, against its OWN declared width, right where it's written —
`4'd20` is an error ("20 does not fit in bits[4]") even in a context that could
otherwise absorb a wider value, and this doesn't defer to `check_literal_fits` the way
a bare `Int` would. Confirmed with Lumi via AskUserQuestion before implementing:
overflow is a compile error, not silent truncation like Verilog's own `4'd20` = 4 —
keeping trace's existing anti-silent-truncation stance (the same reasoning behind
requiring an explicit `trunc()` call elsewhere) rather than adopting Verilog's
behavior just because the syntax is borrowed from it.

Because it already has a real width, emission (`Expr::SizedInt` in `firrtl/expr.rs`)
ignores any caller-supplied hint and always emits `UInt<width>(value)` directly — the
opposite of a bare `Int`, which needs a hint to know what width to emit at.
`compile_bit_select` and `compile_shift` were refactored from matching `Expr::Int`
literally to using `const_eval` (which already transparently accepts either literal
kind), so a sized literal works as a bit-select bound or shift amount for free, no new
code needed there. When a sized literal's own width doesn't match its context's
(narrower than an arithmetic operand, or a bare connect into a wider output), the
mismatch is left to FIRRTL itself to resolve — confirmed empirically, not assumed:
`result := 8'd6` into a `bits[16]` output compiles straight to
`connect __out_result, UInt<8>(6)` and firtool accepts it, zero-extending on
elaboration (`16'h6`), the exact same implicit-widening rule it already applies to two
real registers of differing widths; `x == 8'd6` against a wider `x` behaves the same
way through FIRRTL's own `eq` primop. Pinned by
`sized_literal_widens_via_a_bare_connect_into_a_wider_target` and
`sized_literal_compares_against_a_wider_operand` (tests/firrtl.rs).

`examples/sized_literal.tr` proves it through real firtool and Icarus simulation,
using a sized literal both in arithmetic (`x + 8'd6`) and as a bit-select bound
(`x[8'd3]`).

**Inferred `reg`/`output` types from a sized literal, added 2026-07-31.** `reg name :
ty = init` and `output name : ty = init` may omit `: ty` when `init` is a sized
literal — `reg a = 8'd6` declares an ordinary `bits[8]` register, identical in every
respect to writing `reg a : bits[8] = 8'd6` out by hand. The parser synthesizes the
same `bits[width]` expression the explicit syntax would parse to (an `Expr::Ident("bits")`
Bracket-applied to the literal's own width, both using the literal's span) and splices it
into the item — nothing downstream of the parser (resolve, effects, types, scheduling,
emission) can tell the difference, since it's the identical AST shape either way. This
is why the feature needed no changes anywhere but parser.rs: the inferred type gets
resolved, overflow-checked, and emitted exactly like a hand-written one, confirmed by a
test that writes an out-of-range value against the *inferred* width and gets the same
"does not fit" error a hand-written `: bits[8]` would.

Scope is deliberately narrow: inference only fires when the initializer is *literally*
a sized literal — `reg a = 8'd6 + 8'd1` and `reg a = 6` (a bare, width-less `Int`) both
still require an explicit `: ty`, same restriction-class as shift amounts and
bit-select bounds needing a literal elsewhere in this doc. The reason is the same one
that makes a sized literal special to begin with: it's the only expression with a
definite width before any type-checking has run, so it's the only one a purely
syntactic (parse-time) inference can read a width off of without doing real type
inference. `mem`/`fifo`/`input`/`inst` are unaffected (and unaffectable) — none of them
has an `= init` for a type to be inferred from.

`examples/infer_reg_ty.tr` + `sim/infer_reg_ty_tb.v` prove the inferred width is a real
`bits[8]`/`bits[16]`, not just accepted syntax, through real ports: `acc` (an inferred
`reg`) starts at 6 and accumulates `inc`, while `hi` (an inferred `output`, never
written by any rule) holds its `0xFF00` reset value forever — both observed directly,
not inferred from "it compiled."

**`/` and `%`, added 2026-07-31** (Lumi's pick off TODO.md's expression-surface sublist,
recommended as the most mechanical remaining gap — no new design decision needed, unlike
logical `!`'s bit-vs-whole-value ambiguity). Types.rs already typed both (the same
`max(w(a), w(b))` default rule `+`/`-`/bitwise share); the only gap was firrtl.rs's
`compile_binop` rejecting them outright. Turned out NOT to share `mul`'s "sum, then trim
the excess" shape, and I didn't trust memory of the FIRRTL spec text for this — confirmed
empirically against real firtool first, using a `node` (not a `connect` into an
explicitly-widthed output, which would silently mask the primop's own width behind an
implicit truncate/extend): `div(a, b)`'s own width is exactly the DIVIDEND's width
(`w(a)`, UInt semantics), `rem(a, b)`'s is `min(w(a), w(b))` — both always ≤ the checker's
own target width, so `compile_binop`'s new `Div`/`Rem` arms only ever need a `pad` UP
(never a truncating `tail` down the way `add`/`sub`/`mul` do).

`examples/div_rem.tr` deliberately uses DIFFERENT-width operands (`a : bits[8]`, `b :
bits[4]`) so both directions run for real: `a / b` needs no pad (FIRRTL's div width
already equals the target, 8, since `a` is the wider dividend), `b / a` and both `%`
directions do (FIRRTL's own width is narrower than the checker's target whenever the
operands differ). **This surfaced a genuine Icarus-only simulation gap, unrelated to
div/rem's own correctness — caught by running the real testbench, not assumed from the
unit tests passing:** a cross-width intermediate (one operand zero-extended to match the
other's width before the primop runs) can lower to an `automatic logic` declared INSIDE
an `always` block rather than a top-level `wire` — a NEW source of the same "Icarus
doesn't implement `automatic`-lifetime overrides" limitation `-DSYNTHESIS` already works
around for firtool's debug randomization block (see sim/README.md), never previously hit
because every earlier example only ever crossed an always block with same-width
operands. Worked around with firtool's own `-lowering-options=disallowLocalVariables`
flag (forces the top-level-`wire` form always), added unconditionally to both
`tests/sim.rs`'s `firrtl_to_verilog` and `devenv.nix`'s `simulate` script — confirmed
harmless where not strictly needed (semantically a no-op, wire vs. automatic-local
choice only) and confirmed BOTH paths actually pass end to end, not just the automated
suite: `simulate div_rem` (the documented, user-facing command) and `cargo test` both
report the correct `q1=15 q2=0 r1=5 r2=13` (`a=200, b=13`: `200/13=15`, `13/200=0`,
`200%13=5`, `13%200=13`).

**Logical `!`, added 2026-07-31, resolving the "bit-vs-whole-value ambiguity" flagged
above as the reason it was left out of the div/rem pass.** Lumi raised the actual design
question directly: does this language need `!` to mean something DIFFERENT from `~`
(C-style "nonzero is true" truthiness for a wide value), or is it purely redundant with
`~` given comparisons/guards already produce `bits[1]`? Checked before answering, not
assumed: `check_cond` (types.rs, gating every `if`/`while` condition) already requires
EXACT `bits[1]` type equality — `"condition must be bits[1], got {other} (compare
explicitly)"` — there is no implicit "nonzero is true" coercion anywhere in this
language for a wider `!x` to usefully mean. So the ambiguity resolves itself: wherever
`!` would matter, the operand is already 1 bit, where bitwise-complement and logical
negation are the identical operation. Also checked (and ruled irrelevant): `!=` lexes as
its own atomic token (`BangEq`), never composed from unary `!` + `=`, so nothing about
`!`'s own semantics has any mechanical bearing on `!=` — no `~=` rename is needed either
way, that would be a pure surface-syntax aesthetic choice, not a technical consequence.

Given a genuine choice between "`!` is pure sugar for `~`" and "`!` is a distinct
operator that additionally REQUIRES `bits[1]`," Lumi picked the latter via
AskUserQuestion — a real guardrail, not just an alternate spelling: `!x` on a `bits[8]`
almost certainly means "did you mean a comparison, or `~`?", not "flip every bit," and
now gets caught at type-check time instead of silently compiling to a bitwise complement
nobody intended. Mechanism: `types.rs`'s `Expr::Unary` arm (previously ignoring `op`
entirely — `Neg`/`Not`/`BitNot` all took the identical same-width-passthrough path) now
special-cases `UnOp::Not` to additionally require `Ty::Bits(Width::Known(1))` (tolerating
`Width::Unknown`/`Ty::Int`/`Ty::Unknown` the same way `check_cond` does, for a generic
body not yet concretely instantiated) — `~`/`-` are unaffected. Once that's enforced,
emission (`firrtl/expr.rs`'s `compile_unop`) needed no new logic at all: `UnOp::Not` and
`UnOp::BitNot` now share one match arm, both emitting the identical `not(...)` FIRRTL
primop, proved by asserting `!(x == 0)` and `~(x == 0)` compile to byte-identical FIRRTL
text (`tests/firrtl.rs`'s
`logical_not_compiles_identically_to_bitwise_not_on_a_bits_1_value`) — no new example or
simulation testbench needed, since `~`'s own `not` primop is already proven correct
through real Icarus simulation by `alu_tb.v`. Pinned in `tests/types.rs`'s
`logical_not_needs_a_bits_1_operand`.

**A separate, pre-existing looseness surfaced while investigating, deliberately left
alone rather than folded into this pass:** unlike `if`/`while`, a bare guard `expr?`'s
inner expression has NO `bits[1]` requirement anywhere in the type checker (`type_expr`'s
`Expr::Guard` arm just inherits whatever type the inner expression already has). This is
unrelated to `!` specifically — it would need its own investigation into whether it's
reachable/harmful in practice — noted here rather than silently ignored, not yet added to
TODO.md since its actual impact isn't confirmed.

## Calling a function from a rule

**Achieved 2026-07-31.** A rule may call a user `fn`/`impl` (not `spec` — those stay
verification-only, effects.rs already rejects calling one outside spec-only code before
emission ever runs):

```
Avg(a : bits[8], b : bits[8]) : bits[8] <combines> {
    let sum = a + b
    return sum >> 1
}

module Top {
    input a : bits[8]
    input b : bits[8]
    output result : bits[8] = 0

    rule compute {
        result := Avg(a, b)
    }
}
```

FIRRTL has no function-call concept, so the callee is inlined at its call site — its
body spliced into the caller, the same way a rule-local (`let x = ...` or `x := ...`)
already gets inlined by reference rather than declared as its own wire. A call's
arguments bind to the callee's parameters through that exact mechanism (`Emitter::
locals`, previously only accepting `DefKind::Local`, now also accepts `DefKind::Param`)
— no new substitution machinery needed, just widening an existing kind check.

v0 restricts the callee to a body the inliner can splice with zero ambiguity: zero or
more `let` bindings, then either exactly one trailing `return <expr>` or an `if`/`else`
whose branches both recurse into that same shape. Everything else about a richer
function — state writes, guards, fifo ops, a call to yet another function — is an
explicit error, not silently dropped or partially inlined. The no-further-calls
restriction is worth spelling out: a callee whose own body cannot call anything can
never call itself, directly or through a cycle, so recursion needs no separate check —
it's ruled out by construction, for free, by the same restriction that keeps inlining
simple. A builtin call (`prio`, etc.) is a different, still-unsupported gap — nothing
about builtin-call synthesis is implied by this work.

**Branching callee bodies, added 2026-07-31** (the followup this TODO bullet flagged for
itself when the base feature shipped): `Max(a, b) { if a > b { return a } else { return b
} }` now inlines. `compile_call` delegates the whole body to a new recursive
`compile_callee_body(stmts, hint, span)`: the tail statement is either a `return <expr>`
or an `if`/`else`, and each branch recurses into that identical shape, folding into
`mux(<cond>, <then-value>, <else-value>)` — the same threading pattern the register- and
instance-port-write paths already use for if/else-nested writes (`reg_value_in_stmts`,
`inst_port_value_in_stmts`), reused here for a *return* value instead of a write target.
The `else` is mandatory, unlike a register or port write's optional-branch-holds-the-old-
value fallback: a function's return has no prior value to fall back on, every reachable
path through the callee must produce one, or the call is rejected outright (`an if`
without `else` in tail position, or an `if` anywhere before the tail, both error rather
than silently picking a default). Each branch's own `let`s bind and restore around that
branch's own recursive call — sequenced, not concurrent, so the `else` branch never sees
the `then` branch's locals still bound, even when both branches declare a `let` with the
same name (each `Let` statement gets its own `DefId` regardless of shared spelling).
Scope deliberately excludes state-writing callees in this pass — extending to those needs
`check_module_boundary` re-validated at the call site's module (see the pass's own
followup note in TODO.md), a separate, larger change not needed for a purely
value-computing branch. `examples/call_branch.tr` proves both branches pick correctly
through real firtool and Icarus simulation.

**Call-site module-boundary check, added 2026-07-31**, closing a real, previously
untested gap rather than extending scope: `check_module_boundary` in resolve.rs only
ever runs ONCE, at a state reference's own lexical position — i.e. wherever the
referencing `fn`/`impl`'s body happens to sit in the source — and never again. A `fn`
nested inside module `M` stays visible to a rule in a module nested INSIDE `M` (scopes
nest outward-to-inward, the same way an ordinary local from an enclosing scope stays
visible), so `M { reg v...  Bump(x){ return v+x }  module N { rule r { result :=
Bump(a) } } }` resolves and type-checks cleanly today — the only thing that stopped
it from reaching a user was firtool itself: inlining `Bump` into `N`'s own emitted
FIRRTL block splices in a bare reference to `v`, which doesn't exist in `N`'s block,
and firtool rejects it with "unknown declaration `v`" — a real bug (an unclear
diagnostic from the wrong tool, not a silent miscompile; verified by hand with exactly
that program before writing the fix, not assumed from reading the code) that would only
get worse once callees can write state, since then the leak would be a *write* into a
foreign module's register, not just a read. Fixed in firrtl.rs's `compile_call`, not
resolve.rs: `Resolution::def_owner` (previously private to `Resolver`, now a public
field, since firrtl.rs needs it and resolve.rs already computes it) maps every def to
its owning module; `compile_call` now checks every def in the callee's own *merged*
`sig.reads`/`sig.writes` (effects.rs's fixpoint has already flattened the whole call
graph into this one set, so one check on the immediate callee catches an arbitrarily
deep chain) against `self.module`, the module actually being emitted right now — not
the callee's declaration site. A same-module call (the base feature's and the branching
feature's own common case: a `fn` nested in `M`, called only from a rule also in `M`)
still passes, since `def_owner[state] == self.module` there; `call_reaching_a_different_
modules_state_is_an_error` (tests/firrtl.rs) pins the cross-module case with the exact
program above, and `call_to_a_same_module_fn_that_reads_state_still_works` pins that the
legitimate case wasn't collaterally broken. At the time this landed, state-writing
callees were still rejected outright (`sig.writes` had to stay empty) — see the
"State-writing callees" section below for how that check now generalizes to cover
writes too, for free.

A second correctness subtlety, caught by deliberately constructing and running the
"obviously risky" case before declaring the feature done, not by any test failure or
user report: `Emitter::locals` is keyed by `DefId`, and every call to the SAME function
reuses that function's one set of parameter `DefId`s. Binding params without saving what
was there before is unsound the moment one call nests inside another call to the SAME
function — `Avg(Avg(x, y), z)` — because compiling the outer call's first argument
recurses into the inner `Avg(x, y)` call, which rebinds `Avg`'s param `DefId`s to `x`/`y`
*before* the outer call gets to compile `z`. The outer call would then read the INNER
call's rebound value instead of its own — `z` silently vanishes, replaced by whichever
value the inner call last bound to the same slot. `compile_call` now saves each
`DefId`'s previous binding (`None` if it had none) before rebinding it, and restores it
after the return expression is fully compiled — ordinary save/restore, making calls
properly reentrant regardless of how deeply or indirectly they nest (as an argument, or
through a `let` whose value happens to be a call), not just the syntactically-obvious
case. `nested_call_to_the_same_function_does_not_clobber_the_outer_arguments` (tests/
firrtl.rs) pins the exact failing shape.

One correctness subtlety, for a callee using an implicit width parameter (`bits[N]`):
the callee's own body is type-checked exactly once, generically, independent of any
particular call site, so `N` is never concretely resolved in its `types.expr_tys`
entries. The call EXPRESSION's own type, by contrast, is resolved through the normal
call-site instantiation `type_call` already does for any call (spec or synthesizable
alike) — so the inliner threads the call's own already-concrete width down into the
callee's return expression as an explicit hint, and never falls back to the callee's
own (possibly-generic) width for it.

`examples/call.tr` proves the base (branch-free) feature through real firtool and Icarus
simulation, deliberately choosing operands (`200 + 100`) that overflow `bits[8]` inside
the callee's own `let sum = a + b` — `(200 + 100) mod 256 = 44`, then `>> 1 = 22` — so an
inlined call reusing the wrong (e.g. widened) semantics for its own internal arithmetic
would show up as a wrong answer, not just "firtool accepted it."

**State-writing callees, added 2026-07-31** (the last piece of the calls TODO bullet,
picked up as its own followup pass immediately after the module-boundary fix above):
`Bump(x) { v := x  return x + 1 }` now inlines, both the write to `v` and the return
value, from one call site. Two independent walks handle the two halves — they are NOT
one computation feeding the other:

- The return value still goes through `compile_callee_body`, now widened to also accept
  a leading `Stmt::Assign` (a write) alongside `let`, simply skipped by that walk (a
  side effect it doesn't care about).
- The write goes through a NEW pair of functions, `call_writes_reg`/`call_writes_port`
  (dispatch: is this call, is its already-merged `sig.writes` reaching this specific
  register/port?) and `callee_reg_write`/`callee_port_write` (the actual value-finding
  walk, mirroring `reg_value_in_stmts`/`inst_port_value_in_stmts`'s own if/else
  mux-threading, but over the CALLEE's statements — including binding the callee's own
  `let`s, which `enter_rule`'s one-time rule-level pre-population never sees, since this
  walk can start mid-rule, inside a different item's body entirely).

A state-writing call only reaches the emitted hardware when the call site is a bare
statement (`Bump(a)`, return value discarded) or the entire right-hand side of `:=`
(`result := Bump(a)`) — `call_writes_reg`/`call_writes_port` only ever look in those two
positions. Nested any deeper — an argument to another call, a `let`'s init, `Bump(a) +
1` — is an explicit, up-front error (`check_writing_call_positions`, a new per-rule
validation pass using a new `collect_calls` tree-walker to enumerate every reachable
`Expr::Call`, not just check existence like the older `expr_contains_call`), not a
silent skip. Getting this validation right mattered: without it, a writing call in a
forbidden position would simply never be visited by either walk, and its write would
vanish from the output with no diagnostic at all — the same failure shape as every other
silent-miscompile caught this session, just one syntax position over.

Three real correctness gaps surfaced building this, each caught by deliberately running
the risky case rather than trusting the code read — the same discipline this whole
session's calls work has leaned on:

1. **Validation reachable only from the return-value path.** The original
   `!sig.writes.is_empty()` rejection, and later `compile_callee_body`'s own
   `<sequences>`/`<elaborates>`/`fails`/module-boundary checks, all lived on the path
   that runs when a call's RETURN VALUE is compiled. A call reached ONLY through the
   write-hunt path — a bare `Bump(a)` statement whose return value nothing ever asks
   for — never touched any of that validation at all. Fixed by extracting a shared
   `validate_call` (effect coloring + module boundary) that BOTH `compile_call` and
   `call_writes_reg`/`call_writes_port` call before doing their own thing, so validation
   runs regardless of which walk is asking.
2. **The same gap, one level deeper: transitively-written state.** Even after (1),
   `validate_call` didn't check whether the callee's OWN body calls something else — that
   check still only lived inside `compile_callee_body`. A rule calling `Outer` as a bare
   statement, where `Outer` itself calls `Inner`, which writes `w`, would have `w`
   silently vanish from the emitted hardware: `effects.rs` correctly merges `w` into
   `Outer`'s (and so the rule's) signature, so `schedule.rs` correctly believes the rule
   writes `w` — but `callee_reg_write` scanning `Outer`'s own body finds no direct `w :=`
   (it's one call deeper) and returns nothing, with no error. Fixed by moving the "body
   contains a call" check into `validate_call` too (a new `body_contains_call` tree-walk,
   recursing through `if`/`else`), so it runs for every entry point uniformly, and
   simplifying `compile_callee_body` by deleting its now-redundant per-node version of
   the same check. `a_write_transitively_reached_through_a_bare_statement_call_is_an_
   error` (tests/firrtl.rs) pins the exact shape.
3. **A real asymmetry between the two walks, not a bug, but nearly reported as one.**
   `callee_reg_write` accepts a conditional write (`if`/`else`) in ANY position, since it
   scans the whole callee body regardless of where the write sits. `compile_callee_body`
   only accepts an `if`/`else` in TAIL position (every reachable path must produce a
   return value, mirroring the branching-callee feature's own restriction) — so the
   EXACT SAME callee body inlines when called as a bare statement but is rejected
   ("too complex to inline for its RETURN value") when its return value is also used.
   This is a real, permanent boundary, not a bug: the write-hunt walk only ever needs
   "what value lands in this register," while the return-value walk needs "one
   unambiguous value for every path" — different questions, correctly different answers.
   The original error message claimed `if`/`else` was unconditionally supported while
   rejecting one, which would have read as self-contradictory; reworded to name the
   TAIL-position restriction explicitly and point at the bare-statement path as the
   escape hatch. Pinned both ways: `call_writes_state_conditionally_inside_its_own_
   if_else` (bare statement, succeeds) and `call_to_a_conditionally_writing_function_
   whose_return_value_is_used_requires_tail_position` (return value used, clean error).

`examples/call_writes.tr` proves the combined case (one call site both writing state
and returning a value) through real firtool and Icarus simulation, observing BOTH halves
through real ports so a wrong value on EITHER side would fail the testbench, not just
"firtool accepted it."

**`prio`, the one synthesizable builtin, added 2026-07-31** (the calls TODO bullet's
last remaining item — the user asked to go for both remaining sub-items, state-writing
callees and builtin calls, together; this is the second, landed as its own commit on
its own recon per the state-writing-callees section's own guidance not to conflate two
differently-shaped features). Every other builtin (`bits`, `wire`, `list`, `any`,
`clog2`, `pack`, `trunc`, `len`, `sync`, `race`) is either a type-position construct
resolved entirely in types.rs before any expression is ever "called" (`bits[N]`), a
spec/`chooses`-only construct that can't reach synthesizable code at all (`any`), a
separate FSM/spawn-sequencing gap already rejected by `lower.rs` (`sync`/`race`), or a
real gap with no in-repo caller yet (`clog2`/`trunc`/`pack`/`len` — type-checked
identically to `prio`, but nothing in the repo actually calls one from synthesizable
code today, so they stay an explicit "not yet supported" error rather than a designed
feature). `prio` is the one exception: it has a real, in-repo consumer
(`examples/arbiter.tr`'s `RoundRobin`, refining a `spec AnyGrant` that names it a
nondeterministic choice `any` must be narrowed to a real requester) and a documented
intent — "priority encoder" — but, before this pass, no documented ENCODING. Every
existing test only checked `prio`'s TYPE (`bits[N] -> bits[clog2(N)]`), never its
actual returned value for a given input. That's a real gap in a repo whose stated
practice is verifying claims by running code: there was no code to run.

**Semantics, decided here, not invented in isolation** (confirmed with Lumi before
writing any emission code, since this becomes permanent source-of-truth the moment
it's implemented): `prio(reqs)` is a FIXED-priority encoder — the LOWEST set bit in
`reqs` wins (bit 0 is highest priority), and `reqs == 0` returns `0`, a defined but
not-meaningful value; gating on `reqs != 0` (when that matters) is the CALLER's job,
the same way a `fails` precondition is established by the caller, not the failing
primop itself. Note the naming trap in `arbiter.tr`: the impl is called `RoundRobin`,
but `prio` does NOT rotate — it's a legal refinement of `AnyGrant` (any set bit is a
valid pick, and the lowest one always is one), just a fixed-priority one, not an
actually-rotating one. The name predates this implementation and is a pre-existing
misnomer, not a claim about `prio`'s behavior.

Built as a right-nested `mux` chain, bit 0 outermost so it's checked (and wins) first:
`mux(bit0, 0, mux(bit1, 1, mux(bit2, 2, ... UInt(0))))` (`compile_prio`). Unlike a call
to a user `fn`/`impl`, a call to `prio` does NOT disqualify its enclosing callee from
inlining (`expr_contains_call`/`body_contains_call`, the shared "does this body call
anything else" check `validate_call` runs for every entry point, now only counts a call
whose callee resolves to `DefKind::Fn`/`Impl` — a builtin has no body of its own to
(re)inline, so it carries none of the reentrancy/recursion risk that check exists to
rule out). A call NESTED INSIDE a builtin's own argument still counts, though:
`prio(SomeUserFn(x))` still disqualifies on `SomeUserFn`, pinned by
`nested_user_call_inside_a_builtins_argument_still_disqualifies` (tests/firrtl.rs).

One correctness subtlety, caught by actually trying `examples/call_prio.tr`'s generic
`RoundRobin(reqs : bits[N])` shape (matching `arbiter.tr`'s own signature) rather than
only a concrete-width version: `prio`'s argument's OWN width, looked up directly via
`types.expr_tys`, is unreliable inside a callee body for the same reason noted above for
implicit-width params — the callee's body is type-checked exactly once, generically,
independent of any call site, so `N` is never concretely resolved there. This differs
from the RETURN-value width subtlety already documented above: THAT one is solved by
`compile_call` threading the call's own already-concrete OUTER width down as an explicit
hint — but `prio`'s output width (`clog2(N)`) and its input width (`N`) are different
quantities, so the existing hint can't stand in for the argument's width too. New fix,
`concrete_width_of`: follows the argument expression through `self.locals` substitution
(the same mechanism that already resolves a local/param reference to its bound value)
to whatever it's ultimately bound to — back in some concrete call site's own context,
not the generic callee body — and reads WIDTH from there instead. This is a narrow fix
for `prio`'s specific need, not a general solution: anywhere else a callee-body
expression's width is needed independent of its own return value, this same class of
gap can resurface (noted in TODO.md).

`examples/call_prio.tr` (a generic-width `RoundRobin` wrapping `prio`, matching
`arbiter.tr`'s actual shape) proves the encoding through real firtool and Icarus
simulation, deliberately exercising the discriminating cases a narrower test would
miss: a single bit set, two bits set with the lower one expected to win (a real tie,
not just "the only bit happens to be the answer"), all bits set (lowest still wins),
and the all-zero fallback.

**`trunc`, the second synthesizable builtin, added 2026-07-31.** Unlike `prio`,
`trunc(value, width)` needed no semantics decision — it's unambiguous truncation to the
low `width` bits, already exercised in `tests/types.rs` (`a := trunc(b, 8)`, fixing a
"needs `trunc`" width-mismatch error) even before this pass gave it an emission path.
`compile_trunc` builds exactly `bits(value, width-1, 0)` — the SAME FIRRTL `bits`
primop `compile_bit_select` already emits for `x[hi..lo]`, just reached through a
different spelling; `width` is guaranteed const-evaluable by types.rs's own `"trunc"`
typing rule before firrtl.rs ever sees it (a non-const width types as
`Bits(Width::Unknown)`, which the existing `width_of` already rejects with its own
explicit error — no new check needed). Like `prio`, a call to `trunc` doesn't
disqualify its enclosing callee from inlining, for the same reason (`expr_contains_
call`/`body_contains_call` only count a callee resolving to `DefKind::Fn`/`Impl`).

This pass also settled the shape of the OTHER remaining builtins, closing the "clog2/
trunc/pack/len remain type-only" TODO line's ambiguity rather than just chipping at it
one builtin at a time: `clog2`/`len` type as `Ty::Int`, a compile-time-only type (bit-
width computation in a type position, or a list's length during elaboration) — every
real use in the repo is compile-time, so giving them a RUNTIME hardware meaning would
mean inventing semantics with no grounding anywhere in the design, a fundamentally
different kind of gap than `prio` (which had a real spec-level purpose, `arbiter.tr`'s
refinement, to confirm an encoding against) or `trunc` (unambiguous by construction).
`pack` (concatenation, well-defined) was flagged in this same pass as real but tied to
a `<sequences>`/spawn body in its only DOCUMENTED use — see the next section for why
that turned out not to block implementing it anyway. Scoped this way with Lumi via
AskUserQuestion before writing any code, rather than assuming "close the whole TODO
line" meant treating all four builtins as one uniform task.

`examples/call_trunc.tr` proves it through real firtool and Icarus simulation,
deliberately feeding a value (`0xBEEF`) whose low and high bytes differ, so truncating
to the wrong end would show up as a wrong answer (`0xEF`, not `0xBE`).

**`pack`, the third synthesizable builtin, added 2026-07-31** (a follow-up pass, once
Lumi asked to go for it specifically, after this section had left it flagged as "tied
to spawn" rather than implemented). Revisiting the reasoning above: `pack`'s only
DOCUMENTED example (`Fetch2`'s `pack(h1.result, h2.result)`, above) lives inside a
`<sequences>`/spawn body, which still has no synthesis path of its own — but
concatenation itself doesn't NEED that surrounding feature to be well-defined or
useful. A plain combinational rule can concatenate two register values just as
sensibly as a spawned computation's results can; implementing `pack` now doesn't serve
`Fetch2`'s specific example (spawn still can't be synthesized), but it's a real,
independently useful primitive that doesn't have to wait for spawn to land.

Unlike `prio`, but like `trunc`, `pack`'s WIDTH semantics need no invented decision —
`types.rs`'s own `"pack"` typing rule already sums argument widths, so the result
width always matches automatically; `compile_pack` doesn't even need a hint the way
`compile_prio` did. What DID need deciding, and wasn't pinned down anywhere in the
repo before this pass: concatenation ORDER — which argument becomes the more
significant bits. Went with the FIRST argument as most significant, matching FIRRTL's
own `cat(hi, lo)` primop directly (so `compile_pack` needs no reordering, just folding
multiple arguments left-to-right: `cat(cat(a, b), c)` for three), and the same
"leftmost is most significant" convention as Chisel's `Cat` and Verilog's `{a, b}`
concatenation — strong enough external precedent that, unlike `prio`'s tie-breaking
rule, this didn't need an AskUserQuestion round to confirm before implementing.
Verified empirically anyway, not just asserted: ran the compiled FIRRTL through real
firtool and Icarus with `a = 0xAA`, `b = 0xBB` and confirmed `result = 0xAABB` (not
`0xBBAA`) before writing it into this section as settled fact, and again with three
arguments (`0x11, 0x22, 0x33` → `0x112233`) to confirm the left-to-right fold
preserves the ordering through more than one `cat`. Like `prio`/`trunc`, a call to
`pack` doesn't disqualify its enclosing callee from inlining, for the same reason.

`examples/call_pack.tr` proves it through real firtool and Icarus simulation, with the
same distinguishable-halves technique `call_trunc.tr` uses (`a = 0xAA`, `b = 0xBB`,
checking the RESULT lands as `0xAABB` specifically, not just "some concatenation").

**A callee may call another callee, added 2026-07-31** (the last bundled piece of the
calls TODO line — user picked this over generalizing `concrete_width_of` or lifting the
loop/guard/fifo-op restriction). Before this, a called function's own body was banned
from containing ANY further call at all — a blanket rule that doubled as free recursion
prevention ("a callee that cannot call anything can never call itself") but also blocked
ordinary composition (`Outer` calling `Inner` for a value). Lifted to: a callee's own
body MAY call another callee, used as a value (a `let`'s init, the return expression, or
a state write's own RHS) — the actual restriction moved to two narrower, explicit cases,
plus real cycle detection to replace what the blanket ban used to prevent for free.

**Cycle detection is a STATIC property of the call graph, not a dynamic "currently
inlining" stack — and getting this wrong was a real, caught-before-shipping bug.** The
first design tracked an `Emitter.call_stack: Vec<ItemId>`, pushed/popped around each
recursive compile step, checked in `validate_call` for whether the callee being
validated was already on it. This looked right and passed every test written against
it — until `nested_call_to_the_same_function_does_not_clobber_the_outer_arguments`
(an EXISTING regression test, `Avg(Avg(x, y), z)` called from a rule) started failing
with a false "call cycle: Avg -> Avg". Root cause: compiling the OUTER `Avg` call
dereferences `Avg`'s own param `a`, which is bound (via `self.locals`) to the ARGUMENT
expression — the INNER `Avg(x, y)` call — and compiling that happens lazily, WHILE
`call_stack` still says "currently inlining `Avg`", even though the inner call's text
lives in the RULE's own source, not `Avg`'s. The dynamic stack couldn't tell "A's own
body calls A" (a real, unbounded cycle) apart from "compiling an argument that happens
to invoke the same function" (always finite, bounded by the source file's own static
nesting depth). Fixed by switching to a purely structural check: `direct_callees` walks
a function's own literal body (`let` inits, the return expression, write RHS values, `if`
conditions, bare statements — recursing into a call's own arguments too, so `prio(Widen(
r))` still finds `Widen`) and collects exactly which OTHER functions it names, with NO
dependence on call order or argument substitution; `find_call_cycle` then does an
on-path DFS from the callee being validated (same on-path/fully-visited shape as
`visit_module`'s module-instantiation cycle check) to see whether that graph, followed
transitively, ever leads back to itself. `Avg`'s own body (`let sum = a + b  return sum
>> 1`) contains no call at all, so it's never flagged, regardless of how many times or
how deeply it's invoked at any particular call site. Direct self-recursion never reaches
this check in practice — effects.rs already requires `<elaborates>` for it, and an
`<elaborates>` function can't be inlined at all — so `find_call_cycle` is only ever
exercised by INDIRECT/mutual recursion (`A` calls `B` calls `A`, neither self-recursive),
which effects.rs's own (direct-only) recursion check doesn't catch.

**A nested call MAY itself write state, and MAY be used as a bare statement — lifted
2026-07-31, closing the two restrictions the paragraphs above once described as
deferred.** The guard that used to reject any nested call to a state-writing function
outright (`direct_callees` + a `sig.writes` check in `validate_call`) is gone; in its
place, `validate_call` reuses `check_writing_call_positions_in` (checks.rs) — the SAME
walk that already restricts a RULE body's writing calls to a bare statement or the whole
RHS of `:=` — against the callee's OWN body too. That's the actual fix, not a relaxation:
a writing call still has to sit in one of those two positions, because those are the only
two shapes `call_writes_reg`/`call_writes_port` (writes.rs) know how to find. What changed
is that the write now gets FOUND there, at any depth, instead of being rejected outright.
`callee_reg_write`/`callee_port_write` (the write-hunt one level into a callee's own body)
now recurse a second time — for a bare-statement `Stmt::Expr` or a non-matching
`Stmt::Assign`'s RHS, they call back into `call_writes_reg`/`call_writes_port`, the SAME
functions that originally only handled rule-level writes — making the recursion mutual and
arbitrarily deep: a write threads through any number of nested calls, not just one level.
`compile_callee_body`'s return-value walk was widened symmetrically: a bare-statement
`Stmt::Expr` is now allowed before the tail `return`, but ONLY if it's itself an
`Expr::Call` — a bare guard or fifo op is still rejected, since those already set
`sig.fails`, caught by `validate_call` before this walk ever runs. The narrow case the old
guard carved out on purpose — a state-writing callee calling a PURE helper for its own
write's value (`Bump(x) { v := Helper(x)  return x }`) — still works, now for the more
general reason rather than a special-cased exemption; still pinned by
`a_state_writing_callee_may_still_call_a_pure_helper`. Proof of the full capability:
`examples/call_nested_writes.tr` — `compute` calls `Outer` (bare statement), which calls
`Inner` (also a bare statement), which writes `v_out` — two levels of call between the
rule and the actual write, landing at `v_out = a + 1`, verified through real firtool +
Icarus simulation (`sim/call_nested_writes_tb.v`,
`call_nested_writes_runs_through_real_ports`). The position restriction itself is still
real and still enforced, just now precise about WHERE it's checked: a nested call used as
a `let`'s init (not a bare statement or the whole RHS of `:=`) is still an explicit error,
pinned by `a_nested_call_used_as_a_let_value_that_writes_state_is_still_an_error`.

**A duplicate-diagnostic bug surfaced by this change, caught before commit, fixed at the
error-collection layer.** `validate_call` is a multiply-invoked choke point: for a call
used as BOTH a value and a write source (`result := Outer(w)`, where `Outer` reads/writes
`w`), it runs once via the return-value path (`compile_call`) and again via the write-hunt
path (`call_writes_reg`, hunting for `w`'s own write) — and after this change, both paths
independently run `check_writing_call_positions_in` against the identical callee body,
each pushing the identical error. The same multiplicity affects every other error
`validate_call` can emit (the cycle check, the module-boundary check, the
`sequences`/`elaborates`/`fails` bans) whenever a call site is validated through more than
one path — not something new to this feature, just newly exercised by it. Fixed at the
single choke point every error passes through: `Emitter::error` (mod.rs) now dedups by
`(span, message)` before pushing, since `EmitError` already derives `PartialEq`/`Eq` — one
change covers the whole class rather than teaching each caller to notice it's already
validated a given call. Pinned by strengthening two existing tests from `.any(...)` to
`assert_eq!(err.len(), 1, ...)`:
`a_nested_call_used_as_a_let_value_that_writes_state_is_still_an_error` (the
value-and-write-source shape that actually reproduced it) and
`an_indirect_call_cycle_is_a_clean_error_not_a_hang`.

**A latent width-hint gap this feature newly makes reachable, not something it needs to
fix:** a nested call inside a GENERIC (`bits[N]`) callee body, reached through a binop
(`Outer(x : bits[N]) { return Inner(x) + 1 }`), fails with a clean "no concrete width"
error rather than a miscompile — `compile_binop` computes its own width hint via
`known_width(id)` (`types.expr_tys` for the binop's own, still-generic id), ignoring
whatever hint its own caller threaded down, so the nested call ends up with no hint at
all and falls back to the same dead end. This is the exact same class of gap already
documented for `prio`'s own argument width (`concrete_width_of` follows a value through
`self.locals` back to a concrete call site) — genuinely latent before this feature (no
existing test called a generic function whose return expression was a plain binop, only
ever a bare param reference or a nested call), not a regression this pass introduces.
Verified as a clean error, not a hang or a silent wrong answer, and pinned
(`a_generic_nested_call_through_a_binop_is_a_clean_error_not_a_miscompile`) so it stays
a known, documented boundary rather than resurfacing as a surprise.

`examples/call_nested.tr` proves pure composition through real firtool and Icarus
simulation: `Top`'s rule calls `Outer`, which itself calls `Inner`, with inputs chosen so
the SECOND case deliberately overflows `bits[8]` inside `Outer`'s own `* 2` — proving the
nested call chain reuses ordinary modular arithmetic all the way through, not some wider
intermediate the call boundary might otherwise hide.

## Tooling

**Editor support added 2026-07-30**, `editors/vscode/`: TextMate-grammar syntax
highlighting for `.tr`, plus "Format Document" wired to `trace - --fmt` (the compiler
formats its own stdin and writes to stdout — no separate tool, no drift between what
the CLI and the editor consider correctly formatted).

The formatter (`src/fmt.rs`) is a **reindenter, not a pretty-printer**, and that is a
deliberate choice, not a shortcut taken for lack of time. `--` comments are trivia the
lexer discards outright (see "The effect system" intro / lexer.rs) — there is no token
that carries a comment's text or position forward. A pretty-printer that rebuilds
source from the AST would therefore have nowhere to put a comment back and would
silently delete every one in the file on first format. Reindenting instead — walk the
token stream only for brace/paren/bracket depth, rewrite each source line's leading
whitespace to match, touch nothing else — never looks at comments at all, so it can
never lose one. The tradeoff is real and documented at the point it bites: a line that
continues a statement without opening a bracket (a multiline `impl ... refines Spec`
signature, this document's own `RoundRobin` example) has no depth to hang an indent
off, so it renders flush left even where the source hand-indents it (`tests/fmt.rs`
pins this as a known, accepted limitation rather than a bug to chase). Fixing that
properly needs statement-level awareness a brace counter doesn't have — a real
pretty-printer's problem, which reopens the comment-loss problem it was built to avoid.

No language server. No go-to-definition, hover, or inline diagnostics in the editor —
those still come from running `trace file.tr` directly. The grammar is regex-based, so
it highlights `reads`/`writes`/`combines`/... as effect keywords unconditionally, even
where they're used as ordinary identifiers outside an effect list; harmless for
readability, not a correctness signal.

## Prior art

- **Bluespec / bsc** — transactional-rules semantics; the production scheduler. Open
  source, Haskell (github.com/B-Lang-org/bsc). Use its source as the design document
  for the scheduler.
- **Kôika** (MIT) — formally verified one-rule-at-a-time Bluespec descendant, in Coq.
- **Filament** (Cornell CAPRA) — timeline types: which cycle each signal is valid in.
  The long-term answer for cross-module combinational-loop safety.
- **Dahlia** (Cornell CAPRA) — time-sensitive affine types for predictable memory
  banking. The long-term answer for array-conflict precision (tier 3).
- **Clash** — pure functional signals, reference point.
- **Calyx** — IR with a control language (`seq`/`par`/`if`/`while`) separate from
  structure; candidate alternative target.
