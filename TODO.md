# TODO

## Dependent/refinement type system (Lumi's most immediate large-scale goal, 2026-08-05)

Design committed — see DESIGN.md's "Toward a dependent/refinement type system
(SMT-backed, planned)" section (right after "Statically proven register bounds"). SMT
(Z3, QF_BV), chosen directly by Lumi over a hand-rolled generalization of `bounds.rs`'s
own interval engine, for the generality: it can plausibly prove
`examples/circular_buffer_disjoint.tr` as an ordinary refinement composition instead of
the bespoke `invariant`/joint-guards mechanism that currently exists.

Stage 1 (faithfulness) is SUBSTANTIALLY COMPLETE — every measured `Skipped`-site gap
closed except one deliberately-unattempted sub-case (below). `bounds.rs` split into
`src/bounds/{mod.rs, smt.rs}` (it was the clear size outlier) to make room for this.
`pkgs.z3`/the `z3` crate are wired in (`tests/z3_smoke.rs` pins the linking); scalar
reg/out/param bounds (v5+, including nested calls and commuted `Ne` guards), mem-element
bounds (write-side, v17), struct-field bounds (v18, both directions — write-side named-
field composition AND read-side composition off a Reg/Output), param bounds at any
recognized call position (v12), return bounds (v13), AND the relational invariant
(`circular_buffer_disjoint.tr`'s own case, v20) are all shadow-checked against the existing
interval engine on every
`bounds::check` call, zero mismatches across the whole suite. The relational invariant is
checked via a genuinely general `fire_R` transition encoding (`smt::check_relational_
obligation`) rather than mirroring the hand-rolled mask-enumeration loop — this is the
FIRST half of DESIGN.md's own acid test passing (both of `circular_buffer_disjoint.tr`'s
invariants independently proven by Z3); the second half (collapsing `schedule.rs`'s own
`m[head] != m[tail]` proof into one generic query) was stage 4's separate work — see below,
now done. See DESIGN.md's own stage-1 entry for the three real bugs found extending
mem/struct-field/scalar coverage (all in the shadow check's own reconstruction, not in
`bounds.rs`) — including a genuine vacuous-hypothesis hazard (an unreachable `else`
branch's contradictory guard made the SMT query "prove" anything for free) caught exactly
as the pre-implementation review warned it could be.

A `Skipped`-site census (a temporary debug counter, run once across the whole suite then
removed) turned "still needed" from a guess into a measured list — corrected once already:
the first pass logged the raw `expr_bound` result as evidence the interval engine "proved"
a site, which is wrong (a computed range still has to clear the three-way in-bound check);
re-run with the actual boolean before writing any code against the wrong list. The
corrected census: every `Skipped` site is either a deliberate scope cut already working as
designed (the unreachable-hypothesis case; the relational invariant's `n <= 2` cap, which
the `fire_R` encoding itself doesn't need but this shadow check still honors on purpose —
NOT something to lift for stage 1), or nothing-to-compare (the interval engine also has no
verdict there — this bucket now also correctly includes `commuted_lt_is_not_recognized`,
which the ORIGINAL census wrongly called a gap: that test pins the interval engine
DELIBERATELY rejecting a commuted `Lt`/`Gt`/`Ge` guard too, per `narrow_for_condition`'s own
doc comment — both engines agree, so recognizing that shape in the SMT check would make it
strictly MORE capable than the engine it shadows, which is out of scope for stage 1), or one
of three real, confirmed-exercised translation gaps: (1) a commuted **`Ne`** guard only
(`5 <> i` vs `i <> 5` — `Ne` alone is genuinely order-independent in `narrow_for_condition`,
unlike `Lt`/`Gt`/`Ge`), (2) a struct-field READ composed into an rhs expression (subsumes the
`..base` case — one instance of "reaches into a struct field," not a separate mechanism),
(3) a call nested inside arithmetic/another call's argument rather than a top-level
position. A fourth, related but distinct gap was also found and filed separately, not folded
into (3): a call inside a GUARD's own operand (`if Bump(50) < 5`) makes that whole guard
untranslatable, conservatively skipping every write in that branch regardless of whether it
actually depends on the call — dropping the guard's hypothesis instead of aborting was
considered and rejected as unsound (it can silently defeat the vacuous-hypothesis check from
bug 3, reintroducing that exact hazard through a side door). See DESIGN.md's own stage-1
entry for the full census breakdown and exact test names.

Gap (1) is now CLOSED — `translate_guard` recognizes commuted `Ne` (mirroring `ident_const_
operands`'s own `Ne`-only fallback exactly), zero mismatches. Gap (2) is now CLOSED too —
`translate_expr`'s new `Expr::Field` arm recognizes a struct field read off a bare Reg/Output
`Ident` (via a new `Checker::struct_field_bounds_by_def` helper, re-keying `struct_field_
bounds` by variable instead of struct type), which turned out to close all three named test
sites at once: the `..base` complexity in one test's name is entirely on the WRITE side
(already covered by the existing struct-field write obligation), not the READ this gap was
actually about. Gap (3) is now CLOSED too, via TWO mechanisms — a distinction the first pass
at this fix got wrong and stated incorrectly here, caught by re-running the same
`Skipped`-site verification used for gaps (1)/(2) (skipped for this one initially, then
re-applied): `shadow_check_call_params` recurses into a `Call`'s own args, a `Bracket`'s own
callee/args, and an `Add`/`Sub`/`Mul`'s own operands (exactly the three shapes the census
found actually exercised, not the fully general `check_calls_in` sweep) — this closes a
nested call's own PARAM-argument check, but does nothing for composing a call's own RETURN
value into the enclosing write's bound (`Bump(3) + Bump(4)`), which needed a separate new
`translate_expr` `Expr::Call` arm (keyed by call-SITE `ExprId`, not callee `DefId`, so two
calls to the same fn with different arguments aren't wrongly correlated). That second fix
also surfaced a real double-push risk in `shadow_compare`'s own interval-engine
reconstruction (now guarded by snapshotting/truncating `self.errors`), confirmed necessary
via a synthetic repro, not just theorized. Only the guard-embedded-call sub-case remains,
deliberately unattempted (dropping an untranslatable guard's hypothesis to reach it is
unsound — see DESIGN.md). With that one deliberate exception, stage 1's shadow check now
agrees with the interval engine on every obligation shape either engine can currently prove
anything about.

Z3's own linked version is now pinned too (`tests/z3_smoke.rs`'s `z3_linked_version_matches_
the_devenv_pin`, mirroring the firtool/iverilog/verilator pins in `devenv.nix`'s `simulate`
script), and `--explain-schedule` is confirmed byte-identical across all 84 examples against
the pre-stage-1 baseline commit — stage 1's own stated success criterion, actually run.

**Retiring the old interval-arithmetic engine is stage 4's work, not stage 1's** — even
once the gaps above close. `expr_bound` can't be deleted in stage 1 regardless of
shadow-check coverage, because `site_ranges` (stage 1's own explicit scope cut) is
populated as a side effect of `expr_bound` itself and `schedule.rs`'s `real_range` still
consults it; deletion rides along with `real_range`'s own retirement at stage 4 — done, see
below.

**Stage 2 done, and "unify the six maps" turned out to be the wrong framing — corrected,
not completed as originally stated.** First sub-step (provenance): `Proven<T>` now lives in
its own child submodule with a private field, so `Proven::checked` (called only by the
four `collect_one_*` collectors right after their own const-fold/width verification) is
the ONLY way to construct one; `self.bounded`/`mem_bounds`/`struct_field_bounds`/
`fn_ret_bound` now store `Proven<BoundedDef>` instead of a bare struct anyone in the module
could build ad hoc. Second sub-step (the actual "six maps" duplication): a follow-up
advisor pass, asked for the concrete `RefinementTarget` shape before ~40 call-site edits,
found merging the four maps behind an enum key doesn't pay for itself — the "recognize
this LHS's obligations" step still has three genuinely different AST-shape branches
post-merge, "compute the value to check" still needs a match on target kind (`struct_
field_bound` is a fundamentally different function from `expr_bound`, not the same logic
duplicated), and `fn_ret_bound` isn't even a write-obligation target (a return
postcondition is checked at `Stmt::Return`, a different statement position entirely) — so
it has no business sharing a key space with things a `Stmt::Assign` checks. **The four maps
stay separate, deliberately: different key spaces, different narrowing behavior, and one
isn't the same kind of obligation.** What actually collapsed: the TAIL. `check_stmt`'s
`Stmt::Assign` arm and the shadow-check's mirrored `Stmt::Assign` handling each built a
`Vec` of obligations from the three (unchanged) shape-tests, then drained it in ONE loop
doing `found_writes.insert`/`check_against_bound` (resp. `shadow_compare`) — previously
written out three times with an identical body. Added `struct_field_write_with_two_
bounded_fields_checks_both` (no existing test exercised two bounded fields on one struct
write, and an obligation-dropping bug is exactly what this refactor could hide behind a
suite where every other example has at most one obligation per write). Byte-identical
`--explain-schedule` across all 84 examples, zero regressions (839 tests now). See
DESIGN.md's stage-2 entry for the full reasoning — worth reading before re-attempting a map
merge here, since the concrete reasons it doesn't pay off are specific, not just "seemed
like more code."

**Stage 3 started — its own "arbitrary boolean expressions" bullet was under-specified,
corrected before writing code.** The parser hardcodes `where`'s top-level relation to `Lt`
at PARSE time (`parser.rs`'s `parse_where_bound`), and this language has no `&&`/`||` at
all, so "arbitrary boolean expression" isn't reachable without a new grammar operator —
out of scope for this stage. Corrected v1 scope: any single comparison (`<`/`<=`/`>`/`>=`/
`==`/`!=`) over `_` and in-scope defs, with non-literal operands — still a real capability
increase (commuted forms, arithmetic limits, cross-def bounds), no new operator needed.

First landed: a real, currently-shipping soundness bug found while surveying the grammar,
fixed standalone (not folded into the stage-3 diff). `bounds.rs`'s own `const_fold` handled
only a bare literal, while `types/eval.rs`'s `const_eval` (used by the type-checker's own
init-satisfies-bound check) also folds literal arithmetic — so `reg cnt : [8] where cnt <
8 + 2 = 0` type-checked clean while the bound silently never got registered in `self.
bounded` at all, leaving `cnt := cnt + 100` completely unchecked with zero diagnostic.
Fixed by widening `const_fold` to match `const_eval`'s arithmetic exactly (`Add`/`Sub`/
`Mul`/`Div`/`Rem`/`Shl`/`Shr`, `checked_*` semantics). One narrower residual, documented not
silent: `const_eval`'s `clog2(..)`/`max(..)`/`min(..)` cases still aren't recognized (no
`Resolution` access to identify the builtin from a free function) — no test/example needs
any of them in a where-bound today. Regression test added, byte-identical `--explain-schedule`
(nothing in the existing suite used a non-literal where-bound), zero regressions.

