# trace: an effect-based HDL with transactional semantics

Design document and source of truth for the language and the compiler. The prose
follows Simplified Technical English: short sentences, active voice, one idea per
sentence.

The document has three parts. Part 1 describes the language: syntax and semantics,
independent of how the compiler implements them. Part 2 describes the compiler:
architecture, algorithms, and emission. Part 3 gives implementation status and
prior art.

# Part 1: The language

## The core idea

A clock cycle is a transaction. Register writes are speculative until the clock edge.
Rollback within a cycle is free: do not assert the register enable. The language
builds on this insight. It borrows the semantic core of Verse: failure as control
flow, an effect system, and a choice operator for specification.

Hardware is a set of **guarded atomic rules**, as in Bluespec. Each rule either
commits fully at the cycle boundary or has no effect. The compiler derives all
handshaking from failure. The user never writes a `ready` signal.

```trace
module FifoBridge {
    fifo input  : [8]
    fifo output : [8]

    rule transfer {
        let x = input.Deq[]   -- fails when input is empty
        output.Enq[x]         -- fails when output is full
    }
}
```

Square brackets mark a fallible operation. If any operation in a rule fails, the
whole rule aborts for this cycle. The rule retries next cycle. The compiler turns
the two failure conditions above into ready/valid handshake logic. No stall logic
appears in the source.

A bare `?` tests a condition. Failure of the test aborts the rule.

```trace
rule drain {
    (mode = Draining)?       -- guard: rule only fires in Draining mode
    let x = input.Deq[]
    count := count - 1
}
```

`?` is optional sugar, not required: any expression sitting alone as a
statement — its value computed and then left unused — implicitly guards the
rule the same way, so `mode = Draining` alone means the same thing as
`(mode = Draining)?`. This applies to a fifo op, a call, and a `spawn` too
in the sense that each already has its own established meaning as a bare
statement (a call may or may not fail on its own terms; a fifo op's fail
condition already folds in on its own; a `spawn`'s handle being unused is
the point) — none of those need `?`, and writing one there wouldn't add
anything. Anything else bare must be `[1]`, the same requirement an
`if`/`while` condition already has; a bare non-`[1]` expression (its
value computed and discarded for no reason) is a compile-time error rather
than silently doing nothing. Both `?` and its bare-statement sugar are only
legal in a failure context (a rule, always; a `fn`/`impl`/`spec` only if it
declares `fails`) — see "`fails`: fallibility" below.

## Effects

Effects give each piece of hardware a static "color". The type checker enforces
the colors. Effect annotations sit in angle brackets after the signature.

| Effect                 | Meaning                       | Hardware                        |
| ---------------------- | ----------------------------- | ------------------------------- |
| `combines`             | total, pure, terminates       | combinational logic             |
| `sequences`            | crosses cycle boundaries      | FSM + registers                 |
| `elaborates`           | runs at elaboration time only | no hardware; builds the circuit |
| `fails`                | can fail (inferred)           | guard inputs to the handshake   |
| `chooses`              | nondeterminism, spec-only     | none; model-checker free vars   |
| `reads R` / `writes W` | state access rows             | input to the scheduler          |

The effect _names_ are hardware-descriptive, not Verse's own vocabulary
(`converges`, `suspends`, `allocates`, `decides`, `choice`). The semantics come
from Verse; the words are chosen to read naturally in a hardware context.

The policy for inferred effects (`fails` and the rows) is uniform. The compiler
computes them. A stated effect is an interface assertion. Overstating is legal,
because a conservative claim is sound. Understating is an error.

Three Verse effects were considered and folded away:

- `transacts` — every rule is a transaction by construction. The effect is ambient.
- `varies` (non-deterministic reads) — subsumed by a nonempty `reads` row.
- `diverges` — circuit code cannot diverge inside a cycle by construction.
  Termination of `elaborates` recursion is unchecked in v0.

### `combines`: combinational logic

A `combines` function cannot recurse, loop on circuit values, or suspend. It
always lowers to a pure combinational expression.

```trace
Parity(x : [8]) : [1] <combines> {
    return x[0] ^ x[1] ^ x[2] ^ x[3] ^ x[4] ^ x[5] ^ x[6] ^ x[7]
}
```

The checker rejects circuit-value loops in `combines` code:

```trace
Bad(x : [8]) : [8] <combines> {
    while x <> 0 { x := x >> 1 }
    -- error[E012]: loop bound depends on a circuit value.
    -- Loops over circuit values need `<sequences>` (one iteration per cycle)
    -- or an elaboration-time bound under `<elaborates>`.
}
```

### `elaborates`: elaboration time

`elaborates` code runs once, before synthesis. It builds the circuit. Recursion
and dynamic allocation are legal here and only here.

```trace
AdderTree(xs : list[wire[32]]) : wire[32] <elaborates> {
    if len(xs) = 1 { return xs[0] }        -- `if` on an elab value: unrolls
    let mid = len(xs) / 2
    return Add(AdderTree(xs[..mid]), AdderTree(xs[mid..]))   -- recursion: legal
}
```

An `if` on a circuit value inside `combines` code is a mux. An `if` on an
elaboration value inside `elaborates` code selects what to build. The effect of
the scrutinee decides. The user does not choose a keyword; the checker rejects
mixtures that do not lower.

### `reads` / `writes` rows

Every rule gets a read set and a write set. The compiler infers them. Users can
state them to assert an interface.

```trace
rule refill <reads {pc, mem}, writes {ir}> {
    ir := mem[pc]
}
```

### `fails`: fallibility

Code that can fail carries `fails`: a guard `?`, a fifo operation, or a call to
failing code. The compiler infers it bottom-up through the call graph, exactly
as it always has — inference doesn't change based on what's declared. What's
checked is different: unlike `reads`/`writes` (opt-in — only checked against
the inferred set if declared at all), `fails` is mandatory. A `fn`/`impl`/
`spec` whose computed `fails` ends up true — because of a guard anywhere in
its own body, a fifo operation, or a call to something that itself fails —
must declare `fails` itself, or it's a compile error, not a silently-inferred
color. This holds all the way up a propagating call chain: a function that
merely calls a failing function and returns its result also fails, and also
needs the declaration, not just the site with the `?`.

```trace
Classify(x : [8]) : [2] <combines, fails> {
    (x <> 0)?                 -- fallible: aborts the calling rule's cycle
    return clog2(x)
}

rule step {
    let class = Classify(acc) -- rule stalls until Classify succeeds
}
```

A rule's derived ready logic is the conjunction of guards from every failing
call in its body, however deep.

Context rules:

- A rule body is always a failure context. Failure aborts the cycle and retries.
  Rules never need to declare `fails`, at any depth of a call chain reaching
  into one — a rule is where the chain necessarily ends, since nothing calls a
  rule.
- Whether an item is a failure context at all (able to itself contain `?`, a
  fifo op, or a call to a failing function, before even asking whether its own
  computed `fails` matches what's declared) is checked syntactically, off the
  declared `<...>` list itself, not inferred from the body: inferring it would
  let a bare condition's own presence bootstrap failure-context status for
  itself. Matches Verse's own model, where a failable expression may only
  appear in a context the language knows how to handle both outcomes of —
  there, that context is `<decides>`; here, it's `fails` (declared, or a
  rule).
