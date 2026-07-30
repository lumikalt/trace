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

| Effect                 | Meaning                       | Hardware                        |
| ---------------------- | ----------------------------- | ------------------------------- |
| `converges`            | total, pure, terminates       | combinational logic             |
| `suspends`             | crosses cycle boundaries      | FSM + registers (see lowering)  |
| `allocates`            | runs at elaboration time only | no hardware; builds the circuit |
| `decides`              | can fail (inferred)           | guard inputs to the handshake   |
| `choice`               | nondeterminism, spec-only     | none; model-checker free vars   |
| `reads R` / `writes W` | state access rows             | input to the scheduler          |

The policy for inferred effects (`decides` and the rows) is uniform: the compiler
computes them; a stated effect is an interface assertion. Overstating is legal, because
a conservative claim is sound. Understating is an error.

### `converges`: combinational logic

A `converges` function cannot recurse, loop on circuit values, or suspend. It always
lowers to a pure combinational expression.

```
Parity(x : bits[8]) : bits[1] <converges> {
    return x[0] ^ x[1] ^ x[2] ^ x[3] ^ x[4] ^ x[5] ^ x[6] ^ x[7]
}
```

The checker rejects circuit-value loops in `converges` code:

```
Bad(x : bits[8]) : bits[8] <converges> {
    while x != 0 { x := x >> 1 }
    -- error[E012]: loop bound depends on a circuit value.
    -- Loops over circuit values need `<suspends>` (one iteration per cycle)
    -- or an elaboration-time bound under `<allocates>`.
}
```

### `allocates`: elaboration time

`allocates` code runs once, before synthesis. It builds the circuit. Recursion and
dynamic allocation are legal here and only here. This makes the Chisel confusion between
elaboration time and circuit time (`if` vs `when`, Scala `var` in generator loops) a
type error instead of a silent bug.

```
AdderTree(xs : list[wire[bits[32]]]) : wire[bits[32]] <allocates> {
    if len(xs) == 1 { return xs[0] }        -- `if` on an elab value: unrolls
    mid := len(xs) / 2
    return Add(AdderTree(xs[..mid]), AdderTree(xs[mid..]))   -- recursion: legal
}
```

An `if` on a circuit value inside `converges` code is a mux. An `if` on an elaboration
value inside `allocates` code selects what to build. The effect of the scrutinee decides.
The user does not choose a keyword; the checker rejects mixtures that do not lower.

### `reads` / `writes` rows

Every rule gets a read set and a write set. The compiler infers them; users can state
them to assert an interface. The scheduler consumes these rows (see Scheduling).

```
rule refill <reads {pc, mem}, writes {ir}> {
    ir := mem[pc]
}
```

### `decides`: fallibility

Adapted from Verse's `<decides>`. Code that can fail carries `decides`: a guard `?`, a
fifo operation, or a call to deciding code. The compiler infers it bottom-up through
the call graph. This tracking is what makes handshake derivation compositional: a
rule's derived ready logic is the conjunction of guards from every deciding call in
its body, however deep.

```
Classify(x : bits[8]) : bits[2] <converges, decides> {
    (x != 0)?                 -- fallible: aborts the calling rule's cycle
    return clog2(x)
}

rule step {
    class := Classify(acc)    -- rule stalls until Classify succeeds
}
```

Context rules:

- A rule body is always a failure context. Failure aborts the cycle and retries.
  Rules never need to declare `decides`.
- Elaboration positions are never failure contexts. There is no transaction to
  abort. Guards and fifo ops in `allocates` bodies, state types, and initializers
  are errors.

### Verse effects not carried over

Three other Verse effects were considered and folded away:

- `transacts` — every rule is a transaction by construction. The effect is ambient.
- `varies` (non-deterministic reads) — subsumed by a nonempty `reads` row.
- `diverges` — circuit code cannot diverge inside a cycle by construction.
  Termination of `allocates` recursion is unchecked in v0; accept this.

## `suspends`: multi-cycle code without multi-cycle rollback

**Resolution of the main open question from the first draft.** A `suspends` block is
sugar. It lowers to a continuation register plus one single-cycle rule per segment.
There is no cross-cycle rollback, no checkpoint hardware, ever. The transaction is
always one cycle. "Cycle = transaction" survives intact.

The `tick` statement marks a cycle boundary. It cuts the block into segments.

```
Rmw(addr : bits[8]) <suspends, reads {mem}, writes {mem}> {
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

These three constructs compose suspending code. All three lower to rules and registers.

**`spawn`** starts a parallel FSM. It allocates a new continuation register. Spawn
counts must be static, because dynamic allocation needs `allocates`. A spawn inside a
circuit-value loop is a type error.

**`sync`** joins parallel FSMs. It lowers to a rule guarded on the conjunction of the
joined continuations reaching their end states.

**`race`** takes the first of several suspending computations to complete. The
competing continuations write the same result register. They become conflicting rules.
Arbitration falls out of the ordinary scheduler; there is no separate arbiter construct.

```
Fetch2(pc : bits[16]) <suspends> {
    h1 := spawn ReadBank(bank0, pc)
    h2 := spawn ReadBank(bank1, pc + 1)
    sync(h1, h2)                        -- both reads have landed
    ir := pack(h1.result, h2.result)
}
```

## Choice: specification, not synthesis

The choice operator `|` and the `any` form are **not synthesizable**. They exist for
specification and verification. Nondeterministic choice becomes a free variable in a
model checker, like SVA `$anyseq`. Implementations are checked as refinements of choicy
specs.

```
spec AnyGrant(reqs : bits[N]) : bits[clog2(N)] <converges, choice> {
    i := any(0..N-1)          -- free variable: the checker picks
    reqs[i]?                  -- constrained: the pick must be a requester
    return i
}

impl RoundRobin(reqs : bits[N]) : bits[clog2(N)] <converges>
    refines AnyGrant
{
    -- deterministic logic; checked against the spec
}
```

The `choice` effect marks spec-only code. Only a `spec` may declare it. Using `any`
without it is a type error. Synthesizing code with it is a type error.

`|` is contextual: in an item without `choice` it is bitwise or; in a `choice` item it
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

**Resolution of review point 5.** The `converges` effect
guarantees acyclicity only inside one scope, via a no-forward-reference rule. It does
not guarantee acyclicity across module boundaries. Two internally-acyclic modules wired
output-to-input in a cycle still form a real combinational loop.

v0 position:

- Enforce no-forward-reference inside `converges` scopes. Local cycles are
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

One honest gap the testbench works around: the language has no `input`/`output` port
concept yet, so the emitted `Subleq` module exposes only `clock`/`reset` — nothing else
is observable or drivable from outside. The testbench reaches in with hierarchical paths
(`dut.pc`, `dut.m_ext.Memory[i]`) instead of real ports, which Icarus allows with no
special flags (Verilator would need `--public`). A real ports feature is future work,
not needed for this milestone but needed before hardware can plug into anything larger
than a single-instance simulation.

Sketch, honest about v0 array rules (single-port memory → one access per tick):

```
module Subleq {
    reg pc : bits[16] = 0
    mem m  : bits[16][4096]

    rule step <suspends> {
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