`_`-placeholder unification is done: `resolve.rs`'s `check_bound_self_reference` (reg/out/
param) now accepts `_` alongside the literal name (mem/return/struct-field have required
`_` since their own v18 retrofit; reg/out/param never needed shape-based recognition since
they have a real `DefId`, so were never retrofitted to also accept it). `bounds.rs`'s own
collectors never read the bound's LHS once resolve.rs approves it, so this was a pure
resolve.rs change. Six new tests, byte-identical `--explain-schedule`, zero regressions
(867 tests now).

**Stage 3 is DONE.** Its real capability increase turned out to be comparison-operator
generalization, not "cross-def bounds" as first scoped — a follow-up advisor pass caught
that `where _ < other_reg` isn't a bound at all (a mutable def's value varies per cycle, no
fixed interval to check or export), it's `invariant`'s own mechanism; unifying the two
surface forms is the end-state goal, not this stage. `parser.rs`'s `parse_where_bound` now
accepts `<`/`<=`/`>`/`>=` (was `<`-only), normalized by a new shared `ast::normalize_where_
relation` into the same `(lower, upper)` `BoundedDef` already stores — no representation
change. Self may be on either side (commuted forms fall out for free once `resolve.rs`
checks both, not just `lhs`). Two real bugs found and fixed along the way: a genuine parse
ambiguity (`<=` is now both a one-sided operator AND the two-sided form's own separator,
fixed via lookahead — an existing test pinning the old restriction was rewritten, since that
restriction was exactly what this stage relaxes), and `types/stmt.rs`'s own init-value check
silently using the stale `Lt`-only reading (fixed by sharing the normalization function
instead of letting two modules keep independent copies — the same class of bug as the
`const_fold`/`const_eval` drift closed earlier this stage). Byte-identical `--explain-
schedule`, zero regressions, zero shadow-check panics (878 tests now). See DESIGN.md's
stage-3 entry for the full detail.

Deliberately still out of scope: `==`/`!=` operators, non-literal-but-constant limits
(elaboration params — no example needs this), and any bound referencing a mutable def (see
above).

**Real, independent bug found and fixed via a live user report (between stages 3 and 4): a
bare comparison statement (no `if`, no `?`) didn't narrow bounds, even though DESIGN.md's
own "Comparisons: fallible by default" section (predates this arc) already established it
gates the whole rule, and `effects.rs`/`types.rs`/`firrtl/writes.rs` all already implement
that correctly (confirmed by a passing firrtl.rs test). `bounds.rs`'s own `check_stmt` never
did — a write after a bare guard was checked against the unnarrowed bound, rejecting valid
programs. Fixed by recognizing the same shapes `is_guard_like` (the shared predicate every
other pass already routes through) recognizes, narrowing `state` forward in place (no nested
branch needed, since a false guard means the rule doesn't fire at all). A SECOND bug
surfaced while fixing the first: `shadow_walk_body` (the SMT shadow check's own independent
reconstruction) wasn't touched, and the affected program compiled with ZERO panic even
before that second fix — the mismatch panic only ever compares the shadow check's own
reconstruction against Z3, never against `check_stmt`'s real live result, so both staying
equally stale masked the divergence entirely. This is a real, demonstrated risk in this
arc's own hand-mirrored-code architecture (stage 1's shadow check and the real engine are
independently maintained, not one shared path), not just a one-off bug — worth remembering
for any future `check_stmt` change. Both fixed together, same commit. Three new regression
tests (bare form, explicit `(cond)?` form, forward-only negative control). Byte-identical
`--explain-schedule`, zero regressions, zero shadow-check panics (881 tests now). See
DESIGN.md's own entry for the full detail.

**Stage 4 is DONE — the dependent-type-system arc's acid test now fully closes.**
`schedule.rs`'s whole `IndexForm`/`forms_differ`/`real_range` chain (eight hand-built
disjointness arguments) is deleted outright and replaced with one call per index pair to
`bounds::provably_disjoint_mem_indices` (`src/bounds/mod.rs`, wrapping a new Z3 query in
`src/bounds/smt.rs`), fed by a new `bounds::guarded_mem_accesses` walk pairing each mem-index
site with the guards active there. Two real gaps found and fixed mid-implementation, not
during the earlier design pass: (1) a genuine soundness question — the old `IndexForm` used
WRAPPING arithmetic on purpose, while `smt.rs`'s existing `translate_expr` computes non-
wrapping at 64 bits; reading `firrtl/expr.rs`'s actual emission confirmed real hardware
truncates each arithmetic node to ITS OWN declared width, resolved (via advisor) by
truncating the WHOLE index expression to the mem's real address width only once, at the
end, gated by a new `leaves_wide_enough` check (sound only when every leaf def's own
declared width is at least the address width); (2) the ported `circular_buffer_disjoint.tr`
direct-call test failed after the rewrite because `translate_guard` (shared with `bounds.rs`'s
own obligation checks, deliberately capped to stay a faithful shadow of `narrow_for_
condition`) can't translate either of that example's own load-bearing guards (`push_count -
pop_count < 8`, `push_count <> pop_count` — neither is a bare `ident op const`) — fixed with a
new `translate_condition`, scoped to this query alone, that has no such faithfulness
constraint to honor. Two existing tests pinning "stays unprovable" (a `Sub`-based index under
a proven bound; a non-power-of-two depth) turned out to be limitations of the OLD engine, not
real semantic boundaries — updated to the new, correct `Exemption::Disjoint` result, with the
reasoning recorded inline rather than silently flipped. New demonstrating example
(`examples/mem_disjoint_non_power_of_two.tr`) plus matching `tests/firrtl.rs`/`tests/
schedule.rs` coverage. Byte-identical `--explain-schedule` across all 84 PRE-EXISTING examples
against a pre-change baseline binary (the real behavioral change this stage's own success
criterion expects doesn't happen to move any CURRENT example), zero regressions (883 tests
now), clippy clean. See DESIGN.md's own stage-4 entry for the full detail.