- Elaboration positions are never failure contexts. There is no transaction to
  abort. A bare (undischarged) guard, fifo op, or comparison in `elaborates`
  bodies, state types, and initializers is an error — `logic`-discharging one
  inside `elaborates` code is fine, though (see "Comparisons: fallible by
  default", below): discharge is a no-op there, since every value is already
  a resolved compile-time constant by the time elaboration reaches it.

### Comparisons: fallible by default

`=`/`<>`/`<`/`<=`/`>`/`>=` are a fourth member of the fallible-expression
family above (alongside a guard, a fifo op, and a failing call), not a
standalone `[1]`-valued operator: `a > b` yields `a`'s own value/type on
success, or fails, matching Verse's own `X > 0` (`04_operators`) — the same
"unwrap-or-fail" shape `opt?`/`f.Deq[]` already have, reusing their existing
machinery (fails-inference, guard-folding) rather than a new mechanism.
`type_binop`'s comparison arm returns the left operand's own type, not
`Ty::Bits(Width::Known(1))`; a comparison's fallibility is tracked entirely
by AST shape (like a fifo op's), not by a dedicated sentinel type.

```trace
rule step {
    a <> 0                    -- bare: implicitly gates this rule, same as (a <> 0)?
    result := a > b           -- result gets a's VALUE; the rule gates on a > b holding
    ok := logic a > b         -- discharged: ok gets 0/1, the rule does NOT gate on it
}
```

`logic <expr>` (see "Calling a function from a rule", below) converts a
comparison to a definite `[1]` the same way it already does a fifo op or a
guard-only `<fails>` call — the way to get today's plain-boolean behavior
back, and the only way to use a comparison directly as an `if`/`while`
condition: a BARE comparison there is a type error (its type is no longer
`[1]`), not silently accepted with the wrong meaning. `logic`'s own operand
parses looser than any binary operator specifically so this reads cleanly —
`logic a > b`, no parens — at the cost of the `logic A & logic B`
`and`-combination idiom needing explicit parens on each side now
(`(logic A) & (logic B)`, or just `A and B` — see "`and`: boolean
combination sugar" below); see `logic`'s own entry in TODO.md for the full
precedence tradeoff.

Unlike a fifo op or failing call, a comparison has no dedicated "whole
statement / entire RHS of `:=` / `let` init only" position restriction — no
side effect means no silent-miss risk from a misplaced one, so `a + (a > b)`
type-checks and its guard is found wherever it is, not just when it's the
whole RHS. The one restriction that DOES still apply: a comparison nested
inside an `if`/`while`'s own BODY (not its condition — a statement inside a
branch) is rejected outright, the same restriction a nested guard/fifo
op/failing call already has, and for the identical reason a fifo op's
restriction exists — folding it into the whole rule's guard would be wrong
when the branch might not even be taken. Self-caught by direct probe before
either half of this was tested: `x := a + (a > b)` used to compile clean with
`fires_r = UInt<1>(1)`, silently never gating on `a > b` at all (fixed by
making the guard-fold genuinely search for a nested comparison instead of
only checking a statement's top-level shape); `v := a + (a > b)` inside an
`if` had the identical silent gap one level deeper (fixed by rejecting it,
matching the fifo-op precedent, rather than folding it unconditionally).

### `if`: branch-scoped fallible conditions

A BARE comparison — no `logic` — may sit directly as an `if`'s own
condition, the ergonomic gap the previous section's migration deliberately
left open. Verse-faithful branch-scoping, applied uniformly regardless of
whether an `else` is present (Lumi's call): failure only skips the `then`
branch; it never gates the whole rule the way a top-level bare comparison
(or an explicit `?`) does. `while`'s own condition originally stayed out of
scope here (Verse turned out to have no native `while` at all — see
"`while`: multi-cycle loops" below for the reversal once that was
confirmed) — a bare comparison there now works identically, `logic` still
accepted but no longer required.

```trace
rule r {
    if a > b {
        v := 1              -- v becomes 1 when a > b holds
    } else {
        v := 2               -- v becomes 2 otherwise
    }
}

rule r2 {
    if a > b {
        v := 1               -- v holds its own value when a <= b, same as
    }                        -- an unwritten register path always would
    w := 2                   -- runs EVERY cycle, regardless of a > b
}
```

The value side was already solved before this feature existed: `if
opt.valid? { result := opt.data } else { result := 2 }` compiles to a plain
`mux(opt_valid, opt_data, 2)`, ordinary conditional-write muxing
(`writes.rs`) with no rule-level guard at all. A bare comparison's own
condition-position compilation reuses this unchanged — only the SELECT
itself needed to change, from `a`'s own passthrough value (`type_binop`'s
"yields the left operand" rule) to the comparison's actual boolean test
(`gt`/`lt`/...), the same `compile_guard_unwrap_cond` already built for the
guard-fold. No-else naturally falls out of the SAME mux-threading with no
special-casing: a branch that doesn't write a register already holds its
prior value (`reg_value_in_stmts`'s existing "hold" fallback); the whole-
rule guard already never looked inside `Stmt::If` at all (`compile_guard`'s
statement loop only matches `Stmt::Expr`/`Stmt::Assign`/`Stmt::Let`), so an
if-condition contributing nothing to `fires_rule` needed no new code, only
the type checker (`check_cond`, types.rs) accepting the shape. (`opt.valid`
needs the trailing `?` under the stricter rule below — see "An if/while
condition must itself be fallible".)

v0 restrictions: only a comparison discharges this way — a fifo op or a
failing call as an if's condition remains a type error (unchanged, no new
exemption for either); and only when the comparison is the WHOLE condition,
not nested inside a larger expression (`if (a > b) & c` is rejected, not
silently folded) — combine with `logic` first instead, the same
`(logic A) & (logic B)` idiom `logic`'s own entry above already documents
(`(logic a > b) & c`; `and` doesn't help here specifically since `c` is
already a plain `[1]`, not itself a fallible shape for `and` to wrap).

That last restriction closes a real, pre-existing silent-miscompile gap,
not a hypothetical one: a comparison's own TYPE is its left operand's type
(`type_binop`), so whenever that operand happens to be exactly 1 bit wide,
`(a > b) & c` types as an entirely ordinary `[1]` — indistinguishable from
a genuine boolean by width alone. Before this was caught, `if (a > b) & c`
compiled clean to `mux(and(a, c), ...)`, using `a`'s own passthrough value
in place of `gt(a, b)`; the identical shape reached a bare rule-body guard
statement too (`((a > b) & c)?` folding to `fires_r = and(a, c)`, dropping
`a > b`'s guard entirely), and `while x <> 0` with a 1-bit `x` slipped past
`check_cond`'s width check the same way, silently accepting a bare
comparison `while` was never supposed to allow at all. All three routed
through the same underlying blind spot: `check_cond`'s width check, and
`compile_guard_unwrap_cond`'s own comparison special-case, both only ever
checked whether a condition's own IMMEDIATE shape was a comparison, never
whether one was reachable somewhere inside a larger expression.
`compile_guard_unwrap_cond` itself is UNCHANGED — still exactly as shallow
as before — fixed instead at the one gate every one of these three shapes
must pass through first: `expr_has_undischarged_comparison` (types.rs,
called from `check_cond`) walks a condition's full subexpression tree via
`sub_exprs` (lower.rs) and rejects outright, before emission ever runs,
when an undischarged comparison is reachable anywhere that isn't either
the WHOLE condition (the new `if`-only exemption above) or already
wrapped in `logic`. A future guard-fold call site that reaches
`compile_guard_unwrap_cond` WITHOUT going through `check_cond` first would
reintroduce this exact gap with no test catching it — the fix is a
front-gate, not a fix to the shallow check itself.

#### An if/while condition must itself be fallible

A plain `[1]` value is no longer accepted bare as an `if`/`while`
condition (Lumi's call) — in a rule body OR a callee (`fn`/`impl`)
body alike. The condition must be one of: a bare comparison/fifo
op/failing call as the WHOLE condition (the exemptions above), an
explicit `<expr>?`, or a boolean combination of already-`logic`-
discharged fallibles (`(logic A) & (logic B)`, or its sugar `A and B` —
see "`and`: boolean combination sugar" below). Anything else — an
ordinary state read (`opt.valid`, a `[1]` port used bare) or a bare
`logic <expr>` with nothing else combining it — is a type error with a
hint.

```trace
rule r {
    if opt.valid { ... }        -- error: needs an explicit `?`
    if opt.valid? { ... }       -- fine
    if logic a > b { ... }      -- error: `logic` alone discharges nothing
                                 --   left to test; drop it or wrap: (logic a > b)?
    if a > b { ... }            -- fine (the bare-comparison exemption above)
    if (logic a > b)? { ... }   -- fine (explicit round trip)
    if a > b and c > d { ... }  -- fine (sugar for (logic a>b) & (logic c>d))
}
```

The `logic <expr>` case gets its own hint, not the generic one: `logic`
converts a fallible expression into a plain, already-discharged boolean
(its own entry above) — using THAT bare as a condition just becomes an
ordinary (always-taken) mux select, not a real guard, which is exactly
the confusion this closes. `(logic A) & (logic B)` stays legal despite
also typing as plain `[1]`: an `Expr::Logic` node reachable anywhere in
the condition's own subtree (not just at the top) marks every component
as deliberately discharged, the same `sub_exprs` walk `expr_has_
undischarged_comparison` above already uses for the identical shape of
question — `contains_logic_expr` (types/stmt.rs).

**Callee (`fn`/`impl`) bodies needed one more piece to reach parity,
not a carve-out.** A first pass applied the strict rule to rule bodies
only, exempting callees entirely: `if c?` inside a callee's own body is
a real `Expr::Guard`, and `effects.rs`'s ORDINARY `Expr::Guard` arm
unconditionally sets `sig.fails = true` — so naively enabling `?` there
made the ENCLOSING CALLEE fallible, forcing `<fails>` onto a function
whose only "fallibility" was a boolean mux parameter, and that `<fails>`
cascades to every call site (a failing call folds into the CALLER's own
guard — an entirely different, much bigger change than "be explicit
about a boolean"). Confirmed empirically before either version shipped:
`struct_typed_fn_return_if_else_muxes_per_leaf` (tests/firrtl.rs) —
`Pick(c : [1]) : Pair <combines> { if c? { ... } else { ... } }` — broke
exactly this way.

Lumi's own insight resolved it properly instead of leaving the
carve-out standing: an explicit `?` sitting directly in an if/while's
own condition position is the SAME SHAPE as a bare failing call there —
both are "test something fallible, right here, branch-scoped" — and a
bare failing call in that exact position was ALREADY exempt from
forcing `<fails>` (the three-way `if`-condition discharge above). So
`effects.rs` needed a fourth case, not a type-checker carve-out: `Stmt::
If`/`Stmt::While`'s own condition-inference now routes through a shared
`infer_branch_scoped_cond`, which unwraps one leading `Expr::Guard` and
re-dispatches on ITS inner using the identical comparison/fifo-op/
failing-call/plain-value cases the bare-condition discharge already
had — never falling through to the ordinary `Expr::Guard` arm that sets
`fails`. Once that existed, the type-checker rule needed no `in_rule`
distinction at all: a callee's `if c?` now stays exactly as inert as a
callee's `if Classify(a) { ... }` already was.

### `or`: fallback chains

`A or B or C` tries each fallible alternative in priority order (`A` first) and
uses the first one that succeeds — Verse's own failure-discharging fallback
operator (`08_failure`). v0 restricts every alternative to a fifo `Deq[]` on a
depth-1 fifo (call alternatives, `Enq` alternatives, and depth>1 fifos are all
separate, larger gaps — see TODO.md); a chain's last element may instead be a
plain, always-succeeding default value.

```trace
result := a.Deq[] or b.Deq[] or 0     -- tries a, then b, falls back to 0
result := a.Deq[] or b.Deq[]          -- no default: stays fallible
```

A default tail makes the *whole* chain infallible: the enclosing rule fires
unconditionally, regardless of whether either fifo has data — hand-lowered to
raw FIRRTL and confirmed via Icarus before this was implemented (no guard term
at all is emitted for a defaulted chain). Without a default, the chain stays
fallible exactly like a bare fifo op would: the alternatives' combined
occupancy folds into the rule's own guard, but as an `or` of each alternative's
readiness rather than the `and` every other guard-folding case in this
language uses — at least one alternative must be ready, not all of them. A
winning alternative is dequeued; every other alternative is left untouched,
same first-match-wins convention `prio` already uses.

`or`'s v0 restrictions, checked explicitly rather than silently mis-compiled:
an alternative may only be `Deq[]` (an `Enq[x]` alternative is rejected by
ordinary type-checking — it has no value of its own for `or` to select
between, so its `unit` type can never match a fifo element's `[N]`); a
fifo used as an `or` alternative may not be touched anywhere else in the same
rule (composing an alternative's conditional state transition with an
unconditional touch of the same fifo elsewhere hasn't been verified); and an
`or` chain may only sit directly in one of the same three positions a bare
fifo op can (a whole statement, the entire right-hand side of `:=`, or a `let`
init) — nested inside `if`/`while` or inside a larger expression is rejected
by the same generic position checks a misplaced fifo op already gets. Inside a
callee's own body is a DIFFERENT gap with its own dedicated check
(`check_no_or_in_callee_body`): unlike a bare guard, fifo op, or `logic`,
`or` has no callee-body support to fall back to at all yet — a first attempt
compiled a defaulted chain cleanly with the fifo's own dequeue silently
missing (found by hand-testing, not by construction), which is what that
check exists to close off.

Sequential bare guards already conjoin into one rule's readiness for free
(Verse's `and` needs no dedicated syntax for THAT), but combining two
fallible expressions inside a single larger expression — an `if` condition,
in particular — did need one: see "`and`: boolean combination sugar",
next, for the operator this section's own `logic A & logic B` idiom used to
stand in for by hand.

See `examples/or_fifos.tr` + `sim/or_fifos_tb.v`.

### `and`: boolean combination sugar

`A and B` is pure parse-time sugar for `(logic A) & (logic B)` — the
parenthesized idiom the previous section used to be the only way to spell
(`logic`'s own precedence note above explains why the parens are needed by
hand). `A and B and C` folds left-to-right, wrapping each of the three
operands in its own `Logic` exactly once: `((logic A) & (logic B)) &
(logic C)`, never re-wrapping the growing `&` accumulator (which would
trip `check_logic_args` — a plain `&` expression isn't a legal `logic`
operand). No new AST shape, no new type/effects/emission rule: every
downstream pass sees the identical `Binary(BitAnd, Logic, Logic)` tree
the hand-written idiom already produced and was already tested against —
`and` only teaches the PARSER a new spelling for it (same "sugar with no
AST trace" precedent as `tick sync[...]`'s own desugar).

```trace
if a > b and c > d { ... }        -- sugar for (logic a > b) & (logic c > d)
if f.Deq[] and g.Deq[] { ... }    -- sugar for (logic f.Deq[]) & (logic g.Deq[])
```

Precedence: `and` binds at `(2, 3)` — looser than every real binary
operator (comparisons are the loosest of those, `(3, 4)`, so a whole
comparison forms before `and` ever sees it as an operand) but tighter than
`or`'s `(0, 1)`, so mixing them unparenthesized reads the way every other
language spells it: `A or B and C` groups as `A or (B and C)`, and `A and
B or C` groups as `(A and B) or C`. Confirmed against the actual Pratt
loop, not just intended: `and`'s own arm checks `if 2 < min_bp { break }`
(so it stops at any tighter-binding recursive parse, e.g. as `or`'s own
rhs) and consumes its whole chain with an inner `while` loop rather than
`or`'s across-iterations flattening trick — `lhs` becomes an ordinary
`Binary(BitAnd, ..)` after the first fold, indistinguishable by shape from
a user's own literal `&`, so re-entering the outer arm and shape-sniffing
`lhs` on a later `and` isn't safe the way it is for `Or`'s dedicated AST
variant.

Each operand still needs one of `logic`'s three legal shapes (a fifo op, a
comparison, or a call to a function that can fail and doesn't also write
state) — `check_logic_args_in` rejects anything else, phrased in terms of
`and` rather than `logic` when the operand came from this desugar
(`ast.and_sugar` tracks which `Logic` nodes are synthetic, purely so the
error names the keyword the user actually typed).

**`and` does not mutate, unlike `or`.** `A or B` genuinely dequeues the
winning fifo; `and` inherits `logic`'s pure occupancy/space TEST with no
`Enq`/`Deq` performed — `f.Deq[] and g.Deq[]` reads both fifos' readiness
and dequeues neither. The two sibling-looking operators are NOT
symmetric in this one respect.

### `sequences`: multi-cycle code

A `sequences` block spans more than one cycle. There is no cross-cycle rollback,
no checkpoint hardware. Each cycle is still its own one-cycle transaction.

The `tick` statement marks a cycle boundary.

```trace
Rmw(addr : [8]) <sequences, reads {mem}, writes {mem}> {
    let v = mem[addr]
    tick
    mem[addr] := v + 1
}
```

A value that crosses a `tick` (`v` above) needs a save register, because it must
survive past the cycle where it was computed. `tick` must sit at the top level of
a `sequences` body: it cannot nest inside `if`/`while`. A conditional cycle
boundary has no defined meaning yet.

A `let`-bound value crossing a `tick`, like `v` above, gets promoted to a real
register the same as any other captured value — `render_rule` rewrites the
declaring `let v = ...` statement into an ordinary register write (`v :=
...`) at the point it splices that statement into the generated segment rule,
since `let` itself always binds a fresh local and could never write a
register directly. This rewrite is why `let` needs no special-casing here:
whether a value was first bound with `let` or reassigned later with `:=`, it
crosses a `tick` the same way. The one restriction that remains is on the
*name*, not the binding form: two distinct `let`s of the same name that both
need to cross a tick (for example, two arms of a sequence shadowing `x` in
turn) collide on a single save register and are rejected — give the second
one a different name.

`tick` may name a trailing fallible expression: `tick <expr>`. The segment `tick`
opens does not fire until `<expr>` succeeds; until then, the rule retries every
cycle without otherwise progressing. Bare `tick` is shorthand for a trailing
expression that always succeeds. `<expr>` follows the same rule as any other
fallible operation: an explicit `cond?` guard, or an already-fallible bracket
operation such as `sync[h1, h2]` (see "`spawn`, `sync`, and `race`" below).
`tick` may also sit right after `:=`/`let name =`, letting it gate an
assignment's own right-hand side instead of standing alone:
`value := tick race[h1, h2]` is exactly `tick` followed by `value :=
race[h1, h2]`, just spelled on one line.

### `while`: multi-cycle loops

`while COND { body }` inside a `<sequences>` rule iterates once per cycle —
the loop's own back-edge acts as a cycle boundary, the same role `tick` plays
between straight-line segments, which is exactly why an explicit `tick` still
can't nest inside one (a conditional cycle boundary would be a second,
overlapping way to cut the same segment).

**`COND` now accepts a bare fallible comparison/fifo-Deq/failing-call
directly, no `logic` needed — reversing this section's original call.**
The original reasoning ("Verse's own construct is `if`-shaped only, so
`while` stays restricted the way `if`'s bare-comparison exemption doesn't
extend to it") turned out to rest on a wrong premise: Verse has no native
`while` at all — only `loop` (unconditional, exited via `break`) and `for`
(each iteration its own failure context, a failed filter clause skips to
the next item, doesn't end the loop). Lumi's call once that was confirmed:
"I still want while to work like if, so it takes a fallible as a guard" —
so `check_cond` (types.rs) now passes `allow_bare_comparison: true` for
`Stmt::While` too, identical to `if`'s own flag, and `effects.rs`'s
`Stmt::While` arm gained the matching three-way discharge (comparison/
fifo-Deq/failing-call, none of which set `sig.fails`) mirroring `Stmt::
If`'s arm exactly. `logic COND` is still accepted (unlike `if`, `while`
has always required loops to fit at a `<sequences>` rule's top level,
so there was never an ergonomic reason to drop it entirely) but no longer
required:

```trace
rule r <sequences, fails> {
    cnt := x
    while cnt <> 0 {        -- bare, no logic needed
        cnt := cnt - 1
    }
    result := cnt
}
```

**Why this needed almost no new machinery — the SAME reuse story `if`'s
own bare-condition feature had.** `while COND { body }` lowers (see
"`while` lowering" below) by literally rendering `if COND { <body>; cont
:= 1 } else { cont := 2 }` as SOURCE TEXT and re-running the entire
pipeline on it (re-parse, re-resolve, re-effects, re-types, re-emit) — so
once `COND` itself survives `check_cond` a first time (on the ORIGINAL,
un-rendered `Stmt::While`), the RENDERED text hits `if`'s own, already-
built comparison/fifo-Deq/failing-call discharge on re-entry, with ZERO
new value-compilation or emission code. Confirmed by direct probe against
real emitted FIRRTL before this was written (the discriminating question,
per `advisor`'s review: does the bare guard leak into the whole segment's
`fires_r_sN`, wrongly gating the loop's own re-firing?) — it doesn't:
`node fires_r_s1 = and(eq(__cont_r, UInt<2>(1)), not(fires_r_s0))`, gated
purely on the schedule, with `cnt <> 0` only ever appearing inside the
mux selects (`connect cnt, mux(neq(cnt, UInt<8>(0)), ...)`, `connect
__cont_r, mux(neq(cnt, UInt<8>(0)), UInt<2>(1), UInt<2>(2))`) — exactly
the branch-scoped shape wanted. Verified further with a real firtool +
Icarus simulation reusing the EXISTING `while_countdown_tb.v` testbench
unchanged against a bare-comparison twin of its `.tr` source
(`examples/while_countdown_bare.tr`, `tests/sim.rs`'s `while_countdown_
bare_comparison_runs_through_real_cycles`) — if the bare and `logic`-
wrapped forms are genuinely equivalent hardware, the identical testbench
should pass with no changes, and it does.

A comparison nested inside a larger boolean combination (`while (a > b)
& c { ... }`) is still rejected exactly like `if`'s identical case —
`expr_has_undischarged_comparison` runs unconditionally in `check_cond`,
regardless of `allow_bare_comparison`, so gaining the WHOLE-condition
exemption didn't widen the nested-comparison gap too. Confirmed by direct
probe, not assumed from the structural similarity to `if` alone.

v0 scope: `while` must sit at a sequences rule's (or a spawned fn's) top
level, the same restriction `tick`/`spawn` already have — not nested inside
`if`/`if let`/another `while`. A `return` nested inside a `while`'s own body
is rejected the same way an early `return` anywhere else is (no early
return; `return` must be the last statement of a spawned fn's last segment).

**A `while` loop may only write module state (a reg/mem/output) directly —
not a local.** A local written inside the loop and needed elsewhere, an
accumulator (`acc := acc + n`) or anything else surviving past the loop, is
rejected with a clear error rather than silently accepted or miscompiled:

```trace
rule r <sequences, fails> {
    let acc = 0
    let n = x
    while logic n <> 0 {
        acc := acc + n      -- error: written across a `while` loop boundary
        n := n - 1
    }
    result := acc
}
```

This is narrower than the general case for a real reason, not an arbitrary
cut: `compute_captures` (see "Sequences lowering" below) requires a captured
local be write-once and read only in STRICTLY LATER segments — the same
invariant that makes an ordinary tick-crossing value sound to promote to a
register. A `while` loop's own segment is re-entered every iteration, so an
accumulator writes the SAME segment index every pass and reads its own prior
value in that SAME segment (`acc := acc + n` — semantically sound, a
register read genuinely sees last cycle's value, but indistinguishable from
the read-before-write hazard those checks exist to catch without teaching
`compute_captures` to reason about loop-carried dataflow specifically, which
this pass doesn't attempt). A local computed BEFORE the loop and only READ
inside it (never reassigned there) is unaffected and works today — its
single assignment segment is the pre-loop one, satisfying the existing
invariant exactly:

```trace
let limit = bound
cnt := 0
while logic cnt <> limit {    -- fine: limit is read-only inside the loop
    cnt := cnt + 1
}
```

### `break`: exiting a loop early

`break` exits the enclosing `while`/`while let` loop immediately, independent
of the loop's own condition — trace's spelling of Verse's actual `loop`+
`break` idiom (`loop: if (Cond[]) { ... } else { break }`, confirmed against
the "Book of Verse" the same way the reversal above was) rather than an
invention. Lumi's ask, once "Verse has no native `while`" was confirmed:
"I still want while to work like if, so it takes a fallible as a guard and
breaks on a break keyword" — two separate features, landed as two commits:
the bare fallible guard above, and this.

```trace
rule r <sequences, fails> {
    cnt := 0
    acc := 0
    while go = 1 {
        acc := acc + 1
        cnt := cnt + 1
        if cnt >= limit {
            break
        }
    }
    result := acc
}
```

**v0 scope: `break` may only sit as the LAST statement of a `while`/`while
let`'s own body, or of a `then`/`else` branch of an `if`/`if let` that is
ITSELF in that tail position — nested as deep as the user likes, as long as
EVERY enclosing level stays in tail position.** Nothing may follow it in its
own block (no statements after a `break`-containing `if`, no statements
after a bare `break`) — a deliberate cut that avoids ever needing to hoist
trailing statements into a synthesized branch (see "Implementation" below
for why). An `if` with no `else` written and a tail `break` in `then` gets
its missing `else` SYNTHESIZED (keep looping) — so `if cond { break }` alone
is legal, exactly Verse's own `if (Cond[]) { ... } else { break }` idiom in
spirit, just with the roles of the two branches free to be either order and
the `else` optional when it would just be "keep going" anyway.

```trace
-- Verse's own idiom, spelled directly:
while logic 1 <> 0 {
    if not(Cond[]) {
        break
    }
    DoWork()
}
```

**Statements before a `break` in its own branch still commit — `break`
means "advance past the loop starting NEXT cycle," not "nothing this
iteration happened."** `acc := acc + 1` in the example above still runs on
the cycle `break` is taken (it's earlier in program order, in the SAME
branch), so the loop's very last iteration still contributes to `acc`; only
the iteration AFTER that never happens. Flagged by `advisor`'s review before
implementation, specifically so this got documented rather than left to
read as "break discards the current iteration" (it doesn't).

`break`'s own condition, like any other read inside the loop body, sees a
register's OLD (pre-edge) value — the same "connects land for next cycle,
reads see the old value" rule every other register read in this language
follows, not something special to `break`. A loop with `if cnt >= limit {
break }` therefore breaks one iteration "later" than a naive read suggests:
by the time `cnt`'s OLD value first satisfies the threshold, the CURRENT
iteration has already incremented past it once more (hand-verified against
real simulation — `sim/while_break_tb.v`'s own doc comment works the exact
cycle-by-cycle arithmetic).

**Implementation: a new `Stmt::Break` AST node, fully consumed by sequences
lowering — never reaches firrtl.rs.** Every exhaustive `Stmt` match across
the compiler needed a new arm (resolve.rs, effects.rs, types.rs,
elaborate.rs, lower.rs, firrtl/*.rs) — the Rust compiler's own exhaustiveness
checking found every site, not a memory-based sweep; nearly all of them are
a pure no-op leaf (mirroring `Stmt::Tick`'s identical treatment — `break`
has no sub-expressions, reads, writes, calls, or captures of its own).
Three sites hold the actual new logic:

- `effects.rs`'s `check_stmt` gained a `Stmt::Break` arm requiring
  `<sequences>` on the enclosing item, mirroring `tick`'s identical check —
  deliberately NOT extended to `<elaborates>` the way `while` itself
  nominally can be: an elaboration-time loop is evaluated by a separate
  const-eval interpreter (`elaborate.rs`) that already rejects `while`
  outright ("a loop in `<elaborates>` code is not yet supported"), so
  `break` there is moot regardless, but this check catches it explicitly
  and earlier rather than relying on that.
- `lower.rs` gained `find_break_misplaced` (the PLACEMENT check — mirrors
  `find_nested_tick`'s shape but the opposite question: not "is this
  nested at all" but "is this nested in exactly the one legal shape") and
  `find_break_anywhere` (mirrors `find_tick_anywhere`/`find_while_
  anywhere`'s role in `plan()`'s own top-level gate, so a rule with a
  stray misplaced `break` but no `tick`/`while` alongside it STILL routes
  through the placement check rather than silently skipping lowering
  entirely and reaching firrtl.rs as a no-op nobody rejects).
- `lower.rs` gained `render_loop_body`, replacing the previous "splice
  every body statement verbatim, then unconditionally append `cont :=
  stay`" with a function that recurses into a tail `if`/`if let` ONLY when
  a `break` is actually reachable inside it (`find_break_anywhere` again —
  when there's no `break` anywhere in a loop body, this reproduces the
  OLD rendering byte for byte, not a restructured-but-equivalent one, so
  every PRE-EXISTING `while`/`while let` test kept its exact original
  FIRRTL assertions unchanged). When a `break` IS present, the last
  statement of `stmts` gets special handling: a bare `break` renders as
  `cont := advance` (nothing else reaches the output — the keyword itself
  vanishes); a tail `if`/`if let` recurses into EACH branch with this SAME
  function, synthesizing an empty branch (`cont := stay`) when no `else`
  was written; anything else splices verbatim then `cont := stay`, same as
  before. `find_break_misplaced` and `render_loop_body` are deliberately
  kept in sync on what counts as a legal position — a shape the check let
  through that the renderer couldn't handle would silently drop the
  `break`'s effect rather than erroring on it.

Hand-verified end to end against real firtool + Icarus AND Verilator
simulation before any structural test was written: `examples/while_break.tr`
+ `sim/while_break_tb.v` (`tests/sim.rs`'s `while_break_exits_the_loop_
early_through_real_cycles`) covers a mid-loop break, an immediate break on
the very first iteration, and the loop never even being entered (`go`
false from the start, unrelated to `break` itself). A separate probe
confirmed `break` nested two levels deep (an `if` inside an `if`, both in
tail position) settles correctly and holds — `render_loop_body`'s recursion
has no depth limit, unlike several other v0-scoped constructs in this file
that cap nesting at one level, since the SAME transformation applies
identically at every level with no new kind of risk per level.

### `while let`: looping over Option presence

`while let NAME = opt? { body }` is the loop-shaped sibling of `if let`
(above): each iteration re-checks `opt`'s presence, binding `NAME` to the
unwrapped value for that iteration only — visible ONLY within `body`, never
after the loop, the same new-scoping-rule shape `if let`'s `then_body` has.
No `else`: a loop has nothing to run once instead of looping, the same
reason plain `while` has no `else` either — absence just ends the loop.

```trace
rule r <sequences, fails> {
    while let x = opt? {
        acc := acc + x
        opt := false      -- module state only, refilling/clearing opt each pass
    }
}
```

Inherits `if let`'s v0 restrictions verbatim, not as a new gap this feature
opens: `init` must be an Option's own unwrap (`opt?`, `opt : ?T`) — never a
fifo op, failing call, or comparison — and `NAME` may be used as a whole
value inside `body` but not chased through a further `.field` access. It
also inherits plain `while`'s own v0 restriction just above: the loop body
may only write module state directly, never a local that would need
capturing across iterations — `while let`'s own loop segment is subject to
the identical `compute_captures` write-once invariant, no differently from
a comparison-gated one.

Scoped this way deliberately (Lumi's call): `while` was asked for first
(`while let`, "scoped the same way" as `if let`), and direct probing found
`while` itself had no working FIRRTL emission path at all — building
`while`'s own multi-cycle lowering came first, as its own unit (see
"`while` lowering" below), with `while let` following as a much cheaper
second step once that foundation existed.

### `spawn`, `sync`, and `race`

`spawn` starts an independent, parallel computation. `sync` waits for one or more
spawned computations to finish. `race` waits for the FIRST of two or more to
finish, and permanently blocks every other named one from ever completing.

```trace
module Fetch2 {
    mem bank0 : [16][8]
    mem bank1 : [16][8]
    in pc : [16]
    out ir : [32] = 0

    ReadBank0(addr : [16]) : [16] <sequences> {
        let v = bank0[addr]
        tick
        return v
    }

    ReadBank1(addr : [16]) : [16] <sequences> {
        let v = bank1[addr]
        tick
        return v
    }

    rule fetch2 <sequences> {
        let h1 = spawn ReadBank0(pc)
        let h2 = spawn ReadBank1(pc + 1)
        tick sync[h1, h2]                   -- waits until both finish
        ir := pack(h1.result, h2.result)
    }
}
```

`spawn Callee(args)` requires `Callee` to be a `<sequences>`-declared function.
The expression's type is a handle, written internally as `Ty::Handle(T)` where
`T` is `Callee`'s return type. `h.result` reads the spawned computation's return
value; `h.done` reports whether it has finished, as `[1]`. Both fields are
read-only.

`spawn` must sit at the top level of its enclosing segment, the same restriction
`tick` has. A loop can therefore never contain a `spawn`, so the number of spawns
in a design is always static. Each spawn needs its own handle: reusing a handle
name for a second `spawn` in the same rule is a compile-time error, since each
occurrence gets its own private register set. `let h = spawn ...` is the ordinary
form, same as any other fresh local; the spawn machinery never splices the
trigger statement's own text, so it needs no rewrite of the `let` — it always
synthesizes brand-new register-write lines from the extracted handle instead.

`sync[h1, h2, ...]` waits for every named handle to finish. Square brackets mark
it as a fallible operation, the same convention `f.Deq[]`/`f.Enq[x]` use. It must
appear as its own statement, not nested inside `if`/`while` or embedded in a
larger expression — the idiomatic place is directly after `tick`
(`tick sync[h1, h2]`), so the wait and the cycle boundary read as one step.

```trace
module FirstWins {
    in trigger : [1]
    out result : [8] = 0

    Fast(x : [8]) : [8] <sequences> {
        tick
        return x + 1
    }
    Slow(x : [8]) : [8] <sequences> {
        tick
        tick
        return x + 2
    }

    rule pick <sequences> {
        trigger?
        let hf = spawn Fast(1)
        let hs = spawn Slow(1)
        tick
        race[hf, hs]                        -- waits until either finishes
        if hf.done = 1 {
            result := hf.result
        } else {
            result := hs.result
        }
    }
}
```

`race[h1, h2, ...]` is a guard, same convention and same top-level-only
restriction as `sync` — but it succeeds as soon as ANY named handle finishes,
not all of them, and it permanently blocks every OTHER named handle from ever
completing. `race` does not un-write anything a loser already wrote before
losing — only that loser's FUTURE segments are blocked, so a racer with a side
effect beyond its own return value must not depend on losing having no effect.
A simultaneous finish (both named handles ready the same cycle) is tie-broken
by the ordinary scheduler's own declaration-order/`urgency` priority, the same
as any other same-cycle conflict — exactly one handle ever actually completes,
never both.

Read as a bare statement (as above), `race` is a guard only — read whichever
handle's `.done` came back 1 yourself. `value := race[h1, h2, ...]` (writing
existing state directly) or `let value = race[h1, h2, ...]` (binding a fresh
local) is the value-producing form instead: `value` becomes whichever handle
actually won, directly, with no `if`/`else` of your own needed — the
`RaceValue` example below uses the `:=` spelling, since `result` there is
already the module's own output:

```trace
module RaceValue {
    in trigger : [1]
    out result : [8] = 0

    A(x : [8]) : [8] <sequences> {
        tick
        return x + 10
    }
    B(x : [8]) : [8] <sequences> {
        tick
        return x + 20
    }

    rule pick <sequences> {
        trigger?
        let ha = spawn A(1)
        let hb = spawn B(1)
        result := tick race[ha, hb]
    }
}
```

`race[...]`'s value is only meaningful once its own guard has succeeded, but
that is no obstacle here: `result` above already resolves to the module's own
output, so `result := tick race[ha, hb]` is an ordinary write of already-
existing state, the same as any other `:=`. Writing the race's value into a
brand-new local instead (`let value = race[...]`) works too, and needs the
same save-register treatment as any other fresh value crossing a `tick` — see
"`sequences`: multi-cycle code" above. Either way, `race`'s value-producing
form synthesizes a fresh `value := __race_value(...)` or `let value =
__race_value(...)` line, matching whichever form the destination was written
with. `tick` optionally taking a trailing expression (`tick <expr>`) extends
to this shape too: `result := tick race[...]` puts the tick right next to the
expression it gates, folding `tick \n result := race[...]` onto one line —
the idiomatic spelling, as `RaceValue` shows.

### `chooses`: specification, not synthesis

The choice operator `|` and the `any` form are **not synthesizable**. They exist
for specification and verification. Nondeterministic choice becomes a free
variable in a model checker, like SVA `$anyseq`. Implementations are checked as
refinements of specs that declare `chooses`.

```trace
spec AnyGrant(reqs : [N]) : [clog2(N)] <combines, chooses, fails> {
    let i = any(0..N-1)       -- free variable: the checker picks
    reqs[i]?                  -- constrained: the pick must be a requester
    return i
}

impl RoundRobin(reqs : [N]) : [clog2(N)] <combines>
    refines AnyGrant
{
    -- deterministic logic; checked against the spec
}
```

The `chooses` effect marks spec-only code. Only a `spec` may declare it. Using
`any` without it is a type error. Synthesizing code with it is a type error.

`|` is contextual: in an item without `chooses` it is bitwise or; in a `chooses`
item it is the choice operator. One token, disambiguated by the effect, never by
the parser.

## Optional rule sugar

Gating a rule on an external trigger, written by hand, is `in trigger : [1]`
plus `trigger?` as the rule's own first statement — see `FirstWins`/`RaceValue`
above. `rule foo?` collapses this into one declaration, under the rule's own
name:

```trace
rule step? {
    count := count + 1
}

-- desugars to:
in step : [1]
reg __prev_step : [1] = 1

rule __edge_step {
    __prev_step := step
}

rule step {
    (step & not(__prev_step))?
    count := count + 1
}

schedule {
    conflict_free { __edge_step, step }
}
```

The `?` sits directly after the rule's own name, before any `<effects>` tag
list (`rule step? <reads {...}> { ... }`), mirroring `?T`'s own use as a type
marker. `rule foo? <sequences>` is rejected at parse time (v0 restriction,
not a permanent one): every node this sugar synthesizes shares `foo`'s own
span, which is harmless for every pass that reads AST structure, but
`lower.rs`'s `<sequences>` splicing reconstructs a rule's segments FROM
spans, and the collision panics deep inside it. The manual `in foo : [1]`
+ `foo?` pattern still works fine under `<sequences>` — only the sugar
itself is restricted.

Three things this buys over the hand-written form: `foo` names both the port
and the rule, so there's no separate `trigger`-style name to invent; the enable
is one-shot (fires once per external pulse, not every cycle the port is held
high), which the hand-written form doesn't give you for free; and a port
already held high AT RESET does not count as a rising edge — `__prev_foo`
resets to `1`, not `0`, specifically so `not(__prev_foo)` reads `0` on the very
first post-reset cycle no matter what `foo` itself reads as (Lumi's call, via
`AskUserQuestion` — the alternative, resetting to `0`, would read a port tied
high at reset as a genuine `0→1` pulse and spuriously fire the rule once on
cycle 0).

The `in` port, not a `reg`, is deliberate: only the outside world can drive it,
never another rule in the design, exactly mirroring the hand-written `trigger`
pattern. One-shot behavior needs a compiler-synthesized shadow register
tracking `foo`'s previous value, since an `in` port is driven every cycle and
the compiler cannot "clear" it the way it clears a `reg` at the end of a rule
body. The shadow register's own update (`__prev_foo := foo`) has to run
UNCONDITIONALLY, every cycle, independent of whether `foo`'s own rule fires —
it cannot live inside the gated rule body, or it would only advance the edge
history on cycles the rule actually fires, corrupting the very detection it
exists to support. This is why the desugaring synthesizes a SECOND,
always-firing rule (`__edge_foo`, no guard at all) whose only statement is the
update — reusing the ordinary rule/schedule machinery wholesale, at the cost of
one extra double-underscore-prefixed rule showing up in `--explain-schedule`
output, the same visibility trade-off `sequences` lowering's own synthesized
segment rules already accept.

That second rule makes `__edge_foo`/`foo` an ordinary read/write conflict in
the scheduler's eyes (both touch `__prev_foo`), which is why the desugaring
also synthesizes the `conflict_free { __edge_foo, foo }` exemption above — see
"The schedule block". This is sound, not a workaround: `foo`'s guard reads
`__prev_foo`'s pre-this-cycle value regardless of any same-cycle write to it,
an ordinary register read, so there is no real hazard for `conflict_free` to be
trusting past.

All five items (the `in` port, the shadow register, the two rules, and the
`schedule` block) are synthesized as real AST nodes directly at PARSE time —
`parser.rs`'s `parse_rule`/`desugar_optional_rule` — and spliced into the
enclosing module's item list exactly as if hand-written, the same "real nodes,
spliced inline" move `eat_leading_tick` already uses at statement level. This
differs from `sequences`/`elaborate` lowering, which splice SOURCE TEXT and
re-run the whole front end, specifically because those passes synthesize nodes
only reachable after interpreting the original tree (a segment boundary, an
elaborated call) — `rule foo?`'s expansion is a fixed template needing no such
interpretation, so it can enter the one subsequent resolve → effects → types →
schedule → firrtl run directly, with no retroactive record problem.

## Expression surface

Integer literals (`Expr::Int`) have no width of their own. They type as `Ty::Int`
and absorb a width from context, range-checked at the point they are used.

Sized literals give a definite width up front: `<width>'<radix?><value>` — for
example `8'd6`, `8'hFF`, `8'b1010`, `8'o17`, or `8'6` (defaults to decimal). A
sized literal types directly as `[width]` and is range-checked immediately,
against its own declared width — `4'd20` is an error even in a context that
could absorb a wider value. Overflow is always a compile error, never silent
truncation. `reg`/`out` may omit an explicit `: ty` when the initializer is a
sized literal: `reg a = 8'd6` declares `[8]`.

A bit-vector type is written `[N]` — no `bits` keyword. Told apart from a list
literal (`[a, b, c]`) purely by content, not position: a bracket holding
exactly one item with no trailing comma is always the `[N]` type; anything
else (empty, or 2+ comma-separated items) is a list value. Nothing about a
type expression is otherwise special — arithmetic (`[N+1]`), a call
(`[clog2(N)]`), or bracket application (a memory's `elem_ty[depth]`, a fifo's
own `{depth}elem_ty`) all parse the same as any other expression in that
position. `[N]` is unambiguous everywhere it nests too — inside `list[8]`,
a call argument, a struct field type — since every bracket parses through
this same primary rule regardless of depth. A genuine one-element list VALUE
(as opposed to a type) needs a trailing comma to tell it apart — `[x]` is
the `[N]` type shorthand, `[x,]` is the one-element list, the same
Rust-style escape hatch a one-element tuple literal uses for an identical
reason. Omitting the comma (`Foo([x])`) fails with a clear type error (a
`[N]` type appearing where a value was expected) rather than silently doing
the wrong thing.

`list[T]`'s own single argument gets one more sugar on top: a bare width
expression (`list[8]`, `list[N]`, `list[clog2(N)]`) is shorthand for
`list[[N]]` — the overwhelmingly common case (a list of plain bit-vectors)
never needs the double bracket. `list[Pair]` (a list of some OTHER type,
e.g. a declared struct) still spells its element type out directly, since
only a `[N]`-shaped element has anything left to abbreviate — `list[[N]]`
remains valid too, exactly equivalent to `list[N]`, just longer to write.
One sharp edge: a bare identifier is ambiguous between "width parameter"
and "type name", and it's resolved in favor of the former. `list[Pair]`
with `Pair` misspelled (or naming a module, register, anything but a
declared struct) is not rejected as an unknown type — it's silently taken
as an implicit width parameter instead, producing a list of unconstrained-
width bit-vectors rather than a clear error. Spelling the element type out
in full sidesteps this; there's no sugar-free way to get the old "expected
a type here" diagnostic back for a bare name.

Arithmetic and bitwise operators follow Chisel-style modular width rules:

- `+`/`-` keep the wider operand's width (modular; `pc := pc + 3` is legal on a
  fixed-width register).
- `*` sums the operand widths when both are `bits`. Multiplying by a bare integer
  literal keeps the other operand's width instead.
- `/`/`%` default to the wider operand's width, same as `+`/`-`.
- `<<`/`>>` keep the left operand's own width, matching Verilog's fixed-width
  shift rather than growing or shrinking it. The shift amount may be a literal or
  a runtime value.
- `>>>` is `>>`'s sign-extending sibling: same width rule, but the vacated high
  bits repeat the operand's own top bit instead of filling with zero. This
  language has no signed type (see TODO.md), so arithmetic-vs-logical shift is
  a per-operator choice, not a property of the operand's own type — `x >>> n`
  and `x >> n` are both legal on the same `[N]` value, with different
  results whenever the top bit is set.
- Comparisons are fallible, not plain `[1]` values — see "Comparisons:
  fallible by default", below.
- Unary `-` is two's-complement negate, wrapping within the operand's width.
  Unary `~` is bitwise complement.
- `not` is logical negation, distinct from `~`: it requires a `[1]` operand.
  Every position where `not` is meaningful already carries `[1]`, so the
  two operators agree there; `not` on a wider value is a type error, since
  this language has no "nonzero is true" coercion.

Equality follows Verse's spelling rather than C's: `=` is equality
(`x = 1`, not `x == 1`) and `<>` is not-equal (`x <> 1`, not `x != 1`).
`:=` (bind/reassign) and a `reg`/`out` declaration's own `= init`
initializer are unrelated uses of `=`-shaped tokens, disambiguated by
parser position, not by the equality operator itself. `<`/`<=`/`>`/`>=`
already matched Verse's spelling and are unchanged. Bitwise `&`/`|`/`^`/`~`
have no Verse operator equivalent (Verse spells them as functions,
`BitAnd`/`BitOr`/`BitXor`/`BitNot`) and stay symbolic — an HDL leans on
bitwise operators far too heavily for function-call spelling to be a real
improvement.

Writing a wider value into a narrower target is always an error, naming
`trunc(value, width)` as the fix (`trunc(value)`, width inferred from the
write target, also works — see "Inference: two solvers" below). There is no
silent truncation anywhere in the language. Separately, an oversized-literal
or total-discard-shift diagnostic on one specific operator application (not
a write target) can be silenced with a `.!` suffix on that operator, e.g.
`x >>.! 300` — see the same section for the distinction between the two.

Bit-select and slice: `x[i]` selects one bit; `x[hi..lo]` selects an inclusive
range, both ends given, descending. Both bounds may be compile-time constants; a
single index (`x[i]`) may also be a runtime value. A slice with a non-constant
bound (`x[a..b]`, `a`/`b` not both constant) is a type error: the result width
would depend on a runtime value, which this statically-typed language cannot
express.

Verilog-style indexed part-select fills that gap: `x[base +: width]` and
`x[base -: width]`. `base` may be a runtime value; `width` must be a compile-time
constant, since it fixes the result's width. `+:` counts up from `base`; `-:`
counts down.

Inside a bit-select, `..` is inclusive on both ends, and that convention is fixed,
not configurable — there is no separate `..=` form.

## Module ports

Two declarations, alongside `reg`/`mem`/`fifo`:

```trace
in inc : [8]              -- external combinational signal, read-only
out sum : [8] = 0         -- register-backed, exposed as a port
```

`in` is a pure wire driven from outside the module. Reading one inside a rule
reads this cycle's value. Writing one is an error.

`out` behaves like a plain `reg` inside a rule — same `:=` write, same effect
row, same scheduling — and is also exposed as a module port. It is
**register-backed, never combinational**: a rule's writes are speculative until
the clock edge, and a combinational output would leak that speculative value
outside the module. So `out x` is an ordinary internal register connected
unconditionally to a port; the outside world sees the committed value one cycle
after it is computed. A purely combinational module, with no state at all, is not
expressible in v0.

A third kind, `io name : ty`, is structural rather than rule-integrated —
comparable to `in`/`out` only in that it's declared alongside them, not in how
it behaves:

```trace
io bus : [8]               -- structural bidirectional port; no rule reads or
                            -- writes it (see below)
```

`io` lowers to FIRRTL's `Analog` type, which supports exactly one operation,
`attach` — net-to-net wiring, nothing else. There is no FIRRTL primitive for
"drive this value when enabled, else float," so an `io` port carries no value
a rule body could read or write: no `:=`, no comparison, no `reads`/`writes`
declaration, no effects row at all. The only legal statement touching one is
`attach`, itself structural (module-level, alongside `reg`/`inst`, not inside
a rule — an unconditional net connection has no clock/cycle semantics):

```trace
module Child {
    io bus : [8]
}

module Top {
    io bus : [8]
    inst c : Child

    attach bus, c.bus   -- wires the two `bus` ports straight through
}
```

An `attach` operand is a bare `io` port name (this module's own) or
`instance.port` (a child's); both sides must be `io`-kind and the same width.
An `io` port never `attach`ed to anything is legal, not an error — FIRRTL
itself accepts an unconnected `Analog` port (confirmed directly against
firtool), and trace adds no stricter requirement on top.

Pass-through wiring alone has no leaf to drive from — real tri-state I/O needs
a hand-written Verilog module behind the `io` port, declared as an
`extmodule`:

```trace
extmodule TriBuf from "tribuf.v" {
    in enable : [1]
    in data : [8]
    out sensed : [8]
    io pad : [8]
}

module Top {
    io bus : [8]
    in enable : [1]
    in data : [8]
    out sensed : [8] = 0
    inst t : TriBuf

    rule drive {
        t.enable := enable
        t.data := data
        sensed := t.sensed
    }

    attach bus, t.pad
}
```

`extmodule Name from "path.v" { ports }` declares an external Verilog
module's interface — its real implementation lives entirely in the
referenced `.v` file, which trace's own compiler never reads or validates
(the path is opaque data; FIRRTL emission never mentions it either,
confirmed by hand-lowering one through firtool). Ports use the same
`in`/`out`/`io` vocabulary as an ordinary module (an extmodule's `input`/
`output` map onto exactly the same read/write rules `inst.port` already
gives an ordinary instance), but an extmodule has no `clock`/`reset` port
and no body — if the underlying Verilog needs a clock, declare it as an
ordinary `in` port and wire it explicitly (v0 restriction: no FIRRTL-
Clock-typed port). `inst t : TriBuf` instantiates it exactly like an
ordinary module; `t.enable`/`t.data`/`t.sensed` are driven/read from rule
logic like any other instance port, and `t.pad` (its own `io` port) is
only ever reachable via `attach`.

Getting the `.v` implementation to a simulator is a separate, not-yet-solved
problem: `devenv shell -- simulate` doesn't know to pass it to iverilog
alongside the generated design (see sim/README.md and TODO.md).

```trace
module Accumulator {
    in inc : [8]
    out sum : [8] = 0

    rule accumulate {
        sum := sum + inc
    }
}
```

## Memory, fifo, and submodule declarations

```trace
reg   name : ty (= init)?     -- one register
mem   name : ty                -- a memory: ty is elem[depth], e.g. [16][256]
fifo  name : ty                -- a fifo; ty is an element type (depth 1) or
                                --   {depth}elem_ty (e.g. {4}[8])
inst  name : Module            -- a child module instance
```

A memory access is `m[addr]` to read, `m[addr] := value` to write. A read is
free: it is wired unconditionally, independent of whether any rule fires, so it
may appear anywhere an ordinary expression can. A write may nest inside
`if`/`else` (see "Nested writes" below); outside of `if`/`else` it must sit at a
rule's top level, same as any other statement. A memory has one write port: a
second _unconditional_ write to the same memory in one rule is a compile-time
error, since it would silently discard the first — even to a different
address, only one write can land per cycle. Two `if`-guarded writes to the same
memory are fine and chain by priority (the later one wins when both fire); an
unconditional write followed by a conditional one is also fine, the
unconditional write becoming that `if`'s implicit fallback.

A fifo has two operations, both fallible:

```trace
let x = f.Deq[]   -- fails when f is empty
f.Enq[x]           -- fails when f is full
```

Both failure conditions fold into the rule's guard exactly like an explicit
`?`. `Enq`/`Deq` must
sit at a rule's top level. A rule that both `Enq`s and `Deq`s the same fifo is a
pass-through: this cycle's `Deq` reads the old value, this cycle's `Enq` writes
the new one, and the combined guard is just "the fifo currently holds a value" —
the `Enq` side's own guard is dropped for this specific pairing. Enqueuing, or
dequeuing, the same fifo more than once in one rule is a compile-time error,
regardless of depth: a second `Enq` would silently discard the first candidate
value (one `Enq[x]` fills one slot, not one slot per statement), and a second
`Deq` would silently re-read the same value rather than advance to a new one.
Use a `reg` instead if a rule needs to hold more than one candidate value in a
cycle.

A fifo's depth defaults to 1; `{depth}elem_ty` (e.g. `fifo f : {4}[8]`)
declares a deeper one — depth-first, the preferred spelling, though a
memory's postfix `elem_ty[depth]` (`[8][4]`) also works for a fifo:
both parse to the same underlying elem/depth pair.

An instance's ports are read and written through `.`: `adder.a := x` writes a
child's input port; `result := adder.sum` reads a child's output port. Writing an
output port, or reading an input port, is a type error. A `module` may be
declared inside another module's body; the nested name is visible only within
its enclosing module. Modules share no state with each other, regardless of how
they are lexically arranged: a nested module's rule may not reference its
parent's `reg`.

### Nested writes

A register write, an instance-port write, and a memory write may all live inside
`if`/`else`, threaded through as a mux. A branch that does not write a register
or a port holds that state's current value; a branch that does not write a
memory leaves the write disabled entirely for that cycle, since a memory has no
"hold" fallback. A memory _read_, and a fifo `Enq`/`Deq`, must still sit at a
rule's top level.

```trace
rule step {
    count := count + 1
    if we = 1 {
        m[addr] := data
    }
    read_data := m[read_addr]
}
```

## Structs

```trace
struct Pair {
    valid : bit
    data : [8]
}
```

A `struct` declares an ordered, named field list. `Name{ field: value, ... }`
constructs one — every declared field must be given exactly once, in any
order; a missing field, an unknown field, or a field given twice are all
compile-time errors. `.field` reads a field back:

```trace
module M {
    fifo input : [8]
    reg p : Pair = Pair{ valid: 0, data: 0 }

    rule fill {
        let d = input.Deq[]
        p := Pair{ valid: 1, data: d }
    }
}
```

A `reg`, `in`, or `out` may be struct-typed; a plain local may too, as long as
it's bound directly to a struct literal (`let p = Pair{...}`) — reading a
field off a local that's merely an alias for another struct-typed value
(`let q = p; q.field`) isn't resolved in v0.

A struct field may itself be another struct — nested arbitrarily deep, not
just one level:

```trace
struct Header {
    valid : bit
    seq : [4]
}

struct Frame {
    header : Header
    data : [8]
}
```

A chained field read (`f.header.valid`) walks each level in turn; a nested
struct literal (`Frame{ header: Header{ valid: 1, seq: 0 }, data: d }`)
constructs the whole tree at once, still exhaustive at every level. A struct
that directly or transitively contains itself (`struct A { b : B }` /
`struct B { a : A }`) is a compile-time error — an infinitely-sized type,
caught before it ever reaches flattening (see "Struct emission" under Part
2), not a stack overflow.

`Name{ field: value, ..., ..base }` — struct update — fills every field this
literal DOESN'T name from `base`'s own same-named field instead of erroring
"missing field(s)":

```trace
p := Pair{ data: 5, ..p }   -- keep valid, change data
```

`base` reads directly off `base`'s own flat fields (`compile_struct_field_
read`), so it's restricted to a bare reference (a reg/local/param name), not
a general expression — a call there would need re-evaluating once per field
`..base` supplies, silently duplicating whatever the callee's body does.
`..` may only be the LAST item (Rust's own rule) and only ever fills fields
THIS literal's own list doesn't name — it never recurses into a nested
struct/Option field that's itself only partially given (`Pair{ inner:
Inner{ x: 1 }, ..old }` does NOT reach into `inner`'s own missing fields;
write your own nested `..` there if you want that). `base` must be the exact
same struct type being constructed. Not supported in a reg/output INIT: v0
requires a reg/output's reset value be a fully-explicit compile-time
constant, and `base`'s own flat fields generally aren't known until runtime.

Struct destructuring — `let {field, field: bind, ...} = source` binding
several fields into fresh locals in one statement — is documented under
"Locals" below; both it and struct update are pure sugar over the same
`.field` projection machinery, not new capabilities of their own.

v0 restrictions, all enforced as clean compile-time errors rather than left to
miscompile: a field's type must be `[N]`, another struct, or `?T` (no
`list`, no fifo/mem); a struct has no per-field write — `p.field := x` is rejected,
assign the whole value instead (`p := Pair{...}`, same restriction a `spawn`
handle's `.result`/`.done` fields already have); a struct-typed write's
right-hand side must itself be a struct literal, not another struct-typed
value (`p := q` between two struct-typed regs is rejected, not silently
compiled to a frozen register — `p := Pair{ ..q }` is the supported way to
copy every field from `q`, field by field, not a loophole around this
restriction); a struct-typed fn/rule param IS supported —
an argument may be a struct literal or a reg/another same-typed param,
resolved by chasing through the alias to its flat fields (see "Calling a
function from a rule" below) — and a struct-typed RETURN is too: a callee's
trailing `return <expr>` (a fresh struct literal, another struct-returning
call, or one of the callee's own params passed through unchanged) decomposes
one leaf field at a time, threaded back through the same per-field write
machinery a direct struct-literal write already uses (see "Calling a
function: inlining" under Part 2); a struct-typed port on an *instantiated*
submodule is rejected
(its target module flattens the port to N real ports internally, see
"Struct emission" under Part 2 — wiring it from outside by its bare name has
no way to reach those).

## Option types

`?T` is sugar for a compiler-synthesized `{ valid: [1], data: T }` struct —
it reuses every bit of struct flattening/read/write machinery, not a parallel
mechanism of its own. Verse spells the absent case `false` rather than a
dedicated `none` keyword, tying it into Verse's logic-programming failure
model; a bare value of `T` coerces implicitly to the present case wherever
it's *written* (a reg/output/local init, a state write, an ordinary
assignable position):

```trace
reg opt : ?[8] = false       -- absent
...
opt := 8'd5                  -- present(5), no wrapper syntax needed
opt := false                 -- absent again
```

`false` is a real lexer keyword, not a general boolean literal — there is no
`true` counterpart. This is deliberate: if `false` doubled as a plain zero
bit, `?[1]` would be ambiguous between "absent" and "present, holding 0".
`false` only type-checks against a `?T` target.

`optional <expr>` is an explicit ONE-LAYER "present" constructor — unlike
the bare-value coercion above (which fills EVERY remaining `?` layer at
once, present all the way down), `optional` forces exactly the next
layer's `valid` to true and hands `expr` to that layer's `data`, leaving
`expr`'s own shape to determine what happens at any layer beneath. This
matters once `T` is itself an `Option`: bare coercion can only reach
`??T`'s two fully-agreeing states (fully absent, fully present), but
`optional false` on a `??T` target builds the third — outer present,
inner absent (`Some(None)`) — by forcing presence at the OUTER layer
while `false` constructs absence at the inner one:

```trace
reg oo : ??[8] = optional false   -- Some(None): oo.valid=1, oo.data.valid=0
oo := 8'd5                        -- fully present: oo.valid=1, oo.data.valid=1, oo.data.data=5
oo := false                       -- fully absent: oo.valid=0, oo.data.valid=0
```

`optional` nests one layer per keyword (`optional (optional 5'd3)` forces
both of a `??[5]`'s layers present explicitly, equivalent to the bare
`5'd3` coercion above it since neither leaves an inner layer for `false`
to make interesting) and works at plain `?T` too, where it's stylistic
rather than load-bearing (`reg opt : ?[5] = optional 5'3` reads more
clearly than the bare `5'3`, especially at `?[1]`, where the reader can't
tell "present, holding 0" apart from "absent" without already knowing a
bare value coerces) — `optional`'s single layer and `?T`'s single layer
line up exactly, so there's no room for an inner `false` to add anything
`optional 5'3` alone doesn't already say.

`optional e` has no standalone type of its own (mirroring `false`'s own
`Ty::AbsentLit` sentinel): it only type-checks against a `Ty::Option`
target, which supplies the layer it doesn't know — used anywhere else
(`x := optional 5` where `x : [8]`) it's a clean type error, not a
silent no-op. `optional <alias>`, where `<alias>` is a plain reference
already typed `?T` (a reg, param, local, or field — not a fresh literal
or computed value), is also rejected: this is the same "copying one `?T`
value into another isn't supported yet" v0 restriction below, one layer
up — emission has no way to thread an aliased `?T`'s own live valid/data
pair into another Option's flat fields. `optional Foo(x)`, `Foo` returning
`?T`, is exempt from that restriction the same way a bare `Foo(x)` call
already is (`callee_fail_cond`/`compile_call_field_value` decompose a
call's return per-leaf rather than aliasing a flat register).

Unwrapping goes through the *same* `?` guard operator a fifo `Deq[]` or a
`<fails>` call already uses, generalized: when its operand types as `?T`
instead of `[1]`, `opt?` fails the rule (discarding any writes it would
have made) if `opt` is absent, or evaluates to the unwrapped `T` if present —
the exact same `fails`-folding machinery a fifo occupancy check already
threads into a rule's guard:

```trace
module M {
    fifo input : [8]
    reg opt : ?[8] = false
    out result : [8] = 0

    rule fill {
        let d = input.Deq[]
        opt := d
    }

    rule pass {
        result := opt?   -- only fires (and only writes result) when opt is present
    }
}
```

A non-failing presence check needs no new syntax either — `.valid`/`.data`
read back directly, same as any other struct field:

```trace
if opt.valid {
    result := opt.data
} else {
    result := 0
}
```

`?T`'s guard placement follows the identical "whole statement, entire `:=`
right-hand side, or entire `let` init" restriction a fifo op/failing call
already has — `let x = opt?` folds its failure condition into the rule's
guard the same way `x := opt?` does; a guard nested any deeper (an
arithmetic operand, a call argument, an `if` condition — including chaining
straight into a field, `opt?.valid`) is a compile-time error rather than a
silently-unfolded guard, since `compile_guard`'s fold only looks in those
three positions. Unwrap-via-`?` and non-failing access via `.valid`/`.data`
are two distinct idioms, not composable into one chain — pick one per read.

v0 restrictions, matching structs': a `?T`-typed write's right-hand side must
be `false`, `optional <e>`, or a plain value of `T` — copying one `?T` value
into another (`p := q`, both `?T`, whether written directly or through
`optional q`) isn't supported yet, and neither is a plain LOCAL merely
aliasing another `?T`-typed value (`let o = opt; o.valid` — the exact
restriction a struct-typed local has, `let p = q; p.field`, whether the
alias is read via `.valid`/`.data` or through `?`'s guard fold); `.valid`/
`.data` are read-only; a `?T`-typed fn/rule PARAM is supported (an argument
may be `false`, a plain value of `T`, or a reg/another same-typed param,
resolved the same alias-chasing way a struct-typed param is — see "Calling a
function from a rule" below), and a `?T`-typed RETURN is too, the same way a
struct-typed one is — a `<fails>` callee's own guard fold (`callee_fail_
cond`) and its return value's per-leaf decomposition are two fully
independent passes over the same body, composing exactly the way a scalar
`<fails>` callee's guard and return value already do; a `?T`-typed port on
an instantiated submodule is rejected, same
reason and same fix a struct-typed port needs. `T` may
itself be a struct (`reg o : ?Pair`), or a struct's own field may itself be
`?T` — nested arbitrarily either way, reusing struct's own recursive
flattening; both a `?T`-typed `reg` and a `?T`-typed `out` write-thread
correctly (the two go through separate bookkeeping — `examples/option.tr`'s
`relayed` output exercises the `out` side directly). A `?T` guard folds
correctly from inside a callee body too (`callee_fail_cond`, calls.rs's own
cross-file guard-fold site — the same "is it present" `valid`-field read
`compile_guard`'s three in-rule fold sites use, not `opt`'s own nonexistent
flat name) — for a `reg`/`output`/`input` root; a local/param root's `.valid`/
`.data`/`?` all reduce to the aliasing restriction above instead.

`??T` (`T` itself an `Option`) compiles and simulates correctly, with its
outer and inner `valid` bits independently controllable via `optional`
(above): bare coercion (`8'd5`, `false`) still only ever reaches the two
fully-agreeing states, but `optional false`/`optional (optional 5'd3)`
reach all three well-formed ones, `Some(None)` included. `Ty::Optional`
(types.rs) is what makes this sound rather than a second `check_
assignable`-level copy path: `optional e`'s own type carries `e`'s
`ExprId`, not a precomputed `Ty`, so unification recurses through `check_
assignable` again against the TARGET's own inner on demand — the peel
happens exactly once per `optional`, however many layers the target
actually has, and `.data` stays read-only throughout (nothing new writes
through a `.data` path — `oo.data`'s own presence is set by a SEPARATE
`optional` one level up, not by writing `oo.data` directly).

### `if let`: branch-scoped Option-presence binding

`if let NAME = EXPR { then_body } [else { else_body }]` binds `NAME` to the
UNWRAPPED value of `EXPR` (which must itself be an Option's own `?`-unwrap,
`opt?`), visible ONLY within `then_body` — never `else_body`, never after the
whole statement. Verse's own inspiration is the general failure-context
binding form, `if (X := Expr, Y > 0):` (`08_failure`); this is the narrower
Option-only slice of it (Lumi's call, `AskUserQuestion`): a bare `opt?`
already reads cleanly as the right-hand side, and the general multi-clause
comma-chain form isn't needed for that.

```trace
rule r {
    if let x = opt? {
        result := x        -- x is opt's own unwrapped value here
    } else {
        result := 0
    }
}
```

Branch-scoped, same as the bare-comparison `if` feature above (Lumi's call,
applied uniformly again): `NAME`'s presence never gates the enclosing rule
the way a top-level `opt?` does — `then_body` runs when `opt` is present,
`else_body` (or nothing, with the ordinary "hold" fallback an unwritten
register path already has) otherwise, and the REST of the rule commits
either way. `while` isn't given an equivalent form; Verse's own construct is
`if`-shaped only.

v0 scope, narrower than the general binding form and deliberately so: `NAME`
may be used as a WHOLE value inside `then_body`, but not chased through a
further `.field` access (`x.field` when the unwrapped type is a struct) —
that would need new local-to-root chase-through machinery (`compile_struct_
field_read`'s existing chase-through is PARAM-only, see "Structs" above);
this cleanly rejects instead, the same "cannot find this local's binding"
message an ordinary `let p = opt?; p.field` already gets, not a new error
path. A struct-typed `NAME` used as a whole value (`p := x`) still hits the
separate, pre-existing "struct-typed write's RHS must be a literal"
restriction regardless — inherited for free, not a new gap.

Passing a struct/`?T`-typed `NAME` WHOLE as another function's argument
(`Get(x)`) is the one genuinely useful whole-value case for that type
family, and initially didn't work: `compile_struct_field_read`'s existing
PARAM chase-through (a callee param bound to another struct/Option value
via a plain `Ident`, "Structs" above) recurses with the chased-to value as
the new root, and once that root is `NAME` itself, resolution hit the exact
same "cannot find this local's binding" rejection a direct `x.field` gets —
`if_let_binds` was never consulted along that path, only by `compile_expr_
hinted`'s own `Ident` case. Fixed narrowly, inside the PARAM chase-through's
existing `is_param`-gated branch (`compile_struct_field_read`, expr.rs):
when the chased-to value is itself an `if_let_binds` entry, splice `"data"`
plus the remaining field path onto `opt`'s own root and resolve from there,
instead of recursing with `NAME` as root. Gating this inside the `is_param`
branch (rather than at the top of the function, which was the first attempt
and over-broadened — it made `if_let_bound_structs_own_fields_are_not_
chased_through` start passing where it should fail) is what keeps a direct
`x.field` access rejected: that has `NAME` as `root` itself, a Local rather
than a Param, so `is_param` is false and the branch never runs. Confirmed
both directions by direct probe through real FIRRTL (firtool-checked): a
single-hop struct argument, and a two-level nested-field one (`?Outer`
containing an `Inner` struct field) to pin the path-splice direction —
prepend `"data"` ahead of the remaining path, not replace it, else nested
fields collapse to the wrong flat register name. `a_callee_local_aliasing_
a_struct_typed_param_is_rejected` and `option_typed_local_aliasing_another_
option_value_is_rejected` (the two pre-existing regression tests guarding
against over-broadening this exact chase-through) still pass unchanged.
`?.` safe
navigation (Verse's own multi-hop `opt?.field?.next`, each hop independently
unwrap-or-fail) is a related but separate, larger feature, not attempted
this pass — see TODO.md. `if let` inside a `<sequences>`/spawn-callee body
(crossing a `tick`) isn't supported either: `NAME` deliberately isn't
registered in `lower.rs`'s local-crosses-a-tick capture machinery (that
machinery rewrites `let NAME = init` into `NAME := init` verbatim for a
synthesized register, and there's no equivalent rewrite for the surrounding
`if`/branch structure an `if let` needs to keep), and the existing "a spawned
fn's last segment must end with `return`" check doesn't recognize `Stmt::
IfLet` as a valid segment-ending shape — cleanly rejected, not silently
miscompiled, confirmed by direct probe.

Implementation-wise, `if let` reuses almost every mechanism the earlier
features on this page already built rather than adding new ones: the mux
select is `opt`'s own `.valid`, computed via `compile_guard_unwrap_cond`
(the same function the bare-comparison `if` predicate and the ordinary
guard-fold both already call); the whole-rule guard fold
(`compile_guard`) never looks inside `Stmt::If`/`Stmt::IfLet` at all, so
branch-scoping needed no new code there either. The one genuinely new piece
is resolving `NAME` itself to `opt.data`: a dedicated `if_let_binds:
HashMap<DefId, ExprId>` on the emitter (mod.rs), populated with `opt`'s own
`ExprId` right before compiling `then_body` and removed right after (or, in
a CALLEE body — reentrant via nested inlining, `Avg(Avg(x, y), z)`-style —
saved and restored instead of a plain remove), consulted by `compile_expr_
hinted`'s `Ident` case before it falls back to `locals_snapshots`/`locals`.
Reading `NAME` then routes through `struct_field_path`/`compile_struct_
field_read` with `"data"` appended to `opt`'s own path — the exact same
per-field resolution an explicit `.data` read already uses, including
`opt` itself being a chained field (`frame.maybe?`) or a callee PARAM
(chasing through to the caller's own argument, confirmed working through a
real `<combines>` callee taking a `?T` param).

Every write-threading walk that can reach a branch needed its own `Stmt::
IfLet` arm — mirroring its existing `Stmt::If` arm exactly, with `if_let_
binds` inserted/removed around the `then_body` recursion: `reg_value_in_
stmts`, `mem_write_in_stmts`, `struct_field_value_in_stmts`, `inst_port_
value_in_stmts` (writes.rs), `callee_reg_write`/`callee_port_write`
(writes.rs, save/restore), and `compile_callee_body`/`compile_callee_body_
field` (calls.rs, save/restore). Confirmed correct by direct probe through
real FIRRTL (firtool-checked): `reg_value_in_stmts`, `mem_write_in_stmts`,
`inst_port_value_in_stmts`, `struct_field_value_in_stmts`,
`callee_reg_write`, `callee_port_write`, and `compile_callee_body`.
`compile_callee_body_field`'s arm is verified only by mirroring its `Stmt::
If` sibling exactly — every body shape tried for it either bottoms out at
the same "no `.field` chase-through"/"local, not param, so no passthrough"
restrictions the paragraph below covers (correctly rejected, not a gap) or
requires the whole-value call-argument path below, which doesn't happen to
route through this particular function in the shapes tried. Three more
non-exhaustive
helpers were self-caught missing a `Stmt::IfLet` arm the same way (silent,
not a compile error, since Rust's exhaustiveness checking only catches a
missing arm in an EXHAUSTIVE match, and these three use a `_ => None`/`_ =>
{}` fallback instead): `find_mem_write` (writes.rs) — a memory write buried
inside an `if let` was invisible to the "does this rule write this mem at
all" gate, so the mem never even got a writer port declared, silently
dropping the write entirely, guard and all; `collect_read_sites`
(module.rs) — the analogous gap for a mem READ address referencing an
`if let`; and `stmt_contains` (writes.rs, used by `set_pos`) — a
REASSIGNED local referenced inside an `if let`'s own body would have
silently resolved to the rule's FINAL snapshot instead of the one in scope
at its actual position, the identical reassigned-locals miscompile class
`examples/reassigned_local.tr` exists to guard against. All three fixed and
pinned with regression tests (tests/firrtl.rs) before this was considered
done.

### `if let`: a fifo op's presence

`if let x = fifo.Deq[] { then_body } [else { else_body }]` extends the
Option-presence feature above to a fifo's own occupancy — the "harder half"
TODO.md's four open questions left standing (a fifo op as a branch-scoped
`if`'s condition, not just an Option's). Bare, never `?`-wrapped: unlike an
Option (which needs an explicit unwrap to become fallible), `Deq[]` is
already fallible by default, the same as a comparison — `if let x = f.Deq[]?`
also parses (an explicit, redundant `?` on an already-fallible expression,
same as `(a > b)?`) but the idiomatic spelling mirrors every other bare
`Deq[]` use.

```trace
rule consumer {
    if let x = f.Deq[] {
        result := x         -- a real dequeue happened this cycle
    } else {
        result := 0         -- f was empty; nothing dequeued
    }
}
```

Same branch-scoped semantics as the Option case: `consumer` fires every
cycle regardless of `f`'s occupancy (`if let` never gates the enclosing
rule), but the dequeue itself is real and conditional — it only actually
fires on a cycle where `f` has data. Verified through real firtool +
Icarus AND Verilator simulation, not just structurally: `examples/
if_let_fifo.tr` + `sim/if_let_fifo_tb.v` push one value and confirm
`result`/`was_present` show it exactly one cycle later, absent on every
other cycle (`tests/sim.rs`'s `if_let_fifo_deq_drives_a_real_dequeue_
exactly_when_present`).

**v0 scope, deliberately narrower than the full four-open-questions
design:**
- **`Deq` only, not `Enq`.** There's no value to bind `x` to for an
  enqueue, and enqueuing is never itself "does data exist to read" —
  the whole reason this feature exists.
- **A rule's own top-level `if let` only** — not nested inside another
  `if`/`while`, and no SECOND fifo op/guard/comparison inside `then_body`/
  `else_body` (the pre-existing "nested in if/while" v0 restriction,
  `checks.rs`, unaffected by this feature: it already exempts an `if
  let`'s own `init` from that check, but still applies to anything
  found deeper inside its branches).
- **A failing call (`if let x = Classify(a) { ... }`) is explicitly
  OUT of scope, not just untested** — `types.rs`'s `TypeChecker` has no
  access to `fx`'s computed `fails` signatures (only `resolve.rs`'s def
  kinds), so recognizing "this call can fail" would need threading `fx`
  through a checker that currently doesn't carry it. `checks.rs`'s
  `calls_outside_allowed_positions` also still runs with `allow_let:
  false` for `check_failing_call_positions`, unlike the fifo case where
  `fifo_ops_outside_allowed_positions`'s `IfLet`/`WhileLet` arms were
  ALREADY unconditionally permissive — dead code until this feature,
  confirmed by direct probe (`f.Deq[]?` hit exactly the anticipated "v0
  restriction: a fifo op, failing call, or comparison isn't supported
  here yet" message in `types.rs` before this landed) rather than
  assumed. A failing call as `if let`'s rhs still hits the same message
  a bare comparison does — an explicit rejection, not silent.
- **`while let`, and a `while`'s own condition, stay untouched** —
  Verse's own construct is `if`-shaped only (TODO.md), and nothing here
  argues for extending the loop forms too.

**Implementation, reusing existing mechanisms almost entirely, no new
`Stmt` variant or position-check surface:**
- `types.rs`'s `Stmt::IfLet` arm gained one new `else if` branch (`is_
  fifo_deq`, checking the raw `Expr::Bracket` shape directly — not
  `Expr::Guard`, since this is never `?`-wrapped) alongside the existing
  Option-`Guard` check; `type_expr`'s existing `type_bracket` dispatch
  already returns a `Deq[]`'s own element type, reused as-is.
- `effects.rs`'s `Stmt::IfLet` arm gained a matching branch: reads+writes
  the fifo (a real dequeue is a real effect) but deliberately does NOT
  set `sig.fails` — mirrors the Option-Guard branch's own reasoning
  (this presence check never gates the enclosing rule/fn) rather than
  falling through to the generic `Expr::Bracket` arm, which DOES set
  `fails` for an ordinary, unconditional top-level `Deq[]`.
- `checks.rs` needed NO changes: `fifo_ops_outside_allowed_positions`
  already listed `Stmt::IfLet { init, .. } if is_fifo_op(init) =>
  Some(init)` as an allowed position, and `contains_fifo_op`/`contains_
  guard` already exempt `IfLet`'s own `init` from the "nested in if/
  while" scan — forward groundwork from whenever those functions were
  last touched, dead code until this feature made it reachable.
- `writes.rs`'s `compile_guard_unwrap_cond` (the SAME function the
  Option-presence mux-select and the bare-comparison `if` predicate both
  already call) gained one new branch: when `inner` is a Deq, return
  `fifo_guard_cond` — the fifo's own occupancy test, reused verbatim
  from the pass-through/`or`-alternative machinery rather than computed
  fresh.
- `fifo.rs`'s `rule_fifo_ops` gained a new case recognizing a top-level
  `Stmt::IfLet` whose `init` is a Deq, pushing a `RuleFifoOp` with
  `select: Some(fifo_guard_cond(...))` — the IDENTICAL field `or`-chain
  alternatives already use for a conditionally-gated dequeue.
  `compile_guard`'s per-fifo whole-rule fold already skips any op with
  `select.is_some()` (pre-existing, built for `or`), so this presence
  check correctly contributes NOTHING to the rule's own guard for free.
  `module.rs`'s depth-1 Deq emission already branches on `.select`
  (`when sel: connect valid, 0` vs. unconditional) — also pre-existing,
  needed zero changes.
- The 8 near-identical `let Expr::Guard(opt) = self.ast.expr(init).clone()
  else { unreachable!(...) }` sites across writes.rs/calls.rs (the
  `if_let_binds` value-threading machinery the Option feature's own
  write-up above calls "roughly a dozen call sites") became a 3-way
  match: `Expr::Guard(inner) => inner`, `_ if self.fifo_op(init).is_some()
  => init`, else unreachable. `if_let_binds: HashMap<DefId, ExprId>`'s
  value semantics generalize cleanly to "the expr `x` binds to" either
  way, so every downstream `if_let_binds.insert(def, opt)` call needed no
  changes at all.
- `expr.rs`'s `Ident` read-substitution (the ONE genuinely new piece):
  when `if_let_binds.get(&def)` resolves to a Deq bracket instead of an
  Option-inner expr, recurse through `compile_expr_hinted` instead of the
  hardcoded `.data` field chase — `compile_expr_hinted`'s own top-of-
  function fifo-op check (`fifo_deq_read_expr`) already compiles any Deq
  bracket to its data-read expression generically, the exact path an
  ordinary top-level `x := f.Deq[]` already goes through.

Passing an `if let`-bound Deq value WHOLE as another call's argument
(`Get(x)`, the struct/Option `if let`'s own documented whole-value case
above) is NOT extended to this feature — `expr.rs`'s PARAM chase-through
site for that case is gated on `root_ty` matching a struct/Option shape,
which a scalar `[N]`-typed Deq value never satisfies; untested and
out of scope, not confirmed broken, since fifos overwhelmingly hold
scalar element types in every example this codebase has.

### `if let`: a failing call's own presence

`if let x = Classify(a) { then_body } [else { else_body }]` extends the
same feature to a failing call — the other v0-restricted shape the fifo
section above named as still out of scope when it landed. Bare, never
`?`-wrapped, same reasoning as `Deq[]`: a failing call is already
fallible by default.

```trace
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}

rule compute {
    if let x = Classify(a) {
        result := x          -- Classify's own guard held this cycle
    } else {
        result := 0          -- Classify would have failed
    }
}
```

Same branch-scoped semantics: `compute` fires every cycle regardless of
whether `Classify` succeeds, but `x`'s value and the mux-select both
track `Classify`'s own guard exactly. Verified through real firtool +
Icarus AND Verilator simulation (`examples/if_let_failing_call.tr` +
`sim/if_let_failing_call_tb.v`, `tests/sim.rs`'s `if_let_failing_call_
tracks_the_callees_own_guard`), sweeping `a` through both an absent
(`0`, `Classify` fails) and present (nonzero) case.

**What unblocked this, closing the gap the fifo section above left
open:** `TypeChecker` (types.rs) had no access to `fx`'s computed
`fails` signatures at all — only `resolve.rs`'s def kinds, which don't
carry it. `types::check`'s own signature gained a third parameter,
`fx: &Effects`, threaded through every call site (`main.rs`, `lsp.rs`,
and every test harness that calls it — all of which already had `fx`
in scope from their own preceding `effects::check` call, confirmed by
checking each site rather than assuming). `Stmt::IfLet`'s arm gained a
new `is_failing_call` check (mirroring `checks.rs`'s identical method on
`Emitter`, duplicated rather than shared since the two types can't see
each other) alongside `is_fifo_deq`; `type_expr`'s existing `Expr::Call`
dispatch (`type_call`) already returns the callee's own return type,
reused as-is.

**v0 scope, same shape of restriction as the fifo case:**
- A rule's own top-level `if let` only, no nesting, no second guard/
  fifo op/call inside `then_body`/`else_body` — the pre-existing
  restriction, unaffected.
- A callee that BOTH fails AND writes state, used as `if let`'s init,
  is explicitly rejected — but by a restriction that already existed
  for an unrelated reason, not new logic this feature had to add:
  `check_writing_call_positions_in`'s own "a call to a function that
  writes state may only appear as a whole statement, or as the entire
  right-hand side of `:=`" still applies unchanged (`if let`'s init is
  neither), confirmed by direct probe rather than assumed — a real risk
  named explicitly before writing any code, since `if let`'s mux-select
  has no branch-gating story for a WRITE the way it does for `x`'s own
  value. `tests/firrtl.rs`'s `if_let_with_a_writing_and_failing_call_
  still_hits_the_write_position_restriction` pins this.
- `while let`/a bare `while`'s own condition stay untouched, same
  reasoning as the fifo case (Verse's construct is `if`-shaped only).

**Implementation, reusing `calls.rs`'s existing failing-call-inlining
machinery almost entirely:**
- `effects.rs`'s `Stmt::IfLet` arm gained a matching branch: merges
  reads/writes from the callee's own `EffectSig` but deliberately does
  NOT set `sig.fails` — same reasoning as the fifo-Deq branch beside it.
- `checks.rs` needed two small, deliberately narrow changes (unlike the
  fifo case, which needed none — this shape wasn't pre-exempted):
  `contains_failing_call` gained an `IfLet`/`WhileLet`-specific
  exemption for their own `init` (mirroring `contains_guard`/`contains_
  fifo_op`'s identical pattern, which already had it); `calls_outside_
  allowed_positions` gained a NEW, separate `allow_if_let` parameter —
  deliberately NOT reusing the existing `allow_let` (which also gates
  `Stmt::Let`/`Stmt::WhileLet`) — threaded `true` only through `check_
  failing_call_positions`'s own call site, `false` everywhere else
  including `check_writing_call_positions_in`'s. This separation is
  exactly what makes the writing+failing rejection above still fire:
  loosening the SAME position for both checks would have let an
  ungated write slip through undetected instead.
- `writes.rs`'s `compile_guard_unwrap_cond` gained a matching `Expr::
  Call` branch, reusing `calls.rs`'s `callee_fail_cond` — self-caught a
  real sign bug while verifying against real FIRRTL, not just adding
  the branch and trusting it: despite its name, `callee_fail_cond`
  already returns the callee's own SUCCESS condition (the same sign
  every other `compile_guard` fold term uses, confirmed by reading its
  other call site inside `compile_guard` itself, where the result is
  ANDed directly into the rule's "can fire" guard). An earlier version
  wrapped it in `not(...)`, inverting the mux — caught by running the
  probe's actual emitted FIRRTL (`mux(not(neq(a, 0)), a, 0)`, backwards)
  before writing any test, not by the type checker or by inspection.
- `expr.rs`'s `Ident` read-substitution widened its existing fifo-op
  check to also match `Expr::Call`, delegating to `compile_expr_hinted`'s
  already-generic `Expr::Call` dispatch (`compile_call`, calls.rs) — the
  exact path an ordinary top-level `x := Classify(a)` already goes
  through, so no new value-compilation code was needed here either.
- The 8 `if_let_binds` sites' 3-way match (writes.rs/calls.rs, from the
  fifo feature above) gained one more arm: `_ if ... || matches!(Expr::
  Call { .. }) => init`.

### `if`: a fifo op's own bare condition

`if f.Deq[] { then_body } [else { else_body }]` — the ORIGINAL literal ask
behind this whole run of features (TODO.md's four open questions: "a fifo
op or failing call as a branch-scoped `if`'s condition"), finally built
directly rather than only through `if let`'s bound-name form. Bare, no
`let`, no `x` to read the dequeued value — for when only "did this
happen" matters, not the value itself; `if let x = f.Deq[] { ... }`
(above) is strictly more general (nothing stops leaving `x` unused), so
this is syntactic convenience, not new expressive power.

```trace
rule consumer {
    if f.Deq[] {
        count := count + 1   -- a dequeue happened; the value itself is unused
    } else {
        count := count
    }
}
```

**The no-else/with-else split that governs the bare-comparison `if`
feature does NOT apply here — confirmed by direct probe, not assumed,
before writing the `rule_fifo_ops` case below.** For a bare comparison,
a no-else `if` gates the whole rule (`comparison_conds`'s own position-
blind scan finds it regardless of where it sits) while a with-else `if`
doesn't. For a fifo op, NEITHER shape gates the rule: `compile_guard`'s
whole-rule fold never looks inside `Stmt::If` at all (writes.rs), with
or without an `else` — the asymmetry for comparisons is `comparison_
conds`'s own artifact, and fifo ops have no equivalent blanket scan. So
this feature uses ONE policy unconditionally: `select: Some(fifo_guard_
cond(...))`, the identical mechanism `if let`'s own fifo-Deq case uses,
regardless of whether `else` is present. A no-else bare `if f.Deq[]` is
therefore branch-scoped, same as with an `else` — a genuine, documented
semantic difference from the comparison case, not an oversight.

**Implementation — almost entirely reused from `if let`'s fifo-Deq
feature above, with zero new value-compilation code:** `Stmt::If`'s
own mux-select machinery already called `compile_guard_unwrap_cond
(cond)` for the bare-comparison feature — that function's existing
fifo-Deq branch (built for `if let`) already handles a `Stmt::If`
condition identically, no changes needed there at all. The genuinely
new pieces:
- `types.rs`'s `check_cond` gained one more `if`-only exemption
  (`allow_bare_comparison`-gated, same as the comparison case):
  `is_fifo_deq(cond) || is_failing_call(cond)`.
- `checks.rs`'s `fifo_ops_outside_allowed_positions` gained a `Stmt::If
  { cond, .. } if is_fifo_op(cond) => Some(cond)` arm — genuinely NEW,
  unlike `if let`'s equivalent (which turned out to already be dead-code
  groundwork); a bare `if`'s own condition was never a pre-anticipated
  position here.
- `fifo.rs`'s `rule_fifo_ops` gained a matching `Stmt::If` case, pushing
  a `RuleFifoOp` with `select: Some(fifo_guard_cond(...))` — structurally
  identical to `if let`'s own case just above it.
- `checks.rs`'s fifo/call collision check (formerly `or`-alternative-
  specific wording, in `check_fifo_op_counts`) was generalized to name
  all three conditional-`select` producers (`or`, `if let`'s presence
  check, a bare `if`'s own condition) rather than just `or`, since a bare
  `if`-condition Deq is now a third way to reach the exact same
  conditional/unconditional collision it already caught.

Verified through real firtool + Icarus AND Verilator simulation, not
just structurally: `examples/if_bare_fifo.tr` + `sim/if_bare_fifo_tb.v`
(`tests/sim.rs`'s `if_bare_fifo_deq_condition_drives_a_real_dequeue_
exactly_when_present`), the un-bound twin of `if_let_fifo.tr` — same
push-then-check-one-cycle-later shape, `was_present` (not a dequeued
value, since there's no name to bind) tracking the fifo's own occupancy
exactly.

### `if`: a failing call's own bare condition

`if Classify(a) { then_body } [else { else_body }]` — the failing-call
half of the same original ask, built the identical way: bare, no `let`,
for when only success/failure matters, not the callee's return value.
Same "no-else/with-else split doesn't apply" finding as the fifo case
(confirmed the same way, by direct probe before writing any `select`-
threading code): `compile_guard_unwrap_cond`'s Call branch (built for
`if let`'s failing-call feature above) already handles a `Stmt::If`
condition identically, no new value-compilation code needed.

```trace
rule compute {
    if Classify(a) {
        count := count + 1   -- Classify succeeded; its return value is unused
    } else {
        count := count
    }
}
```

**Implementation, even smaller than the fifo case since a failing call
has no `fifo.rs`-style state-transition to additionally wire:**
- `effects.rs`'s `Stmt::If` condition-inference arm gained a matching
  `Expr::Call` branch beside its existing comparison one: merges
  reads/writes from the callee's own `EffectSig`, doesn't set `sig.
  fails` — mirrors `Stmt::IfLet`'s identical branch.
- `checks.rs`'s `calls_outside_allowed_positions` gained a `Stmt::If
  { cond, .. } if allow_if_let && matches!(Expr::Call) => Some(cond)`
  arm, reusing the SAME `allow_if_let` flag `if let`'s failing-call
  feature introduced (both flow through `check_failing_call_positions`
  only, `check_writing_call_positions_in` unaffected — the writing-and-
  failing-callee rejection this flag was built to preserve holds for
  the bare-`if` shape too, confirmed by direct probe the same way).
- `checks.rs`'s `contains_failing_call` gained a `Stmt::If` arm to its
  existing init-exemption (previously `IfLet`/`WhileLet` only), sharing
  the match arm with `IfLet` since both only need `then_body`/
  `else_body` scanned, not their own condition/init.

Verified through real firtool + Icarus AND Verilator simulation:
`examples/if_bare_failing_call.tr` + `sim/if_bare_failing_call_tb.v`
(`tests/sim.rs`'s `if_bare_failing_call_condition_tracks_the_callees_
own_guard`), the un-bound twin of `if_let_failing_call.tr`.

### Branch-mutual-exclusivity: two `Deq[]`s on the same fifo, one per branch

TODO.md's "four open questions" left one deliberately unanswered even
after the bare-`if` feature above landed: `check_fifo_op_counts` rejects
a SECOND `Deq[]` on the same fifo per rule, unconditionally, today — but
two `Deq[]`s in mutually exclusive branches (one in `then`, one in
`else` of the SAME `if`) are provably not a double-dequeue. Lumi's call
on revisiting it: "we need some static analysis to prove it, but I do
want to allow branching on it" — so this became real, but scoped to the
NARROWEST provable case, not a general dataflow analysis over arbitrary
branch structure.

```trace
rule consumer {
    if route = 1 {
        out_a := f.Deq[]
    } else {
        out_b := f.Deq[]
    }
}
```

Both dequeues are real (either branch clears the fifo's own occupancy),
and — same branch-scoped convention every other fallible-condition
feature above establishes — neither gates the rule; only the taken
branch's own write happens, the other output holds its prior value via
the pre-existing if/else register-write mux (`reg_value_in_stmts`,
unchanged).

**Two prerequisite pieces, landed in order, each independently useful.**

**1. A fifo `Deq[]` may now sit directly as an ordinary statement inside
an `if`'s `then_body`/`else_body` at all** — previously ANY fifo op
nested anywhere inside an `if`/`while` was rejected outright
(`contains_fifo_op`'s blanket recursive scan, `check_guard_placement`),
a broader restriction than the same-fifo double-touch check. `fifo.rs`'s
`rule_fifo_ops` gained a new case, structurally next to its `if let`/
bare-`if`-condition cases: for an `if` whose own `cond` is NOT itself a
fifo op or failing call (see the composition boundary below), walk
`then_body`/`else_body` for a bare Deq (`fifo_op_stmt`'s recognized
shapes only — a whole statement, `:=` RHS, or `let` init; ANYTHING
deeper, a fifo op nested inside a FURTHER if/while within the branch, or
buried in a larger expression, still isn't supported and falls through
to the ordinary `contains_fifo_op` rejection). Enq stays excluded — no
`select`-gating machinery exists for it yet, same reason `if let`'s fifo
case excluded it (there's no bound name for a value with no reader to
matter for, but more fundamentally here: extending it is real future
work, not free).

This piece alone is useful with no second op at all — a Deq
conditionally gated on some UNRELATED condition:

```trace
rule consumer {
    if keep = 1 {
        out_v := f.Deq[]
    }
}
```

**The `select` field couldn't stay a pre-built string.** Every earlier
`RuleFifoOp::select` producer (`or`, `if let`'s presence check, bare
`if`'s own fifo/call condition) builds its condition from `fifo_guard_
cond` alone — pure string formatting, no expression compilation needed.
This case's condition is the enclosing `if`'s arbitrary `cond` expression
(a comparison, a plain boolean register, anything `compile_guard_unwrap_
cond` already handles) — compiling it for real needs `enter_rule`/
`set_pos` context already established for the CORRECT rule and
statement position, which `rule_fifo_ops` doesn't have (it's called from
`checks.rs` too, before that context exists for the rule being checked).
So `RuleFifoOp::select` became an enum:

```rust
enum FifoSelect {
    Cond(String),           // pre-built, the three earlier producers
    Branch(ExprId, bool),   // the enclosing if's own `cond`, is_then
}
```

`Branch`'s `ExprId` is compiled lazily, at the actual module.rs emission
site — the ONE place in this whole feature that already has correct
`enter_rule`/`set_pos` context — via `compile_guard_unwrap_cond`, negated
with `not(...)` for an `else`-branch op. Every consumer that only ever
checked `select.is_some()` (`compile_guard`'s whole-rule fold,
`check_fifo_op_counts`'s conditional/unconditional mix check) needed no
changes at all; only the one site that reads the STRING (module.rs's
depth-1 Deq emission) needed to grow a match over the two variants.

v0-restricted to **depth-1 fifos only** — a new `check_branch_fifo_op_
depth`, mirroring `check_or_shape`'s identical depth restriction on `or`
alternatives, for the identical reason: `emit_fifo_depth_n` (module.rs)
has no `select`-gating logic in its head/count update path to extend,
only the depth-1 path does.

**2. `check_fifo_op_counts` gained `mutually_exclusive_branch_pair`** —
the actual collision relaxation. Exactly two ops on the same fifo, both
`FifoSelect::Branch`-selected off the IDENTICAL enclosing `if`, with
opposite `is_then`, are allowed; anything else (three or more touches,
an unconditional touch mixed in with a conditional one, two DIFFERENT
`if`s even with textually identical conditions) still collides exactly
as before. `ExprId` equality is sufficient to prove "the identical `if`
statement": two lexically distinct `if`s always get distinct `cond`
`ExprId`s in this AST (a tree, never hash-consed/interned), so comparing
the stored `ExprId`s IS comparing "same `if`-statement identity," no
separate `StmtId` needed. This is the narrowest, purely syntactic
mutual-exclusivity proof — no general dataflow/SMT-style reasoning about
arbitrary branch trees, deliberately: proving exclusivity across a
longer if/else-if chain, across two unrelated `if`s, or through nesting
deeper than one level is still out of scope, unattempted.

**A real latent bug this surfaced, self-caught before it ever shipped:**
`module.rs`'s per-fifo touch-collection loop tracked `enq`/`deq` as
`Option<RuleFifoOp>` per rule — a second touch silently OVERWROTE the
first. Before this feature, that was always safe (`check_fifo_op_counts`
never let two Deqs on the same fifo coexist in the first place), but the
whole point of `mutually_exclusive_branch_pair` is to let exactly that
happen — collapsing two mutually-exclusive Deqs into one `Option` would
have silently dropped one's state transition with no error, exactly the
class of bug this emitter's checks exist to close off elsewhere. Fixed
by widening the Deq slot to `Vec<RuleFifoOp>` (the Enq slot and the
depth>1 emission path stay `Option`-shaped — both are still provably
≤1-per-rule-per-fifo by construction, so the depth>1 path's own
signature needed no changes, just a lossless `Vec` → `Option` conversion
at its call site).

**The composition boundary, flagged by `advisor`'s second-opinion review
before implementation and built in from the start, not discovered
later:** a nested op is rejected outright when the enclosing `if`'s own
condition is ITSELF a fifo op or failing call (`f.Deq[]`'s own bare-`if`-
condition feature, above) — composing "did the branch fire" with "was
ITS OWN condition's dequeue/call also successful" hasn't been reasoned
about, the same v0 boundary `check_fifo_op_counts`'s pre-existing
conditional/unconditional mix rejection already applies elsewhere.
`rule_fifo_ops`'s branch-nested case explicitly excludes this shape
(`self.fifo_op(cond).is_none() && !matches!(Expr::Call)`), and `checks.
rs`'s matching exemption (`contains_disallowed_branch_fifo_op`) uses the
IDENTICAL eligibility test — deliberately, so a statement the placement
check lets through is GUARANTEED to actually get a state transition from
`rule_fifo_ops`, never silently vanish from emission (exactly the
failure mode `rule_fifo_ops`'s own doc comment warns every fifo-touch
question in this file to avoid).

Hand-verified end to end against real firtool + Icarus simulation before
any test was written (the discipline every feature in this run follows):
first the single-branch-nested-Deq case alone (a parked value that
stays put across cycles until the gating branch is actually taken, not
dropped or double-read), then the two-mutually-exclusive-Deqs case
(pushing two values with different routing, confirming the un-taken
output genuinely holds its prior value rather than glitching). See
`examples/branch_fifo_deq.tr` + `sim/branch_fifo_deq_tb.v`
(`tests/sim.rs`'s `two_deqs_on_the_same_fifo_in_then_and_else_are_
really_mutually_exclusive`, plus its Verilator twin) for the proven
example, and `tests/firrtl.rs` for the full structural coverage of both
the newly-allowed shapes and the still-rejected boundary cases (Enq
nested in a branch, two Deqs in the SAME branch, depth>1, two levels of
nesting, and the composition-boundary case above).

### `?.` safe navigation

`opt?.field?.next` — Verse's own multi-hop chained unwrap-and-field-access
(TODO.md's "`?.` safe navigation"): each `?` independently unwraps-or-fails
one Option layer, and `.field` reads the unwrapped struct's own field,
chaining through however many `?T` layers the whole expression needs.
There's no dedicated `?.` token — `?` and `.field` are both ordinary
postfix operators, chained the same way any two postfix operators compose
(`a?.b?.c` parses as `Field{base: Guard(Field{base: Guard(a), name: "b"}),
name: "c"}`, two `Guard` nodes at different depths, no new parser surface
at all).

```trace
struct Inner {
    c : [8]
}
struct Outer {
    b : ?Inner
}
module M {
    reg a : ?Outer = false
    out result : [8] = 0
    rule r {
        result := a?.b?.c   -- fires only when BOTH a and a.b are present
    }
}
```

Usable anywhere a bare `opt?` already is — a whole bare statement, the
entire right-hand side of `:=`, or the entire init of a `let` — with every
`?` along the chain folding its own condition into the rule's guard, ANDed
together (`and(a_data_b_valid, a_valid)` above, not just the innermost
hop's). A `?.` chain reached through anything else — nested in arithmetic,
a call argument, an `if` condition — is still a compile-time error, the
identical v0 restriction a single un-chained `opt?` already has.

**Implementation is almost entirely reuse, not new machinery.** `opt?.field`
already parsed and type-checked correctly before this feature — `Expr::
Guard`'s type-check already peels `?T` to `T` generically, and `Expr::
Field`'s already looks up a field on whatever type its base produced, so a
chain type-checks by the SAME two rules applied repeatedly, no new type-
level code at all. The only real gaps were structural (which positions
allow a `Guard`) and emission (how to find the physical flattened register a
chain's root and full field path resolve to):

- `guard_chain_spine` (lower.rs), shared by both remaining gaps: every
  `Expr::Guard` reachable from a root by descending ONLY through `Expr::
  Guard`/`Expr::Field` — the chain's own spine. A `Guard` reached by
  descending into anything else isn't part of it.
- `checks.rs`'s `guards_outside_allowed_positions` now computes each
  statement's allowed set as this WHOLE spine (not just a single exact-
  match `Expr::Guard`), so `a?.b?.c` is legal at the same three positions
  a bare `opt?` already was, while a guard reached any other way still
  gets rejected with the identical message.
- `writes.rs`'s `compile_guard` (the rule's own guard-fold) gained
  `guard_chain_conds`, folding every hop's condition on the spine — proven
  to matter, not just theoretically: `and(a_data_b_valid, a_valid)`, not
  a single term, is what actually fires `rule r` above; if only `b`'s own
  presence folded, `a` absent with a stale/undefined `b` would read
  garbage as if it were present.
- `struct_field_path` (expr.rs) gained one new arm: a `Guard` on the
  spine just pushes `"data"` onto the accumulated path and recurses into
  its own inner, the same "unwrapping doesn't move data around" rule
  `compile_expr_hinted`'s existing un-chained Guard-value arm already
  used for the single-hop case — generalized so it composes at any depth.
  `compile_expr_hinted`'s own `Field`/`Guard` value-read arms needed ZERO
  changes: both already delegate to `struct_field_path`, so they pick up
  chain support automatically.

**`if let`/`while let` deliberately do NOT get multi-hop chaining — a
scope boundary, not an oversight.** Every one of their own mux-select call
sites (writes.rs/calls.rs, roughly a dozen: `reg_value_in_stmts`, `mem_
write_in_stmts`, `struct_field_value_in_stmts`, `inst_port_value_in_stmts`,
`callee_reg_write`/`callee_port_write`, `compile_callee_body`/`compile_
callee_body_field`) extracts `init`'s own IMMEDIATE `inner` and folds only
that one hop's condition (`compile_guard_unwrap_cond(opt)`) — never the
whole spine. `if let x = a?.b?` would silently read `a.data.b.valid`
without also gating on `a.valid`'s own presence: wrong hardware that still
passes a structural FIRRTL check, not caught by anything short of running
it. Fixing every one of those call sites is real, separate work this pass
didn't attempt — `guards_outside_allowed_positions` keeps `IfLet`/`WhileLet`
restricted to a single bare `Expr::Guard`, same as before this feature,
rejecting a chained init with the same message a misplaced guard gets.

**Proven end to end, not just structurally:** `examples/optional_chain.tr`
+ `sim/optional_chain_tb.v` (`tests/sim.rs`'s `optional_chain_sugar_runs_
through_a_genuinely_absent_intermediate_hop`) drives the one state that
actually discriminates a correct multi-hop fold from a naive one — `a`
present, the intermediate `b` absent — through real firtool + Icarus, not
just checking the emitted FIRRTL text contains the right `and(...)`.

## Locals

`let` is the ONLY way to declare a fresh local. `x := value` never declares —
it is always either a write to an EXISTING definition (a reg/output/fifo/mem/
inst port, or a `let`-bound local being reassigned), or, if `x` doesn't
resolve to anything at all, a compile-time "cannot find" error rather than a
silent fresh declaration. Splitting "define" (`let`) from "mutate" (`:=`)
this way is a deliberate Verse-alignment choice (Verse itself keeps `x := e`
a pure definition and requires `set x = e` for mutation) — it's what makes
`pc := c` unambiguously a register write and `let a = m[pc]` unambiguously a
local binding, instead of the two being distinguished only by whichever name
happened to already be in scope.

A local may be reassigned (via `:=`, after its initial `let`) within one
rule. A later read sees whichever binding was active at that read's own
position in the source, not the local's final value:

```trace
rule r {
    let x = a
    first_val := x    -- sees a
    x := b
    second_val := x   -- sees b
}
```

A local whose width never resolves to a concrete `[w]` anywhere in the rule
(for example, one used only as a memory-read index) still rejects reassignment.

Inside a `sequences` body, a local can cross a `tick` regardless of whether it
was first bound with `let` or later reassigned with `:=`; see "`sequences`:
multi-cycle code" above.

Inside a fn/impl body specifically (not a rule body), a `let`-bound local is a
compile-time error if nothing ever reads it back anywhere in that same body:
a fn/impl's only observable outputs are its return value and its state
writes, and a local — unlike state — is invisible outside its own body, so a
never-read one is dead by construction. The check is deliberately narrow, not
a general "unused local" lint: a rule-level scratch local (the ordinary
reassignment pattern above) is legitimate and stays completely unchecked, and
so does any local that's read at least once. Since `x := value` can no
longer declare a local at all, the classic mistake this check used to catch
— `log := d` inside a fn meaning to reach some MODULE's `log` reg, when that
fn has no lexical access to it at all (modules share no state with each
other) — is now caught earlier and more precisely, as a plain "cannot find
`log`" error at the `:=` itself, before this unread-local check ever runs.

`let {field, field: bind, ...} = source` destructures several fields of a
struct or `?T` value into fresh locals in one statement, instead of one
`let bind = source.field` projection per field:

```trace
let {valid, data: d} = opt   -- sugar for:
-- let valid = opt.valid
-- let d = opt.data
```

Bare-brace, not Rust's `let StructName{...} = source` — the parser has no
type information at this point to validate a struct name against
`source`'s actual type, and unchecked text that reads like an assertion is
exactly the kind of sharp edge this codebase avoids elsewhere. Mostly
sugar: `parse_stmt` expands it into ordinary `Stmt::Let` nodes at parse
time (the same "one source statement, several AST statements" shape
`tick <expr>` already uses), so no other pass needs to know destructuring
exists — each field's `.field` projection is type-checked, resolved, and
emitted exactly as if hand-written, which means every existing
restriction applies for free, with its existing error message: a typo'd
field name is "struct `Pair` has no field `vlaid`", and destructuring a
LOCAL that itself aliases another struct/Option value hits the same "not
aliased from another value" rejection an equivalent hand-written
projection would. `source` must be a bare reference (a reg/local/param
name) — a call there would desugar to one re-evaluation of the call per
destructured field (`Make().a`, `Make().b`), silently duplicating
whatever the callee's body does instead of binding one shared result;
bind it with an ordinary `let` first, then destructure that. No nested
destructuring (`{maybe: {valid}}`) — single-level field projection only.
`field: bind` renaming exists because `valid`/`data` are the two field
names every `?T` has: destructuring two `?T` values in one rule needs it
to avoid the second silently shadowing the first (`let` rebinding a name
is legal, not an error — see above).

Exhaustive by default, the same way struct construction is: every field
of `source`'s type must be named, or the pattern must end in a trailing
`..` to explicitly discard the rest (`let {valid, ..} = opt` skips
`data`) — naming only some fields with no `..` is a compile-time error
("missing field(s): data — name them, or add `..` to discard the rest"),
`..`'s escape-hatch mirroring construction's own `..base` spread. Unlike
the rest of destructuring, this one piece can't stay pure parser sugar —
knowing whether a pattern is exhaustive needs `source`'s resolved type,
which the parser doesn't have. The parser instead records each pattern's
named fields and rest-flag in a side list on `Ast` (`Destructure`,
ast.rs) — not a new `Stmt`/`Expr` variant, since unlike `Expr::Optional`
or struct update's `base` field, no downstream pass (resolve/effects/
elaborate/lower/firrtl) needs to know a run of `Stmt::Let`s came from a
destructuring pattern at all; only types.rs reads the side list, once,
at the very end of `check()`, by which point every body's field types
are already known. A named field that doesn't actually exist on
`source`'s type is left to its own "no field `x`" error rather than
also reported missing — `let {vlaid, data} = p` on a two-field struct
reports the typo once, not the typo AND a bogus "missing `valid`".

## Calling a function from a rule

A rule may call a user `fn`/`impl` (not `spec` — those stay verification-only).
`return` belongs to a callee's body, not a rule's: a rule has no return value,
so `return <expr>` at a rule's top level is a compile-time error rather than
something to write here.

```trace
Avg(a : [8], b : [8]) : [8] <combines> {
    let sum = a + b
    return sum >> 1
}

module Top {
    in a : [8]
    in b : [8]
    out result : [8] = 0

    rule compute {
        result := Avg(a, b)
    }
}
```

A callee's body may:

- bind zero or more `let`s,
- write registers, instance ports, or a bare-statement/`:=`-RHS call to another
  callee that itself writes state,
- and end in either a trailing `return <expr>` or an `if`/`else` whose branches
  both recurse into that same shape (the `else` is mandatory: every reachable
  path must produce a value).

A callee's body may **not** contain a loop. A nested call is legal only as a
value (a `let`'s init, the return expression, or a write's own right-hand
side, including as an argument to a builtin like `prio`); a call nested any
deeper — as part of a larger expression — is an error. A call cycle, direct
or through another function, is an error.

A callee's body **may** contain bare, top-level guards and/or fifo ops
(`fails`, per the section above) — calling it folds every one of them into
the caller's own rule guard, substituted against that call's actual
arguments, exactly as if they had been written directly in the caller:

```trace
Classify(x : [8]) : [8] <combines, fails> {
    (x <> 0)?
    return x
}

rule step {
    result := Classify(a)    -- rule's own guard becomes (a <> 0)?
}

Push(x : [8]) : [8] <combines, fails> {
    buf.Enq[x]
    return x
}

rule enqueue {
    result := Push(a)    -- rule's own guard becomes not(buf's valid)
}
```

A callee's fifo ops combine with the caller's own (and with fifo ops reached
through a _different_ callee call in the same rule) using the identical
Enq+Deq pass-through rule a rule's own body already follows — a rule that
directly `Deq`s a fifo while calling a callee that `Enq`s that SAME fifo gets
one combined `valid`-only guard, not the separately-computed (and always
false) AND of each op's own individual guard. A `let`-bound callee-local
feeding a `Deq` into a later `Enq` within the same callee (the
`fifo_bridge.tr` pattern, wrapped in a callee — see `examples/call_fifo.tr`)
works the same way a rule-level local does. Declaring the local needs no
special callee handling: `x := input.Deq[]` on a name that doesn't yet
resolve is simply the ordinary "cannot find `x`; use `let x = ...`"
resolve-time error every fresh-declaration site gets now (see "Locals"
below), not a callee-specific case. REASSIGNING a callee-local afterward
(`let x = ...` followed later by `x := ...` in the same callee body) is a
clean compile-time error — a callee's own locals never reach the
position-snapshot machinery that makes rule-level `:=` reassignment
sound (see "Locals" below; that machinery is rebuilt only per top-level
`rule` item, `enter_rule`, firrtl/writes.rs, never for a callee body
reached through inlining), and unlike the rule-level case, a callee-local
reassignment can't simply be threaded through the substitution map that
resolves callee-local reads (`self.locals`) either: that map substitutes
each local's ORIGINAL binding expression at every use site, not a
snapshot of its value, so `let z = x; x := x + 1; return z` would
silently give `z` `x`'s NEW value instead of the value `z` was actually
bound to — a real fix needs `locals_snapshots`-style eager, position-
indexed resolution extended into callee inlining, not attempted yet. See
TODO.md.

The fold only understands one shape: the callee's _entire_ fail condition must
reduce to bare guards and fifo ops sitting directly at its own top level —
not nested inside one of its own `if`/`else` branches (legal syntax there,
since both are ordinary allowed statements in a branch, but invisible to the
fold's flat scan), and not a nested call to another failing function (v0
folds one level only — a callee calling ANOTHER failing callee is still an
error). Anything outside that shape is a compile-time error rather than a
partial, silently-wrong fold. The call itself may only appear as a whole
statement or the entire right-hand side of `:=` — the same two positions a
state-writing call is already restricted to, and for the identical reason:
nested any deeper (an argument, a `let`, `Classify(a) + 1`), the guard
wouldn't be found there and would silently stop gating the rule.

A struct- or `?T`-typed param is supported: the argument may be a struct
literal/`false`/a plain value of the wrapped type, or a reg/output/input/
another same-typed param, chased through to that value's own flat fields:

```trace
struct Pair {
    valid : bit
    data : [8]
}

UsePair(p : Pair) : [8] <combines> {
    return p.data
}

module M {
    reg q : Pair = Pair{ valid: 1, data: 8'd7 }
    out out_v : [8] = 0
    rule r {
        out_v := UsePair(q)    -- p.data resolves to q_data
    }
}
```

A `?T`-typed param's own guard (`o?` inside the callee) folds into the
caller's rule guard exactly as a module-level `?T` reg's does — reusing the
identical fold, just with a param binding in between rather than a bare
reg reference (see "Option emission" under Part 2). Chasing through an
argument only happens when the argument is itself a plain reference of the
*same* type as the param (genuine aliasing, `UsePair(q)` with `q : Pair`) —
a plain `T`-typed value or local passed to a `?T` param still coerces to
present the ordinary way, unaffected (`UseIt(x)` with `x : [8]` and
`UseIt(o : ?[8])` coerces `x` present, it does not chase `x`).
Chaining through nested calls composes (`Outer(p) { return Inner(p) }`,
called as `Outer(q)`, resolves all the way back to `q`'s own fields) — one
restriction carries over from ordinary struct/Option LOCALS: a `let` INSIDE
a callee body that merely re-binds a param (`let x = p; x.field`) is
rejected, the identical restriction a rule-level `let o = opt` has (see
"Option types" above) — reference the param directly, don't re-bind it to
a local first.

A struct- or `?T`-typed RETURN is supported too. The trailing `return <expr>`
(or an `if`/`else` whose branches both end that way) may build a fresh
literal, chain into another struct/Option-returning call, or pass one of the
callee's own params through unchanged:

```trace
MakePair() : Pair <combines> {
    return Pair{ valid: 1, data: 8'd7 }
}

Passthrough(p : Pair) : Pair <combines> {
    return p                     -- src's own fields, unchanged
}

module M {
    reg src : Pair = Pair{ valid: 1, data: 8'd9 }
    reg dst : Pair = Pair{ valid: 0, data: 0 }
    rule copy {
        dst := Passthrough(src)  -- dst_valid := src_valid, dst_data := src_data
    }
}
```

A `<fails>` callee's guard fold and its struct/Option return's own per-leaf
decomposition are two fully independent passes over the same body — the
guard folds exactly the same way regardless of the callee's return type, and
the return decomposes exactly the same way regardless of whether the callee
also fails. `return p` (a bare param, unchanged) is the ONLY shape a callee's
own return may chase through an alias for — the identical restriction the
param side has one level deeper: a callee-local that merely re-binds a param
and returns THAT (`let x = p; return x`, as opposed to reading a field off
it, `let x = p; return x.data`, which resolves the ordinary way through the
field-read path) is rejected, not silently resolved.

A generic parameter's own width, used independently of the callee's return
value (for example, as an argument to `prio` inside a generic callee), is only
resolvable by following it back to a concrete call site; used elsewhere, this
fails cleanly rather than compiling to the wrong width.

Of the builtins, `prio`, `trunc`, and `pack` are synthesizable as calls:

- **`prio(reqs)`** is a fixed-priority encoder. The lowest set bit wins (bit 0 is
  highest priority); `reqs = 0` returns `0`, a defined but not meaningful value
  — gating on `reqs <> 0` is the caller's job.
- **`trunc(value, width)`** truncates to the low `width` bits. The 1-argument
  form, `trunc(value)`, infers `width` from context instead of naming it:
  types.rs types it `Bits(Width::Unknown)` (bottom-up inference has no
  target-width visibility at the call site), and FIRRTL emission resolves the
  real width top-down from the existing `hint` mechanism that already threads
  a write target's width through `compile_expr_hinted` — the same channel a
  2-argument `trunc` never needed. This also resolves through a `let`-bound
  local: `writes.rs`'s `enter_rule` normally compiles a local's RHS eagerly at
  bind time, but falls back to lazy, read-site-hinted compilation whenever the
  local's own type doesn't resolve to a concrete `Bits(Width::Known(_))` —
  exactly what `Width::Unknown` produces — so `let x = trunc(a) ... b := x`
  picks up `b`'s width for `x` at its read site. A 1-argument `trunc` with no
  reachable hint (nested in an arithmetic operand rather than a write target
  or a hinted local) is a compile error naming `trunc(value, width)` as the
  fix — a known, deliberate boundary, not a gap to close later.
- **`pack(a, b, ...)`** concatenates its arguments, most significant first: the
  first argument becomes the high bits, matching FIRRTL's `cat`, Chisel's `Cat`,
  and Verilog's `{a, b}` concatenation. The result width is the sum of the
  argument widths.

`logic <expr>` is different: a real prefix OPERATOR (`Expr::Logic`, ast.rs), not
a call — no parens, no comma-separated arguments. Unlike every other prefix
operator here (`not`/`optional`/`spawn`, all parsed at `PREFIX_BP`, tighter
than any binary operator), `logic`'s operand parses as a full expression
(binding power `0`, the same as a parenthesized group's inner parse) —
deliberately loose, so `logic a > b` reads as `logic (a > b)` without parens,
since a bare fallible comparison (see TODO.md) is `logic`'s main operand shape
going forward. It converts a fallible expression into a plain `[1]`
value — `1` if `expr` would succeed, `0` if it would fail — without gating the
enclosing rule (the failure is *discharged*, not propagated) and without
performing `expr`'s own side effect. `expr` must be exactly one of two shapes
today: a fifo op (`logic f.Deq[]`/`logic f.Enq[x]`, which reads the fifo's own
occupancy/space condition but never actually dequeues/enqueues) or a call to a
guard-only `<fails>` fn/impl (`logic Classify(x)`, which reads the callee's
already-substituted guard condition but never runs its body). A call whose
fail condition also writes state is a compile-time error, not silently
supported or silently wrong — see below.

The loose operand parse has one real consequence: this language's bitwise
operators (`&`/`|`/`^`) bind *tighter* than comparisons (Rust-style, see
`precedence_matches_rust_not_c`), so there is no binding-power threshold that
lets `logic` swallow a bare comparison without ALSO swallowing `&`/`|`/`^` —
`logic A & logic B` (the `and`-combination idiom just below) now needs
explicit parens on each side, `(logic A) & (logic B)`, the same way any other
expression needs parens to `&`-combine two things wider than a single token.
A bare `logic A & logic B` instead parses as one `logic` wrapping the whole
`&` expression (`logic (A & (logic B))`), which `check_logic_args`
(firrtl/checks.rs) rejects with a clear error rather than silently doing
something else.

Ported from Verse's own `logic{ exp }` (a curly-brace cast in Verse's syntax;
`logic <expr>`, no delimiter at all, matches this language's own `not`/
`optional` prefix-keyword convention instead), confirmed against Verse's
`02_primitives` chapter: "To convert an expression that has the `<decides>`
effect to `true` on success or `false` on failure, use `logic{ exp }`". See
`examples/logic_probe.tr` + `sim/logic_probe_tb.v`, which also proves a
`logic`-probing rule and a rule doing the real fifo touch coexist correctly
the same cycle under `conflict_free` (a plain occupancy read has nothing to
hazard against a same-cycle write to the same register — the read always sees
the pre-edge value regardless).

The guard+write restriction is not just "hard to implement": a callee whose
fail condition folds into a caller's guard AND also writes state is fully
supported for an ordinary direct call (see above), but wrapping one in
`logic` would mean silently discarding its write — running only the
guard-fold half of what a real call does — which is a confusing footgun even
where it would be internally safe, not a feature. It also isn't an ad hoc
trace limitation: Verse's own `logic{}` only accepts a `<decides>`-effect
expression, and `<decides>` in Verse's effect system means
side-effect-free-but-fallible by construction — a `<transacts>`
(state-writing) computation is not legal `logic{}` input there either. trace
enforces the same boundary with an explicit check rather than a separate
effect category, since trace has no `<decides>`/`<transacts>` type-level
split.

`clog2` and `len` type as `Ty::Int`: they are compile-time-only, not synthesizable
values. `wire`, `list`, and `any` are type- or elaboration-position constructs
with no runtime hardware meaning (`bits` no longer has surface syntax of its
own — see "Expression surface" — but the AST node it used to name is still
synthesized internally by every `[N]` type). `sync` and `race` are covered
above.

## The schedule block

Two rules conflict when one writes what the other reads or writes. Conflicting
rules cannot fire the same cycle; an urgency order picks the winner. The default
order is declaration order. A `schedule` block overrides it:

```trace
schedule {
    urgency writer_a > writer_b > writer_c
    mutually_exclusive { writer_a, writer_b }   -- claim: never both fire; checked
    conflict_free { reader, writer }            -- claim: safe to fire together; trusted
}
```

`urgency a > b` states that `a` wins any conflict with `b`. It is not limited to
two names: `urgency a > b > c > d` states a whole priority CHAIN in one
directive — `a` beats `b`, `b` beats `c`, and so on — which the scheduler
expands into every pairwise conflict resolution that chain implies (Kahn's
algorithm over the derived edges, `schedule.rs`), not just the adjacent pairs.
A design with N mutually-conflicting rules needs one chained directive, not one
`urgency` statement per pair.

`mutually_exclusive { a, b }` claims the two rules never actually fire the same
cycle, even though their static read/write sets conflict — for example, two
writers whose enable conditions are known to be exclusive at runtime, which the
scheduler cannot see into a guard to prove. This claim is **checked**: the
compiler inserts a simulation assertion, and a violation reports the exact rule
pair.

`conflict_free { a, b }` claims it is safe for both rules to fire the same
cycle — for example, a memory with independent read/write address ports, where a
same-cycle read and write to different addresses do not hazard. For a pair
sharing only mem accesses, this specific precondition — the two addresses
actually differ — is **checked**, not just trusted: both addresses are real
compiled FIRRTL signals (the mem's independent reader/writer ports), so the
emitter inserts a simulation assertion alongside the waived stall, exactly like
`mutually_exclusive`'s own check (see "Scheduling" below). For any non-mem
shared state a `conflict_free` pair might also touch, or for a runtime-valued
index the scheduler can't observe (which is still the common case — module
inputs like `write_addr`/`read_addr` have no provable value set at all), the
claim stays **trusted, unchecked**. When BOTH sides' indices happen to be
compile-time constants, or the same affine base, instead, the scheduler proves
disjointness on its own rather than asking for this annotation, or a runtime
check, at all — see "Arrays: one resource each" below.

`conflict_free` is only meaningful for a read/write conflict, never a
write/write one: v0 gives no meaning to two rules writing the same register or
memory port the same cycle, since there is no arbitration between their writes.
`conflict_free` on a write/write pair is a compile-time error, not a documented
footgun; `mutually_exclusive` is the correct annotation for a write/write pair
claimed never to coincide, since its checked assertion stays sound even if the
claim turns out false.

## Arrays: one resource each

In v0, a whole array (`mem`) is one conflict resource: any two accesses to the
same array conflict unless both are reads. Two exceptions exist, both scoped to
**read/write** pairs only, both a syntactic check living entirely in
`schedule.rs` — NOT dependent or refinement types, no new types or
propositions, just one more index shape the same proof recognizes:

```trace
rule a { x := m[3] }      -- reads {m}
rule b { m[7] := y }      -- writes {m}
-- v0: proven disjoint (3 <> 7, both compile-time constants).

rule e { x := m[i] }      -- reads {m}, i a plain register/input
rule f { m[i+1] := y }    -- writes {m}, same base def i, constant offset 1
-- v0: proven disjoint (i <> i+1 always, given m's depth is a power of two —
-- see below) even though i is a RUNTIME value neither rule's own source
-- states as a literal.

rule c { x := m[i] }      -- reads {m}, i a runtime value
rule d { m[j] := y }      -- writes {m}, j a DIFFERENT runtime value
-- v0: c conflicts with d -- i and j could coincide at runtime, and proving
-- otherwise needs real range tracking, not a syntactic check; this stays the
-- general, conservative case.
```

The first case (both indices literal constants) needs only the constants to
differ. The second (an affine expression of a shared base — `m[i]`, `m[i+1]`,
`m[i-1]`) needs: the SAME base def on both sides (a different register's value
could coincide — `m[i]` vs `m[j]` for two distinct registers is deliberately
`c`/`d` above, not provable), AND the mem's own depth to be exactly a power of
two. That second condition is load-bearing, not caution for its own sake: index
arithmetic wraps modulo the base's own width, and that modular argument is only
sound when every representable address is a real, distinct memory cell — v0 has
no bounds check on an index against a non-power-of-two depth at all (an
out-of-range index is currently undefined, left entirely to firtool), so the
proof simply never depends on that undefined behavior rather than guessing at
it. Offsets are actually compared modulo 2^(the SMALLER of the mem's own
address width and the base's own declared width): `i + k` wraps at the base's
own width first, which can be narrower than the address width connected to the
mem port, and two offsets differing mod the wider width can still alias mod the
narrower one — the base's width must be a concretely-known `bits[N]` or the
comparison fails closed. A base is never chased through a rule-local (a local
can be reassigned mid-rule; resolving through the wrong binding would be the
exact reassigned-local/`Avg(Avg(x,y),z)` bug class already shipped and fixed
twice in this codebase) — only a bare register/input reference counts.

The soundness argument above rests on both rules reading the IDENTICAL
pre-edge value of the shared base within one cycle — true regardless of which
rule writes it, since registers are speculatively written and read pre-edge
(this scheduler's own core invariant). This never needs checking as a separate
side condition: if either rule also writes the base, that register becomes
shared state between the pair alongside the mem, and the scheduler's
requirement that EVERY shared def be this one mem rejects the whole pair
outright — the case where the invariant would matter cannot reach this proof
at all.

Any index that isn't a bare constant or an affine expression of a shared base
(or the two bases differing, or the depth not being a power of two) falls back
to the ordinary conservative conflict. A **write/write** pair is never
exempted this way even when both addresses are provably distinct: v0 emits one
shared, priority-muxed write port per mem (see "The schedule block" and Part
2's FIRRTL emission), so two "safe" writers would still race on that one port
— proving their addresses disjoint doesn't change that there's only one write
port to land on. A design with one memory otherwise serializes on it, one
access per cycle. That is honest behavior for a single unbanked, single-port
memory.

## Combinational loops

Inside one `combines` scope, no forward reference is allowed, so a local cycle
cannot be written. This does not, by itself, guarantee acyclicity across module
boundaries: two internally-acyclic modules wired output-to-input in a cycle can
still form a real combinational loop. See Part 2 for how this is checked.

# Part 2: The compiler

## Architecture

Front end only: no backend of its own. The compiler emits an existing IR as
text.

- Primary target: **FIRRTL** text (`.fir`) → `firtool` (CIRCT) → Verilog.
- Alternative to evaluate: **Calyx**, semantically closer to the rule/control
  world.

```
logos lexer
  → hand-written recursive-descent parser (Pratt core for expressions)
  → index-based AST (arena, NodeId(u32), side tables)
  → semantic passes: effect check → unification → width fixed-point → scheduling
  → simulator + FIRRTL/Calyx emission
```

## Implementation decisions

- **Host language: Rust.**
- **Hand-written recursive descent parser**, not a parser library, for
  the best error messages and because the syntax is expected to churn.
- **Pratt parser for expressions.** The language is expression-oriented with
  many custom operators (`|`, `?`, `:=`, ranges); Pratt makes each operator tier
  one binding-power table entry.
- **logos** for the lexer. **ariadne**/**codespan-reporting** for diagnostics.
- **Braces and newlines in v0**, not indentation sensitivity. Indentation, if
  added later, is a lexer-only change (a Python-style indent stack emitting
  synthetic tokens); the parser does not need to know.
- **Index-based AST**: nodes in per-kind arenas, children referenced by
  `NodeId(u32)`, analysis results in side tables. This avoids borrow-checker
  fights in annotation passes.

## Inference: two solvers

**Pass 1: parameter and type inference.** Ordinary unification. Runs at
elaboration time, where recursion is legal.

```trace
Fifo(depth : int, T : type) { ... }

f := Fifo(_, [8])
f.Enq[x]                  -- x : [8] unifies; depth still free
g := connect(f, deep16)   -- deep16 : Fifo(16, _) → depth := 16, T := [8]
```

**Pass 2: width inference.** A monotone fixed-point over an interval lattice.
Runs after elaboration, on the concrete circuit. Widths only grow; the pass
terminates at the fixed point. A body that fails to reach a fixed point (widths
growing without bound) is a compile error naming the enclosing item.

```trace
let sum = a + b           -- |sum| = max(|a|, |b|); modular, Chisel-style
let prod = a * b          -- |prod| = |a| + |b|
let idx = pc + 3          -- an int literal absorbs the other width: [16]
sum := trunc(sum, 8)      -- explicit narrowing; silent truncation is an error
```

Absorbing is range-checked, not just width-tagged: `pc + 100000000` against a
`[16]` `pc` is a compile error (`100000000 does not fit in [16]`), the same
diagnostic an out-of-range state write already gets — the literal's WIDTH
silently becoming `pc`'s is fine (that's the whole point of absorption), but
its VALUE not fitting there is exactly the class of near-certainly-a-bug an
assignment-shaped literal already catches, and a binary operand deserves the
same scrutiny. A shift's amount operand (`x >> 300`) is deliberately excluded
from THIS check — a shift count isn't a value bounded by the shifted
operand's own domain, so "does 300 fit in [8]" is the wrong question — but
it's not left unchecked: a separate rule catches a constant shift amount
`>= w` (`x >> 300` on an [8] `x` discards every bit, always landing on
all-zero/all-sign — near-certainly a bug, the same "value coerces silently
but the coercion is a mistake" class, just a different bound). `x << 7` on
an [8] `x`, which loses seven of eight bits without hitting that `>= w`
threshold, is deliberately NOT caught — that's a data-dependent partial
loss, not a statically-knowable total discard, and catching it structurally
would mean growing `<<`'s result width instead of keeping it at the left
operand's own (the way `*` already grows to `a + b`), a bigger design change
than a diagnostic addition.

Both of those checks (the literal-fits range check and the shift-amount
total-discard check) are per-operator-application, not per-value, so a
single suffix on the operator itself can silence exactly one and leave every
other use of the same values alone: `.!` after any binary operator (`a
+.! 100000000`, `x >>.! 300`) marks that one `Expr::Binary` node as an
explicit "I know, let it through," recorded in a side table
(`Ast.lossy: HashSet<ExprId>`, same shape as `destructures`) rather than a
new AST node — `type_binop` checks `self.ast.lossy.contains(&at)` and skips
`check_literal_fits`/`check_shift_amount` for that one application while an
unmarked sibling expression using the same operands still errors normally.
`.!` is scoped narrowly: it silences the two absorption/shift-amount
sanity checks above, not `check_assignable`'s "would silently truncate"
check on a write target — `x := a +.! b` where `a + b` is wider than `x`
still wants `trunc` (either form) to narrow it, `.!` alone does not make
that assignment legal.

A local reassigned within a rule is re-typed to a fixed point across its whole
body: its tracked width reflects the widest binding across every reassignment,
not any one binding's own natural width — this is what lets a bare-literal first
binding (`s := 0`, later rebound via wider arithmetic) compile at the correct,
final width instead of its own narrow one.

## Scheduling

The compiler builds a conflict matrix from the read/write rows. It reports its
reasoning:

```sh
trace build cpu.tr --explain-schedule
```

```
rule step_s1 conflicts with rule refill:
    both write {mem}          (mem is one resource in v0; see Arrays)
urgency: step_s1 > refill     (declaration order; no annotation given)
derived stall: refill fires only when step_s1 is blocked or idle
```

`mutually_exclusive`'s checked claim emits a FIRRTL `assert` right after the
pair's own fire signals:

```
assert(clock, not(and(fires_a, fires_b)), not(reset), "mutually_exclusive claim violated: rule a and rule b both fired the same cycle") : mutually_exclusive_check_0
```

The claim checked is exactly "these two rules never both fire the same cycle" —
not anything about the addresses or values touched, since v0 cannot observe
those independent of firing. The assertion's own enable is gated on
`not(reset)`, since a rule's guard may read state that has not settled to its
real post-reset value on the reset cycle itself, and a spurious both-fire there
would be a false violation.

`conflict_free` waives the derived stall the same way. When every def the
pair shares is a `mem`, it ALSO emits one assertion per (mem, qualifying read
site) checking that specific def's addresses never coincide while both rules
fire:

```
assert(clock, not(and(fires_read, and(and(fires_write, UInt<1>(1)), eq(read_addr, write_addr)))), not(reset), "conflict_free claim violated: rule read and rule write accessed the same address in `m` the same cycle") : conflict_free_mem_check_0
```

(`examples/conflict_free_mem.tr`'s own emission — `and(fires_write, UInt<1>(1))`
is that rule's write-enable, unconditional here since the rule has no internal
branching to mux; a branch-conditional writer's own muxed enable takes its
place instead, see `mem_write_in_stmts`.) A read site nested inside an if/else
is silently excluded from this check, never asserted against: read ports are
driven unconditionally regardless of which branch a cycle actually takes (see
"Arrays: one resource each"), so a branch-local read's wired-up address can be
stale/irrelevant on a cycle that doesn't take that branch — asserting against
it would be a false alarm on a correct design, not a caught hazard. For any
non-mem shared state, `conflict_free` still emits no assertion: there is
nothing sound to check on a plain register or fifo the same way.

Every mem's `read-under-write` is set to `old`: a read and a write to the SAME
address the SAME cycle sees the mem's PRE-EDGE contents, never the write
landing mid-cycle — exactly the pre-edge-read invariant every register already
has, stated for mems too rather than left to a tool's own default. This is a
DIFFERENT, complementary guarantee from the assertion just above, not a smaller
version of it: the assertion checks a stated PRECONDITION (do these two
addresses actually differ), while `old` defines what happens on the cycle that
precondition is violated, OR on an ordinary, unexempted design where two
independent addresses simply happen to coincide at runtime with no annotation
or conflict pair involved at all (`examples/mem_write_branch.tr`'s
conditional `m[addr] := data` against its own `m[read_addr]`, same rule). Costs
nothing today: firtool already lowers a same-cycle read/write to a
combinational read against the write's own nonblocking assign at
`read-latency => 0` (every mem in v0), confirmed byte-identical Verilog output
against the previous `undefined` setting — this only becomes load-bearing if
`read-latency` is ever raised above 0.

`mutually_exclusive` and `conflict_free` are named to match Bluespec's own
vocabulary for these two ideas, rather than inventing new terms bsc already has
words for.

**A third, auto-derived exemption needs no annotation and no assertion
either: a proven disjointness claim.** A read/write pair sharing only mem
accesses that recognize as compile-time constants, or as an affine expression
of the SAME base def (register/input) with the mem's own depth a power of two,
and are provably different, is dropped from the conflict matrix on its own —
the scheduler found the proof itself, so there is nothing left to trust OR to
check (see "Arrays: one resource each" for exactly what's recognized and why).
`--explain-schedule` reports it distinctly from both other exemptions:

```
rule write conflicts with rule read: write meets read on {m}
    index sites proven disjoint (constant addresses, or the same base plus a constant offset): no stall derived (no annotation needed)
```

Deliberately narrow, matching v0's existing bar of failing closed rather than
guessing: a mem sharing even ONE index that doesn't recognize as either shape,
two DIFFERENT bases, a non-power-of-two depth, or a write/write pair (v0's
single shared write port makes two "disjoint" writers meaningless — see
"Arrays: one resource each"), all stay fully conservative, exactly as if this
proof did not exist. A user's own `conflict_free`/`mutually_exclusive` on a
pair this already clears is legal, harmless overstatement, same as claiming
either on a pair that never conflicted at all.

**Tier 3, not v0: provable disjointness across DIFFERENT bases.** Dahlia-style
banked and affine array types would let the compiler prove two accesses
disjoint from genuinely different variables (`m[i]` against `m[j]`, two
distinct registers whose values happen never to coincide) via real range
tracking — a whole type-system feature on its own, out of scope for v0 so the
scheduler work stays bounded. The same-base affine case (`m[i]` against
`m[i+1]`) is NOT this; it's a syntactic check that needs no range tracking at
all, since a shared base's value is identical on both sides by construction.

The checked `conflict_free` assertion above is not a smaller version of this
tier, and does not retire it: it checks a runtime PRECONDITION (do these two
addresses happen to differ THIS cycle), not a static proof (do they ALWAYS
differ). For the driving case — `conflict_free_mem.tr`'s `write_addr`/
`read_addr`, module inputs with no provable value set at all — no static
range-tracking scheme could ever close this anyway; a primary input's possible
values are unbounded by definition. A checked assertion is the correct, and
only, mechanism for that shape, tier 3 or not.

## Combinational loops: what the checker does

- Enforce no-forward-reference inside `combines` scopes; this makes a local
  cycle inexpressible.
- Do not build a whole-program cycle checker. The backend is firtool
  (CIRCT), and its `CheckCombLoops` pass is the whole-circuit loop check
  this pipeline relies on — but `trace` never runs firtool itself. The
  compiler only emits FIRRTL text (`src/firrtl/mod.rs`); firtool is
  invoked separately, by `devenv.nix`'s `simulate` script and by
  `tests/sim.rs`'s own integration tests, never by `trace`'s own binary.
  There is no code anywhere in `src/` that parses a firtool diagnostic or
  remaps one back to a `.tr` source span — a comb-loop error, if firtool
  ever raised one, would name FIRRTL-level identifiers in whatever tool
  ran firtool, not a trace source position.
- Known CIRCT limitation, not currently reachable through anything this
  compiler emits: `CheckCombLoops` has a blind spot when a circuit
  contains more than one `public module` (CIRCT issue #7435, open) —
  corrected from an earlier, mistaken citation of #1138, a since-closed,
  unrelated same-module bug (a bare `a <= b; b <= a` loop CIRCT's checker
  used to miss entirely; fixed by CIRCT PR #1388). `firrtl::emit`
  (`src/firrtl/mod.rs`) enforces exactly one top module per circuit — a
  hard compile error ("FIRRTL emission needs exactly one top module")
  otherwise — and only that module is ever emitted `public module`; every
  other emitted module, including every instantiated submodule, is plain
  `module`. #7435's trigger (two or more independent `public module`s in
  one circuit) is therefore not something anything `trace` itself emits
  can produce today.

The airtight fix is Filament-style timeline types on ports, which make
cross-module feedback inexpressible by construction. That is a large feature,
not v0.

## Sequences lowering

A `sequences` block is sugar. `tick` cuts it into segments; each segment
lowers to an ordinary single-cycle rule, guarded on a continuation register.

```trace
Rmw(addr : [8]) <sequences, reads {mem}, writes {mem}> {
    let v = mem[addr]
    tick
    mem[addr] := v + 1
}
```

lowers to (shown as source; the real lowering is internal):

```trace
reg cont   : {S0, S1} = S0
reg v_save : [8]

rule rmw_s0 {
    (cont = S0)?
    v_save := mem[addr]
    cont := S1
}

rule rmw_s1 {
    (cont = S1)?
    mem[addr] := v_save + 1
    cont := S0
}
```

Each segment is an ordinary rule. Two kinds of step exist:

- **Progress step.** The rule fires, commits, and writes the next continuation
  value.
- **Blocked step.** A guard inside the segment fails. The rule aborts for this
  cycle; the continuation register keeps its value; the segment retries next
  cycle. This reuses the same failure-as-backpressure mechanism as `Deq[]`, not
  a new concept.

A value that crosses a `tick` gets a save register, promoted from the local it
came from. This is only sound when the local is single-assignment and read only
in later segments: promoting a local read-after-write in the SAME segment to a
register would change its semantics from "sees the new value" to "sees the old
value" (a register is speculative until the clock edge). A local reassigned
across segments, or read in its own assignment segment, is a compile error
rather than a silent miscompile. A captured local's type must be a concrete
`[w]`.

The lowering splices the original binding statement verbatim into the
generated segment rule, relying on it staying a valid register write once
promoted. That holds immediately for `x := value`, since `x` still parses as
an ordinary write once it becomes a `reg`. It does NOT hold for `let x =
value` as written, since `let` always binds a fresh local rather than writing
an existing one — spliced verbatim it would silently shadow the register
instead of writing it. `CapturedLocal` closes this gap with a targeted
rewrite instead: for a `let`-bound capture, it records the prefix span
covering exactly `let x = ` (from the declaring statement's own start through
the init expression's own start), and `render_rule`/`render_spawn_segments`
replace that prefix with `x := ` before splicing the rest of the statement
verbatim — the same trick as `:=`, just needing one rewritten prefix to reach
it. A `:=`-declared capture has no such prefix span and needs no rewrite.

Because `let` may shadow, two textually-distinct `let x = ...` bindings
(different `DefId`s) can both need to cross a `tick` within the same rule —
one arm of a sequence, then a later arm shadowing the same name. Both would
otherwise become captures sharing the name `x`, and thus the same save
register (`reg x : [8] = 0` emitted twice), which fails to re-resolve
downstream rather than erroring cleanly at lowering time. `compute_captures`
rejects this directly once two captures resolve to the same name, before any
text is generated.

A `sequences` rule reports its own cost: segment count and saved-register bits.

### `while` lowering

A top-level `while COND { body }` cuts a segment boundary the same way
`tick` does, but unlike `tick` (a pure separator with no content of its own)
it gets a dedicated segment holding exactly itself (`Segment::while_cond:
Option<ExprId>`, `Some` only for this one) — `render_rule`/`render_spawn_
segments` unwrap `COND`/`body` from that one statement at render time and
render it as an `if`/`else` self-loop instead of the ordinary straight-
line-then-advance shape:

```trace
rule r_s1 {
    (__cont_r = 1)?
    if COND {
        <body, spliced verbatim>
        __cont_r := 1        -- stay: loop back to this same segment
    } else {
        __cont_r := 2        -- advance: the loop is done
    }
}
```

This needed **zero** changes to `firrtl.rs` — the existing `Stmt::If`
write-threading (already covering every reg/mem/instance-port/callee write
path) produces exactly the right mux for a register conditionally written
across the loop's two mutually-exclusive continuation values, since a
generated `while` segment's body is, by the time it reaches emission,
ordinary `if`/`else` trace text like any other. The entire new surface is in
`lower.rs`: segment-cutting, the render-time text shape above, and the
capture-rejection messaging below.

`split_into_segments` only ever inspects a body's TOP level, so `find_
nested_while` (mirroring `find_nested_tick`/`find_nested_spawn` exactly)
rejects a `while` nested inside `if`/`if let`/another `while` before
splitting ever runs — left unchecked, a nested one would silently fold into
whichever segment it landed in as ordinary, un-lowered text. `plan()`'s own
top-level gate (deciding whether a `<sequences>` rule needs `plan_rule` at
all) had to change from "does the body contain a top-level `Tick`" to
scanning for a `Tick` OR `While` ANYWHERE in the body, not just the top
level — a `while` nested inside an `if` has no TOP-LEVEL tick/while either,
so the shallow version would skip straight past `plan_rule` (and therefore
`find_nested_while`'s own rejection) entirely, leaving the nested `while` to
fail some later, more confusing way instead (self-caught: the first version
of this change silently accepted a nested `while` with no error at all).

A `return` nested inside a `while`'s body needs no new check: `find_returns`
already walks into `Stmt::While { body, .. }` recursively (finding the
nested `Return`), and the existing "must be the LAST statement of the LAST
segment" check (`plan_spawn`) can never be satisfied by a statement nested
one level inside a `Stmt::While` — that requires the found return to equal
the OUTER statement being checked, never true for anything nested.

`compute_captures` (the write-once, read-only-in-later-segments checks
above) is UNCHANGED — a `while` loop's own segment index is just another
segment number to it. What changes is only the ERROR MESSAGE: `compute_
captures` computes the set of segment indices with `while_cond.is_some()`
and, when a rejected def's touched segments intersect that set, swaps the
generic "assigned in multiple segments"/"write-once" text for one naming
the real cause ("written across a `while` loop boundary") — the generic
text would read as nonsense for an accumulator, since there's no OTHER
segment reassigning it, just the loop's own one, executed every iteration.

Six pre-existing recursive scans in this file — `find_returns`, `find_
nested_tick`/`find_tick_anywhere`, `find_nested_spawn`/`find_spawn_
anywhere`, `find_unsupported_construct` — turned out to be missing a
`Stmt::IfLet` arm (only `Stmt::If`/`Stmt::While` were matched), a gap dating
to when `if let` first added `Stmt::IfLet` to the AST and updated `scan_
stmts`/`collect_renames`/`stmt_exprs` but missed this second family of
functions. Self-caught while extending this exact code for `find_nested_
while`: a `tick` nested inside `if let`'s own body silently escaped
detection, surfacing as the confusing "a spawned fn's last segment must end
with `return`" instead of the clear "must be at the top level, not nested
in if/while" every other nested-tick shape already gets. Fixed by adding
the missing arm to all six, mirroring their existing `Stmt::If` arm exactly.

A `while` as a rule's own last top-level statement still gets a trailing
empty segment after it (matching a trailing bare `tick`'s existing
tolerance) — its only content is `cont := 0`, wrapping back to the rule's
own start. One idle cycle between the loop finishing and the rule becoming
eligible to fire again from segment 0, not a bug.

### `while let` lowering

`while let`'s own segment (`Segment.is_while_loop`, shared with plain
`while` — a single `bool` marker, not `Option<ExprId>`: which of `Stmt::
While`/`Stmt::WhileLet` applies, and therefore which shape to render,
depends entirely on which statement `stmts[0]` actually is, so render time
just matches on it directly rather than duplicating that data into the
segment) renders as literal **`if let` source text**:

```trace
rule r_s1 {
    (__cont_r = 1)?
    if let NAME = EXPR {
        <body, spliced verbatim>
        __cont_r := 1        -- stay: loop back to this same segment
    } else {
        __cont_r := 2        -- advance: the loop is done
    }
}
```

`while_loop_header` (lower.rs) is the one function that knows both shapes:
`Stmt::While` renders `if COND {`, `Stmt::WhileLet` renders `if let NAME =
EXPR {`, shared by `render_rule` and `render_spawn_segments` alike. Reusing
`if let`'s OWN syntax verbatim — not hand-rolling an equivalent mux — means
this needed **zero** new emission code, on top of the zero plain `while`
already needed: the rendered text re-enters the full pipeline (re-parse,
re-resolve, re-check) as an ordinary `if let` statement and gets its mux
synthesis, `if_let_binds`, and every write-threading arm for free. `NAME`'s
own scoping (visible only inside `body`, invisible after the loop) falls
out of `if let`'s existing resolve.rs treatment unchanged, for the same
reason.

Every place that needed a `Stmt::WhileLet` arm mirrors its already-built
`Stmt::IfLet` sibling exactly: `find_returns`, `find_nested_tick`/`find_
tick_anywhere`, `find_nested_spawn`/`find_spawn_anywhere`, `find_nested_
while`/`find_while_anywhere` (a `while let` nested inside `if`/`while` is
rejected the identical way, and `find_while_anywhere` itself now matches
`Stmt::WhileLet { .. }` as a terminal "found one" case, not just `Stmt::
While`), `find_unsupported_construct`, `scan_stmts`, `collect_renames`,
`stmt_exprs`, `split_into_segments` (cuts on `Stmt::While { .. } | Stmt::
WhileLet { .. }` together), `compute_captures`'s `while_segments` set
(built from `is_while_loop`, so a `while let` accumulator gets the
identical sharpened rejection message plain `while`'s does), and the
parallel resolve.rs/types.rs/effects.rs/elaborate.rs arms (a `while let`'s
own `init`, like `if let`'s, is `Expr::Guard(inner)` over `Ty::Option(T)`,
requires the same `<sequences>`/`<elaborates>` declaration `while`'s
`cond` does, and is rejected the same way inside `<elaborates>` code).

**Two pre-existing gaps, both self-caught while writing this section's own
worked example (`examples/while_let_drain.tr`), not while building the
segment-cutting/rendering machinery itself:**

- **Six recursive scans in lower.rs were already missing a `Stmt::IfLet`
  arm** — `find_returns`, `find_nested_tick`/`find_tick_anywhere`, `find_
  nested_spawn`/`find_spawn_anywhere`, `find_unsupported_construct` matched
  only `Stmt::If`/`Stmt::While`, a gap dating to `if let`'s own landing (that
  section's own bullet, TODO.md, is updated to match). Confirmed live: a
  `tick` nested inside `if let`'s body used to surface as "a spawned fn's
  last segment must end with `return`" instead of the clear "not nested in
  if/while" every other nested-tick shape gets. Fixed all six, alongside
  adding their `Stmt::WhileLet` arms in the same pass.
- **`collect_renames` (lower.rs, the spawn callee-body text-rewrite pass)
  had no `Stmt::IfLet` arm either** — a captured param referenced INSIDE an
  `if let`'s own branches, in a spawned callee, never got renamed to its
  private register name, since nothing recursed into `then_body`/`else_
  body` to find that reference. Confirmed live: a hard resolve-error
  failure on the second pass (not a silent miscompile, but a real gap).
  Fixed, with the matching `Stmt::WhileLet` arm added at the same time.
- **A THIRD, older gap found this pass, unrelated to `if let`/`while`/
  `while let` specifically, and later fixed in a follow-up session:** a
  `let`-bound local declared INSIDE a plain `if`'s branch used to fail to
  resolve on EVERY read, not just a second one — a same-branch local read
  even exactly ONCE already failed, confirmed directly (`git stash` back
  to the pre-fix state and re-probed) rather than assumed from an earlier
  characterization of this gap that turned out to be imprecise. Root
  cause: `enter_rule` (writes.rs) only walks a rule's own TOP-LEVEL
  statements to build `locals_snapshots`, and none of the write-threading
  walks that DO recurse into branches (`reg_value_in_stmts` and its three
  siblings — `mem_write_in_stmts`, `struct_field_value_in_stmts`,
  `inst_port_value_in_stmts`) had a `Stmt::Let` arm either, so a
  branch-local's binding was never registered anywhere ANY read could
  find it. Confirmed with a PLAIN `if` (no `if let` involved), so this
  predates every feature on this page. Originally worked around in
  `examples/while_let_drain.tr` by recomputing `cnt - 1` at each use
  instead of binding it once, and pinned as a known gap rather than fixed
  in this pass, since it was a materially different, pre-existing problem
  (branch-body local tracking in general) than anything `while let` itself
  needed to build.
  **Fixed in a follow-up session:** each of the four branch-recursing
  write-threading walks now binds a branch-local `let` into `self.locals`
  (the same lazy `ExprId`-substitution map a callee's own `let`s already
  use) as it encounters one, saved/restored around each recursive branch
  call. Sound here specifically — unlike the SEPARATE callee-local-
  reassignment restriction ("Calling a function from a rule" above),
  which this fix is deliberately NOT the same shape as — because a
  branch-local `let` is bound exactly once and never reassigned via `:=`
  afterward; reassignment of a rule-level local (branch-nested or not) is
  already `locals_snapshots`/`set_pos`'s own working mechanism, untouched
  by this fix. `examples/while_let_drain.tr`
  reverted to the natural form (`let new_cnt = cnt - 1`, read twice) now
  that the workaround is unnecessary — reconfirmed through the FULL
  `<sequences>`/`while let` splice-and-reparse pipeline (not just a plain
  `if`) via real Icarus simulation, since `new_cnt` ends up declared
  inside the re-parsed `if let` segment's own branch body once `while
  let`'s own rendering runs. Pinned by
  `a_branch_local_read_more_than_once_resolves_both_reads` (tests/
  firrtl.rs, renamed from its former known-gap name).

## Spawn, sync, and race lowering

`spawn Callee(args)` is macro-expansion, not module instantiation, consistent
with how every other call is inlined. At the spawn's call site, `Callee`'s own
body is cut into segments by the same segmentation and capture algorithm a
`sequences` rule uses, scoped to a fresh continuation register unique to this
spawn occurrence.

Every register a spawn occurrence needs is named
`__{kind}_{rule}_{handle}[_{name}]`, prefixed by both the enclosing rule and the
handle — not just the handle, since two different rules could otherwise pick
the same handle name and collide:

- `__cont_{rule}_{handle}` — the spawn's own continuation register.
- `__done_{rule}_{handle}` — set once the callee's last segment runs; reset to 0
  on every (re)trigger.
- `__result_{rule}_{handle}` — written by the callee's `return` expression, the
  same save-register pattern used for a value crossing a `tick`.
- `__arg_{rule}_{handle}_{param}` — one per parameter, written once at trigger
  time from the caller's own argument expression. A spawned body's references
  resolve on later cycles, so the argument value has to be latched rather than
  substituted in place, the way an ordinary combinational call's arguments are.
- `__save_{rule}_{handle}_{local}` — one per callee-internal captured local, so
  two spawns of the same callee never collide on a save-register name.

`h.result` reads `__result_{rule}_{handle}`; `h.done` reads
`__done_{rule}_{handle}`.

A spawn's continuation register resets to plain 0, the same as every other
continuation register in this emitter. A callee's own segment-0 rule cannot fire
before it is ever triggered, and cannot race the trigger on the same cycle it
fires, for three structural reasons together: the trigger's own segment rules
are always emitted (and so always win derived-stall priority on any shared
cycle) before the spawn's callee segments; the trigger resets `done` to 0 on
every fire, overwriting any spurious completion; and `sync` cannot observe
`done` before the trigger's own segment has run at least once, since it sits
behind the rule's own mandatory `tick`.

`spawn` staying top-level in its segment (the same restriction `tick` has) is
what makes "spawn counts are static" hold: no loop construct can contain a
spawn, so there is no dynamic spawn count to reason about.

`sync[h1, h2, ...]` lowers to one `(h{i}.done = 1)?` guard per handle, inserted
in place of the call, gating the segment it is written in exactly like any other
guard. The segment containing `sync` does not fire, and does not advance its
own enclosing continuation, until every named handle has finished.

`tick <expr>` is parser sugar, not a new construct this pass has to know about.
The parser desugars it into a bare `tick` immediately followed by `<expr>` as
its own statement — the opened segment's leading statement — so `tick
sync[h1, h2]` and writing `tick` then `sync[h1, h2]` on the next line produce
identical trees. Every pass downstream of parsing (segmentation, capture
computation, sync detection, effect checking) sees only the desugared form.

`race[h1, h2, ...]` lowers to one `((h1.done | h2.done | ...) = 1)?` guard —
the OR-mirror of `sync`'s AND — plus, for EVERY handle named in the group, an
extra `(h{other}.done = 0)?` guard clause on EVERY ONE of that handle's own
segment rules, for every OTHER handle in the same group. This needs no separate
cancellation latch/register at all: a competitor's `done` register IS the
resolving signal, read directly, so there is no one-cycle lag for a stray write
to land through. The moment any named handle's `done` becomes visible, every
other named handle's next segment sees its own extra guard fail and is
permanently blocked — it can never fire again until the enclosing rule
re-triggers it, which also resets every competitor's `done` back to 0. (An
earlier draft tried a separate `cancelled` register, SET the cycle the race
resolves — that is inherently one cycle late: a loser whose own last segment
happens to be ready on the exact same cycle the race resolves would fire
anyway, since the cancellation write isn't visible until the cycle after. Hand-
tracing a two-racer example surfaced this before it shipped; reading `done`
directly has no such gap.)

Two handles CAN legitimately both attempt their final segment on the exact same
cycle (each reads the other's `done`, still 0 mid-cycle, so both fire) — but
this makes their final segments mutually conflict in the ordinary scheduler
(each reads what the other writes), so if they'd otherwise land the same cycle,
the derived-stall machinery picks exactly one by priority (declaration order,
or an explicit `urgency` directive); the loser of that tie stalls one cycle,
then sees the winner's `done` and is permanently blocked. Verified via
`--explain-schedule` on a hand-lowered two-spawn example, and via a real
firtool+Icarus simulation asserting the loser's own `done`/`result` registers
never change (`examples/race.tr` + `sim/race_tb.v`) — not assumed, since a
racing spawn reading another spawn's `done` is a genuinely new conflict shape
(every existing case has the ENCLOSING rule reading a spawn's `done`, never one
spawn reading another's).

`value := race[h1, h2, ...]` (the value-producing form) lowers to the same
guard PLUS one more line: `value := __race_value(h1.done, h1.result, h2.done,
h2.result, ...)` — or `let value = __race_value(...)`, matching whichever form
the destination was originally written with (`plan_rule`/`plan_spawn` detect
both `Stmt::Assign` and `Stmt::Let` shapes at the trigger site). Either way
`__race_value` is a compiler-internal builtin (never written by a user; it
never survives a real `race[h1, h2]` reaching emission, same as `sync`) that
`compile_race_value` (firrtl/calls.rs) compiles straight to a right-nested
priority `mux` — `h1`'s pair outermost, so it wins a tie against a later pair,
the same declared-first-wins convention `prio` uses. This is deliberately NOT
the natural-looking lowering (splice an `if`/`else` picking the winner's
result): a local first bound INSIDE `if`/`else` does not resolve outside it in
this language — confirmed empirically before ruling that approach out. `mux`,
unlike `if`/`else`, is an ordinary combinational expression with no statement-
level scoping, so it composes correctly whether `value` stays an ordinary
same-segment local (substituted as a string wherever read, true same-cycle
semantics, no register) or gets promoted to a captured register (crosses a
later `tick`) — both already-existing, already-generic mechanisms, no special
casing needed for either.

Honestly: this mux's own tie-break priority is unreachable dead code for
handles racing each other. The cancellation mechanism above already makes any
two of a race group's own final segments mutually conflict, so the ordinary
scheduler guarantees at most one of them is EVER actually done — two `1`s
reaching `__race_value` at once should never happen for this construct's real
use. Built as a real priority mux anyway rather than left as an unchecked
assumption (correct and cheap either way, and a safe fallback if that
invariant is ever weakened later) — and confirmed unreachable by real
simulation, not just reasoned about: `examples/race_value.tr` +
`sim/race_value_tb.v` race two spawns that would be ready the identical cycle,
and directly assert the loser's own `done` stays 0 (the scheduler picks
exactly one to actually complete, by spawn declaration order) rather than
`__race_value`'s own priority ever needing to break a real tie.

## FIFO synthesis emission

A depth-1 fifo (the default) is one data register plus one valid bit. `Deq[]`
succeeds only while valid; `Enq[x]` succeeds only while not valid, unless the
same rule both `Enq`s and `Deq`s the fifo (the pass-through case, below). Both
failure conditions combine, by AND, into the rule's own guard signal — the same
signal that carries every explicit `?`.

A rule that both enqueues and dequeues the same fifo combines the two guards
into one: `valid = 1` alone, since this cycle's `Deq` frees the slot this
cycle's `Enq` refills. The `Enq` side's own `not(valid)` guard is dropped for
this specific pairing, not weakened.

A depth-N (`[N]elem_ty`, N>1) fifo is N data-slot registers plus `head`/`count`
pointer registers — a circular buffer; `tail` (the next write position) is
derived, `head + count` wrapped back into `[0, N)`, not stored. `Deq[]`
succeeds while `count > 0`; `Enq[x]` succeeds while `count < N`; the combined
pass-through guard, same reasoning as depth-1, is just `count > 0`. `Deq[]`
reads the slot at `head` (a plain register read, so it sees the pre-edge value
even on a pass-through cycle); `Enq[x]` writes the slot at `tail`. On a
pass-through, `count` is left unconnected rather than computed as
"+1, then -1" from the same pre-edge value — composing those two updates
that way was tried, hand-traced, and found to silently drop the `Enq`'s credit
(a real bug caught before it reached the emitter — see fifo.rs's module doc
comment); leaving `count` unconnected holds it at its current value, correctly
netting to "unchanged" without the composition bug. This whole shape (data
slots, `head`/`count`, wraparound, the pass-through's `count` non-update) was
hand-verified against a real firtool+Icarus simulation of a depth-3 circuit —
chosen non-power-of-2 specifically to stress the wraparound arithmetic — before
being ported into the emitter (`examples/fifo_depth.tr`,
`sim/fifo_depth_tb.v`).

```trace
module FifoPassthrough {
    fifo f : [8]

    in seed : [1]
    in seed_value : [8]
    out last_out : [8] = 0

    rule load {
        (seed = 1)?
        f.Enq[seed_value]
    }

    rule step {
        let x = f.Deq[]
        f.Enq[x + 1]
        last_out := x
    }

    schedule {
        urgency load > step
    }
}
```

A local's value, referenced after being bound (`let x = input.Deq[]`, then `x`
used later), is inlined by recompiling whatever it was bound to — FIRRTL has no
`let`-bound name of its own, only wires and declarations.

## Port-based memory access

Loading and observing a `mem` through ordinary module ports needs no dedicated
compiler machinery: it falls out of `in`/`out` plus an ordinary guarded
write.

```trace
module PortRam {
    mem m : [16][256]

    in addr : [8]
    in write_data : [16]
    in write_en : [1]
    out read_data : [16] = 0

    rule write {
        (write_en = 1)?
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

`write` outranking `read` means a cycle that writes never races a same-cycle
read: `read_data` correctly holds its old value on a write cycle rather than
reading a half-committed word.

Booting a design from a cold, initially-empty `mem` needs one addition beyond
per-word loading: a way to know when loading has finished, since the design's
ordinary rules would otherwise start firing the instant reset clears, racing
the load. The chosen mechanism is an external `boot_done` pulse the loader
asserts when done, rather than a fixed word count baked into the hardware —
this makes no assumption about program length or how the words arrive. A
`booted` register, set once by a `finish_boot` rule gated on `boot_done`, gates
every other rule's guard until loading is complete.

A loader that asserts its own write-enable and `boot_done` on the same cycle
loses that cycle's word, since `finish_boot` outranks the loader by design; a
real loader must deassert its own write-enable before signaling done.

## Submodule emission

A `module` may instantiate another by name (`inst name : Module`), not by
lexical nesting. Modules compose as a flat instantiation graph; FIRRTL has no
nested-module concept, so a lexically-nested `module` declaration still becomes
its own top-level FIRRTL block, found regardless of source depth.

Each instance port is its own conflict resource, distinct from an array's
whole-array resource model: two rules writing different ports of the same
instance do not conflict, and can fire the same cycle. A port name is static and
lexical, known at resolve time, so telling two ports apart needs no runtime
disjointness proof.

Emission finds the one top-level module nobody else instantiates, then walks
the instantiation graph out from it, emitting every reachable module once. A
module instantiating itself, even indirectly, is a compile error rather than
infinite recursion. Every instance's `clock`/`reset` connect unconditionally,
since FIRRTL requires every instance input driven on every path; every other
input port defaults to 0, then the firing rule (at most one) that writes it
overrides via last-connect.

A child's `out` is one cycle behind its inputs, like any `out`; a
parent's own output built from a child's output is a second register hop behind
that. Latency compounds once per hop through the hierarchy — a real,
honestly-modeled consequence of composition.

## Extmodule emission

An `extmodule` is never walked the way an ordinary module is (it has no
body/rules/`inst`s of its own to recurse into, and is never a candidate
for "the top") — it's collected separately (`all_extmodules`) and
declared once, at most, per referenced extmodule: a bare port list plus
`defname = Name`, nothing else (no body, since firtool never needs one —
the real implementation is the referenced `.v` file, entirely outside
this pass's concern). `inst t : TriBuf`'s own wiring reuses the ordinary
instance machinery unchanged (same `types.module_ports`/`find_port` an
ordinary module's ports already populate), with exactly one difference:
no `connect t.clock, clock`/`connect t.reset, reset` — an extmodule
declares no such port to connect one to (v0 restriction, see "Module
ports" above). Every ordinary `in`/`out` port still gets the same
default-0-then-override-on-fire wiring as any other instance input, and
an `io`-kind port (`t.pad`) gets none at all, reachable only via
`attach` — identical to how an ordinary module instance's own `io` port
already works.

## Struct emission

FIRRTL does support a real bundle type (`{ field : ty, ... }`), confirmed by
hand-lowering one through firtool directly (`regreset`, `mux`, and
struct-typed ports all accept it) — but the emitted text here never uses it.
The same hand-lowering probe also confirmed that firtool's own lowering to
Verilog *flattens* a bundle-typed port or register to exactly `{name}_
{field}`, one signal per field. Since that's the shape trace needs
regardless (a struct-typed reg/output/input's fields are read/written
individually, never as one opaque wire), emission skips the bundle detour
entirely: a struct-typed `reg name : S` becomes N plain registers, one per
LEAF field, named `name_field`; a struct-typed `out`/`in` follows the same
`{port}_{field}` naming an `Output`'s existing internal-backing-register
split already uses. `Types::struct_fields` (an ordered `(name, Ty)` list per
struct `DefId`) drives the expansion; every leaf field's type must resolve
to a concrete `[N]` or emission errors, mirroring the "no concrete bit
width" check an ordinary scalar reg already has.

A struct field may itself be another struct — `struct_field_widths`
recurses through any `Ty::Struct` field, joining names with `_` at every
level it descends (`Frame.header.valid` flattens to `f_header_valid`, not
just `f_valid`), so an arbitrarily nested struct produces exactly the same
flat shape a single-level one already did, just carried further. A struct
that directly or transitively contains itself would make that recursion
never terminate — caught up front instead, by `check_struct_cycles`
(types.rs) walking the struct-to-struct field graph once `struct_fields` is
fully populated and erroring on any cycle, so `struct_field_widths` itself
never needs to guard against one.

A struct-typed reg/output has no single `name := ...` statement to find the
way a scalar reg's write-threading walk (`reg_value_in_stmts`) looks for —
the user writes the *whole* struct (`p := Pair{...}`), one literal covering
every field at once. `struct_field_value_in_stmts` mirrors that walk's
if/else mux-threading structure but, on finding a matching whole-value
assignment, pulls out just one LEAF field's own sub-expression per call —
one call per flat register, walking a field PATH (`["header", "valid"]`, not
just `"valid"`) into any nesting depth via `compile_field_path_value`,
shared with the read side (`expr.rs`'s `compile_struct_field_read`, which
peels a chased `.field.field` chain back down to its root value and the same
path before dispatching). `compile_field_path_value` dispatches on the
*current* type at each level it descends (`Ty::Struct` vs `Ty::Option`, see
"Option emission" below) rather than assuming a struct literal all the way
down — needed once a struct field could itself be `?T`, whose own literal
form (`false`, or a bare coerced value) isn't another `Expr::StructLit` to
keep walking structurally. A struct-typed write whose right-hand side isn't
literally a struct literal (`p := q` between two struct-typed regs) is
rejected at type-check time rather than silently compiling to nothing here:
the field-path walk can only decompose a literal, so a non-literal RHS would
otherwise leave the register frozen at its reset value with no error at all.

A struct-typed port on an *instantiated* submodule is rejected outright
(v0): the target module's own emission flattens its port to N real FIRRTL
ports, but the instance-wiring code only ever has the port's bare,
unflattened name to wire by (`module_ports`' entry is still one `(name,
kind, Ty::Struct)` triple) — driving `inst.p` against a module that actually
declares `p_valid`/`p_data` would either reference a nonexistent port or
silently default-wire a single bit (`port_bit_width`'s `unwrap_or(1)`
fallback, built for scalar ports and never meant to see a struct). Caught
explicitly at instance-collection time instead of surfacing as either.

## Option emission

`?T` never emits a real FIRRTL bundle either, for the identical reason a
`struct` doesn't — it flattens to exactly two leaf entries, `valid` (1 bit)
and `data` (`T`'s own flat shape: one register if `T` is `[N]`, or `T`'s
own recursive field list, `data`-prefixed, if `T` is itself a struct or
another `?T`). `option_field_widths` is `struct_field_widths`'s Option
counterpart, sharing the exact same recursion/joining convention.

`?T` has no literal AST form of its own the way a struct literal does —
`opt := false` and `opt := 8'd5` are both just an ordinary expression, not a
dedicated `OptionLit` node. `compile_field_path_value`'s `Ty::Option` arm
(writes.rs) synthesizes `valid`/`data` directly instead of looking for a
sub-expression that doesn't exist: `valid` is a constant `0`/`1` depending on
whether the expression is literally `Expr::Absent`; `data` is the expression
itself when present (there's no separate wrapper syntax to unwrap — the
coerced expression *is* the `T` value) or a don't-care zero when absent.
`data`'s own path continuation (when `T` is itself struct/Option-shaped)
recurses into `compile_field_path_value` with the *same* expression,
dispatched against `T`'s type — presence doesn't add a layer of the AST to
peel off, only a layer of the flat-path naming. `option_lit_field_const`
(mod.rs) is the identical shape one level earlier, computing a `?T`-typed
reg/output's constant reset init instead of a live write's value.

This arm's whole "is it literally `Expr::Absent`, else definitely present"
logic depends on an invariant it doesn't itself enforce: the expression must
be `false` or a plain value of `T` being coerced present, never *another*
`?T`-typed expression. For a state WRITE that invariant comes from
`type_write`'s Option-to-Option rejection (a separate, earlier check) — but
a plain `let` has no target type to check against, so `let o = opt` (`opt`
itself `?T`) types by ordinary inference and reaches this arm with the
invariant already broken, `expr` itself `Ty::Option`. Self-caught while
investigating `?T`-typed fn params (which bind an argument through this
same path): without a guard, this silently concluded "definitely present"
and hardcoded `UInt<1>(1)`/a zero-width constant, completely ignoring
`opt_valid`'s actual runtime value — a real miscompile, not just a missing
feature. Fixed by checking `expr`'s own type at the top of the arm and
returning `None` (routed to a clean error by the caller) whenever it's
itself `Ty::Option` — the same aliasing restriction a struct-typed local
already has (`let p = q`, `p` merely aliasing another struct-typed value,
rejected identically), now closed for Option too instead of silently
"supported" through a bug.

The `?` guard operator, generalized: `opt?` used as a *value*
(`compile_expr_hinted`'s `Expr::Guard` arm, expr.rs) compiles to a read of
`data` off `opt`'s own field path — reusing the identical peeling/dispatch
`.field` access already has, so `opt?` and `opt.data` compile to the exact
same FIRRTL text (the difference is entirely on the *guard* side). `opt?`'s
contribution to a rule's *guard* (`compile_guard_unwrap_cond`, writes.rs) is
computed separately: for a `?T`-typed operand it reads `valid` instead of
compiling the operand as an ordinary `[1]` condition, then folds into
the rule's guard through the same `fails`-folding machinery a fifo
occupancy check or a `<fails>` callee's condition already uses. This fold
only runs at the three positions `check_guard_positions` (checks.rs) allows
a guard to sit at — a whole bare statement, the entire right-hand side of
`:=`, or the entire init of a `let` — mirroring `check_failing_call_
positions`/`check_fifo_op_positions` exactly; a guard nested any deeper (an
arithmetic operand, a call argument, an `if` condition, or a field access
chained straight off it, `opt?.valid`) is rejected outright rather than left
to silently read `data` with no corresponding guard term.
`compile_guard_unwrap_cond` is `pub(crate)` and has a fourth caller besides
`compile_guard`'s own three: `callee_fail_cond` (calls.rs), which folds a
bare-statement guard found inside a CALLEE's own body into the caller's
rule guard. Advisor caught this cross-file site pre-commit compiling a
`?T` guard's inner expression as an ordinary condition regardless of type —
for a `?T` operand that emitted a reference to a register that was never
declared (`opt`, not `opt_valid`), a real firtool-rejected miscompile, not
merely a wrong value; routing it through the same helper closed it.

A `?T`-typed port on an instantiated submodule is rejected the same way and
for the same reason a struct-typed one is (see "Struct emission" above) —
`?T` flattens to `{name}_valid`/`{name}_data` in the target module's own
port list, unreachable by the bare, unflattened name instance-wiring uses.

## Calling a function: inlining

FIRRTL has no function-call concept, so a callee is inlined at its call site:
its body is spliced into the caller, the same mechanism that already resolves an
ordinary rule-local reference.

A call's arguments bind to the callee's parameters the same way a local binds to
its own value. Compiling a call saves and restores each parameter's previous
binding around the call, making calls properly reentrant regardless of how
deeply or indirectly they nest — including a call nested inside an argument to
another call of the _same_ function.

A struct/Option-typed param resolves a `.field` read (`compile_struct_field_
read`, expr.rs) by CHASING through the binding rather than trying (and
failing) to decompose it as a literal: when the bound argument is itself a
plain `Expr::Ident` whose type EXACTLY matches the param's own declared type
— a reg/output/input, or another same-typed param/local — this calls itself
again with that argument as the new root, the identical substitution a real
inliner performs, bottoming out at a reg/output/input's own flat field names
(or, for an argument that's genuinely a literal/coercible value instead, the
existing `compile_field_path_value` decompose/coercion path, untouched). The
exact-type-match guard is what keeps this from misfiring on the ordinary
coercion case: a plain `[8]` argument passed to a `?[8]` param has a
DIFFERENT type from the param (`[8]` vs `?[8]`), so it correctly
falls through to being coerced present rather than chased. This chase-
through is deliberately PARAM-only, not extended to plain rule-level `let`
aliasing (`let o = opt`, no call involved) — that stays rejected exactly as
before (see "Option types" under Part 1), a narrower, separate capability
than "params work" implies.

This same chase-through is what closed a real miscompile found while
building it: `compile_field_path_value`'s `Ty::Option` arm assumed its
expression was always `Expr::Absent` or a plain `T`-typed value coerced
present — an invariant `type_write`'s Option-to-Option rejection enforces
for a state WRITE, but a plain `let` has no target type to check against,
so `let o = opt` (`opt` itself `?T`) typed fine by ordinary inference and
reached the arm with that invariant already broken. Without a matching
guard INSIDE the arm, `o.valid`/`o.data` silently hardcoded `UInt<1>(1)`/a
zero constant, completely ignoring `opt`'s actual runtime value — surfaced
by passing a reg as a param argument (`UseIt(opt)`, binding `o` to `opt` the
exact same way a `let` would), fixed at the arm itself (checking whether the
expression's OWN type is itself `Ty::Option` before treating it as
"definitely present") so it covers both the rule-`let` case (now a clean
error) and the param case (now correctly resolved) from one choke point.

A struct/Option-typed RETURN decomposes the same way a struct-literal WRITE
does — one leaf field at a time — except each leaf's value comes from
re-inlining the callee's own body instead of reading a literal's field
expression directly. `compile_field_path_value` (writes.rs) gained a new
`Expr::Call` case, checked ahead of its existing `Ty::Struct`/`Ty::Option`
literal-shape dispatch: when the expression being decomposed is itself a
call, `compile_call_field_value` (calls.rs) binds that call's params/`let`s
via the same reentrant `bind_callee_context`/`restore_callee_context` the
guard fold already uses, then hands off to `compile_callee_body_field` — a
field-path-aware sibling of `compile_callee_body` with the identical body
shape (`let`s, then a trailing `return`, or an `if`/`else` whose branches
both recurse and combine via a per-leaf `mux`) but extracting just ONE leaf
field's value at the `Return` arm instead of one whole scalar. Each leaf
field re-walks the callee's body and re-binds independently — sharing one
binding across leaves is the exact reentrancy bug `Avg(Avg(x, y), z)`
already taught this codebase not to repeat (see the params-work paragraph
above), now at the return side instead of the argument side.

Before this, `p := MakePair()` (a struct-returning call as a struct write's
RHS) was rejected outright by `type_write`'s literal-only check — with that
gate lifted, an advisor-recommended probe (compile with the gate disabled,
see WHERE it breaks before designing further) surfaced that the write
vanished SILENTLY: `struct_field_value_in_stmts`'s per-statement scan finds
a matching `Assign`, calls `compile_field_path_value`, gets `None` back (no
`Expr::Call` case existed yet), and treats that exactly like "this
statement doesn't write the field" — the same silent-drop shape the
`?T`-aliasing bug earlier this session had, just one call deeper. Closed by
giving the new dispatch its own real error (`compile_call_field_value`
checks whether `validate_call` already reported one — it always does on
`Err` — and only adds its own "too complex to inline" message when neither
did, so a genuine emission failure is never silent either).

The return-side twin of a param's passthrough (`Passthrough(p) { return p
}`) needed its own placement, not a blanket `Expr::Ident` case inside
`compile_field_path_value`: that function is ALSO reached from `compile_
struct_field_read`'s pre-existing Local-arm fallback (an ordinary struct
field READ off a local bound to something unresolvable), and adding Ident-
chasing there fires in THAT context too — a first attempt did exactly this
and silently relegalized a callee-local aliasing a param (`let x = p; return
x.data`), caught by the EXISTING regression test for that exact rejection
failing, not by inspection. The fix lives instead in `compile_callee_body_
field`'s own `Return` arm, checking `ret_expr` directly: only a LITERAL
`return p` (never a param reached by chasing through some intermediate
local) delegates to `compile_struct_field_read`, gated on `ret_expr`'s type
exactly matching the target type — an ordinary `T`-into-`?T` present-
coercion (`return x`, `x : [8]`, callee returns `?[8]`) has a
DIFFERENT type and must still fall through to the ordinary coercion-
synthesis path, a second real regression this fix caught and fixed before
shipping (a probe built specifically for the guard-fold intersection, per
advisor's flagged priority, is what surfaced it: `opt_valid`/`opt_data` were
briefly reading nonexistent `x_valid`/`x_data` registers instead of
coercing `x` present).

Cycle detection on the call graph is a static property of a function's own body
(which other functions it names, found by walking `let` inits, return
expressions, write right-hand sides, and conditions), not a dynamic
"currently inlining" stack: a dynamic stack cannot tell "a function's own body
calls itself" apart from "compiling an argument that happens to invoke the same
function," and the two need different answers.

A call reaching a state-writing callee is validated against the module actually
being emitted, not the callee's declaration site, since a callee visible by
lexical scoping can still reach state that belongs to a different module's own
emitted FIRRTL block.

A state-writing call only reaches the emitted hardware when the call site is a
bare statement or the entire right-hand side of `:=`; nested any deeper (an
argument, a `let`'s init, part of a larger expression) is an explicit error, not
a silent skip, since neither is a shape the writer-hunting walk knows how to
find.

A failing call — one whose callee's own effect signature can fail — is folded
into the caller's rule guard in two independent pieces that both write into
`compile_guard`'s own condition list, one per fail source:

- **Guards**: `callee_fail_cond`, the same reentrant param-substitution
  `compile_call` uses for a return value, run instead against the callee's
  own bare top-level guard expressions — params (AND the callee's own
  top-level `let`s, via the shared `bind_callee_context`/`restore_callee_
context` helpers, so a guard referencing a preceding callee-local resolves
  too, not just a parameter) bind to the call's actual arguments, the
  callee's guard conditions compile under that binding (so `(x <> 0)?`
  compiles to `neq(a, 0)` when called `Classify(a)`, never a reference to the
  unbound parameter name), AND-reduced.
- **Fifo ops**: `fifo.rs`'s `rule_fifo_ops` is the single enumerator every
  fifo-touch question in the emitter routes through — module.rs's per-fifo
  state-transition emission, `compile_guard`'s Enq+Deq pass-through
  precondition, and `check_fifo_op_counts`'s double-op collision check all
  call it, rather than each independently re-scanning a rule's statements
  (three independent scans reaching through the call boundary would drift
  out of agreement with each other, exactly the silent-miscompile class this
  whole area exists to close off). It finds every fifo op a rule performs,
  direct or reached through exactly one bare-statement/`:=`-RHS call to a
  failing callee, so a rule that directly touches a fifo AND calls a callee
  that ALSO touches it combine into one correct pass-through guard, and a
  rule that already touches a fifo directly plus a callee that touches the
  SAME fifo the SAME way is caught by the ordinary double-Enq/double-Deq
  collision check, unchanged. `compile_fifo_op_value` compiles an `Enq`'s
  value under the same `bind_callee_context` binding as the guard fold — so
  a `let`-bound callee-local fed from a `Deq` earlier in the SAME callee (the
  `fifo_bridge.tr` pattern wrapped in a callee) resolves correctly too.

`validate_call` gates both on `check_fails_is_foldable_guard`: a callee is
only eligible when `sig.fails` is _entirely_ explained by bare, top-level
guards and/or fifo ops — checked by comparing "guards (or fifo ops) anywhere
in the body, including inside `if`/`else` branches" against "guards (or fifo
ops) at the top level only" (the shape the folds actually reach) and
rejecting if either pair differs, or a nested call to another failing
function is found. Trusting `sig.fails` at face value here — inlining any
callee that merely CAN fail, without confirming the fold actually reaches
every source of that failure — would silently drop part of the real fail
condition, letting the caller's rule fire on a cycle it shouldn't; the same
silent-drop class `check_writing_call_positions_in` already guards against
for writes. `check_failing_call_positions` extends that same writing-call
positional restriction (bare statement or `:=` RHS only, checked via the
shared `calls_outside_allowed_positions` traversal) to a failing call at the
rule level, for the identical reason. `check_guard_placement` treats a
failing call the same as a bare guard or fifo op for the existing "must
precede any write, not nested in `if`/`while`" placement rule.

A generic callee's own body is type-checked once, independent of any call site,
so an implicit width parameter is never concretely resolved inside it. The
callee's return-value width comes from the call expression's own concrete
instantiation, threaded down as an explicit hint. A builtin argument's width
(needed by `prio`, for example) is resolved the same way, by following the
argument back through parameter substitution to a concrete call site.

## Elaboration lowering

`<elaborates>` calls (DESIGN.md's `AdderTree`) do not go through the ordinary
callee-inlining machinery above at all — they are reduced away entirely before
it ever runs, by a separate pre-pass, `elaborate.rs`.

The first design tried splicing freshly-synthesized `Expr` nodes straight into
the `Ast` during FIRRTL emission, matching how ordinary callee inlining works.
It does not work: `firrtl::Emitter`'s `Ast` reference is shared and read-only
for a real reason — every synthesized node would need real type/width
records, and `types.rs` only ever runs once, before emission, over the
original tree. Emission-time synthesis has no way to backfill that.

Instead, `elaborate.rs` mirrors `lower.rs`'s own sequences-lowering pattern:
`plan` walks ordinary (non-`<elaborates>`) code for the outermost reachable
`<elaborates>` call, interprets it to completion with a real interpreter over
`Stmt`/`Expr` (sequential execution, real `if`/`return` control flow, real
list slicing and `len()`), and reduces it to a string of real trace _source_
syntax — not FIRRTL. `render` splices that text over the call expression's
own span, the same text-splice-then-reparse round trip `lower::render` already
uses. The caller re-runs the whole frontend (lex/parse/resolve/effects/types)
on the spliced source, so every synthesized expression gets ordinary, real
type checking for free, and a nested `<elaborates>` call found while
interpreting (e.g. `AdderTree`'s own recursion) never becomes its own splice
site — the interpreter recurses directly and folds the result into the same
string, so only the outermost call reachable from ordinary code ever needs an
edit.

`wire[T]` has no representation in the interpreter: every list element is an
ordinary `<combines>`-valued expression (never a fifo op, guard, or
state-writing call — none of those can appear in an elaboration position at
all), read exactly as many times as the source references it, so there is no
aliasing/re-instantiation risk `wire` would need to guard against. A list's
concrete length is real only inside the interpreter (`ElabValue::List`);
`Ty::List` (`types.rs`) deliberately never tracks it, the same way a generic
`[N]` callee body is checked once regardless of call-site width.

`MAX_DEPTH` (64) is a hard compiler backstop against non-terminating
recursion, an explicit error rather than a stack overflow or a hang. This
does not contradict "termination of `elaborates` recursion is unchecked in
v0" above — that is a language guarantee gap, not license for the compiler
itself to hang on a malformed or genuinely non-terminating input.

The real pipeline is now a three-stage text-splice-and-reparse chain:
`--elaborate` (this pass) → `--lower` (sequences) → `--firrtl` (emission),
matching `devenv.nix`'s `simulate` script. Elaboration runs first, since
DESIGN.md's own ordering ("runs once, before synthesis") means a
`<sequences>` rule's own tick-segmentation must never see an unreduced
`<elaborates>` call.

## Tooling

Editor support (`editors/vscode/`): TextMate-grammar syntax highlighting for
`.tr`, and "Format Document" wired to `trace - --fmt`.

The formatter (`src/fmt.rs`) is a reindenter, not a pretty-printer. `--`
comments are trivia the lexer discards outright; no token carries a comment's
text or position forward. A pretty-printer that rebuilt source from the AST
would have nowhere to put a comment back, and would silently delete every one on
first format. Reindenting instead walks the token stream only for
brace/paren/bracket depth and rewrites each line's leading whitespace — it never
looks at comments, so it can never lose one. One case beyond plain brace
counting is special-cased directly, not left as a gap: a multiline `impl
... refines Spec` signature (`refines` on its own line, outside any
bracket — the one construct the parser's own grammar, `parse_fn`'s
`skip_newlines()` before `Refines`, allows to continue a signature this
way) gets one extra indent level, matching the `RoundRobin` example below
and `examples/arbiter.tr`. Every OTHER multiline-signature position is
either inside an open paren/bracket (already handled by plain depth
counting) or directly before `{`, which stays at the signature's own
depth by design — no other construct in the grammar needs this kind of
special-casing, so the formatter stays a brace counter plus this one
lexical rule, not a real statement-aware pretty-printer.

One more targeted exception, `split_stray_closers`: a closing bracket
left stranded at the end of a multi-line block's last content line (a
deleted newline, an editor merge gone wrong — e.g. `return x + 20    }`
where the block's `{` opened several lines earlier) gets pushed onto its
own new line before the ordinary reindent pass runs. The discriminator
is unambiguous and needs no statement awareness: a closing bracket whose
matching OPENER sits on an earlier line was already, definitionally, a
multi-line block — never a deliberate single-line style (`if x = 1 { y
:= 1 }`, common throughout `examples/`), since that always has both ends
on the same line. A bracket that's already alone on its line is left
untouched even when other content follows it after (an `if {...} else
{...}`'s first `}`), and a stacked run of closers (`}))`) splits out
together as one unit rather than one bracket per line, because a closer
never itself counts as "content before the next one" — matching how such
a run already renders once it's correctly placed. This still can't touch
a comment: `--` comments run to end of line and are never tokenized, so
a real token (a closing bracket included) can never sit after one on the
same line in valid source — inserting a newline only ever happens
immediately before a real token, never near a comment's span.

A language server (`trace --lsp`, `src/lsp.rs`) backs diagnostics,
go-to-definition, and hover in the VS Code extension. It drives the exact
same `lex -> parse -> resolve -> effects -> types` pipeline `main.rs`
drives for the CLI — the same functions, called in the same order, so
there is only ever one place that knows how to compile a `.tr` file, not
a second implementation to keep in sync. It's a hand-dispatched JSON-RPC
loop over `lsp-server`/`lsp-types` (full-document sync only: every
`didChange` recompiles the whole in-editor buffer, cheap enough at these
file sizes). `Position.character` is a UTF-16 code-unit offset — the LSP
spec's default, and, as of this writing, the only encoding
`vscode-languageclient` actually accepts: it hardcodes `positionEncodings:
['utf-16']` in what it advertises and throws on any `initialize` result
that claims otherwise, so an earlier version of this that declared
`PositionEncodingKind::UTF8` (cheaper — this compiler's own `Span =
Range<usize>` is already byte-based, so UTF-8 would've needed no
conversion at all, and the spec has allowed it since 3.17) failed
immediately against the real client with "Unsupported position encoding
(utf-8)." `LineIndex` (`src/lsp.rs`) does the resulting byte-offset <->
UTF-16-code-unit conversion on every position in and out, using
`str::encode_utf16`/`char::len_utf16` rather than assuming ASCII — this
repo's own `--` comments use em dashes routinely, and a line containing
one would otherwise silently misalign every position after it on that
line. Diagnostics accumulate from whichever pipeline phases actually
ran; matching `main.rs`'s own early-return-on-error chain, a phase after
the first one with errors never runs at all (its `Resolution`/`Types`
would be built on an already-invalid AST) — a file with a parse error
shows only those parse errors, and go-to-definition/hover both need at
least a clean `resolve` pass (definition) or `types::check` pass (hover's
inferred-type text; definition works without it) to answer anything.
Go-to-definition and hover resolve identifier *uses* (looked up through
`Resolution::expr_defs`, the same side table `is_guard_like` and every
other resolve-consuming pass reads) AND declaration sites themselves
(hovering the `counter` in `reg counter : [8]` resolves, not just a later
`counter := ...`) — a declaration's own name is never an `Expr::Ident`
(it's plain data on an `Item`, resolved once at def-creation time), so
`thing_at` (`src/lsp.rs`) falls back to a direct linear scan over every
`resolve::Def`'s own `span` whenever the `Expr::Ident` lookup finds
nothing, resolving straight to a `DefId` with no `ExprId` in hand. Hover on
a declaration site has no `ExprId` to key `Types::expr_tys` with, so it
reads the def-keyed `Types::local_tys`/`Types::state_tys` maps instead —
together they cover every def with a scalar type; a `rule`/`fn`/`module`/
... declaration (neither map has an entry for those kinds) shows just its
kind, the same fallback an untyped use site already had. A fn/spec/impl def
is a further special case of that same fallback: it has no single scalar
`Ty` the way a reg or a local does (its "type" is a whole signature —
params, return, effects), so hovering one renders a Markdown hover instead
of the plain scalar text every other def gets — a fenced ` ```trace `
code block holding the signature exactly as it's legal to write
(`Outer(x : [8]) : [8] <combines>`, sliced straight out of the source at
each param/return type annotation's own span rather than re-derived from a
`Ty`, so it never needs to special-case `Ty::Unknown` or any other display
edge case), followed by a placeholder description line (`"A function."`/
`"A spec."`/`"An impl."`) — real doc comments don't exist in the language
yet, so this is a deliberate stand-in for where one would go once they do,
not a claim that one currently exists. `FnKind::Fn` gets NO leading keyword
in the rendered signature, matching source syntax exactly: `parser.rs`
dispatches a plain function on a bare `Ident` at item position, only
`spec`/`impl` consume a real keyword token (`ast.rs`'s own debug-dump
function prints a synthetic `fn ` prefix there for its own readability,
but that's a debug-only convenience, not source syntax hover should echo).

A `:=` write TARGET (`v` in `v := 1`) is a live `Expr::Ident` use, exactly
like a read, and gets one from `resolve.rs`'s own `expr_defs.insert(lhs,
def)` for it — but `types.rs`'s `type_write` (the function that types a
write, called from `Stmt::Assign` instead of the ordinary `type_expr`)
used to never call `type_expr(lhs, ...)` itself, since a write target has
no "value" to compute a `Ty` FROM the way a read does; it only ever looked
its type up from `state_tys`/`locals` to check the RHS against. Nothing
else populates `Types::expr_tys` besides `type_expr`'s own wrapper, so a
write target's entry there simply never existed, and hover silently fell
back to showing no type at all — reported against an `out` port's own
write site specifically, but the gap was in `type_write`'s single shared
`Expr::Ident` arm, so it was general to every write-target kind (`reg`/
`out`/`fifo`/a reassigned local), not particular to `out`. Fixed by having
`type_write` insert into `Types::expr_tys` itself at the same two points
it already resolves a write target's type — the `state_tys` lookup (used
verbatim) and the `Local` branch (the just-widened `merged` type, the
correct type AS OF that specific write, not the local's final fixed-point
type another write later in the same body might widen further).
An effect keyword itself (`reads`/`writes`/`combines`/`sequences`/
`elaborates`/`fails`/`chooses`, inside a `<...>` list) is a THIRD hover
case, independent of both the use-site and declaration-site paths above:
it's plain syntax on `Item::Rule`/`Item::Fn` (`ast.rs`'s `Effect` struct),
never an `Expr::Ident` and never given a `resolve::Def` either — nothing
else ever references an effect the way a call references a fn, so there's
no def to fall back to matching against. `effect_hover` (`src/lsp.rs`)
scans `ast.items` directly for whichever `Effect::name` span contains the
cursor and renders a small, hand-written Markdown doc — a description and
a runnable example, one per keyword, transcribed from DESIGN.md's own
"Effects" section — same rationale a real doc-comment hover would have,
just hand-maintained since the language has none yet. Works even on a
file with resolve errors (checked before `res`/`thing_at` are needed at
all), matching how diagnostics themselves degrade no further than a
parse error. Deliberately narrow: only an effect's own NAME matches, never
`reads {pc, mem}`'s bracketed argument names — those name real state, not
the effect itself, and get their own, fourth resolution path instead (see
below), not the effect's Markdown doc.

A `reads {a, b}`/`writes {a}` row argument is that fourth case: unlike
every hover target above, it isn't found by anything already in
`lsp.rs` — it's `resolve.rs`'s own `check_effect_args` that already looks
each argument name up (to validate it names real state) and simply
discarded the answer once validated. A new `Resolution::effect_arg_defs:
HashMap<Span, DefId>` keeps it instead, keyed by the argument `Name`'s own
span (an effect-row argument is a plain `Name`, never an `ExprId`, so it
can't join `expr_defs`), populated for every name that resolves to SOME
def — even one the surrounding match then rejects as the wrong kind (a
rule named in `reads`, say) — since the def is still real and still worth
hovering regardless of whether using it there is legal. `thing_at`
(`src/lsp.rs`) tries this as its second case, between a live `Expr::Ident`
use and the `Def::span` declaration fallback, via a new `effect_arg_at`
helper mirroring `ident_at`'s own linear scan. Its return type changed from
`(Option<ExprId>, DefId)` to `(Span, Option<ExprId>, DefId)` to carry this
through: the `Span` is always the actual site under the cursor (an
expression's own span, a row argument's own span, or the declaration's own
span), computed once inside `thing_at` itself rather than re-derived by
each caller from whichever branch matched — `hover`/`goto_definition` used
to each recompute "which span do I highlight" from `Option<ExprId>`, which
would have been actively wrong for a row argument (highlighting the STATE
DECLARATION's span while the cursor sits somewhere else in the document
entirely) had the old two-way `None` case been reused unchanged for a
third, meaningfully different kind of `None`. The compiler is single-file (no
cross-file imports exist yet), so a definition location is always in the
SAME document as the request — no cross-file URI resolution needed
anywhere in `lsp.rs`. Verified end-to-end with a hand-rolled JSON-RPC
client script (not just unit-level): the full `initialize` ->
`didOpen` -> `hover`/`definition` -> `shutdown` -> `exit` sequence over
real stdio framing, confirming correct byte-offset<->position math,
correct definition-jump and inferred-type-in-hover output, and — the one
real bug this caught — a clean process exit (an earlier version deadlocked
on shutdown: `run()` was still holding the `Connection` alive across
`io_threads.join()`, so the writer thread's channel never closed and the
join blocked forever; fixed by moving `Connection` into `main_loop` so it
drops, closing that channel, before the join). Also fuzzed against every
shipped `examples/*.tr` file truncated at five different cut points plus
several hand-picked adversarial prefixes (`""`, `"module M {"`, `"schedule
{ mutually_exclusive {"`, ...) to confirm no pipeline phase panics on
malformed/mid-edit input — a panic would kill the whole server, not just
fail one request, unlike a CLI invocation.

The grammar is regex-based — still pattern matching, not semantic
analysis, so it can be fooled (a
comparison `x < reads` against a variable actually named `reads` reads as
an opened effects list for that one line — see the grammar's own
`#effects-list` comment) — but it scopes the contextual effect/directive
words to where they're actually meaningful instead of matching them
everywhere: `combines`/`sequences`/`elaborates`/`fails`/`chooses`/`reads`/
`writes` only inside a `<...>` effects list (`#effects-list`, anchored on
`<` immediately followed by one of those words, since `<` is also the
less-than operator and this language has no generics to disambiguate
against otherwise), and `urgency`/`mutually_exclusive`/`conflict_free`
only inside a `schedule { ... }` block (`#schedule-block`, anchored on
the real lexer keyword `schedule` itself — no ambiguity risk there,
unlike the effects-list case — with `#schedule-body` recursing through
`mutually_exclusive`/`conflict_free`'s own nested `{ name, name }` lists
so the scope correctly spans the WHOLE block). Confirmed against the
real VS Code tokenizer (`vscode-textmate` + `vscode-oniguruma`, not just
reasoned about), not just the shipped examples: a rule/reg/schedule-
directive name that happens to collide with one of these words (`reg
reads : [8]`, `reg mutually_exclusive : [1]`) renders as a plain
identifier now, and every genuine effect/directive-word usage across
every example under `examples/` still highlights correctly (a raw grep
count of every non-comment occurrence matches the tokenizer's own count
exactly).

# Part 3: Status and prior art

## Milestone: SUBLEQ

The proof-of-concept target is a SUBLEQ CPU: a one-instruction machine whose
read-modify-write-branch step is one natural multi-cycle transaction.
`examples/subleq.tr` implements it; `sim/subleq_tb.v` simulates a small program
(`mem[11] -= mem[10]`, then an unconditional jump, then halt) through real
`firtool` + Icarus, checking both the halt address and the computed result.
`examples/subleq_boot.tr` is the same design loaded through real ports instead
of a testbench poking memory directly, with a `boot_done` handshake gating the
CPU's own rules until loading finishes.

## Testbenches written in trace

`examples/tb_accumulator.tr` proves a testbench can be written IN trace,
not just hand-written Verilog: its `Top` module `inst`s `Accumulator`
(the same DUT `examples/accumulator.tr` uses) and drives it across real
clock cycles via a `<sequences>` rule — ordinary instance-port writes,
`tick`-separated segments, no new language surface needed at all. The
DUT and its testbench have to live in the SAME file (no cross-file
imports exist yet, see "Language features with no synthesis path yet"),
matching `examples/submodule.tr`'s own existing `inst`-two-modules-in-
one-file shape.

The one real difference from writing a testbench directly in Verilog:
`dut.inc := 5` inside a rule is a plain combinational connect, not a
latch — it holds ONLY on the cycle that write actually fires. A value
meant to hold across several cycles has to be re-asserted every one of
those cycles (or stashed in a register and read back), not written once
and left alone the way a hand-written `reg inc = 5;` would be.

Checking the result needs no new construct either: `dut.sum = 26`, the
final statement of `drive`'s `<sequences>` body, is an ordinary bare
comparison — fallible by default (see "Comparisons: fallible by
default"), the SAME mechanism `?`/fifo ops/failing calls already fold
into a rule's guard via `comparison_conds`. Folded into the LAST segment
of a lowered `<sequences>` rule, that comparison becomes a checkpoint for
free: if it holds, the segment fires and the continuation register
(`__cont_drive`) wraps back to 0; if it doesn't, the segment never
fires and `__cont_drive` sticks at that segment forever instead —
verified directly (not just argued): a deliberately-wrong checkpoint
(`examples/tb_accumulator_failing.tr`, `dut.sum = 99`) sticks at segment
6 under both firtool+iverilog and firtool+Verilator, proven by
`tests/sim.rs`'s `tb_accumulator_checkpoint_failure_stalls_and_is_
observable`. `Top` therefore has no output ports at all —
`sim/tb_accumulator_tb.v`'s job shrinks to clock/reset generation and a
hierarchical peek at `dut.__cont_drive` (the same access pattern
`fifo_bridge_tb.v` already uses for a port-less DUT), reporting which
segment a stall happened at if the checkpoint failed. No `assert`
keyword, no new `Stmt` variant, no enable-gating work beyond what
`writes.rs`'s existing guard-folding already does for every other
fallible construct — a checkpoint in a `<sequences>` testbench is just a
comparison in the right position.

## Implementation status

Implemented and proven through real `firtool` + Icarus simulation, one example
per feature under `examples/` with a matching testbench under `sim/`, unless
noted:

- Guarded atomic rules, effects, `reads`/`writes` rows, `fails` inference.
- `sequences`/`tick` lowering (`examples/rmw.tr`, `examples/subleq.tr`).
- `spawn`/`sync` (`examples/fetch2.tr`, `examples/fetch2_gated.tr`).
- `race`, both guard-only and value-producing forms, with true loser
  cancellation (`examples/race.tr`, `examples/race_value.tr`).
- `chooses`/`any`/spec refinement (checked, not synthesized).
- The full expression surface: arithmetic, bitwise ops, static and dynamic
  shifts including arithmetic (sign-extending) `>>>`, unary negate/complement/
  not, bit-select and slice, indexed part-select, sized literals, the `[N]`
  bit-vector type (told apart from a list literal by content, not position).
- Module ports (`in`/`out`), including boot-loading a memory through
  ports with a `boot_done` handshake (`examples/subleq_boot.tr`).
- `io` ports + `attach` (`examples/io_bus.tr`, pass-through wiring) and
  `extmodule` (`examples/extmodule_tribuf.tr`, a real bidirectional
  tri-state bus behind a hand-written Verilog blackbox — proven end to
  end in `tests/sim.rs`'s `extmodule_tribuf_runs_a_real_bidirectional_bus`,
  two instances sharing one wire, driver alternating sides — `devenv
  shell -- simulate extmodule_tribuf` also runs this directly now,
  `simulate` having learned to discover and pass an extmodule's `.v`
  file to iverilog, see TODO.md).
- FIFO synthesis, including the enqueue/dequeue pass-through case
  (`examples/fifo_bridge.tr`, `examples/fifo_passthrough.tr`).
- Port-based memory access (`examples/port_ram.tr`).
- Local reassignment, resolved at each read's own position
  (`examples/reassigned_local.tr`, `examples/checksum.tr`).
- Submodule instantiation, per-port conflict resolution, lexical module
  nesting (`examples/submodule.tr`, `examples/submodule_multi_port.tr`,
  `examples/submodule_nested.tr`).
- Calling a `fn`/`impl` from a rule: values, branching bodies, state-writing
  callees, calls nested through other callees, a callee whose own body is a
  bare top-level guard and/or fifo op (folded into the caller's own rule
  guard, including the Enq+Deq pass-through case split across the call
  boundary — `examples/call_guard.tr`, `examples/call_fifo.tr`), the
  synthesizable builtins `prio`/`trunc`/`pack` (`examples/call*.tr`).
- `logic <expr>`: a fallible expression's success as a plain `[1]` value,
  discharged rather than propagated, with no side effect of its own
  (`examples/logic_probe.tr`).
- `A or B or C`: a fallback chain over fifo `Deq[]` alternatives, with an
  optional infallible default tail (`examples/or_fifos.tr`).
- The `schedule` block: `urgency`, `mutually_exclusive` (checked simulation
  assertion), `conflict_free` (rejected outright on a write/write conflict;
  for a read/write mem pair, its "addresses actually differ" precondition
  gets a checked simulation assertion too — `examples/conflict_free_mem.tr`
  — trusted/unchecked only for non-mem shared state), plus an auto-derived
  third exemption needing no annotation at all: a read/write pair whose mem
  indices are either ALL compile-time-constant integers
  (`examples/mem_disjoint_rw.tr`) or an affine expression of the SAME base
  register/input with the mem's own depth a power of two (`m[i]` vs
  `m[i+1]`, `examples/mem_disjoint_affine.tr`), and provably different
  either way.
- `elaborates`: compile-time tree recursion over a `list`, one-sided list
  slices (`xs[..mid]`/`xs[mid..]`), via `elaborate.rs`'s own text-splice
  pre-pass, not the ordinary callee-inlining machinery (`examples/
adder_tree.tr`, DESIGN.md's own `AdderTree`).
- General `struct` types: declaration, exhaustive-field-checked
  construction, whole-value read/write, arbitrary nesting (a struct field
  may itself be a struct, cycle-rejected), flattened to N plain
  registers/ports per LEAF field with no real FIRRTL bundle ever emitted
  (`examples/struct_pair.tr`, `examples/struct_nested.tr`).
- `?T` option types: `false` (absent)/implicit-coercion (present)
  construction, unwrap via the generalized `?` guard operator (folds into a
  rule's guard the same way a fifo `Deq[]` already does, including through a
  `let` init), non-failing `.valid`/`.data` presence check, `T` itself a
  struct or another `?T` (`examples/option.tr`).
- `?.` safe navigation: multi-hop chained unwrap-and-field-access
  (`opt?.field?.next`), each `?` folding its own condition into the
  rule's guard independently, usable anywhere a bare `opt?` already is
  (`examples/optional_chain.tr`, proven through a genuinely absent
  intermediate hop — not just a structurally-accepted level — in
  `tests/sim.rs`'s `optional_chain_sugar_runs_through_a_genuinely_
  absent_intermediate_hop`). `if let`/`while let` deliberately stay
  single-hop only (v0 restriction, see "`?.` safe navigation" above).
- `rule foo?`: optional/enable sugar, desugaring at parse time into an
  implicit `in` port, a rising-edge shadow register (reset to `1`, not `0`,
  so a port already held high at reset does not spuriously fire the rule),
  an always-firing edge-tracking rule, and a synthesized `conflict_free`
  exemption (`examples/optional_rule.tr`, proven through real reset
  behavior and repeated edges, not just held-high levels, in
  `tests/sim.rs`'s `optional_rule_sugar_runs_through_real_reset_and_edges`).

Not yet implemented:

- **Combinational-only (stateless) modules.** `out` is register-backed by
  design, so a pure function of inputs cannot be expressed without a cycle of
  delay.
- **Array banking / provable disjointness across DIFFERENT bases (tier 3).**
  v0 arrays are one conflict resource each, except that a read/write pair is
  auto-proven disjoint when its indices are either both compile-time-constant
  integers, or an affine expression of the SAME base register/input with the
  mem's own depth a power of two (see "Arrays: one resource each"/"The
  schedule block"). Two DIFFERENT bases (`m[i]` vs `m[j]`, distinct registers)
  stay fully conservative — this is the real tier-3 gap, needing range
  tracking, not a syntactic check — as does any write/write pair regardless
  of index shape.
- **A Verilator simulation path.** Icarus only today.

## Prior art

- **Bluespec / bsc** — transactional-rules semantics; the production
  scheduler. Open source, Haskell (github.com/B-Lang-org/bsc). Used as the
  design reference for the scheduler.
- **Kôika** (MIT) — formally verified one-rule-at-a-time Bluespec descendant,
  in Coq.
- **Filament** (Cornell CAPRA) — timeline types: which cycle each signal is
  valid in. The long-term answer for cross-module combinational-loop safety.
- **Dahlia** (Cornell CAPRA) — time-sensitive affine types for predictable
  memory banking. The long-term answer for array-conflict precision (tier 3).
- **Clash** — pure functional signals, reference point.
- **Calyx** — IR with a control language (`seq`/`par`/`if`/`while`) separate
  from structure; candidate alternative target.
