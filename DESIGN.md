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

**Achieved 2026-07-30.** Modules stay flat and top-level — no lexical nesting — and
compose by name:

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

- **Conflict model.** An instance's whole port set is one conflict resource, the same
  conservative model v0 arrays already use ("Arrays and aliasing" above) — no per-port
  precision yet. Two rules that touch different ports of the same instance still
  conflict; only one can drive it per cycle. This reuses the scheduler unchanged: an
  `inst` def is just another kind of state, so a port write/read infers a write/read of
  the instance's `DefId` exactly like a mem index does for the whole array.
- **No nested writes.** A port write must stay at a rule's top level, same restriction
  as a memory write and for the same reason: neither is threaded through a `mux` yet.

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