**The dependent/refinement type system rollout (stages 1–4) is now complete.** The acid test
(`examples/circular_buffer_disjoint.tr`, both invariants proven AND `m[head] != m[tail]`
proven disjoint, no `Item::Invariant`-specific induction and no bespoke `schedule.rs`
disjointness argument anywhere) passes end to end.

## Emission (`src/firrtl/`)

- Calling a user `fn`/`impl` from a rule inlines the callee at its call
  site; see DESIGN.md's "Calling a function from a rule" for the full
  design history. Remaining real restrictions, not just historical
  color:
  - A callee body must be zero-or-more `let`s + state writes, then a
    trailing `return` or an `if`/`else` (mandatory `else`) recursing
    into that same shape. Bare, top-level guards AND fifo ops are now
    both allowed — folded into the caller's own rule guard
    (`check_fails_is_foldable_guard`, `callee_fail_cond`, `fifo.rs`'s
    `rule_fifo_ops`/`compile_fifo_op_value`) — but only when the
    callee's ENTIRE fail condition reduces to that shape: a guard or
    fifo op nested inside one of the callee's own `if`/`else` branches,
    or a nested call to another failing function (v0 folds one level
    only), still reject outright, explicitly, rather than fold only
    part of the real condition. A callee-local feeding a `Deq` into a
    later `Enq` must be `let`-bound, not `:=` (see DESIGN.md's "Calling
    a function from a rule"). A `while` loop in an ordinary
    (non-`<elaborates>`) callee body remains entirely unimplemented — no
    unroll-and-splice machinery exists for it, not scoped to be lifted,
    just not yet attempted. `<elaborates>` recursion/unrolling (DESIGN.md's
    `AdderTree` example) is separate, already-built machinery — see below.
  - A nested call composes as a value, or as a state-writing bare
    statement / `:=` RHS, as long as it doesn't form a call cycle
    (static call-graph check) — nested any deeper than those two write
    positions (an argument, a `let`, `Bump(a) + 1`) is a clean, explicit
    error.
  - Of the builtins, `prio`/`trunc`/`pack`/`zext`/`sext`/`popcount`/
    `reverse`/`rotl`/`rotr`/`mux`/`logic` are synthesizable as calls;
    `clog2`/`len` are compile-time-only (`Ty::Int`), and `wire`/`list`/
    `any`/`sync`/`race` aren't applicable to a plain combinational callee
    body at all (the old explicit `bits[N]` spelling no longer has
    surface syntax at all — see the `[N]` entry below). `rotl`/`rotr`'s
    rotate amount can be either a compile-time constant (a single static
    two-slice `cat`) or a dynamic expression (a double-width `cat` fed
    through `dshr`, `n` reduced modulo `value`'s own width via `rem`).
    `max`/`min` are DUAL-purpose (the only builtins that are): variadic
    (≥2 args), and when every argument is itself `Ty::Int` they're
    compile-time-only, foldable inside a type-position width expression
    matching two differently-sized generic params up to their common
    width (`bits[max(n, m)]`, folded pairwise in `const_eval_expr`,
    `types/eval.rs` — that folding has no error-reporting side channel,
    so a bad arity there silently resolves to `Width::Unknown` rather
    than a named diagnostic, same pre-existing shape `clog2` misuse
    already has); the moment at least one argument is a real `Bits`
    value, they're a genuine synthesizable comparator+mux instead
    (`compile_max_min`, left-folded pairwise `mux(gt/lt(next, acc), next,
    acc)`, a bare `Int` sibling absorbing and checked to fit exactly like
    `type_binop`'s own `(Bits, Int)` rule). Called directly as an
    ordinary VALUE (not in a type position) DOES validate arity, in
    `type_builtin_call`, for both cases. The two directions' result width
    is asymmetric, NOT both "max of the arguments' widths" like `mux` —
    `max` needs its widest `Bits` argument's width, `min` only its
    NARROWEST (`min(a, b)` can never exceed either operand) — caught by
    advisor review before shipping (reusing `max`'s width rule for `min`
    too silently rejected legal narrow-target writes), fixed alongside a
    trailing `bits(acc, w-1, 0)` truncation in `compile_max_min` (FIRRTL's
    own `mux` width is max-of-arms regardless of which direction built
    the chain, so `min`'s narrower declared width needs bringing back
    down explicitly — the same class of gap `compile_popcount`'s own
    truncation exists for).
    `zext`/`sext`'s width argument reaching `hint` (fixed this session —
    it was silently dropped, never threaded past `compile_builtin_call`,
    latent until `max`/`min` gave it a real generic-body caller) now
    resolves a direct return/write-target position but still fails
    cleanly, not silently, when nested inside further arithmetic in a
    generic body (`return zext(x, max(n,m)) + zext(y, max(n,m))`) — the
    identical boundary `trunc`'s 1-argument form already has, `compile_
    binop` computing each operand's hint from its own static type rather
    than an outer one.
  - A generic callee body's own width resolution (`Emitter::
    resolve_bits_width`, `src/firrtl/expr.rs`) chases a value through
    `self.locals` substitution, arithmetic/shift/bitwise combination
    (`combine_bits_width`, shared with `type_binop`), the synthesizable
    builtins' own result-width rules (`prio`, `pack`, `popcount`,
    `reverse`, `rotl`/`rotr`, `mux` — NOT `trunc`/`zext`/`sext`, whose
    result width is a separately-spelled argument rather than a function
    of their value argument's width, so they rely on the `hint` mechanism
    instead, same as `trunc`'s own 1-argument form), and a nested call to
    ANOTHER user-defined generic function (`resolve_nested_call_width`,
    re-running `type_call`'s own `env`-building + return-type evaluation
    at emission time) — covering a bare literal beside a generic-width
    value nested anywhere in the body (guards, shifts, further arithmetic
    on a builtin's or a nested call's own result), not just a direct
    return. Two params sharing one implicit width name whose arguments
    resolve to genuinely conflicting widths still fails cleanly ("no
    concrete width"), not a miscompile — the one shape left where a
    call's own width truly isn't well-defined.
  - Any two-argument call — a user `fn`/`impl` or a builtin alike — also
    has a Haskell-style backtick infix spelling (`parser.rs`, `` Backtick
    `` token/`BACKTICK_BP`): `` a `Avg` b `` is exactly `Avg(a, b)`, ``
    a `max` b `` is exactly `max(a, b)`. Pure parse-time sugar — builds
    the identical `Expr::Call` node a prefix call would, so nothing
    downstream (resolve/effects/types/firrtl) needs to know the surface
    spelling was infix. Binds tighter than every named binary operator
    (Haskell's own default `infixl 9` backtick fixity) but looser than
    prefix/postfix; left-associative. Always exactly two arguments — a
    variadic builtin like `max`/`min` still needs its ordinary prefix
    call syntax for three or more.

  See `examples/call.tr`, `examples/call_branch.tr`,
  `examples/call_writes.tr`, `examples/call_prio.tr`,
  `examples/call_trunc.tr`, `examples/call_pack.tr`,
  `examples/call_zext.tr`, `examples/call_sext.tr`,
  `examples/call_popcount.tr`, `examples/call_reverse.tr`,
  `examples/call_rotl.tr`, `examples/call_rotr.tr`,
  `examples/call_rotl_dynamic.tr`, `examples/call_rotr_dynamic.tr`,
  `examples/call_mux.tr`, `examples/call_max_min.tr`,
  `examples/call_max_min_mixed_width.tr`,
  `examples/generic_width_max_min.tr`, `examples/call_backtick.tr`,
  `examples/call_nested.tr`, `examples/call_nested_writes.tr`,
  `examples/call_guard.tr`, `examples/call_fifo.tr`.
- `<elaborates>` recursion/unrolling (`src/elaborate.rs`) exists —
  DESIGN.md's own `AdderTree` example (compile-time tree recursion over a
  `list[[N]]`, one-sided slices `xs[..mid]`/`xs[mid..]`). `MAX_DEPTH`
  (64) is a hard compiler backstop against non-terminating recursion —
  DESIGN.md states termination is unchecked in v0 as a language
  guarantee, but the compiler itself still won't hang. `while` inside
  `<elaborates>` code stays unimplemented (v0 restriction: use
  recursion, `AdderTree` style). See `examples/adder_tree.tr` +
  `sim/adder_tree_tb.v`.
- Expression surface still excludes:
  - A fully dynamic slice (`x[hi..lo]` with a non-const `hi`/`lo`) —
    its result width would be dynamically sized, which this language
    can't express, so it's a compile-time type error by design, not a
    gap to close. Use `x[base +: width]`/`x[base -: width]` (dynamic
    start, static width) instead.

  Everything else is implemented: arithmetic/bitwise/compare ops, static
  AND dynamic-amount shifts (logical `<<`/`>>` and arithmetic
  sign-extending `>>>`), unary `-`/`~`/`not`, `/`/`%`, static AND dynamic
  bit-select/indexed part-select, Verilog-style sized literals (+
  inferred `reg`/`out` types from one), the `[N]` bit-vector type (`bit`/
  `uN` sugar retired in favor of it — see below).
  See DESIGN.md's "Expression surface" section for the full history and
  width-rule derivations (several confirmed empirically against real
  firtool, not assumed from spec text); `examples/alu.tr`,
  `examples/dynamic_shift.tr`, `examples/arith_shift.tr`,
  `examples/div_rem.tr`, `examples/sized_literal.tr`,
  `examples/infer_reg_ty.tr`, `examples/dynamic_bit_select.tr` for
  coverage.
- A local reassigned at a rule's top level now resolves each reference
  at its own textual position (still rejected inside `if`/`else` — no
  example needs that, and it's a materially different, larger change).
  One narrow remaining case still rejects reassignment explicitly: a
  local whose width never resolves to a concrete `[w]` anywhere in
  the rule (e.g. used only as a mem-read index). See DESIGN.md's
  "Locals" section, `examples/reassigned_local.tr` +
  `sim/reassigned_local_tb.v`.
- A memory written to more than once (unconditionally) in one rule is a
  compile-time error, closing a real silent-miscompile gap a fifo/spawn
  audit found. See DESIGN.md's "Memory, fifo, and submodule
  declarations" section. (Its erstwhile companion gap — a `let`-bound
  value crossing a `tick` being rejected — is gone entirely now; see
  below.)
- **`let` required everywhere; `let` can now cross a `tick` (Verse-
  alignment pass).** One new restriction, not possible before this
  change (previously ANY `let` crossing a tick was rejected outright, so
  it couldn't arise): two textually-distinct `let x = ...` bindings that
  both cross a tick within the same rule (shadowing — different
  `DefId`s, same name) would otherwise both become captures sharing one
  save-register name; `compute_captures` now rejects this directly
  ("shadows another captured value of the same name") instead of
  emitting two colliding `reg x` lines that would fail to re-resolve
  downstream.
- A callee-local reassigned via `:=` is rejected outright, not silently
  dropped at FIRRTL emission. One check, `check_no_reassigned_locals_in_
  callee_body` (firrtl/checks.rs), run once at `validate_call`
  (calls.rs) — the single choke point every inlining entry point
  (`compile_call`, `callee_fail_cond`, `call_writes_reg`/
  `call_writes_port`, `compile_call_field_value`) already goes through.
  Recurses into `if`/`if let`/`while`/`while let` bodies too, so a
  branch-nested variant is caught the same way.
  **The REAL fix this rejection defers, not attempted here:** extending
  `locals_snapshots`-style eager-text, position-indexed local resolution
  into callee inlining — `self.locals` would need to stop being a plain
  `HashMap<DefId, ExprId>` for scalar locals specifically, while STAYING
  an `ExprId` map for struct/`?T`-typed params (`compile_struct_field_
  read`'s param chase-through resolves those structurally, not by
  scalar value) — two interacting mechanisms, not a type swap. A naive
  fix (just start tracking `Stmt::Assign`-to-Local in `self.locals`, the
  same way `Stmt::Let` already is) was tried and REJECTED before
  shipping: `let z = x; x := x + 1; return z` currently (correctly, if
  accidentally) returns `x`'s value AT BIND TIME for `z`, since the
  reassignment is dropped entirely today; naively threading the
  reassignment through the lazy `ExprId` map would make `z` resolve to
  `x`'s NEW value instead, since `self.locals[z]` stores a live
  reference to `x`, not a copy of what `x` was at the time. A future
  session picking this up should start from the `let z = x` finding
  above, not from the originally-reported case alone, since that's the
  shape a partial fix keeps getting wrong.
- `list[T]`/`wire[T]`'s own single argument sugars a bare width —
  `list[8]`, `list[N]`, `list[clog2(N)]` are all shorthand for
  `list[[N]]` (that is, `list[bits[N]]`); `list[Pair]` (a list of some
  OTHER type, e.g. a declared struct) is unaffected.
  Confirmed a sharper edge: `eval_elem_ty`'s "already type-shaped" check
  only special-cases an `Ident` that resolves to a declared `Struct`; a
  misspelled struct name (or any other free identifier, e.g.
  `list[SomeModule]`) is indistinguishable from a genuine implicit width
  param, so it's silently accepted as `list[bits[Unknown]]` rather than
  erroring "expected a type here" the way it did before this change.
  Documented as a known sharp edge in DESIGN.md rather than fixed — a
  bare identifier is a genuine ambiguity the sugar can't resolve without
  losing the implicit-param case it exists to support.
- `let {field, field: bind, ...} = source` struct/`?T` destructuring:
  bare-brace, not Rust's `let StructName{...} = source` — deliberately
  NOT supporting a struct-name prefix, since the parser has no type
  information to validate it against `source`'s actual type.
  `source` is restricted to a bare identifier: a call source would
  desugar to one re-evaluation of the call PER destructured field
  (`Make().a`, `Make().b`), silently duplicating whatever the callee's
  body does instead of binding one shared result — enforced
  syntactically (a dedicated parser check, not the generic
  `expect_terminator` message), so `let {a,b} = Make()` reports "must be
  a plain reference... not a call/field access/other expression" instead
  of a confusing "expected end of statement" pointing at `(`.
  No nested destructuring — single-level field projection only.
  **Exhaustive by default, `..` to opt out:** naming only some fields
  with no trailing `..` is a compile-time error ("missing field(s): b —
  name them, or add `..` to discard the rest"), mirroring
  `Expr::StructLit`'s own missing-field check on the construction side.
- `Name{ field: value, ..., ..base }` struct update: `..base` fills
  every field this literal doesn't name from `base`'s own same-named
  field. `base` is restricted to a bare identifier: a call there would
  need re-evaluating once per field `..base` supplies, silently
  duplicating whatever the callee does. `..` may only be the LAST item
  (Rust's own rule, parser-enforced) and never recurses into a nested
  struct field that's itself only partially overridden — matches Rust's
  own `..` semantics exactly (never a recursive merge).
  Not supported in a reg/output INIT (a compile-time-constant position):
  `base`'s own flat fields generally aren't known until runtime.
- A constant shift amount `>= w` on a `[w]` operand is caught
  (`check_shift_amount`, `src/types.rs`); a genuinely dynamic
  (non-constant) amount is silently skipped — nothing for `const_eval`
  to evaluate. Deliberately narrower than "any lossy shift": `x << 7` on
  `[8]` loses seven of eight bits and is NOT flagged — that's
  data-dependent partial loss, not a statically-known total discard, and
  catching it structurally would mean growing `<<`'s result width (like
  `*` already does, `Bits(a + b)`) instead of keeping it at the left
  operand's own — a real semantic change to shift's width rule, not a
  diagnostic addition, and its own separate design conversation if
  wanted later.
- `.!` is a general postfix marker (`expr.!`, plus a binary operator's
  own mid-application spelling, `a +.! 100000000`/`x >>.! 300`) marking
  one specific `ExprId` as an intentional lossy/overflowing op —
  suppresses whichever of `check_literal_fits`/`check_shift_amount`/
  `check_assignable`/`Expr::SizedInt`'s own check is anchored at exactly
  that id, never a sibling expression using the same values. DOES reach
  `check_assignable` now (unlike the earlier binary-operator-only form):
  `x := a +.! b` silences the write's own truncation check too, since
  the marked `Expr::Binary` IS the write's whole RHS — the identical id
  both checks are anchored at. FIRRTL emission masks an oversized
  literal to its own width (`mask_to_width`, `src/firrtl/expr.rs`)
  unconditionally, since FIRRTL's literal syntax demands an exact fit
  even where `.!` let the type checker's own complaint through.

## Language features with no synthesis path yet

- Combinational-only (stateless) modules: `out` is register-backed by
  design (see DESIGN.md's "Module ports"), so a pure function of inputs
  can't be expressed without a cycle of delay.
- A real signed type (proper sign extension/preservation through
  arithmetic, comparisons, truncation, and casts — a second dimension
  cutting across the whole type system, not a small addition).
  Deliberately deferred in favor of the narrower `>>>` (arithmetic,
  sign-extending shift) operator, added as a per-operator choice on
  ordinary `[N]` rather than a new type. Revisit only if a real
  design needs more than a shift — signed compare/add/mul, or
  sign-aware truncation/widening.

## Closures and partial application (design-level, not scheduled)

Design committed (2026-08-06) — see DESIGN.md's "Closures and partial
application (planned)" section (right after "Calling a function from a
rule"). `_` as an elided parameter, Scala-style, doubling as partial
application with no second mechanism; reuses `Expr::Wildcard`, whose only
existing meaning is the `where _ < K` refinement self-reference in a TYPE
position, disjoint from the new value-position meaning, so there is no real
ambiguity to arbitrate; a precisely pinned value-slot scoping
rule instead of Scala's own "smallest enclosing expression" (which real
Scala users get surprised by); closures resolved entirely by substitution,
never a first-class value, so no new closure type and no effect
polymorphism needed. Lumi explicitly ruled out both cases that would have
forced real effect polymorphism — a separately-compiled module system, and
a combinator library big enough to need per-definition error locality.

**Two of three consumers SHIPPED (2026-08-06), both proven through real
firtool + Icarus simulation:**

1. **`map` over an elaboration-time `list`**
   (`examples/map_double.tr`, `map_double_runs_through_real_ports`). A
   new `elaborate.rs` interpreter builtin, alongside its existing `len`/
   list-slicing support — NOT `calls.rs`'s body-substitution inlining
   (an earlier draft of this entry and of DESIGN.md's own section
   claimed that; `elaborate.rs`'s own doc comment is explicit that list
   recursion is "a REAL interpreter... not a splice-and-compile pass
   like `firrtl::calls`'s ordinary callee inlining" — corrected once
   actually checked against source, see advisor's catch). `_` resolves
   via a `placeholder: Option<&ElabValue>` threaded through the whole
   `eval_elab_expr` family; requires exactly one `_` in the closure
   argument, checked before evaluating (`count_wildcards`). Composes
   with existing list consumers for free — `AdderTree(xs.map(Double(_)))`
   needed zero changes to `AdderTree` itself.
2. **A `let`-bound closure, callable later, possibly more than once**
   (`let f = Add(_, 5); r1 := f(x); r2 := f(x + 1)`,
   `examples/closure_let.tr`, `closure_let_runs_through_real_ports`; also
   proven crossing a `<sequences>` `tick`,
   `examples/closure_let_sequences.tr`). Lumi picked this fuller scope
   over the narrower call-site-only alternative after being asked
   explicitly which was wanted. NEITHER of the two mechanisms originally
   proposed for this (not `map`'s interpreter-placeholder, not `calls.rs`
   substitution — the latter is callee-local-only AND only fires at read
   sites, so it can't give an unused closure zero effect the way this
   needed) — a FOURTH, new mechanism instead: `closures.rs`, an entirely
   new, EARLIEST pipeline stage (ahead of the first `effects::check`)
   that erases every closure-shaped `let` from the source text outright.
   A bare read (`xs.map(f)`) splices the body VERBATIM, `_` intact — the
   thing that makes `xs.map(f)` and `xs.map(Add(_, 5))` the same program,
   so `map`'s own builtin needs zero awareness a closure-local was ever
   involved. A call (`f(3)`) splices with `_` replaced by that call's own
   argument, positionally, arity-checked first. `f := ...` is a clean
   `resolve.rs`-level error (`Resolution::closure_inits`).
   **Side effect: closed a pre-existing, unrelated gap** — `--firrtl`
   never actually chained `elaborate`/`lower` before this (every
   `<elaborates>` example hard-errored through the CLI even though
   `cargo test` already proved the passes worked); needing a real
   chained path for closures at all is what finally closed it.
   `pipeline.rs` (new: `resolve_src`/`check`/`splice_closures`/
   `splice_elaborate`/`splice_lower`/`schedule_checked`/`emit`) is the
   ONE shared implementation both `main.rs` and `tests/sim.rs` now call,
   specifically so the CLI path and the tested path can't silently
   diverge (this was `devenv.nix`'s `simulate` script's own exposure:
   three separate `cargo run` invocations, now collapsed to one
   `--firrtl` call that chains internally).

Still not started. Known open items, not yet designed:
- The `<sequences>`-loop (runtime-length) consumer — e.g. draining a
  fifo of unknown occupancy, accumulating into a `reg`/`out` with a
  closure (`while let x = f.Deq[] { total := f(total, x) }`). No new
  combinator syntax needed — once `while let` itself accepts a fifo
  `Deq[]` as its binding, the already-shipped closure mechanism (item 2
  above) handles the rest for free; a named `fold`/`drain` on top would
  be new surface syntax nobody's asked for (dropped from this entry's
  earlier draft per advisor review).
  **Prerequisite fixed (2026-08-07):** `while let x = f.Deq[]` itself
  isn't wired yet (`types/stmt.rs`'s `WhileLet` arm only accepts an
  Option unwrap) — mechanically small, mirrors `IfLet`'s own already-
  shipped fifo-Deq branch (`checks.rs` already has the groundwork:
  `fifo_ops_outside_allowed_positions`'s `WhileLet` arm). But wiring it
  would have hit a real, separate correctness bug head-on: depth > 1
  fifos ignored a conditional `Deq`'s own guard entirely in `emit_fifo_
  depth_n` (module.rs), corrupting `count` on an idle cycle — and a
  drain loop is only meaningful on depth > 1 (a depth-1 fifo holds at
  most one item, nothing to loop over). Fixed as its own unit, ahead of
  and independent of `while let` itself — see DESIGN.md's "FIFO
  synthesis emission" and the "Correction (2026-08-07)" callout in "if
  let: a fifo op's own presence"; `examples/fifo_depth_if_let.tr` +
  `fifo_depth_if_let_gates_head_and_count_on_the_guard` proves it against
  real firtool + Icarus. `while let`'s own extension is still unbuilt.
  The "drain and forward" bridge shape (`while let x = in.Deq[] {
  out.Enq[x] }`) stays separately blocked — `checks.rs`'s "nested fifo
  op in loop body" restriction, plus `Enq` having no `select`-gating
  mechanism at all; out of scope for the accumulate-into-a-reg case.
- The rest of the combinator library (`zip` etc.) is not designed —
  only `map` is built, and `fold`/`drain`-shaped iteration turns out not
  to need a name of its own (see above).
- Threading a closure argument through an intermediate USER-defined `fn`
  parameter (not just a builtin) plausibly extends the existing struct/
  Option param chase-through ("Calling a function: inlining"), but this
  hasn't been designed or attempted.

## Verse alignment: more of its failure system and operators (design-level, not scheduled)

`fails` gating (commit `e97795e`) ported Verse's `<decides>`-requires-a-
context rule. Verse's book has more in this vein — surveyed
[08_failure](https://verselang.github.io/book/08_failure/),
[04_operators](https://verselang.github.io/book/04_operators/), and
[13_effects](https://verselang.github.io/book/13_effects/) for what else
might translate to an HDL. Triaged below; nothing here is scheduled or
committed to.

Worth building:

- `logic <expr>` boolean-success operator — see DESIGN.md's "Builtins"
  section for the full semantics. `logic A & logic B` needs explicit
  parens on each side — `(logic A) & (logic B)` — to keep its old
  two-separately-discharged-values meaning; the bare form instead wraps
  the whole `&` expression in one `logic`, which `check_logic_args`
  (firrtl/checks.rs) cleanly rejects (bitwise `&` can't take a
  still-fallible left operand) rather than silently doing something
  else.
- `or` fallback chain (v0: `Deq[]`-only alternatives, depth-1 fifos,
  optional infallible default tail) — see DESIGN.md's "`or`: fallback
  chains" section for the full write-up. Call alternatives, `Enq`
  alternatives, depth>1 fifos, and `or` nested in `if`/`while` or a
  callee's own body are all separate, larger gaps, not silently
  accepted.
  `A and B` sugar: `and` does NOT mutate the way `or` does — `f.Deq[]
  and g.Deq[]` reads both fifos' occupancy and dequeues neither
  (`logic`'s existing pure-test emission, inherited as-is) —
  documented directly in DESIGN.md's `and` section rather than left as
  a silent asymmetry between two sibling-looking operators. See
  DESIGN.md's "`and`: boolean combination sugar".
- Comparisons returning their left operand in a failure context (Verse:
  `X > 0` yields `X` on success, fails otherwise) — see DESIGN.md's
  "Comparisons: fallible by default" for the full write-up. One
  restriction still applies: a comparison nested inside an `if`/
  `while`'s own BODY (not its condition) is rejected outright, matching
  the EXISTING guard/fifo-op/failing-call restriction there — folding
  it into the whole rule's guard would be wrong when the branch might
  not even be taken.
- trace's `not` is confirmed to be a plain `[1]` boolean operator
  (`types.rs`'s operand-must-already-be-`[1]` rule), not Verse's
  "test success/failure without committing" operator — `17f21e9` was a
  respelling, not a semantics port, and that's fine as-is: the two
  positions where Verse's discharge semantics would matter (`not`
  wrapping a failing call or a bare `expr?` guard) are already
  hard-rejected with a clean compile error. A fifo op (`Enq`/`Deq`) is
  only recognized in the exact structural positions `fifo_op_stmt`
  matches (a bare statement, the whole RHS of `:=`, a `let` init) —
  anywhere else (arithmetic, an `if`/`while` condition, wrapped in
  `not`, a call argument) it's now a clean "not yet supported" error
  (`check_fifo_op_positions`/`collect_fifo_ops`, mirroring calls' own
  `check_failing_call_positions`/`collect_calls`) instead of the silent
  miscompile it used to be (no occupancy guard, no state transition, the
  fifo's raw register read as if valid). Rule bodies only — a callee's
  own body already independently rejects any shape this permissive
  (`check_fails_is_foldable_guard`), confirmed separately.
- Cross-tick fails/rollback safety in `sequences`: a guard/fifo-op/
  failing-call after a `tick` gets the ordinary per-rule guard-placement
  treatment, not some special-cased or missing check.
  Message-quality note, not a gap: a `<sequences>` rule that spawns
  something but has no top-level `tick`/`sync` of its own (`rule go
  <sequences> { h := spawn Body() }`) is never touched by `lower::
  plan` (skips any `<sequences>` rule with no top-level tick) and
  correctly errors — "still a `<sequences>` rule with `tick`; run
  sequences lowering first" — but that message is misleading here since
  lowering DID run, it just had nothing to do; could be sharpened to
  name the real issue (a spawning rule needs its own tick/sync to
  observe the result) if this trips someone up in practice.

Speculative, bigger, not committed to:

- **Option type core (`?T`)** — sugar over a compiler-synthesized
  `{valid: bit, data: T}` struct, reusing struct's own flattening/read/
  write machinery end to end (see DESIGN.md's "Option types"/"Option
  emission" sections, `examples/option.tr`). Absence is the literal
  `false`; `T` may itself be a struct or a struct's own field.
  - Struct- and `?T`-typed fn/rule PARAMS: an argument may be a struct
    literal/`false`/a plain value of `T`, or a reg/output/input/another
    same-typed param, resolved by chasing through the alias to that
    value's own flat fields. A plain rule-level `let` merely re-binding
    a param/aliasing another `?T` value STILL rejects (deliberately
    narrower than "params work" — general alias resolution for `let`
    wasn't asked for and wasn't built).
  - Struct- and `?T`-typed RETURNS: supports a fresh literal return,
    chaining through a nested struct/Option-returning call, and a
    callee returning one of its OWN params unchanged (`Passthrough(p) {
    return p }`) — but NOT a callee-local that merely re-binds a param
    and returns THAT (`let x = p; return x`), the identical restriction
    the param side already has one level in.
  - `??T` (nested Option), independent layers: `optional <expr>` makes
    `Some(None)` (outer present, inner absent) expressible. Bare-value
    coercion alone (no `optional`) still only ever reaches the two
    fully-agreeing states (`nested_option_reaches_only_fully_absent_or_
    fully_present`, tests/firrtl.rs) — `optional` is a strictly new
    capability layered on top of it, not a loosening.
  - `option{...}` explicit-construction syntax: not a general
    struct-literal-style `option{ valid: ..., data: ... }` constructor;
    not pursued, no concrete use case for one beyond what `optional`
    already closes.
- `if`: branch-scoped fallible conditions — a BARE comparison, fifo op,
  or failing call, directly as an `if`'s own condition, Verse-faithful
  branch-scoping applied uniformly to the with-else and no-else shapes
  alike: failure skips only the `then` branch and the REST of the rule
  still commits, even with no `else`. See DESIGN.md's "`if`: branch-
  scoped fallible conditions" for the full write-up.
- `if let`: branch-scoped Option-presence binding — `if let NAME = opt?
  { then_body } [else { else_body }]` binds `NAME` to `opt`'s own
  unwrapped value, visible ONLY within `then_body`. `NAME` may be used
  as a whole value inside `then_body` but not chased through a further
  `.field` access (struct-typed `T`) — cleanly rejected with the same
  message an ordinary `let p = opt?; p.field` already gets. `if let`
  inside a `<sequences>`/spawn-callee body (crossing a `tick`) isn't
  supported — still a known v0 gap (a captured local crossing a `tick`
  from inside `if let`'s body isn't built), just with a clean error
  now. A writing-and-failing callee used as `if let`'s init is still
  cleanly rejected — by the same untouched write-position check any
  ordinary writing call already has, not new logic.
- `while`: multi-cycle loops — a top-level `while COND { body }` cuts
  its own self-looping segment. **A `while` loop may only write module
  state directly, not a local** — an accumulator, or any local
  surviving past the loop, is rejected with a message naming the real
  cause instead of the generic write-once capture text. This is the one
  deliberately deferred half: `compute_captures`'s write-once/read-only-
  in-later-segments invariant would need teaching to reason about
  loop-carried dataflow (a same-segment self-referential read is
  semantically sound for an accumulator — a register genuinely sees
  last cycle's value — but indistinguishable from the read-before-write
  hazard those checks exist to catch, without new machinery this pass
  attempts). A local computed BEFORE the loop and only READ inside it
  (never reassigned there) already works today, unaffected.
- `while let`: looping over Option presence — `while let NAME = opt? {
  body }`. Inherits `if let`'s v0 restrictions verbatim (Option-only
  `init`, no `.field` chase-through) and plain `while`'s own (module
  state only inside the loop body, no locally-captured accumulator).
- `break`: exiting a loop early — v0-restricted to TAIL position: the
  last statement of a `while`/`while let`'s own body, or of a `then`/
  `else` branch of an `if`/`if let` that is ITSELF in that tail
  position, nested as deep as the user likes as long as every enclosing
  level stays in tail position. Statements before a `break` in its own
  branch still commit (`break` means "advance past the loop starting
  NEXT cycle," not "nothing this iteration happened") — and `break`'s
  own condition, like any other read in the loop body, sees a
  register's OLD (pre-edge) value, so a threshold-based break lands one
  iteration "later" than a naive read suggests.
- Two `Deq[]`s on the same fifo, one in `then` and one in `else`, are
  allowed only for the narrowest provable case: exactly two ops on the
  same fifo, both selected off the IDENTICAL enclosing `if`, with
  opposite branches — v0-restricted to depth-1 fifos and exactly one
  level of nesting (a fifo op nested inside a FURTHER if/while within
  the branch still isn't supported), and a nested op is REJECTED when
  the enclosing `if`'s own condition is itself a fifo op or failing
  call. **Still open beyond this narrowest case**: proving exclusivity
  across a longer if/else-if chain, across two unrelated `if`s, or
  through nesting deeper than one level — all deliberately out of
  scope for now, not attempted.
- A plain `[1]` value is no longer accepted bare as an if/while
  condition in a RULE body at all — see DESIGN.md's "An if/while
  condition must itself be fallible" for the full rule (the `logic`-
  bare-condition footgun this closes, the `(logic A) & (logic B)`
  carve-out, and why callee bodies keep the old lenient behavior).
- `?.` safe navigation (`opt?.field?.next`, multi-hop, each `?.` its
  own independent unwrap-or-fail) — see DESIGN.md's "`?.` safe
  navigation" for the full write-up. **`if let`/`while let` deliberately
  did NOT get multi-hop chaining — a real correctness trap caught
  before it shipped.** Their own mux-select machinery reads `init`'s
  immediate `inner` alone as the presence check, never the whole chain
  — `if let x = a?.b?` would have silently read `a.data.b.valid`
  without also gating on `a.valid`; `guards_outside_allowed_positions`
  keeps `IfLet`/`WhileLet` restricted to a single bare `Expr::Guard`,
  and a chained init is rejected with the same message a misplaced
  guard gets. Fixing every one of those call sites (folding the whole
  spine into each) is real, separate work, not attempted here.

Explicitly considered and NOT being ported, so a future session doesn't
re-propose these from a fresh read of the same chapters:

- **`Err()`** (Verse's unrecoverable runtime error) — synthesized
  hardware has no runtime to propagate an error into; the closest
  analogue, an elaboration-TIME fatal check, is a different feature
  already reachable via a plain compile error, not a gap.
- **`first`/`for` runtime failure-filtering** — the synthesizable case
  (pick the first of several candidates that's actually valid) is
  already covered by the `prio` builtin; no new control-flow syntax
  needed.
- **Compound assignment** (`+=`/`-=`/etc.) — purely cosmetic sugar over
  existing `:=`/`Assign`; low value, skip unless a real example makes
  the spelled-out form genuinely painful.
- **Classes/structs/interfaces/persistence/module-path generality**
  from Verse's later chapters (09–12, 14–18) — general-purpose OOP/heap/
  persistence machinery with no hardware analogue; trace's `spec`/
  `impl`/`refines` already covers the HDL-relevant slice of "an
  interface," see DESIGN.md's "`chooses`: specification, not synthesis."
- **Verse's effect-subtyping rules** (fewer effects ⊆ more effects,
  join-on-conditional-selection) — already effectively matched: trace's
  standing overstate-ok/understate-error policy for `reads`/`writes`/
  `fails` gives "fewer effects is a safe substitute for more" without a
  separate subtyping mechanism.

## Cost model and formal verification (design-level, not scheduled)

- **Partially addressed (2026-08-06): a `<sequences>` rule's own STRAIGHT-
  LINE cycle count is now a checked, queryable quantity** (`lower::
  sequences_cycle_count`, surfaced in `--explain-schedule` and LSP hover —
  see DESIGN.md's "Sequences lowering" section). This closes the narrowest
  slice of the cost-opacity complaint below: a fixed-length transaction's
  own duration is no longer invisible. Still fully open: a `while`/`spawn`-
  bearing transaction's duration stays unreported (`None`, correctly, not
  guessed); port-level interval TYPES (composing a callee's own duration
  into a CALLER's timing, across `inst` boundaries) don't exist at all;
  and rollback COST specifically (checkpoint/squash machinery once a
  transaction has crossed a `tick`) is entirely unaddressed — a cycle
  COUNT existing is not the same as its rollback cost being modeled.
  **Scoped stage 2 (port-level interval types) the same day and found no
  viable next step, not just an unscheduled one.** `inst`/module ports have
  no invocation boundary to anchor an interval against — every `out` port
  is a continuously-live, per-cycle resource with no fixed start/end event
  (unlike Filament's own model, where a component is invoked once and its
  ports' validity windows are relative to that invocation); a literal port
  interval type doesn't have an obvious meaning here without a much larger,
  foundational change to the execution model, not an additive feature.
  `spawn`/`sync` DOES have real invocation shape (a handle's start/end are
  well-defined), so it looked like the more promising foundation — but the
  two concrete things it could be used for both dissolve on inspection:
  concurrent spawns of a callee touching shared module state already get
  correctly serialized by the ordinary per-cycle scheduler conflict
  analysis (verified directly: two rules each spawning a callee that
  writes the same `reg` produce a derived stall between their lowered
  segments, same as any two hand-written rules would — nothing unchecked);
  and a static "you synced too early" check on `sequences_cycle_count`
  can't work because the count is a lower bound, not an exact duration
  (fallible ops inside a segment retry), so it would reject programs that
  are correct at runtime, to catch a case (`sync` firing before the callee
  is done) that's already harmless since `sync` retries until `done`
  regardless. Stage 1 is where this line of work correctly stops until
  something changes the premises above.
- Multi-cycle `<sequences>` transactions (`tick`/`spawn`/`sync`) hide the
  real cost of what they express: rollback within one cycle is free
  (nothing has committed yet), but rolling back a transaction that has
  already crossed a `tick` needs real checkpoint/squash machinery — the
  same thing an out-of-order core builds, and it's expensive. The
  language currently lets a five-cycle atomic transaction be written as
  casually as a single-cycle write, with the effect system tracking
  correctness but not cost — the standard "cost opacity" criticism of
  HLS tools generally, and there's no reason to expect trace is immune
  to it once designs get larger.
- Closest research precedents, if this is ever addressed structurally
  rather than by convention: Filament (Cornell CAPRA) — "timeline
  types" encode which cycle each signal is valid in directly in the
  type system, rather than as a side effect-system check the way
  trace's `mutually_exclusive`/`conflict_free` directives do now; Dahlia
  — affine types for predictable memory banking, relevant to v0 arrays'
  current one-conflict-resource-per-array limit (see "Scheduler /
  arrays" below); Kôika — a Coq-verified one-rule-at-a-time Bluespec
  descendant, the closest existing precedent for formally proving this
  project's own scheduling model correct rather than trusting it by
  testing alone.
- Formal verification of the scheduler/conflict semantics specifically
  (not the whole compiler) looks like the highest-value next step if
  this is ever picked up: a small satellite Lean/Coq model of just
  `schedule.rs`'s `mutually_exclusive`/`conflict_free` arbitration,
  validated against the Rust implementation via generated test vectors
  — not a full rewrite, and not an attempt to encode cycle-validity in
  Rust's own type system, which (unlike Lean's) isn't dependently typed
  and can't express "valid in cycle n" natively.
- Not scheduled, no plan to act on this now — trace's actual discipline
  today (real firtool/Icarus runs, discriminating negative tests) is
  substituting empirical verification for formal proof, case by case
  rather than once and for all. If this is ever worth addressing head
  on, porting just the scheduler/cost-model layer to a language built
  for it wouldn't be a large loss — the conflict matrix and urgency
  logic already live in one fairly self-contained module,
  `schedule.rs`.

## Rules: optional/enable sugar

`rule foo? { body }` is sugar for an implicit, rising-edge-triggered
enable port sharing the rule's own name — see DESIGN.md's "Optional
rule sugar" for the full design writeup.

**One real, documented restriction: `rule foo? <sequences>` is rejected
at parse time.** Every synthesized node shares `foo`'s own span;
`lower.rs`'s `<sequences>` splicing reconstructs a rule's segments FROM
spans, and the collision would panic (`lower.rs`'s "overlapping
lowering edits" assert) rather than emit silently-wrong hardware, so
it's rejected instead with a clean parse-time error. The manual `in foo
: [1]` + `foo?` pattern is unaffected — only the sugar itself is
restricted, and only under `<sequences>`.

**Left as-is, not decided against, just not done here:** whether the
hand-written `trigger`/`trigger?` pattern in the `spawn`/`race` examples
should migrate to this sugar. Left alone — a manually-named, still-
legal, more general pattern the sugar doesn't replace (a level-
sensitive or `reg`-backed enable still needs the manual form).

## `io` ports + `extmodule`

`io name : ty` (a third port kind alongside `in`/`out`) lowers to
FIRRTL's `Analog<N>`; `extmodule Name from "path.v" { ports }` names an
external Verilog module's interface.

An extmodule instance gets no `connect t.clock`/`connect t.reset` (v0
restriction: an extmodule has no such port at all; a blackbox needing a
clock declares one as an ordinary `in` port instead). The `.v` path
itself (`from "tribuf.v"`) is opaque data trace's compiler pipeline
never reads or validates — FIRRTL text has zero linkage to the
implementation file, entirely a downstream build/simulation concern.

## Scheduler / arrays

- The genuinely general case — two arbitrary, unrelated, unscaled
  bases with no supporting `invariant` — remains exactly as unprovable
  as before; Dahlia-style banked/affine array types (real range
  tracking) are still tier 3, explicitly deferred in DESIGN.md.
- `invariant <expr>`/`bounds::provably_disjoint_under_joint_guards`
  (the relational-bounds mechanism that closes DESIGN.md's "Tier 3"
  circular-buffer case, `examples/circular_buffer_disjoint.tr`) ships
  with several v1 restrictions, not oversights:
  - Every named register's own coefficient must be exactly ±1 — no
    scalar multiplication in the declared linear combination.
    Extending to arbitrary integer coefficients would need
    `linear_form`/`recognize_invariant`/`recognize_comparison` to
    track weighted sums, not just a signed-presence set.
  - At most TWO rules may write registers a single `invariant` names
    (`check_relational_bound_induction`'s own gate) — a
    three-or-more-way producer/consumer arrangement fails this bound
    closed today, not scoped to lift.
  - No `While`/`IfLet`/`WhileLet` inside a rule contributing to an
    `invariant` — `walk_deltas` has no per-iteration delta story for a
    loop at all, an unconditional restriction regardless of whether
    the loop actually touches a relevant def.
  - `provably_disjoint_under_joint_guards`'s own "linking fact" shape
    is fixed at exactly 4 terms (2 cancelling pairs — the FIFO
    pointer/counter shape specifically); a relational fact with a
    different arity (three index bases, or more than 2 "other" defs)
    isn't recognized at all.
  - `range_fits_modulus` (same function) fails closed on any
    guard-narrowed range that would need a genuine cross-modulus
    wraparound reduction — only the "already fits" case is
    implemented.
- `where` on mem elements (`mem m : [8][20] where _ < K`) is WRITE-side
  only. Read-side propagation was DROPPED entirely: `expr_bound`'s
  `Bracket` arm's own value composes to `None`, exactly as before this
  feature existed — `total := m[i]` still fails as an unsupported
  expression shape. This narrows the feature's final scope from what
  was first implemented and documented: a genuinely useful, SOUND
  write-side proof ("catches an out-of-range value being written to
  this mem"), and nothing about reads. A declared mem bound with zero
  write sites anywhere in the program is still dead, misleading
  metadata even with reads no longer trusting it.
- `where` on struct fields DOES support sound read composition (unlike
  mem elements). `in q : Pair`, a struct-typed INPUT PORT, is real and
  directly tested — its bits arrive over an external wire, never
  through a checked `StructLit`, exactly as untrusted as an unwritten
  mem address; `q.data` must NOT compose, mirroring why a mem read
  stays uncomposed. A `Call`-sourced struct write (`p := MakePair()`,
  the type-checker's OTHER permitted shape) has no field-by-field
  verification mechanism and correctly, conservatively fails — a
  documented v1 restriction pinned by its own regression test, not a
  silent skip. A nested struct-of-struct field chain (`outer.inner.x`)
  is ALSO deliberately deferred: `struct_field_bound`'s match doesn't
  special-case an `Expr::Field` shape, so it safely composes to `None`
  rather than risking a wrong answer.
- A `conflict_free` mem read/write pair the disjointness proof can't
  close gets a checked runtime assertion instead (`firrtl/module.rs`'s
  `conflict_free_mem_check_N`), scoped to read sites that are
  unconditional top-level statements in their rule (`read_addrs` in
  `module.rs`) — a branch-nested read is silently excluded rather than
  risking a false-positive assertion on a correct design, since read
  ports are wired up unconditionally regardless of which branch a
  cycle actually takes. Non-mem shared state in a `conflict_free` pair
  stays fully trusted/unchecked, as before.
- `combines` combinational-loop checking delegates entirely to
  firtool's `CheckCombLoops`, run only by `devenv.nix`'s `simulate`
  script and `tests/sim.rs` — never by `trace` itself, and its
  diagnostics are never remapped to `.tr` source spans.

## Milestone: SUBLEQ

- `sim/subleq_tb.v`'s driven program is deliberately minimal (one
  `subleq` arithmetic instruction, one unconditional jump, one
  self-loop halt) — enough to discriminate the branch-vs-fallthrough
  mux, not much more. Running something that actually computes (a
  short counted loop, or a multiply via repeated subtraction) would be
  a more convincing real workload than the current three-instruction
  proof-of-concept. A nice-to-have, not blocking anything.

## Editor tooling (`editors/vscode/`)

- The VS Code extension wires the language server up via
  `vscode-languageclient` (`editors/vscode/extension.js`), spawning
  `trace --lsp` — the client side of the LSP wiring couldn't be
  verified in this environment (no way to drive a real VS Code
  Extension Development Host headlessly); only the server side has
  been verified.
- A fn/spec/impl hover's placeholder description line (`"A
  function."`/`"A spec."`/`"An impl."`) stands in since real doc
  comments don't exist yet.
- Grammar is regex-based (TextMate), still pattern matching, not
  semantic analysis.
- Formatter is a reindenter, not a pretty-printer, by deliberate choice
  (see DESIGN.md's "Tooling" section).
- Not published to a marketplace; local install only (see the
  extension's own README).
