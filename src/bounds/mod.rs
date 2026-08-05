//! Statically proven register value bounds: `reg i : [w] where i < K =
//! v0` declares that `i`'s value is ALWAYS in `[0, K)` — proven, not
//! trusted, by induction over every write site in the program, not a
//! runtime-checked assertion.
//!
//! This exists to close one real, documented gap: `schedule.rs`'s mem-
//! disjointness proofs (v1 constants, v2 same-base affine, v3 banking)
//! all require a mem's own depth to be an EXACT power of two, purely to
//! avoid ever depending on out-of-range-index behavior (undefined in
//! this compiler). A proven bound on a mem's own index base lets
//! `schedule.rs` prove the index stays inside the mem's REAL depth
//! directly, without that power-of-two workaround, for the one base it
//! has a bound for (see `schedule.rs`'s own doc comment for how this
//! plugs in as a THIRD, independent disjointness argument).
//!
//! v0 restrictions, all deliberate scope cuts, not oversights:
//! - `reg`/`out` only (v8: `out` gained this too — see below). An `in`
//!   has no internal write site at all — a bound on it would be a
//!   TRUSTED external contract (this module's whole point is proof, not
//!   trust), a different feature entirely; parser.rs still rejects
//!   `where` on it.
//! - A single strict upper bound against a compile-time constant,
//!   optionally paired with an explicit LOWER end too (`where L <= i <
//!   K`, inclusive): a bare `where i < K` is exactly `where 0 <= i < K`
//!   (0 is always the implicit floor of an unsigned `bits[N]` value).
//!   No multi-variable or otherwise arbitrary bound expressions on
//!   either end.
//! - RHS composition supports only bare idents/literals, `Add`, `Sub`,
//!   and `Mul` (v11) — the motivating patterns (`i := i + 1` under a `<
//!   K` guard, `cnt := cnt - 1` under a `> 0`/`>= 1` guard, doubling
//!   under a narrowing guard) need nothing else. `Sub` fails closed
//!   (returns "unknown") whenever the subtrahend's range could exceed
//!   the minuend's — a real, checked soundness condition, not a
//!   syntactic restriction (see `expr_bound`'s own doc comment). `Mul`
//!   composes cleanly because both operands are non-negative
//!   (`bits[N]`): a product's extremes correspond exactly to the
//!   operands' own extremes (`a_lo` times `b_lo`, `a_max` times
//!   `b_max`), no sign-corner-case reasoning needed the way general
//!   integer interval multiplication would require. Anything else (a
//!   call, a shift, ...) still composes to "unknown" unconditionally.
//!   Either way, an unprovable write fails the write-site check closed
//!   (a compile error asking for an explicit restructure), never
//!   silently "assumed in range."
//! - No interaction with `schedule.rs`'s v3 banking argument — that
//!   argument's soundness rests on a modular (not real-integer) fact a
//!   proven bound doesn't slot into, and no example needs the
//!   combination.
//! - `narrow_for_condition` narrows the UPPER end on `if <reg> < <const>`
//!   and the LOWER end on `if <reg> > <const>`/`if <reg> >= <const>`. A
//!   `<>`-shaped guard narrows EITHER end, but only when the excluded
//!   constant equals the CURRENT frozen bound's own floor or ceiling
//!   exactly (`if <reg> <> <lower>` narrows the lower end up by one;
//!   `if <reg> <> <upper - 1>` narrows the upper end down by one) —
//!   excluding any OTHER constant would split the range into two
//!   disjoint pieces this single-interval representation can't express,
//!   so that case stays a no-op (v9's own driving example, `examples/
//!   output_bounded_ne.tr`, uses the upper-edge form; `while cnt <> 0 {
//!   cnt := cnt - 1 }`, the lower-edge form, is likewise provable in
//!   general — but NOT on the actual `while_countdown.tr` file, since
//!   `cnt` there carries no `where` bound at all, and adding one
//!   wouldn't help: its `cnt := x` reads an unbounded `in` port every
//!   cycle, which stays unprovable regardless of this feature). `Ne`'s
//!   commuted guard (`<const> <> <reg>`, v10) IS recognized — this
//!   function only ever treats a guard as a pass/fail predicate
//!   deciding which branch to check, never consuming a comparison's own
//!   RETURNED value, so `x != k` and `k != x` (the same fact about the
//!   same two values) narrow identically; this is narrower than
//!   claiming `<>` is symmetric as a language construct in general
//!   (`type_binop`'s own Verse-inspired rule makes a comparison yield
//!   its LHS's own type/value on success, so the two orderings genuinely
//!   differ wherever that returned value is consumed). `Lt`/`Gt`/`Ge`
//!   stay single-order — `k < x`/`x < k` are different claims even as
//!   bare predicates. The `else` branch of an `if <>` (a provable
//!   singleton, `<reg> == <const>`) was ALSO left unnarrowed through
//!   v14, conservative but costless since no example needed the extra
//!   precision there — v15 below closes this: the `else` branch is now
//!   narrowed to the exact singleton.
//! - **Cross-boundary bound propagation (v12): a `fn`/`impl` PARAMETER
//!   can carry a `where` bound too**, checked as an obligation at every
//!   CALL site — the one thing this module's own per-item induction
//!   structurally couldn't reach before (it only ever walked write
//!   sites within ONE item's own body, with no way to check that a
//!   CALLER upholds a callee's declared precondition). A bounded
//!   param's own `DefId` is collected into the exact same `self.bounded`
//!   map a reg/out populates, so the callee's OWN body trusts its
//!   param's declared range unconditionally, exactly like a reg's
//!   declared bound is the base case of ITS OWN induction — zero new
//!   narrowing/composition logic needed there. The NEW work is entirely
//!   at the call site: `Expr::Call`'s own `expr_bound` arm checks each
//!   argument's provable range against the callee's declared param
//!   bound. Checked at three expression positions: a `Stmt::Assign`'s
//!   RHS, a bare `Stmt::Expr` call statement (how a VOID fn/impl — no
//!   return value, called purely for its `writes` effect — is actually
//!   invoked in this language; the realistic shape, not an edge case),
//!   and any position `expr_bound`'s own `Add`/`Sub`/`Mul` recursion
//!   already reaches (a call nested inside an argument or operand). NOT
//!   checked as of v13: a call inside an `if`/`while` condition —
//!   closed by v14 below.
//! - **Return-bound propagation (v13): a fn/impl's return type can
//!   carry a `where _ < N` postcondition too** — the mirror of
//!   v12 in the OTHER direction. Checked against every `Stmt::Return`
//!   in the fn's own body (a NEW checked position — `current_ret_
//!   bound`, set once per item at the top of `check_item`), then
//!   trusted at every call site: `Expr::Call` returns `Some((lower,
//!   upper))` from the callee's own `fn_ret_bound` entry instead of
//!   unconditionally `None`, letting a caller compose with the call's
//!   own result (`total := Bump(3) + Bump(4)`). `_` (`Expr::Wildcard`;
//!   `result` through v17) is a shape placeholder, not a real scoped
//!   binding — a return value has no `DefId` of its own (unlike a
//!   reg/out/param's self-reference, checked by `DefId` equality
//!   against an existing declaration), so `resolve.rs`'s `check_ret_
//!   bound_shape` checks it by matching the AST shape instead, and this
//!   module keys its own postcondition table (`fn_ret_bound`) by the
//!   FN's own `DefId` rather than folding it into `self.bounded`.
//!   Opt-in, not blanket inference: a fn with no declared postcondition
//!   still composes to
//!   `None`, exactly as before this feature. Trust at the call site is
//!   NOT unconditional on the declaration alone: `check_return_site_
//!   exhaustiveness` requires at least one actual `Stmt::Return` to
//!   have been checked against a declared postcondition, or it's a
//!   compile error — an advisor pass caught that a fn with a declared
//!   postcondition and an empty body would otherwise let every caller
//!   trust it with ZERO obligations ever verified (confirmed
//!   empirically before being closed).
//! - **Widened checked positions (v14): a call inside an `if`/`while`'s
//!   own CONDITION, or an `if let`/`while let`'s own `init`, is now
//!   checked too** — the last position v12/v13 left open. Pure
//!   coverage widening, not a new capability: `check_calls_in` walks
//!   `cond`/`init` (via `crate::lower::sub_exprs`), finds every
//!   "outermost" `Call` (stopping the descent the instant one is found
//!   — `expr_bound`'s own `Call` arm already recurses into ITS OWN
//!   args, so continuing further would double-check the same site),
//!   and checks it via `expr_bound` purely for the side effect, same
//!   idiom a bare `Stmt::Expr` call statement already uses.
//!   `narrow_for_condition` itself is untouched. A SECOND, adjacent gap
//!   was found empirically while writing this feature's own tests, not
//!   assumed away: a call nested as ANOTHER call's own argument
//!   (`Outer(Bump(50))`) was only reached when the OUTER param's own
//!   declared bound gated evaluating that argument at all — with none,
//!   the old code skipped calling `expr_bound` on it entirely, silently
//!   missing the inner call's own violation. Fixed by calling `expr_
//!   bound` on every argument to a call unconditionally; the bound
//!   CHECK itself still only fires when a declared bound exists. THREE
//!   more instances of this exact shape (gate the recursive `expr_
//!   bound` descent on whether there's a bound/postcondition to check
//!   against, rather than always descending and gating only the
//!   CHECK) surfaced from a second advisor pass's suggestion to grep
//!   for it, rather than finding each independently: `Stmt::Assign`'s
//!   own early returns (a write to an unbounded reg used to skip `rhs`
//!   entirely), `Stmt::Return`'s own `current_ret_bound` gate (a `return`
//!   inside a fn with no declared postcondition used to skip `e`
//!   entirely), and `Add`/`Sub`/`Mul`'s own `self.expr_bound(*lhs,
//!   ...)?` chained directly into `self.expr_bound(*rhs, ...)?` (an
//!   unprovable LHS short-circuited before `rhs` was ever evaluated).
//!   All four fixed the identical way: compute the descent
//!   unconditionally, gate only the eventual check/arithmetic on
//!   whether a bound exists. Reusable lesson for any future gap of this
//!   shape: grep for `if let Some(...) = ... { ... expr_bound(...) }`
//!   (or an early `?`/`return` before an `expr_bound` call) rather than
//!   fixing instances one at a time as they're found. A THIRD advisor
//!   pass (a targeted follow-up probe, not another blind grep) found a
//!   FIFTH instance the grep above structurally could not surface:
//!   `Stmt::Assign`'s own `lhs` was never passed to `expr_bound` at all
//!   unless it was a bare `Expr::Ident` — not gated, simply never
//!   reached — so a mem write's own index (`m[Bump(50)] := 1`, an
//!   `Expr::Bracket`) silently skipped `Bump`'s own argument check.
//!   Fixed by sweeping `lhs` through `check_calls_in` unconditionally,
//!   same as `cond`/`init` above. Lesson past "grep for the gating
//!   shape": a grep only finds calls that exist and are merely gated;
//!   it can't find a position never wired to `expr_bound` at all —
//!   that needs a distinct "what's never reached" pass, not a grep.
//! - **`else`-branch negated-condition narrowing (v15): the `else`
//!   branch of an `if` is now narrowed on the NEGATED condition**
//!   (`examples/else_branch_narrowing.tr`), instead of inheriting the
//!   raw, unnarrowed entry state as it did through v14. Before writing
//!   any code, checked (per this arc's own "don't build an inert
//!   feature" discipline, first established at v11) whether the
//!   v11/v13/v14-deferred interval-set domain was finally worth building
//!   now that `Mul` exists — and found it's PROVABLY inert, doubly so:
//!   every check in this file (write-bound, width-clamp, `Sub`'s own
//!   fail-closed condition) reads only a range's EXTREMES, and `Add`/
//!   `Sub`/`Mul` are all monotonic given unsigned operands, so punching
//!   an interior hole in a range can never move a downstream min/max —
//!   this generalizes v11's "Mul specifically" finding to ANY monotonic
//!   composition, present or future. Separately, `schedule.rs`'s own
//!   disjointness proofs (the consumer this module's own doc names as
//!   the whole point of a proven bound) only ever read a def's FLAT,
//!   whole-program declared range (`Bounds.ranges`, populated straight
//!   from `self.bounded`) — no per-branch narrowing, edge OR hole, ever
//!   reaches that consumer at all, so an interval-set wouldn't even
//!   change what `schedule.rs` sees. `narrow_for_else` is the real,
//!   non-inert alternative found instead: `Lt`/`Ge`/`Gt` each mirror an
//!   existing `narrow_for_condition` formula in the opposite direction
//!   (`else` of `i < k` is `i >= k`, etc.); `Ne`'s own negation is
//!   genuinely new — `else` of `i <> k` is the EXACT singleton `i == k`,
//!   sound for ANY `k`, mid-range included, unlike `narrow_for_condition`
//!   own `Ne` arm (which only narrows a THEN branch's edge case). A
//!   singleton needs no interval-set at all — it's just an ordinary
//!   one-piece interval — so this is the actual non-inert capture of
//!   the same underlying idea the interval-set domain was chasing, using
//!   the representation already in place. Every arm's raw result is
//!   CLAMPED against the def's own current `(lo, hi)` and only inserted
//!   when non-empty — a single uniform rule, not a per-arm guard: an
//!   advisor pass found that `Lt`/`Gt`/`Ge`, not just `Ne`, can each
//!   produce a degenerate EMPTY range when their condition is always
//!   true for the current bound (`cnt >= 0` on an unsigned `cnt`), and
//!   inserting that unclamped let an unrelated write vacuously accept
//!   rather than conservatively fail; the clamp is a no-op for those
//!   three (their raw results already derive from `lo`/`hi`) but is
//!   exactly what keeps `Ne`'s own raw `(k, k+1)` — which does NOT
//!   derive from `lo`/`hi` at all — from being inserted for a `k`
//!   nowhere near the def's actual proven range. Either way, an
//!   unreachable `else` branch left on the raw entry state stays sound:
//!   a false premise proves anything, and `else_state` is discarded at
//!   the end of the branch regardless. This clamp is `narrow_for_else`'s
//!   OWN rule, deliberately not backported to `narrow_for_condition`
//!   above: that function has the identical unclamped hazard (`if i <
//!   0` inserts the empty `(0, 0)`; `if i >= 20` on `i < 10` inserts the
//!   inverted `(20, 10)`), pre-existing since v9, but the same dead-
//!   branch/false-premise argument makes it just as harmless there, and
//!   no example depends on tightening it.
//! - **`Expr::Bracket` (a mem/fifo access) now recurses into `callee`/
//!   `args` for the side effect of checking any nested `Call`**
//!   (`examples/mem_read_call_check.tr`), instead of falling to the
//!   catch-all `_ => None` with ZERO recursion as it did through v15.
//!   Found empirically while designing a later feature (exporting this
//!   pass's own per-site facts to `schedule.rs`), not assumed: a mem
//!   access used as a VALUE, not an assignment target (`y := m[Bump
//!   (50)]`, `return m[Bump(50)]`, `Outer(m[Bump(50)])` as a call
//!   argument), silently skipped the nested call's own argument
//!   obligations entirely, since every one of those positions routes
//!   through this same shallow `expr_bound` call and `Bracket` had no
//!   arm at all. A mem/fifo access's own VALUE still has no provable
//!   bound — this arm still returns `None`, unchanged — `check_calls_in`
//!   (the established "find every outermost Call, check it, discard
//!   the bound" idiom already used elsewhere in this file) is reused
//!   here rather than duplicated.
//! - **Per-site proven ranges exported to `schedule.rs` (v16)**: this
//!   module's own `Bounds.ranges` only ever exported a def's FLAT,
//!   whole-program DECLARED range — `schedule.rs`'s own mem-
//!   disjointness proof had no visibility into any branch-local
//!   narrowing this pass proves internally (`if i < 10 { m[i] := x }`
//!   proves a tighter fact for THIS site than `i`'s raw declared bound,
//!   but `schedule.rs` only ever saw the latter). Before building
//!   anything, checked whether the v11/v13/v14/v15-deferred interval-
//!   set domain was FINALLY the answer here — it's not, for the same
//!   reason as before (every check remains extremal, every composition
//!   remains monotonic) — this is a genuinely different capability, the
//!   first in the whole arc a bolted-on post-pass structurally cannot
//!   express: a per-`ExprId` fact, not a per-def one. New `Bounds.
//!   site_ranges: HashMap<ExprId, (u64, u64)>`, populated by widening
//!   `check_calls_in`'s own stopping condition from "just `Call`" to
//!   EVERY shape `expr_bound` has a dedicated arm for (`Int`,
//!   `SizedInt`, `Ident`, `Add`/`Sub`/`Mul`, `Call`, `Bracket`) and
//!   giving it a real return value (previously discarded `()`); `expr_
//!   bound`'s own `Bracket` arm captures that value and exports it,
//!   keyed by the index's own `ExprId`, whenever `callee` resolves to a
//!   `mem` (a fifo shares this `Bracket` shape but has no consumer —
//!   `effects.rs`'s `mem_read_idx`/`mem_write_idx` are mem-keyed
//!   specifically). No other call site needed to change: every existing
//!   checked position already routes through `check_calls_in` or
//!   `expr_bound` directly, both now handling `Bracket` uniformly, so a
//!   mem access anywhere gains the export automatically. `schedule.rs`'s
//!   `real_range` consults `site_ranges` first, falling back to its own
//!   independent walk — consulted-then-fallback, never replaced, so the
//!   whole proof stays fail-closed for any `ExprId` this pass never
//!   visited. Verified structurally (not assumed) that no `ExprId` is
//!   ever visited by this pass's forward walk more than once under a
//!   different state (each Rule/Fn item is walked exactly once, `if`/
//!   `while` branches are disjoint subtrees walked once each, no fn is
//!   ever inlined per call site), so a plain `insert` is sound with no
//!   merge-on-conflict needed. `examples/mem_site_narrowing.tr`: two
//!   regs each merely declared `< 20` (individually insufficient — both
//!   ranges span the mem's whole depth and fully overlap) proven
//!   disjoint once each is narrowed by a DIFFERENT `if` guard, in its
//!   own accessing rule, to a disjoint half — pre-fix, the scheduler
//!   inserts a real stall between the two rules; post-fix, it proves
//!   disjointness automatically and removes it, with no annotation.
//! - **`where` on mem elements, write-side only (v17)**: at v16's own
//!   decision point, "where on struct fields / mem elements" was the
//!   alternative not picked; picked up directly here. `mem m : [8][20]
//!   where _ < K` declares a bound on every value ever WRITTEN to
//!   `m`, checked at every write site via the identical per-site
//!   induction argument a reg/out's own bound already uses. The self-
//!   reference placeholder is `_` (`Expr::Wildcard`; `elem` through
//!   v17, retrofitted for consistency once v18 introduced struct-field
//!   bounds needing the same shape) — a mem element has no scoped
//!   `DefId` of its own to compare against, unlike a reg/out/param's
//!   bound (a real binding already in scope). Checked by SHAPE
//!   in `resolve.rs`'s `check_mem_bound_shape`, mirroring
//!   `check_ret_bound_shape` exactly. Kept in its OWN map
//!   (`self.mem_bounds`), not folded into `self.bounded`: a mem's bound
//!   is a flat, whole-array fact checked at every write site, never
//!   narrowed per-branch the way `self.bounded`'s per-item `state` map
//!   is — the same reason `fn_ret_bound` is kept separate.
//!
//!   **A mem READ's own value still composes to `None`, deliberately —
//!   not the capability this feature first shipped with.** An earlier
//!   version handed the declared bound back at every read site too
//!   (composing through `Add`/`Sub`/`Mul`, a further write-site check,
//!   etc., for free). An advisor pass caught a real soundness hole in
//!   that before it shipped: the write-site induction proves "every
//!   WRITTEN value satisfies the bound," which is NOT "every read
//!   returns an in-range value" — a mem has no `init`/reset at all
//!   (unlike a reg/out, whose induction has a verified BASE case), so a
//!   read at an address never written, or in an early cycle before the
//!   corresponding write has happened, returns uninitialized data the
//!   write-site proof says nothing about. Worse, that unproven value
//!   could reach `Stmt::Let` (`let a = m[pc]`), then `m[a]`, exporting a
//!   FABRICATED "proven" range into `Bounds.site_ranges` — `schedule.
//!   rs`'s own disjointness proof would trust it, exactly `subleq.tr`'s
//!   own shape (an index loaded out of the mem itself), turning a false
//!   proof into a real aliasing bug in synthesized hardware. This
//!   module's own standing distinction (a bound on an `in` port would be
//!   a TRUSTED external contract, "a different feature entirely" from
//!   this module's proof-only mandate) applies identically: a mem's
//!   declared bound is only ever a claim about what was WRITTEN, never a
//!   substitute for proving what a read returns. So `total := m[i]`
//!   still fails as an unsupported expression shape, exactly as before
//!   v17 — this feature's actual scope is narrower than first shipped:
//!   catching an out-of-range WRITE, nothing about reads.
//!
//!   `check_item`'s own early-return guard widened to a THREE-way check
//!   (`self.bounded`/`fn_ret_bound`/`mem_bounds` all empty) — flagged
//!   explicitly during planning as the same "gated the descent on the
//!   wrong condition" class of bug v14 found four times in a row: a
//!   program with ONLY a bounded mem would otherwise skip the whole body
//!   walk, silently letting every mem write through unchecked.
//!   Discriminated by a dedicated driving example with no OTHER bounded
//!   def anywhere in the module.
//!
//!   A declared mem bound with literally zero write sites anywhere in
//!   the program is dead, misleading metadata even with reads no longer
//!   trusting it — the same "a declared contract with nothing to prove
//!   it against" class v13 flagged for fn return bounds. Closed the same
//!   way: a new `check_mem_bound_is_proven`, a REAL user-facing error
//!   (unlike `check_write_site_exhaustiveness`, which panics — that
//!   one's an internal-consistency check against `effects.rs`'s own
//!   independent oracle, extended to `self.mem_bounds`'s keys too, since
//!   a mem write is `sig.writes.insert(mem_def)` there exactly like a
//!   reg/out's own write). Both checks were bug-reintroduction-verified
//!   independently, and — discovered empirically, not planned —
//!   disabling either the write-site check OR the three-way guard trips
//!   the PANIC check first (`effects.rs` says written, this pass's own
//!   walk disagrees), a stronger safety net than the plan anticipated.
//!
//! - **`where` on struct fields, WITH sound read composition (v18)**:
//!   the other half of v16's own left-open alternative, picked up
//!   directly after Lumi asked for it by name. `struct Pair { data :
//!   [8] where _ < 50 }` is checked at every `StructLit` construction
//!   site (a reg/out's mandatory `= init`, or any later whole-value
//!   write — `types/stmt.rs` requires a struct-typed state write be one
//!   of those, or a `Call`, never a bare copy).
//!
//!   **This overturns v16/v17's own characterization of struct fields
//!   as "smaller, pure plumbing."** A struct-typed reg/out has exactly
//!   the checked-base-case-plus-checked-every-write induction that
//!   already makes a SCALAR reg/out's own bound sound — no "unwritten
//!   address" concept exists for a struct the way it does for a mem —
//!   so read composition (`total := p.data`) IS sound here, the
//!   opposite of what v17 found. Struct-field `where` turned out to be
//!   the RICHER of the two v16 alternatives, not the smaller one.
//!
//!   That argument has one real hole, found during DESIGN REVIEW this
//!   time (before any code was written — contrast v17, whose analogous
//!   hole was only caught mid-implementation): `in q : Pair` (a
//!   struct-typed input port, real and tested — `tests/firrtl.rs`'s
//!   `struct_typed_input_port_flattens_and_reads_by_field`) never
//!   passes through a checked `StructLit` at all, exactly as untrusted
//!   as an unwritten mem address. Closed by a `DefKind` gate in the new
//!   `struct_field_bound`: a struct value's field is trusted ONLY when
//!   the base is a bare `Reg`/`Output` ident (against the flat `self.
//!   struct_field_bounds` declared fact) or a `Local` this pass traced
//!   back to a checked literal via a new `struct_origins: HashMap<
//!   DefId, ExprId>` (sound because a struct-typed local can ONLY ever
//!   bind directly to a literal in this language — aliasing another
//!   struct value is a separate, pre-existing v0 restriction); every
//!   OTHER `DefKind` (`Input`, `Mem`, `Fifo`, `Io`, `Param`, ...) falls
//!   through to `None` unconditionally.
//!
//!   `struct_origins` is threaded as a THIRD parameter alongside
//!   `state`/`locals` through `expr_bound`/`check_calls_in`/`check_
//!   body`/`check_stmt`, cloned and discarded per nested scope exactly
//!   like `locals` — caught by ADVISOR REVIEW OF THE PLAN, before any
//!   code was written: a flat `self`-field (the first design, reasoning
//!   by analogy to `site_ranges`'s own "insert once" precedent) would
//!   have silently leaked a branch-local struct binding's trust past
//!   its own branch, since `site_ranges`'s reasoning (keyed by
//!   `ExprId`, read at the SAME site) doesn't transfer to something
//!   keyed by `DefId` and read at a DIFFERENT program point.
//!
//!   `check_item`'s own guard widened to a FOUR-way check (`self.
//!   bounded`/`fn_ret_bound`/`mem_bounds`/`struct_field_bounds` all
//!   empty) — also caught by advisor review of the plan, the identical
//!   bug class v14 found four times and v17 flagged again for its own
//!   three-way guard.
//!
//!   Unlike v13/v17, no new "zero check sites" exhaustiveness check is
//!   needed: a struct-typed reg/out's `= init` is mandatory, so every
//!   such def always has at least one guaranteed, checked site — and a
//!   struct type used ONLY via `in`/mem/fifo has a bound that's inert
//!   for those instances (never checked, but also never TRUSTED at any
//!   read), not a "trusted with nothing verified" gap. Also unlike the
//!   scalar reg/out bound (base case checked in `types/stmt.rs`, via
//!   `const_eval`), this feature's own init check (`check_struct_field_
//!   inits`) lives entirely in `bounds.rs` — a struct field's init
//!   value is always a literal int, which `expr_bound` already
//!   natively evaluates, nothing here needs `types.rs`'s own machinery.
//!
//!   A `Call`-sourced struct write and a nested struct-of-struct field
//!   chain (`outer.inner.x`) are both documented, deferred v1
//!   restrictions — the former correctly fails (no field-by-field
//!   mechanism exists for it), the latter safely composes to `None`
//!   (`struct_field_bound`'s match doesn't special-case `Expr::Field`).
//!
//!   **A third finding, empirical, from a first implementation
//!   attempt**: adding `Expr::Field` to `check_calls_in`'s stop-list
//!   (by analogy to `Bracket`'s own v16 addition) broke an EXISTING
//!   v16 regression test (`call_argument_hidden_under_a_field_access_
//!   in_a_mem_index_is_still_checked`), since `struct_field_bound`
//!   doesn't recurse into an unrecognized `base` (a `Call`) the way the
//!   generic `sub_exprs` walk does — reverted immediately.
//!
//!   **Two more real holes, found by advisor review AFTER this feature
//!   had already shipped and been committed — both in the base case,
//!   neither caught by the pre-implementation plan review.** (1) A
//!   struct-typed reg/out's `= init` is NOT actually mandatory the way
//!   a scalar reg/out's is: the scalar requirement is tied to THAT
//!   def's OWN `where` clause, but a v18 bound lives on the field, so
//!   `reg p : Pair` (no `where` on `p` itself) needs no init at all —
//!   `check_struct_field_inits` silently skipped it, unconditionally
//!   trusting `p`'s field with NOTHING ever verified, a real false
//!   proof for a two-sided bound (a no-init reg resets to all-zero
//!   fields, violating a nonzero floor). Fixed by rejecting a missing
//!   init whenever the struct type has any bounded field. (2) A
//!   struct-typed LOCAL can be REASSIGNED via `:=` (`types/stmt.rs`'s
//!   "must be a StructLit or Call" restriction only applies to a
//!   state target, not a `DefKind::Local`), so `struct_origins`'s entry
//!   from the local's own `Stmt::Let` kept pointing at its ORIGINAL
//!   binding forever, tracing a REASSIGNED local's field back to a
//!   stale, superseded value instead of whatever it was actually
//!   reassigned to. Fixed by having `Stmt::Assign` overwrite `struct_
//!   origins` on every reassignment to a struct-typed local, exactly
//!   like `Stmt::Let` does for the initial binding. Both confirmed by
//!   direct reproduction before fixing, both bug-reintroduction-
//!   verified, both closed by a dedicated `tests/bounds.rs` regression
//!   test.
//!
//! - **Reassigned locals never invalidate their forward-flow bound
//!   (v19)**: found by a LATER advisor pass over v18's own local-
//!   reassignment fix above, and turned out to be general — not
//!   struct-field- or v18-specific at all, and present since bounds.rs's
//!   very first commit (`bd109cc`). `locals` (the forward-flow map
//!   behind EVERY local's own bound, not just a struct field's origin)
//!   is written ONLY at `Stmt::Let` — `Stmt::Assign` never updates it
//!   for a reassigned SCALAR local either, so `let x = 10; x :=
//!   untrusted_input; total := x` composed `x` to its stale `let`-time
//!   bound with no branch involved at all. v18's own fix (overwrite
//!   `struct_origins` in `Stmt::Assign`) only ever helped the UNBRANCHED
//!   case anyway: inside an `if`/`while` the overwrite lands on a clone
//!   (`then_origins`/`loop_origins`, ...) that gets discarded when the
//!   branch ends, so the outer map still has the pre-branch value. A
//!   `while` loop rules out "poison the outer entry on branch exit" as a
//!   complete fix too: the body is checked ONCE against its entry
//!   snapshot, so a reassignment on iteration 1 is invisible when
//!   checking iteration 2's own read of the same local — confirmed via a
//!   driving repro before choosing a different shape. Fixed with
//!   `collect_reassigned_locals`: a pre-scan, run once per rule/fn body
//!   before any bound tracking begins, that finds every `DefKind::Local`
//!   ever targeted by a `Stmt::Assign` anywhere in that body (including
//!   nested inside `if`/`while`/`if let`/`while let`) and excludes it
//!   from EVER getting a `locals`/`struct_origins` entry, in any scope —
//!   the same "conservatively refuse rather than build merge/poisoning
//!   machinery" call already made for v17's mem reads and v18's `Call`-
//!   sourced struct writes. This SUPERSEDES v18's own `Stmt::Assign`
//!   overwrite (removed — it's not just redundant but actively wrong to
//!   keep: inserting a trusted entry there would reopen a narrower,
//!   still-unsound window between one reassignment and the next).
//!   Swept every existing test and example first for a `let`-bound
//!   local later reassigned AND read in an ASSIGNMENT-shaped bounded
//!   position (`total := x`) — nothing relies on it. The exclusion
//!   itself is syntax-position-agnostic by construction, not just by
//!   sweep coverage: `expr_bound`'s `Expr::Ident` arm is the SINGLE
//!   lookup path `locals` is ever read through, used identically
//!   whether the local appears as an assignment's rhs, a `Bump(x)`
//!   call argument, or nested inside arithmetic — there is no separate
//!   call-argument code path the exclusion could fail to reach.
//!   Confirmed with a dedicated call-argument repro (advisor follow-up)
//!   in addition to the sweep, so this costs zero expressiveness
//!   against the current suite in every checked position, not just the
//!   swept one. Confirmed via three escalating repros (unbranched,
//!   inside an `if`, inside a `while`) before fixing, bug-
//!   reintroduction-verified, closed by three dedicated `tests/
//!   bounds.rs` regression tests. `--explain-schedule` byte-diff and a
//!   `--firrtl` sanity check (both the compiler's own emitter and real
//!   `firtool`) came back clean.
//!
//! - **Relational bounds across MULTIPLE `reg`/`out` defs (`invariant
//!   <expr>`)**: DESIGN.md's "Tier 3, not v0" circular-buffer case —
//!   `push`/`pop`'s FIFO pointer/counter invariant (`head - tail ==
//!   push_count - pop_count`, mod depth) is a fact about a COMBINATION of
//!   four registers' joint write history, not any one or two registers'
//!   own values, and every prior bound in this file is exactly that: a
//!   single def's own range. A new item, `invariant <expr>` (a signed sum
//!   of `reg`/`out` idents, coefficients restricted to exactly ±1 — v1, no
//!   scalar multiplication — optionally reduced modulo an explicit `%
//!   <const>`, an existing operator, no new grammar; compared via
//!   `<`/`<=`/`=` against a literal), collected into `relational_bounds`
//!   and checked by a DEDICATED induction (`check_relational_bound_
//!   induction`) entirely separate from `check_item`/`check_body` — the
//!   base case sums each involved def's own literal `= init`; the
//!   inductive step finds every rule that writes an involved def
//!   (`walk_deltas`, computing each rule's own NET signed delta to the
//!   combination) and checks every SUBSET of those rules co-firing the
//!   SAME cycle, not just each in isolation — a genuinely different
//!   induction step than any single-def bound ever needed, since two
//!   simultaneously-firing rules' deltas can stack in a way neither
//!   alone would reveal. Sound only because v0's own scheduling model
//!   guarantees no two co-firing rules ever write the SAME def (a
//!   write-write conflict is rejected outright, `schedule.rs:451`), so
//!   summing independently-computed deltas is exactly the joint effect —
//!   a `bounds.rs` soundness argument now depending on a `schedule.rs`
//!   invariant, flagged explicitly rather than left implicit.
//!
//!   A rule's own leading guard narrows the induction hypothesis, but
//!   ONLY when its linear form EXACTLY matches the invariant's own terms
//!   (same defs, same coefficients) — a non-matching guard is silently
//!   ignored, always sound (a wider hypothesis than strictly justified is
//!   still safe, never the reverse). `circular_buffer_disjoint.tr`'s own
//!   `push_count - pop_count < 9` needs exactly this (both `push`'s `<
//!   8` and `pop`'s `push_count <> pop_count` narrow it — the latter a
//!   bare ident-vs-ident comparison, recognized by folding both sides into
//!   `(lhs - rhs) <op> 0`, not `push_count - pop_count <> 0` literally);
//!   its `(head - tail - push_count + pop_count) % 8 = 0` needs no guard
//!   at all, since every rule's own delta to that combination cancels to
//!   exactly zero. Confirmed genuinely different proof shapes, not
//!   assumed: weakening `push`'s guard from `< 8` to `<= 8` (admitting the
//!   exact occupancy-8 case it exists to rule out) makes the FIRST fact
//!   fail to verify — the load-bearing negative, this feature's own
//!   `m[i]` vs `m[2 - i]` analogue — while the second is unaffected either
//!   way.
//!
//!   One subtlety caught before shipping: `head`'s own `else { head := 0
//!   }` branch (taken when `head == 7`, via the SAME `where head < 8` +
//!   negated-condition narrowing v15 already established) computes delta
//!   `0 - 7 = -7`, but the `then` branch (`head := head + 1`) computes
//!   `+1` — NOT equal as raw integers, but congruent mod 8 (the second
//!   fact's own declared modulus). Requiring EXACT integer equality
//!   between a branch pair's own deltas (checked first, before the
//!   congruence relaxation) would have wrongly rejected the example's own
//!   motivating case; `deltas_agree` compares modulo the bound's declared
//!   modulus instead, since that's genuinely all the final arithmetic
//!   ever needs (`shift_preserves`'s own final containment check already
//!   reduces the combined delta mod `modulus`).
//!
//!   Gate #1 (flagged before implementation): a rule writing a named
//!   register only through a `fn`/`impl` call — never a directly-visible
//!   `Stmt::Assign` — must fail this bound closed, not silently compute a
//!   delta of zero for a write that's actually there. `walk_deltas`'s own
//!   findings are cross-checked against `effects.rs`'s `sig.writes`
//!   (which already accounts for a write reached transitively through a
//!   call, unlike this pass's own direct-assignment walk); any mismatch
//!   fails the whole bound closed. Bug-reintroduction-verified with a
//!   dedicated test.
//!
//!   Two side conditions rejected at DECLARATION time, not discovered
//!   mid-induction: the declared modulus must be a power of two dividing
//!   the involved defs' own native width (`M | 2^W` — reducing a mod-16
//!   wrapping computation to a NON-divisor, e.g. mod 3, isn't congruence-
//!   preserving); the declared range must fit within its own modulus
//!   (`upper <= M` — a wider range would silently accept anything once
//!   reduced). `check_item`'s own early-return guard widened again
//!   (FIVE-way, `relational_bounds.is_empty()`) for consistency with this
//!   arc's established discipline, though not actually load-bearing for
//!   THIS feature's own soundness (its induction never depends on `check_
//!   item` running at all).
//!
//!   v1 restrictions, deliberate scope cuts matching this file's existing
//!   bar: at most TWO rules may write registers a single invariant names
//!   (more fails closed — a real restriction, not a silent gap); a write's
//!   RHS must be `<def> +/- <const>` or a literal under a branch that
//!   pins `<def>` to an exact singleton (anything else fails closed); no
//!   `While`/`IfLet`/`WhileLet` inside a contributing rule's body at all
//!   (unconditional — no motivating example needs one).
//!
//! - **The `schedule.rs` consumer, `provably_disjoint_under_joint_
//!   guards`**: only VERIFIED relational bounds are ever exported
//!   (`Bounds.relational`, filtered by `Checker::failed_relational` — a
//!   live fail-open closed before this consumer existed to exploit it: an
//!   invariant that failed its base case or inductive step used to stay
//!   in `relational_bounds` right alongside a verified one). The new
//!   argument itself — the EIGHTH `forms_differ`-adjacent case, and the
//!   first needing no shared base/multiplier/proven range at all — finds
//!   a verified equality-to-zero fact naming the two mem-index bases as a
//!   cancelling `±1` pair plus one further cancelling pair (`other`), a
//!   SECOND verified fact bounding that same `other` pair with a REAL
//!   absolute range, narrows it via both accessing rules' own joint
//!   guards (`narrow_combo_range`, re-run here rather than cached from
//!   this file's own internal induction), and concludes the two bases
//!   differ whenever that narrowed range excludes the derived target
//!   value. `linear_form`/`recognize_comparison`/`narrow_combo_range`/
//!   `leading_guards` were refactored from `Checker` methods into free
//!   functions (with thin wrappers kept for every existing in-file call
//!   site) so this new public function — which has no `Checker` to call
//!   a method on — could reuse the same reasoning rather than
//!   reimplementing it. `--explain-schedule` on `circular_buffer_
//!   disjoint.tr` now reports `{push_count, pop_count}` — `m` dropped,
//!   stall still derived (an ordinary scalar hazard, unrelated to mem
//!   indexing, exactly as documented) — byte-diff identical on every
//!   OTHER example; an initial version that unconditionally subtracted
//!   proven mem defs from the reported set broke six EXISTING examples'
//!   own diagnostic text (`{m}` → `{}`) before `schedule.rs`'s own `on`
//!   computation was fixed to keep the FULL set whenever the pair ends up
//!   fully exempted anyway (see `schedule.rs`'s own doc comment). See
//!   DESIGN.md's "Tier 3" section for the full accounting.
//!
//! # Why a single forward walk, not a fixpoint (unlike `types.rs`'s Pass 2)
//!
//! `types/collect.rs`'s `WIDEN_CAP` loop exists because a WIDTH is one
//! property unified across an entire body: an early read may need
//! whatever width a LATER rebinding forces (Chisel-style single wire). A
//! BOUND is a per-program-point fact, not a whole-body-unified one —
//! narrower here, wider there, by design (an `if i < 8` guard only
//! narrows `i` INSIDE that branch). A single forward walk is the
//! architecturally correct model for this problem, not an approximation
//! of a fixpoint. Cross-cycle soundness comes from INDUCTION over write
//! sites: each item that can write a bounded def is checked once here
//! (assuming the invariant held at entry — i.e. the def's declared
//! bound, verified as this pass's own base case is the init check
//! already done in `types/stmt.rs`'s `check_where_bound_init`), and that
//! per-item argument, repeated across every item, is the whole proof; no
//! global fixpoint across items is needed.
//!
//! # Frozen reads, not forward-mutated (unlike a `Stmt::Let` local)
//!
//! Every read of a `reg`/`out` within a rule/fn body sees the value from
//! the START of the cycle (DESIGN.md: "a register read... sees the OLD
//! (pre-edge) value... the same rule every other register read in this
//! language follows"; `out` "behaves like a plain `reg` inside a rule —
//! same `:=` write, same effect row, same scheduling"), never a value
//! written earlier in the SAME body by an earlier statement. So a
//! bounded def's tracked bound is FROZEN at its declared limit for the
//! whole walk of one item — narrowed only by an enclosing `if <bounded
//! def> < <const>` guard's own branch, and reverted once that branch
//! ends. Writes are CHECKED against it, never allowed to update it.
//! Only `Stmt::Let` locals get real (blocking) forward-flow tracking,
//! needed for the realistic `let next = i + 1; i := next` shape — EXCEPT
//! a local ever reassigned via `Stmt::Assign` anywhere in its own body,
//! which `collect_reassigned_locals` excludes from tracking entirely
//! (v19, see above): its bound is never trusted at any read, in any
//! scope, once reassigned even once.
//!
//! # Branch/loop scoping (a deliberate v1 simplification)
//!
//! `Stmt::Let` is body-wide visible past the `if`/`while` it's declared
//! in (unlike `Stmt::IfLet`/`Stmt::WhileLet`'s own branch-only scoping —
//! see `ast.rs`'s own doc comment on `IfLet`). This pass does NOT
//! attempt to track a local's bound past the branch/loop it was
//! computed in: entering ANY nested scope (`if`/`while`/`if let`/`while
//! let`) clones both the frozen state-bound map and the locals map,
//! walks the nested body against the clone, then DISCARDS it — the
//! outer walk continues against the untouched originals. This is
//! strictly conservative, never unsound: a local whose bound genuinely
//! would still be known after the branch (per the language's real
//! scoping rules) is simply treated as "unknown" there instead, which
//! only ever causes a write to fail closed (a compile error), never a
//! false proof. Extending this to a real branch-merge/join would need
//! more machinery than any current example motivates.

mod smt;

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, Param, Stmt, StmtId};
use crate::effects::Effects;
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};
use crate::types::{Ty, Types, Width};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundsError {
    pub span: Span,
    pub message: String,
}

/// Every `reg` with a proven `where` bound, and the `(lower, upper)`
/// range itself (lower inclusive, upper exclusive). Presence in this
/// map, regardless of the value, means the bound was successfully
/// proven across the whole program — a reg with no `where` clause is
/// simply absent. Consumed by `schedule.rs`'s own disjointness
/// arguments.
#[derive(Debug, Default)]
pub struct Bounds {
    pub ranges: HashMap<DefId, (u64, u64)>,
    /// v16: a per-SITE proven range, keyed by the specific `ExprId`
    /// this pass visited it at — tighter than (or equal to) `ranges`'
    /// own flat, whole-program range whenever that expression sits
    /// under a narrowing condition (`if i < 10 { m[i] := x }` proves a
    /// tighter range for THIS `m[i]`'s own index than `i`'s raw
    /// declared bound). Populated only for a mem access's own index
    /// expression (`check_calls_in`'s widened stop-list, `expr_bound`'s
    /// `Expr::Bracket` arm) -- absence here is never a signal that the
    /// expression is unprovable, only that this pass never computed
    /// (or never visited) a site-specific fact for it; `schedule.rs`'s
    /// own `real_range` falls back to its independent walk whenever a
    /// lookup here misses.
    pub site_ranges: HashMap<ExprId, (u64, u64)>,
    /// Every VERIFIED `invariant` (DESIGN.md's "Tier 3, not v0" circular-
    /// buffer case) -- an `Item::Invariant` this pass recognized but
    /// failed to prove (a bad base case, or a failed inductive step) is
    /// deliberately NOT here, even though `recognize_invariant` accepted
    /// its shape (see `Checker::failed_relational`'s own doc comment: a
    /// live fail-open, closed before any consumer existed to exploit
    /// it). Consumed by `schedule.rs`'s own `provably_disjoint_under_
    /// joint_guards` -- see that function's own doc comment for how.
    pub relational: Vec<RelationalFact>,
}

/// The exported (verified-only) form of a `RelationalBound` -- same
/// fields, but a `pub` type so `schedule.rs` can read them without
/// reaching into `bounds.rs`'s own private `Checker` state. No `span`:
/// nothing outside `bounds.rs` itself ever needs to point an error at a
/// declaration site through this type.
#[derive(Clone, Debug)]
pub struct RelationalFact {
    pub terms: Vec<(DefId, i64)>,
    pub modulus: u64,
    pub lower: u64,
    pub upper: u64,
}

/// The provenance judgment (DESIGN.md's stage 2 "toward a dependent/
/// refinement type system" section) as a real TYPE distinction, not a
/// naming convention: every value ever stored in `self.bounded`/
/// `mem_bounds`/`struct_field_bounds`/`fn_ret_bound` is, today, only
/// ever inserted by one of the four `collect_one_*` collectors, each of
/// which const-folds the declared bound and resolves a concrete width
/// FIRST -- but nothing stops a future edit from constructing a raw
/// `BoundedDef { lower, upper, width }` ad hoc elsewhere in this module
/// (its fields are private to `bounds`, not to any one function) and
/// inserting an unchecked/assumed fact into one of those maps by
/// accident.
///
/// `Proven` lives in its own child submodule specifically so `value` is
/// private to THAT submodule, not merely to `bounds` -- `checked` is the
/// only way code outside `refinement` can ever build one, and every
/// caller of it is a `collect_one_*` collector immediately after its own
/// verification. An assumed fact (an `in` port's declared type, an
/// unwritten mem read, ...) is never representable by this type at all,
/// structurally, regardless of anyone's discipline.
mod refinement {
    #[derive(Clone, Copy)]
    pub(super) struct Proven<T> {
        value: T,
    }

    impl<T> Proven<T> {
        /// Called ONLY by a `collect_one_*` collector, immediately after
        /// it has confirmed `value` came from a const-folded, width-
        /// checked declaration -- see each call site for its own specific
        /// verification.
        pub(super) fn checked(value: T) -> Self {
            Proven { value }
        }
    }

    impl<T> std::ops::Deref for Proven<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.value
        }
    }
}
use refinement::Proven;

/// One bounded def's (a `reg`, `out`, or fn/impl param, v12) own
/// declared facts, collected once up front.
#[derive(Clone, Copy)]
struct BoundedDef {
    lower: u64,
    upper: u64,
    width: u64,
}

/// One `invariant` item's own declared fact (see `Item::Invariant`'s
/// doc comment, ast.rs) -- a signed sum of `reg`/`out` `DefId`s (v1
/// restriction: every coefficient is exactly +-1, no scalar
/// multiplication -- matching this feature's only two motivating facts,
/// `push_count - pop_count` and `head - tail - push_count + pop_count`),
/// evaluated modulo `modulus` and declared to stay within `[lower,
/// upper)`. Unlike `BoundedDef` (a single def's OWN range, narrowed
/// per-branch in `self.bounded`'s `state` map), this is a flat,
/// whole-program fact about a COMBINATION of defs, checked by a
/// dedicated induction (`check_relational_bound_induction`) entirely
/// separate from the ordinary per-item `check_body` walk -- see that
/// function's own doc comment for the full argument.
/// `recognize_comparison`'s own return shape: `(op, terms, const)` — a
/// type alias purely to keep that signature (and its one call site)
/// readable, per clippy's own suggestion; no behavior of its own.
type Comparison = (BinOp, Vec<(DefId, i64)>, u64);

/// One `Stmt::Assign`'s own write obligation: the def to mark found, its
/// declared bound, the computed value to check against it, and the
/// context string for the error message -- see `check_stmt`'s own
/// `Stmt::Assign` arm, which builds a `Vec` of these (one shared tail
/// for what were three duplicated `found_writes`/`check_against_bound`
/// call sites, not a new mechanism).
type WriteObligation = (DefId, BoundedDef, Option<(u64, u64)>, String);

#[derive(Clone)]
struct RelationalBound {
    terms: Vec<(DefId, i64)>,
    modulus: u64,
    lower: u64,
    upper: u64,
    span: Span,
}

pub fn check(ast: &Ast, res: &Resolution, fx: &Effects, ty: &Types) -> (Bounds, Vec<BoundsError>) {
    let mut checker = Checker {
        ast,
        res,
        fx,
        ty,
        bounded: HashMap::new(),
        fn_params: HashMap::new(),
        fn_ret_bound: HashMap::new(),
        ret_bound_span: HashMap::new(),
        current_ret_bound: None,
        current_fn_def: None,
        current_reassigned_locals: HashSet::new(),
        found_writes: HashSet::new(),
        found_returns: HashSet::new(),
        site_ranges: HashMap::new(),
        mem_bounds: HashMap::new(),
        mem_bound_span: HashMap::new(),
        struct_field_bounds: HashMap::new(),
        relational_bounds: Vec::new(),
        reg_out_inits: HashMap::new(),
        failed_relational: HashSet::new(),
        errors: Vec::new(),
    };
    checker.collect_bounded_defs();
    checker.collect_bounded_params();
    checker.collect_mem_bounds();
    // v18: collect every struct field's own declared bound FIRST, then
    // check every struct-typed reg/out's `= init` against it in a
    // second, separate pass -- interleaving collection with checking
    // would risk checking a reg whose struct type's own fields haven't
    // been collected yet (a struct declared textually after the reg
    // that uses it, or forward-referenced across modules).
    checker.collect_struct_field_bounds();
    checker.check_struct_field_inits();
    // Tier-3 relational bounds (`invariant`, DESIGN.md's "Tier 3, not
    // v0" circular-buffer case): collected and checked entirely
    // separately from every other bound above, via its own dedicated
    // induction -- see `check_relational_bound_induction`'s own doc
    // comment. Runs AFTER `collect_bounded_defs` so `self.bounded`
    // (needed for a matching guard's own `where`-bound narrowing, e.g.
    // `head`/`tail`'s `where < 8`) is already populated.
    checker.collect_relational_bounds();
    checker.check_relational_bound_inits();
    for i in 0..checker.relational_bounds.len() {
        checker.check_relational_bound_induction(i);
    }
    let bodied = checker.collect_bodied_items();
    for id in &bodied {
        checker.check_item(*id);
    }
    checker.check_write_site_exhaustiveness();
    checker.check_return_site_exhaustiveness();
    checker.check_mem_bound_is_proven();
    // Stage 1 of DESIGN.md's SMT-backed type-system plan: shadow-check
    // every scalar write obligation this pass just verified via interval
    // arithmetic, independently via Z3. Panics on a real disagreement
    // (not a `Skipped` site) -- see `shadow_check_bounds`'s own
    // doc comment for why this is safe to run unconditionally rather
    // than gated behind a flag.
    let mut mismatches = checker.shadow_check_bounds();
    mismatches.extend(checker.shadow_check_relational_bounds());
    if let Some(first) = mismatches.first() {
        panic!(
            "bounds.rs internal error: the SMT shadow check (DESIGN.md's stage 1) disagrees \
             with the interval-arithmetic engine on {} site(s) -- first, at {:?}: interval \
             engine says in-bound = {}, SMT says {:?}",
            mismatches.len(),
            first.span,
            first.interval_says_in_bound,
            first.smt
        );
    }
    let bounds = Bounds {
        ranges: checker
            .bounded
            .iter()
            .map(|(def, b)| (*def, (b.lower, b.upper)))
            .collect(),
        site_ranges: checker.site_ranges,
        relational: checker
            .relational_bounds
            .iter()
            .enumerate()
            .filter(|(i, _)| !checker.failed_relational.contains(i))
            .map(|(_, rb)| RelationalFact {
                terms: rb.terms.clone(),
                modulus: rb.modulus,
                lower: rb.lower,
                upper: rb.upper,
            })
            .collect(),
    };
    (bounds, checker.errors)
}

/// The `schedule.rs` consumer half of DESIGN.md's "Tier 3, not v0"
/// circular-buffer case: whether `idx_a`/`idx_b` (two BARE-IDENT mem-
/// index bases -- `schedule.rs`'s own `IndexForm` recognition already
/// narrows to this shape before ever calling this) are PROVABLY
/// DISTINCT whenever `rule_a`/`rule_b` (their own accessing rules) both
/// fire the SAME cycle. This is the genuinely general argument every
/// `forms_differ` case in `schedule.rs` (v1 through v7) can't make on
/// its own: none of them need a shared base, a shared multiplier, or a
/// per-register proven range the way this does -- this needs a
/// RELATIONAL fact linking `idx_a`/`idx_b` to two OTHER defs this
/// module can independently bound.
///
/// Recognizes exactly ONE shape (v1, deliberately narrow): a VERIFIED
/// equality-to-zero fact (`link`, `bounds.relational`) naming `idx_a`/
/// `idx_b` as a cancelling `+-1` pair (`ca == -cb`) plus EXACTLY one
/// further cancelling pair (`other`); and a SECOND verified fact
/// (`range_fact`) whose own terms equal `other` (or its negation),
/// carrying a REAL absolute range (not just a congruence) for that
/// same pair. `circular_buffer_disjoint.tr`'s own two invariants are
/// exactly this: `link` = `head - tail - push_count + pop_count ≡ 0
/// (mod 8)`, `other` = `{push_count: -1, pop_count: +1}`, `range_fact` =
/// `push_count - pop_count < 9` (the SAME two defs, negated sign,
/// modulus 16).
///
/// The derivation (`link`'s own equation, `cb = -ca`): `idx_a - idx_b ≡
/// ca * (link.lower - other_expr) (mod link.modulus)`, so `idx_a ==
/// idx_b` iff `other_expr ≡ link.lower (mod link.modulus)` -- notably
/// INDEPENDENT of `ca`'s own sign, so `idx_a`/`idx_b`'s own coefficients
/// never need to be untangled further. `range_fact`'s own guard-
/// narrowed range (via `rule_a`'s and `rule_b`'s own leading guards,
/// `narrow_combo_range` -- the SAME function `bounds.rs`'s own
/// induction uses internally, re-run here rather than cached from it)
/// gives a REAL range for `other_expr` (if `range_fact.terms ==
/// other`) or for `-other_expr` (if `range_fact.terms == -other`, in
/// which case the target flips to `(link.modulus - link.lower) %
/// link.modulus` instead -- avoiding any interval negation, which the
/// general case would otherwise need). Concludes `idx_a != idx_b` when
/// that range excludes the target.
///
/// v1 restrictions (fails closed / `false`, never a silent gap): `link`
/// must have exactly 4 terms (2 cancelling pairs, the FIFO pointer/
/// counter shape -- no larger combination); `range_fact`'s own guard-
/// narrowed range must already fit within `link.modulus` WITHOUT
/// wraparound (`range_fits_modulus` -- a genuine cross-modulus
/// reduction, needed whenever `range_fact`'s own native modulus differs
/// from `link`'s, is not attempted); `idx_a`==`idx_b` (the SAME def) is
/// never asked about -- callers already exclude that via `IndexForm`'s
/// own base comparison.
pub fn provably_disjoint_under_joint_guards(
    ast: &Ast,
    res: &Resolution,
    bounds: &Bounds,
    idx_a: DefId,
    idx_b: DefId,
    rule_a: ItemId,
    rule_b: ItemId,
) -> bool {
    let rule_guards = |rule: ItemId| -> Vec<ExprId> {
        match ast.item(rule) {
            Item::Rule { body, .. } => leading_guards(ast, body),
            _ => Vec::new(),
        }
    };
    let guards_a = rule_guards(rule_a);
    let guards_b = rule_guards(rule_b);

    for link in &bounds.relational {
        if link.terms.len() != 4 || link.upper != link.lower + 1 {
            continue; // v1: exactly the 4-term, equality-to-zero shape
        }
        let ca = link
            .terms
            .iter()
            .find(|(d, _)| *d == idx_a)
            .map(|(_, c)| *c);
        let cb = link
            .terms
            .iter()
            .find(|(d, _)| *d == idx_b)
            .map(|(_, c)| *c);
        let (Some(ca), Some(cb)) = (ca, cb) else {
            continue;
        };
        if ca != -cb {
            continue;
        }
        let other: Vec<(DefId, i64)> = link
            .terms
            .iter()
            .filter(|(d, _)| *d != idx_a && *d != idx_b)
            .copied()
            .collect();
        if other.len() != 2 {
            continue;
        }
        let negated_other: Vec<(DefId, i64)> = other.iter().map(|(d, c)| (*d, -c)).collect();

        for range_fact in &bounds.relational {
            let sign = if same_terms(&range_fact.terms, &other) {
                1i64
            } else if same_terms(&range_fact.terms, &negated_other) {
                -1i64
            } else {
                continue;
            };
            let mut range = (range_fact.lower, range_fact.upper);
            for &guard in guards_a.iter().chain(guards_b.iter()) {
                range = narrow_combo_range(ast, res, &range_fact.terms, guard, range);
            }
            let Some((lo, hi)) = range_fits_modulus(range, link.modulus) else {
                continue;
            };
            if lo >= hi {
                continue; // an empty/vacuous range proves nothing about `other_expr`'s value
            }
            let lower_mod = link.lower % link.modulus;
            let target = if sign == 1 {
                lower_mod
            } else {
                (link.modulus - lower_mod) % link.modulus
            };
            if !(lo..hi).contains(&target) {
                return true;
            }
        }
    }
    false
}

/// Whether `range` (a real, non-wrapping `[lo, hi)` in its own NATIVE
/// modulus) can be trusted UNCHANGED as a range modulo `to_modulus` --
/// true only when it already fits (`hi <= to_modulus`), i.e. no value
/// in it is large enough to need actual modular reduction. A range that
/// exceeds `to_modulus` fails closed (`None`) rather than attempting a
/// genuine cross-modulus wraparound reduction (`shift_preserves`'s own
/// "reduce by SOME multiple `k`, require no straddle" logic could be
/// adapted for it, but no motivating case needs the extra generality
/// yet -- v1 restriction, not an oversight).
fn range_fits_modulus(range: (u64, u64), to_modulus: u64) -> Option<(u64, u64)> {
    let (lo, hi) = range;
    if lo >= hi {
        return Some(range); // vacuous either way
    }
    if hi <= to_modulus { Some(range) } else { None }
}

struct Checker<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    fx: &'a Effects,
    ty: &'a Types,
    bounded: HashMap<DefId, Proven<BoundedDef>>,
    /// Every `Item::Fn`'s own `DefId` (from `res.item_defs`) mapped to
    /// its cloned `params` list (v12) — consulted at each `Expr::Call`
    /// site to check the corresponding argument's own provable range
    /// against a bounded param's declared bound. Cheap to clone once
    /// per fn during collection rather than re-deriving per call site.
    fn_params: HashMap<DefId, Vec<Param>>,
    /// Every `Item::Fn`'s own `DefId` mapped to its declared return
    /// postcondition (v13) — the mirror of a bounded param, but keyed
    /// by the FN's own def rather than a param's (a return value has
    /// no `DefId` of its own; nothing to key `self.bounded` by).
    /// Consulted at each `Expr::Call` site to let a caller compose with
    /// the call's own provable range, and at the top of `check_item` to
    /// check every `Stmt::Return` in the callee's OWN body against it.
    fn_ret_bound: HashMap<DefId, Proven<BoundedDef>>,
    /// Every `fn_ret_bound` entry's own declaration span — kept
    /// separate from `BoundedDef` (which has no span field, and is
    /// shared with reg/out/param bounds that don't need one) purely so
    /// `check_return_site_exhaustiveness` has somewhere to point an
    /// error at a declared postcondition with no `return` to check it
    /// against.
    ret_bound_span: HashMap<DefId, Span>,
    /// The CURRENT item's own declared return postcondition, if any —
    /// set once at the top of `check_item` (from `fn_ret_bound`, via
    /// the item's own `DefId`) and consulted by every `Stmt::Return` in
    /// that one body. `None` for a `rule` (no return value at all) or a
    /// `fn`/`impl` with no declared postcondition (nothing to check).
    current_ret_bound: Option<BoundedDef>,
    /// The CURRENT item's own `DefId`, set alongside `current_ret_
    /// bound` — lets `Stmt::Return`'s own arm record into `found_
    /// returns` WHICH fn actually had a checked return site, not just
    /// that some `Stmt::Return` was seen somewhere.
    current_fn_def: Option<DefId>,
    /// Every `DefKind::Local` the CURRENT item ever reassigns via
    /// `Stmt::Assign`, anywhere in its body -- set once at the top of
    /// `check_item` via `collect_reassigned_locals`, read by `Stmt::
    /// Let`'s own arm to decide whether a local's bound is trustworthy
    /// to record at all. See `collect_reassigned_locals`'s own doc
    /// comment for why this exists (a pre-existing, pre-v18 hole where
    /// a reassigned local's STALE bound kept composing).
    current_reassigned_locals: HashSet<DefId>,
    /// Every bounded def this pass found an ACTUAL `Stmt::Assign` for,
    /// anywhere in the program — cross-checked against `fx`'s own
    /// per-item write sets once the whole walk finishes (defense in
    /// depth: catches this pass's own walk silently missing a real
    /// write site, rather than trusting-by-construction that it never
    /// would).
    found_writes: HashSet<DefId>,
    /// Every fn `DefId` this pass found an ACTUAL `Stmt::Return(Some(_))`
    /// for, checked against its OWN declared postcondition — cross-
    /// checked against `fn_ret_bound`'s own keys once the whole walk
    /// finishes (v13). Unlike `found_writes` (defense in depth against
    /// this pass's own walk missing a site that `effects.rs`
    /// independently confirms exists), there's no independent oracle
    /// here — a fn with a declared postcondition and literally no
    /// `return` statement in its body is possible to WRITE (nothing
    /// upstream requires one), and without this check its postcondition
    /// would be trusted at every call site with ZERO obligations ever
    /// verified: a real fail-open soundness hole, not just an internal
    /// invariant.
    found_returns: HashSet<DefId>,
    /// v16: per-site proven ranges for a mem access's own index
    /// expression, exported into `Bounds.site_ranges` once the whole
    /// walk finishes — see that field's own doc comment. Populated by
    /// `expr_bound`'s `Expr::Bracket` arm; never merged or overwritten,
    /// since no `ExprId` is ever visited by this pass's forward walk
    /// more than once under a different `state` (see that arm's own
    /// doc comment for why).
    site_ranges: HashMap<ExprId, (u64, u64)>,
    /// v17: every `mem` with a declared `where _ < K` bound, keyed by
    /// its own `DefId` -- kept separate from `self.bounded` rather than
    /// folded in, since a mem's bound is a flat, whole-array fact
    /// checked at every write site (never narrowed per-branch the way
    /// `self.bounded`'s per-item `state` map is), the same reasoning
    /// `fn_ret_bound` is kept separate from `self.bounded` for. Consulted
    /// ONLY at a `Stmt::Assign` write site (`m[i] := rhs` checks `rhs`
    /// against it) -- deliberately NOT consulted by `expr_bound`'s own
    /// `Expr::Bracket` arm at a mem READ, which still composes to `None`
    /// unconditionally: proving every WRITE stays in range says nothing
    /// about what an uninitialized or not-yet-written READ returns (a
    /// mem has no `init`/reset the way a reg/out does) -- see that arm's
    /// own doc comment for the soundness hole this avoided.
    mem_bounds: HashMap<DefId, Proven<BoundedDef>>,
    /// Every `mem_bounds` entry's own declaration span -- mirrors `ret_
    /// bound_span` exactly, needed by `check_mem_bound_is_proven` to
    /// point an error at a declared bound with no write site to prove it
    /// against.
    mem_bound_span: HashMap<DefId, Span>,
    /// v18: every struct FIELD with a declared `where _ < K` bound,
    /// keyed by `(struct's own DefId, field name)` -- a flat, per-
    /// struct-TYPE declared fact, mirroring `mem_bounds` exactly (never
    /// narrowed per-branch, since a struct has no per-field `DefId` to
    /// key a `state`-style map by). Consulted at every `StructLit`
    /// construction site (a reg/out write, a struct-typed local's own
    /// binding, or a struct-typed reg/out's `= init`) via `struct_field_
    /// bound`, and read back at `Expr::Field` -- SOUNDLY, unlike a mem
    /// element: every struct value in this language is built by an
    /// exhaustive `StructLit` (types/stmt.rs rejects any struct-typed
    /// state write that isn't one, or a `Call`), so a struct-typed reg/
    /// out has exactly the checked-init-plus-checked-every-write
    /// induction that already makes a scalar reg/out's own bound sound
    /// -- no "unwritten address" concept exists for a struct the way it
    /// does for a mem. See `struct_field_bound`'s own doc comment for
    /// the DefKind gate this soundness argument depends on (an `in`/
    /// mem/fifo-typed struct value never passes through a checked
    /// `StructLit` at all, exactly like an unwritten mem address).
    struct_field_bounds: HashMap<(DefId, String), Proven<BoundedDef>>,
    /// Every declared `invariant` (DESIGN.md's "Tier 3, not v0"
    /// circular-buffer case) recognized by `collect_relational_bounds`
    /// -- an item that FAILED to recognize (an unsupported shape,
    /// reported at collection time) never gets an entry here at all,
    /// same "collect only what was successfully validated" convention
    /// `self.bounded`/`mem_bounds`/`struct_field_bounds` already follow.
    relational_bounds: Vec<RelationalBound>,
    /// Every `reg`/`out`'s own `= init` expression, keyed by `DefId` --
    /// populated by `collect_relational_bounds`'s own walk (piggybacked
    /// onto the same traversal, rather than a separate one) purely so
    /// `check_relational_bound_inits` has a reverse `DefId -> init`
    /// lookup; `res.item_defs` only maps the other direction.
    reg_out_inits: HashMap<DefId, Option<ExprId>>,
    /// Indices into `relational_bounds` that FAILED either the base case
    /// (`check_relational_bound_inits`) or the inductive step (`check_
    /// relational_bound_induction`) -- flagged before any `schedule.rs`
    /// consumer existed: without this, an unproven `RelationalBound`
    /// would sit in `relational_bounds` right alongside a verified one,
    /// indistinguishable to any downstream reader that didn't separately
    /// re-run the whole induction. `Bounds.relational` (exported once
    /// the whole walk finishes) includes ONLY the entries NOT in this
    /// set -- a real, would-have-been-live fail-open closed before it
    /// was ever exploitable, since nothing consumed `relational_bounds`
    /// externally until this field existed.
    failed_relational: HashSet<usize>,
    errors: Vec<BoundsError>,
}

impl<'a> Checker<'a> {
    fn error(&mut self, span: Span, message: String) {
        self.errors.push(BoundsError { span, message });
    }

    /// Every `reg`/`out` with a `where` bound, keyed by its own `DefId`.
    /// The bound's shape (`Binary { Lt, Ident(self), <const> }`) is
    /// guaranteed by construction: the parser only ever builds a
    /// `where` clause this way (hard-requires the literal `<` token),
    /// and `resolve.rs` already requires the LHS self-reference this
    /// same def — so nothing left to validate here but extracting the
    /// range and the def's own declared width. `lower` (`None` for the
    /// one-sided surface form) is trusted to already const-fold to less
    /// than the upper limit — `types/stmt.rs`'s `check_where_bound_init`
    /// already validated exactly that as this bound's base case.
    fn collect_bounded_defs(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Reg {
                    bound: Some(bound),
                    lower,
                    ..
                }
                | Item::Output {
                    bound: Some(bound),
                    lower,
                    ..
                } => {
                    if let Some(&def) = self.res.item_defs.get(&id) {
                        self.collect_one_bounded_def(def, *bound, *lower);
                    }
                }
                _ => {}
            }
        }
    }

    /// Every `fn`/`impl` parameter with a `where` bound (v12), collected
    /// the same way `collect_bounded_defs` collects reg/out — inserted
    /// into the SAME `self.bounded` map, so a bounded param's OWN body-
    /// walk (as `check_item` walks `Item::Fn` bodies too) treats it
    /// exactly like a bounded reg with zero new narrowing/composition
    /// logic. Also records each fn's own `params` list, keyed by the
    /// fn's own `DefId`, consulted at call sites to check arguments.
    fn collect_bounded_params(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Fn {
                    params,
                    ret,
                    ret_bound,
                    ret_lower,
                    ..
                } => {
                    for param in params {
                        if let Some(bound) = param.bound {
                            let def = def_of_name(self.res, &param.name);
                            self.collect_one_bounded_def(def, bound, param.lower);
                        }
                    }
                    if let Some(&fn_def) = self.res.item_defs.get(&id) {
                        self.fn_params.insert(fn_def, params.clone());
                        // v13: the mirror of the param loop above, but
                        // for the fn's own declared return postcondition.
                        // `ret.is_none()` here means resolve.rs already
                        // reported "a return bound needs a declared
                        // return type" -- nothing to check the width
                        // against, so silently skip (same convention
                        // `collect_one_bounded_def` already follows for
                        // an unfoldable bound: "types.rs already
                        // reported this").
                        if let (Some(bound), Some(ret)) = (*ret_bound, *ret) {
                            self.collect_one_ret_bound(fn_def, ret, bound, *ret_lower);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The shared body behind `collect_bounded_defs`/`collect_bounded_
    /// params` (a reg/out/param are otherwise structurally identical
    /// here — see those functions' own doc comments). Takes the def
    /// directly (not an `ItemId`) since a param has none of its own.
    fn collect_one_bounded_def(&mut self, def: DefId, bound: ExprId, lower: Option<ExprId>) {
        let Expr::Binary { rhs, .. } = self.ast.expr(bound) else {
            return;
        };
        let Some(upper) = const_fold(self.ast, *rhs) else {
            return; // types.rs already reported this
        };
        let lower_val = match lower {
            Some(l) => match const_fold(self.ast, l) {
                Some(v) => v,
                None => return, // types.rs already reported this
            },
            None => 0,
        };
        let Some(width) = base_width(self.ty, def) else {
            // A concretely-known `bits[N]` width is exactly what every
            // check below needs to clamp a composed bound against — an
            // unknown width (e.g. a non-elaboration-constant size
            // expression, silently `Ty::Bits(Width::Unknown)` per
            // `types/eval.rs`, with no error of its own) must not let
            // the whole `where` clause go unchecked with zero
            // diagnostic.
            let span = self.ast.expr_spans[bound.0 as usize].clone();
            self.error(
                span,
                "a `where` bound needs a reg/out/param with a concretely-known `bits[N]` width \
                 to check against (v0 restriction)"
                    .to_string(),
            );
            return;
        };
        self.bounded.insert(
            def,
            Proven::checked(BoundedDef {
                lower: lower_val,
                upper,
                width,
            }),
        );
    }

    /// The return-bound sibling of `collect_one_bounded_def` (v13) —
    /// same const-fold-and-report shape, but keyed by the FN's own
    /// `DefId` into `fn_ret_bound` rather than by a state/param def
    /// into `self.bounded` (a return value has no `DefId` of its own),
    /// and sourcing its width from `ret_width` (the raw `ret` type
    /// expression) rather than `base_width` (a `DefId`'s entry in
    /// `state_tys`/`local_tys`, which doesn't exist for a return
    /// value). Kept separate rather than forcing a shared abstraction:
    /// the two width sources are genuinely different, and this file
    /// only extracts a shared helper once near-identical branches
    /// exist WITHIN one function (see `check_against_bound`'s own doc
    /// comment), not across two top-level collectors like these.
    fn collect_one_ret_bound(
        &mut self,
        fn_def: DefId,
        ret_ty: ExprId,
        bound: ExprId,
        lower: Option<ExprId>,
    ) {
        let Expr::Binary { rhs, .. } = self.ast.expr(bound) else {
            return;
        };
        let Some(upper) = const_fold(self.ast, *rhs) else {
            return; // types.rs already reported this
        };
        let lower_val = match lower {
            Some(l) => match const_fold(self.ast, l) {
                Some(v) => v,
                None => return, // types.rs already reported this
            },
            None => 0,
        };
        let Some(width) = ret_width(self.ast, ret_ty) else {
            let span = self.ast.expr_spans[bound.0 as usize].clone();
            self.error(
                span,
                "a return bound needs a concretely-known `bits[N]` return type to check \
                 against (v0 restriction)"
                    .to_string(),
            );
            return;
        };
        self.fn_ret_bound.insert(
            fn_def,
            Proven::checked(BoundedDef {
                lower: lower_val,
                upper,
                width,
            }),
        );
        self.ret_bound_span
            .insert(fn_def, self.ast.expr_spans[bound.0 as usize].clone());
    }

    /// Every `mem` with a `where _ < K` bound (v17), collected the
    /// same top-level-item-walk shape `collect_bounded_defs` uses for
    /// reg/out.
    fn collect_mem_bounds(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Mem {
                    bound: Some(bound),
                    lower,
                    ..
                } => {
                    if let Some(&def) = self.res.item_defs.get(&id) {
                        self.collect_one_mem_bound(def, *bound, *lower);
                    }
                }
                _ => {}
            }
        }
    }

    /// The mem-bound sibling of `collect_one_ret_bound` immediately
    /// above — same const-fold-and-report shape, but keyed by the MEM's
    /// own `DefId` into `self.mem_bounds` rather than the fn's into
    /// `fn_ret_bound`, and sourcing its width from `mem_elem_width` (the
    /// mem's own ELEMENT type, read out of `Ty::Mem { elem, .. }` in
    /// `state_tys`) rather than `ret_width` (a raw, unbound type
    /// expression — a return value has no `DefId`/`state_tys` entry of
    /// its own at all, unlike a mem). Kept as its own function for the
    /// same reason `collect_one_ret_bound`'s own doc comment gives: the
    /// width source genuinely differs each time, not worth forcing into
    /// one shared helper across three top-level collectors.
    fn collect_one_mem_bound(&mut self, def: DefId, bound: ExprId, lower: Option<ExprId>) {
        let Expr::Binary { rhs, .. } = self.ast.expr(bound) else {
            return;
        };
        let Some(upper) = const_fold(self.ast, *rhs) else {
            return; // types.rs already reported this
        };
        let lower_val = match lower {
            Some(l) => match const_fold(self.ast, l) {
                Some(v) => v,
                None => return, // types.rs already reported this
            },
            None => 0,
        };
        let Some(width) = mem_elem_width(self.ty, def) else {
            let span = self.ast.expr_spans[bound.0 as usize].clone();
            self.error(
                span,
                "a mem bound needs a concretely-known `bits[N]` element type to check against \
                 (v0 restriction)"
                    .to_string(),
            );
            return;
        };
        self.mem_bounds.insert(
            def,
            Proven::checked(BoundedDef {
                lower: lower_val,
                upper,
                width,
            }),
        );
        self.mem_bound_span
            .insert(def, self.ast.expr_spans[bound.0 as usize].clone());
    }

    /// Every struct FIELD with a declared `where _ < K` bound (v18),
    /// walking every `Item::Struct` in the program directly -- unlike
    /// `collect_bounded_defs`/`collect_mem_bounds`, this never needs
    /// `res.item_defs` (a field has no `DefId` of its own to check
    /// self-reference against; `resolve.rs`'s `check_struct_field_bound_
    /// shape` already validated the shape by TEXT), just the struct
    /// ITEM's own `DefId` (to key `struct_field_bounds` by) and its
    /// `fields: Vec<Param>` directly off the AST.
    fn collect_struct_field_bounds(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Struct { fields, .. } => {
                    let Some(&struct_def) = self.res.item_defs.get(&id) else {
                        continue;
                    };
                    for field in fields.clone() {
                        if let Some(bound) = field.bound {
                            self.collect_one_struct_field_bound(
                                struct_def,
                                &field.name.text,
                                bound,
                                field.lower,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The struct-field sibling of `collect_one_mem_bound`/`collect_one_
    /// ret_bound` immediately above — same const-fold-and-report shape,
    /// but keyed by `(struct_def, field name)` into `struct_field_
    /// bounds` rather than a single `DefId`, and sourcing its width from
    /// `struct_field_width` (the field's own declared type, read out of
    /// `Types.struct_fields`) rather than `mem_elem_width`/`ret_width`.
    /// Kept as its own function for the same reason those two give: the
    /// width SOURCE genuinely differs each time.
    fn collect_one_struct_field_bound(
        &mut self,
        struct_def: DefId,
        field_name: &str,
        bound: ExprId,
        lower: Option<ExprId>,
    ) {
        let Expr::Binary { rhs, .. } = self.ast.expr(bound) else {
            return;
        };
        let Some(upper) = const_fold(self.ast, *rhs) else {
            return; // types.rs already reported this
        };
        let lower_val = match lower {
            Some(l) => match const_fold(self.ast, l) {
                Some(v) => v,
                None => return, // types.rs already reported this
            },
            None => 0,
        };
        let Some(width) = struct_field_width(self.ty, struct_def, field_name) else {
            let span = self.ast.expr_spans[bound.0 as usize].clone();
            self.error(
                span,
                "a struct field bound needs a concretely-known `bits[N]` field type to check \
                 against (v0 restriction)"
                    .to_string(),
            );
            return;
        };
        self.struct_field_bounds.insert(
            (struct_def, field_name.to_string()),
            Proven::checked(BoundedDef {
                lower: lower_val,
                upper,
                width,
            }),
        );
    }

    /// The base case for every struct-typed reg/out's own bounded
    /// field(s) (v18) — mirrors `types/stmt.rs`'s `check_where_bound_
    /// init` in PURPOSE (an induction needs a verified starting point),
    /// but lives entirely in `bounds.rs` rather than `types.rs`: unlike
    /// a scalar bound, which needs `const_eval` (a types.rs-only
    /// capability) to verify a compile-time-constant init, a struct
    /// field's own init value is always a literal int
    /// (`Expr::Int`/`SizedInt`), which `expr_bound`/`struct_field_bound`
    /// already natively evaluate — so there's nothing here that needs
    /// `types.rs`'s own machinery. A struct-typed reg/out's `= init` is
    /// mandatory (parser-enforced, same as a scalar reg/out), so this
    /// ALSO means there's no "declared bound with zero possible check
    /// sites" gap analogous to v13/v17's own closed exhaustiveness
    /// holes to worry about here: every struct-typed reg/out always has
    /// at least this one guaranteed, checked site. (A struct type used
    /// ONLY via `in`/mem/fifo, never via any reg/out, has a field bound
    /// that's simply inert for those instances -- never checked, but
    /// ALSO never trusted at any read, per `struct_field_bound`'s own
    /// `DefKind` gate -- so that's not a soundness gap, just a bound
    /// with no effect for that particular origin.)
    fn check_struct_field_inits(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Reg { init, .. } | Item::Output { init, .. } => {
                    let Some(&def) = self.res.item_defs.get(&id) else {
                        continue;
                    };
                    let Some(Ty::Struct {
                        def: struct_def, ..
                    }) = self.ty.state_tys.get(&def)
                    else {
                        continue;
                    };
                    let struct_def = *struct_def;
                    let fields: Vec<String> = self
                        .struct_field_bounds
                        .keys()
                        .filter(|(sd, _)| *sd == struct_def)
                        .map(|(_, name)| name.clone())
                        .collect();
                    if fields.is_empty() {
                        continue; // this struct type has no bounded field at all
                    }
                    // Caught by advisor review AFTER this feature had
                    // already shipped: unlike a scalar reg/out (where
                    // `parse_state_decl` requires an explicit `= init`
                    // whenever THAT def's OWN `where` clause is present),
                    // nothing forces an init here -- the bound lives on
                    // the STRUCT FIELD, not on `p` itself, so `reg p :
                    // Pair` with no init at all is ordinary, legal syntax
                    // the parser has no way to reject at parse time (it
                    // doesn't yet know whether `Pair` has any bounded
                    // field). Without this check, `p`'s field would still
                    // be unconditionally trusted at every read via the
                    // `Reg`/`Output` arm of `struct_field_bound`, with
                    // NOTHING ever verified against it -- concretely a
                    // FALSE proof for a two-sided bound (`where 10 <= _ <
                    // 20`), since a struct-typed reg with no init resets
                    // to all-zero fields in FIRRTL, which violates any
                    // bound whose floor is above 0. So a struct type with
                    // any bounded field now REQUIRES every reg/out of
                    // that type to declare an explicit init -- the same
                    // "the induction needs a verified starting point"
                    // requirement `where_clause_requires_an_explicit_
                    // init` already enforces for a scalar bound, just
                    // triggered by the STRUCT's own bounded fields
                    // instead of the reg's own `where` clause.
                    let Some(init) = init else {
                        let span = self.ast.item_spans[id.0 as usize].clone();
                        self.error(
                            span,
                            format!(
                                "`{}`'s struct type has a bounded field, so it needs an \
                                 explicit `= init` to verify it satisfies the declared bound \
                                 (v0 restriction)",
                                self.res.def(def).name
                            ),
                        );
                        continue;
                    };
                    let init = *init;
                    for field_name in fields {
                        let bounded = *self.struct_field_bounds[&(struct_def, field_name.clone())];
                        let computed = self.struct_field_bound(
                            init,
                            &field_name,
                            &HashMap::new(),
                            &HashMap::new(),
                            &HashMap::new(),
                        );
                        let span = self.ast.expr_spans[init.0 as usize].clone();
                        let context = format!("init field `{field_name}`");
                        self.check_against_bound(computed, bounded, span, &context);
                    }
                }
                _ => {}
            }
        }
    }

    /// Every `Item::Invariant` (`DESIGN.md`'s "Tier 3, not v0"), plus
    /// (piggybacked onto the same walk) every `reg`/`out`'s own `= init`
    /// expression into `reg_out_inits` -- `check_relational_bound_inits`
    /// needs a `DefId -> init` reverse lookup that nothing else in this
    /// file maintains (`res.item_defs` only maps the other direction).
    /// An `Item::Invariant` whose expression doesn't recognize as this
    /// feature's one supported shape reports its own error here and
    /// simply never gets a `relational_bounds` entry -- the same
    /// "collect only what was successfully validated" convention every
    /// other `collect_*` function in this file already follows.
    fn collect_relational_bounds(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id).clone() {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Reg { init, .. } | Item::Output { init, .. } => {
                    if let Some(&def) = self.res.item_defs.get(&id) {
                        self.reg_out_inits.insert(def, init);
                    }
                }
                Item::Invariant { expr } => {
                    if let Some(rb) = self.recognize_invariant(expr) {
                        self.relational_bounds.push(rb);
                    }
                }
                _ => {}
            }
        }
    }

    /// Parses the TOP-LEVEL shape of an `invariant <expr>` item: an
    /// optional `% <const>` modulus peeled off the LHS (an existing
    /// binary operator, `BinOp::Rem` -- no new grammar), a comparison
    /// (`<`/`<=`/`==`) against a literal constant, and a linear form
    /// (`linear_form`) underneath. Every failure path reports its own
    /// error and returns `None` -- there is no silent "collected but
    /// inert" state for an unrecognized invariant, matching this file's
    /// existing "fails closed, with a diagnostic" convention.
    fn recognize_invariant(&mut self, expr: ExprId) -> Option<RelationalBound> {
        let span = self.ast.expr_spans[expr.0 as usize].clone();
        let unsupported = "an `invariant` must be a comparison (`<`, `<=`, or `==`) of a \
                            signed sum of `reg`/`out` idents (each with coefficient exactly \
                            +-1) -- optionally wrapped in `% <const>` -- against a literal \
                            constant";
        let Expr::Binary { op, lhs, rhs } = self.ast.expr(expr).clone() else {
            self.error(span, unsupported.to_string());
            return None;
        };
        let Some(c) = const_fold(self.ast, rhs) else {
            self.error(span, unsupported.to_string());
            return None;
        };
        // Peel an optional `% <const>` off the LHS -- the declared
        // modulus, when the user wrote one explicitly (Fact 2's shape:
        // `head - tail - push_count + pop_count) % 8 == 0`). Absent
        // (Fact 1's shape: `push_count - pop_count < 9`), the modulus
        // defaults to the involved defs' own native width below.
        let (inner, modulus_expr) = match self.ast.expr(lhs).clone() {
            Expr::Binary {
                op: BinOp::Rem,
                lhs: inner,
                rhs: m,
            } => (inner, Some(m)),
            _ => (lhs, None),
        };
        let Some(terms) = self.linear_form(inner) else {
            self.error(span, unsupported.to_string());
            return None;
        };
        if terms.is_empty() {
            self.error(span, unsupported.to_string());
            return None;
        }
        if terms.iter().any(|(_, coeff)| coeff.abs() != 1) {
            self.error(
                span,
                "each register in an `invariant`'s linear combination must appear with \
                 coefficient exactly +-1 (v1 restriction: no scalar multiplication)"
                    .to_string(),
            );
            return None;
        }
        let mut width = None;
        for (def, _) in &terms {
            let Some(w) = base_width(self.ty, *def) else {
                self.error(
                    span,
                    "an `invariant` needs every named reg/out to have a concretely-known \
                     `bits[N]` width (v0 restriction)"
                        .to_string(),
                );
                return None;
            };
            match width {
                None => width = Some(w),
                Some(w0) if w0 != w => {
                    self.error(
                        span,
                        "every reg/out named in the same `invariant` must share the same \
                         declared width (v1 restriction)"
                            .to_string(),
                    );
                    return None;
                }
                _ => {}
            }
        }
        let width = width.expect("terms is non-empty, checked above");
        let Some(natural) = 1u64.checked_shl(width as u32) else {
            self.error(
                span,
                "this invariant's own native width is too wide to check".to_string(),
            );
            return None;
        };
        let modulus = match modulus_expr {
            Some(m) => match const_fold(self.ast, m) {
                Some(v) => v,
                None => {
                    self.error(span, unsupported.to_string());
                    return None;
                }
            },
            None => natural,
        };
        // Side condition (flagged before any of this was implemented):
        // a modulus that doesn't divide the operands' own native
        // wraparound isn't congruence-preserving -- reducing a mod-16
        // wrapping computation to its low 3 bits is sound (8 | 16);
        // reducing it to, say, mod 5 is not. Rejected HERE, at
        // declaration time, rather than silently mis-proving later.
        if modulus == 0 || !modulus.is_power_of_two() || natural % modulus != 0 {
            self.error(
                span,
                format!(
                    "an `invariant`'s declared modulus ({modulus}) must be a power of two \
                     dividing the involved registers' own native width (2^{width} = \
                     {natural}) -- a modulus that doesn't divide the native wraparound isn't \
                     congruence-preserving"
                ),
            );
            return None;
        }
        let (lower, upper) = match op {
            BinOp::Lt => (0, c),
            BinOp::Le => (0, c.saturating_add(1)),
            BinOp::Eq => (c, c.saturating_add(1)),
            _ => {
                self.error(span, unsupported.to_string());
                return None;
            }
        };
        // A second side condition: a declared range that doesn't fit
        // within its own modulus is meaningless (and would silently
        // accept anything once reduced) -- rejected here, not
        // discovered mid-induction.
        if lower >= upper || upper > modulus {
            self.error(
                span,
                format!(
                    "this invariant's declared range [{lower}, {upper}) must fit within its \
                     own modulus ({modulus})"
                ),
            );
            return None;
        }
        Some(RelationalBound {
            terms,
            modulus,
            lower,
            upper,
            span,
        })
    }

    /// Decomposes an `Add`/`Sub` tree of `reg`/`out` idents into a
    /// signed sum of `(DefId, coefficient)` pairs (merging repeated
    /// occurrences of the same def) -- `None` for anything this v1
    /// Thin wrapper -- see the free function of the same name (kept
    /// callable both as `self.linear_form(..)` from every existing site
    /// in this file, and as a free function from `provably_disjoint_
    /// under_joint_guards`, which has no `Checker` to call a method on).
    fn linear_form(&self, expr: ExprId) -> Option<Vec<(DefId, i64)>> {
        linear_form(self.ast, self.res, expr)
    }

    /// The base case: every relational bound's own combination, folded
    /// from each involved def's literal `= init` (via `reg_out_inits`),
    /// must already lie in its declared `[lower, upper)` range mod
    /// `modulus` -- the same "the induction needs a verified starting
    /// point" requirement every other bound in this file already
    /// enforces, just checked against a combination instead of a single
    /// def.
    fn check_relational_bound_inits(&mut self) {
        for index in 0..self.relational_bounds.len() {
            let rb = self.relational_bounds[index].clone();
            let mut total: i64 = 0;
            let mut ok = true;
            for (def, coeff) in &rb.terms {
                let Some(Some(init_expr)) = self.reg_out_inits.get(def).copied() else {
                    ok = false;
                    break;
                };
                let Some(v) = const_fold(self.ast, init_expr) else {
                    ok = false;
                    break;
                };
                total += coeff * (v as i64);
            }
            if !ok {
                self.error(
                    rb.span.clone(),
                    "cannot verify this invariant's base case: every reg/out it names needs \
                     a literal `= init`"
                        .to_string(),
                );
                self.failed_relational.insert(index);
                continue;
            }
            let reduced = total.rem_euclid(rb.modulus as i64) as u64;
            if !(rb.lower..rb.upper).contains(&reduced) {
                self.error(
                    rb.span.clone(),
                    format!(
                        "this invariant's base case fails: at reset, the combination's value \
                         is {reduced} (mod {}), outside the declared [{}, {}) range",
                        rb.modulus, rb.lower, rb.upper
                    ),
                );
                self.failed_relational.insert(index);
            }
        }
    }

    /// The inductive step: every RULE that writes any def named in
    /// `self.relational_bounds[index]` contributes its own NET delta to
    /// the combination (`walk_deltas`); every SUBSET of these rules'
    /// simultaneous firing is checked to keep the combination inside its
    /// declared range (`shift_preserves`), using whichever of the
    /// firing rules' own leading guards happen to match this
    /// combination's exact shape to narrow the induction hypothesis
    /// (`narrow_combo_range`) -- see DESIGN.md's "Tier 3" writeup for
    /// the full worked example this mirrors (`circular_buffer_disjoint
    /// .tr`'s `head - tail == push_count - pop_count` FIFO invariant).
    ///
    /// v1 restricts this to AT MOST TWO contributing rules, and requires
    /// (gate #1, flagged before this was written) that `effects.rs`'s
    /// own `sig.writes` for each contributing rule -- which already
    /// accounts for a write reached transitively through a `fn`/`impl`
    /// call, unlike this pass's own direct-`Stmt::Assign` walk -- agree
    /// EXACTLY with what `walk_deltas` found: a rule that writes a named
    /// def only through a call (never a directly-visible assignment)
    /// fails this bound closed rather than silently computing a delta
    /// of zero for it and proving something a real write could break.
    ///
    /// The co-fire enumeration (every subset of the contributing rules)
    /// is sound only because v0's scheduling model guarantees no two
    /// SIMULTANEOUSLY-firing rules ever write the SAME def: a write-
    /// write conflict is rejected outright (`schedule.rs`'s own
    /// `Exemption::ConflictFree`-on-`WriteWrite` error), so each
    /// contributing rule's delta can be summed independently for
    /// whichever subset actually fires, with no ordering or double-
    /// counting question to resolve. This makes a `bounds.rs` soundness
    /// argument depend on a `schedule.rs` invariant -- flagged
    /// explicitly here (as the deferred `schedule.rs`-consumer half of
    /// this feature already is in DESIGN.md) so a future change to that
    /// scheduling model doesn't silently invalidate this one.
    fn check_relational_bound_induction(&mut self, index: usize) {
        let rb = self.relational_bounds[index].clone();
        let relevant: HashSet<DefId> = rb.terms.iter().map(|(d, _)| *d).collect();
        let base_state: HashMap<DefId, (u64, u64)> = self
            .bounded
            .iter()
            .map(|(d, b)| (*d, (b.lower, b.upper)))
            .collect();

        struct Contribution {
            guards: Vec<ExprId>,
            delta: i64,
        }
        let mut contributions: Vec<Contribution> = Vec::new();

        for id in self.collect_bodied_items() {
            let Item::Rule { body, .. } = self.ast.item(id).clone() else {
                continue; // a plain `fn`/`impl` never fires on its own
            };
            let Some(sig) = self.fx.sigs.get(&id) else {
                continue;
            };
            let sig_writes: HashSet<DefId> = sig
                .writes
                .iter()
                .filter(|d| relevant.contains(d))
                .copied()
                .collect();
            if sig_writes.is_empty() {
                continue; // this rule never touches anything this bound names
            }
            let Some(deltas) = self.walk_deltas(&body, &relevant, &base_state, rb.modulus) else {
                self.error(
                    rb.span.clone(),
                    format!(
                        "cannot verify this invariant: rule `{}` writes a register it names \
                         through a shape this pass doesn't recognize -- only `<def> := <def> \
                         +/- <const>`, a literal write under a branch that pins `<def>` to an \
                         exact value, or matching if/else branches are supported (v1 \
                         restriction)",
                        self.rule_name(id)
                    ),
                );
                self.failed_relational.insert(index);
                return;
            };
            let direct: HashSet<DefId> = deltas.keys().copied().collect();
            if direct != sig_writes {
                self.error(
                    rb.span.clone(),
                    format!(
                        "cannot verify this invariant: rule `{}` writes a register it names, \
                         but not through a directly-visible `Stmt::Assign` -- likely through a \
                         `fn`/`impl` call, which this pass doesn't trace into",
                        self.rule_name(id)
                    ),
                );
                self.failed_relational.insert(index);
                return;
            }
            let delta: i64 = deltas
                .iter()
                .map(|(d, v)| {
                    let coeff = rb
                        .terms
                        .iter()
                        .find(|(td, _)| td == d)
                        .map(|(_, c)| *c)
                        .unwrap_or(0);
                    coeff * v
                })
                .sum();
            contributions.push(Contribution {
                guards: self.leading_guards(&body),
                delta,
            });
        }

        if contributions.len() > 2 {
            self.error(
                rb.span.clone(),
                "cannot verify this invariant: more than two rules write registers it names \
                 (v1 restriction -- only a two-rule producer/consumer pair is supported)"
                    .to_string(),
            );
            self.failed_relational.insert(index);
            return;
        }

        let n = contributions.len();
        for mask in 0u32..(1 << n) {
            let mut delta = 0i64;
            let mut range = (rb.lower, rb.upper); // the induction hypothesis
            for (i, contribution) in contributions.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    delta += contribution.delta;
                    for &guard in &contribution.guards {
                        range = self.narrow_combo_range(&rb, guard, range);
                    }
                }
            }
            if !shift_preserves(range, delta, rb.modulus, rb.lower, rb.upper) {
                self.error(
                    rb.span.clone(),
                    format!(
                        "cannot verify this invariant is preserved (checked across every \
                         subset of the rules that write registers it names, including all \
                         firing the same cycle): the combination could reach a value outside \
                         the declared [{}, {}) range (mod {})",
                        rb.lower, rb.upper, rb.modulus
                    ),
                );
                self.failed_relational.insert(index);
                return;
            }
        }
    }

    /// Thin wrapper -- see the free function of the same name.
    fn leading_guards(&self, body: &[StmtId]) -> Vec<ExprId> {
        leading_guards(self.ast, body)
    }

    /// Thin wrapper -- see the free function of the same name.
    fn narrow_combo_range(
        &self,
        rb: &RelationalBound,
        guard: ExprId,
        range: (u64, u64),
    ) -> (u64, u64) {
        narrow_combo_range(self.ast, self.res, &rb.terms, guard, range)
    }

    /// One rule body's own NET delta to every def in `relevant`, as
    /// exact signed integers reduced mod `modulus` -- `None` the instant
    /// any write to a relevant def can't be pinned to an exact constant
    /// delta, or any OTHER statement shape touches a relevant def at all
    /// (a v1 restriction: `While`/`IfLet`/`WhileLet` are never supported
    /// here, matching/motivated by nothing in `circular_buffer_disjoint
    /// .tr` needing them). An `if`/`else` with MISMATCHED deltas (mod
    /// `modulus`) on its two branches also fails closed -- this feature
    /// needs ONE static delta per rule, not a per-path case split.
    fn walk_deltas(
        &self,
        body: &[StmtId],
        relevant: &HashSet<DefId>,
        state: &HashMap<DefId, (u64, u64)>,
        modulus: u64,
    ) -> Option<HashMap<DefId, i64>> {
        let mut deltas: HashMap<DefId, i64> = HashMap::new();
        for &sid in body {
            match self.ast.stmt(sid).clone() {
                // A relevant def READ anywhere -- a guard, an `if`
                // condition, a mem index, an ordinary RHS -- is always
                // harmless to this delta walk; only a WRITE to a
                // relevant def matters, and a def in `relevant` (always
                // `DefKind::Reg | DefKind::Output`, per `linear_form`'s
                // own gate) can only ever be WRITTEN via a bare `Ident`
                // `Stmt::Assign` LHS -- a mem/fifo/struct-field write's
                // own LHS shape (`Bracket`/`Field`) can reference a
                // relevant def only as something being READ (an index,
                // a base), never as the def being assigned. So nothing
                // here needs a fail-closed check beyond `exact_delta`'s
                // own restrictiveness for the one shape that DOES write
                // a relevant def.
                Stmt::Assign { lhs, rhs } => {
                    if let Expr::Ident(_) = self.ast.expr(lhs)
                        && let Some(def) = self.res.expr_defs.get(&lhs).copied()
                        && relevant.contains(&def)
                    {
                        let d = self.exact_delta(def, rhs, state)?;
                        *deltas.entry(def).or_insert(0) += d;
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_state = self.narrow_for_condition(cond, state);
                    let then_deltas =
                        self.walk_deltas(&then_body, relevant, &then_state, modulus)?;
                    let else_deltas = match else_body {
                        Some(else_body) => {
                            let else_state = self.narrow_for_else(cond, state);
                            self.walk_deltas(&else_body, relevant, &else_state, modulus)?
                        }
                        None => HashMap::new(),
                    };
                    if !deltas_agree(&then_deltas, &else_deltas, modulus) {
                        return None;
                    }
                    for (d, v) in then_deltas {
                        *deltas.entry(d).or_insert(0) += v;
                    }
                }
                // `While`/`IfLet`/`WhileLet` are unconditionally
                // unsupported here (v1 restriction) -- not because a
                // relevant def read/written inside one is unsound to
                // model, but because this pass has no per-iteration
                // delta story for a loop at all, and no motivating
                // example needs one. A rule using one of these for a
                // reason UNRELATED to any relational bound still fails
                // this bound closed (an honest, documented over-
                // restriction, not a silent gap).
                Stmt::While { .. } | Stmt::IfLet { .. } | Stmt::WhileLet { .. } => {
                    return None;
                }
                Stmt::Expr(_) | Stmt::Let { .. } | Stmt::Return(_) | Stmt::Tick | Stmt::Break => {}
            }
        }
        Some(deltas)
    }

    /// The exact signed delta `rhs` applies to `def`'s own current
    /// value, given `def`'s bare write `def := rhs` -- recognizes
    /// exactly two shapes: `def +/- <const>` (the delta IS the
    /// constant, regardless of `def`'s own current value), or a bare
    /// literal (the delta is `literal - def`'s CURRENT value, which
    /// needs `def` pinned to an exact singleton in `state` -- e.g. the
    /// `else { head := 0 }` branch of `if head < 7 {...} else {...}`,
    /// where `narrow_for_else` (already applied by the caller) pins
    /// `head` to exactly 7 via its own `where head < 8` bound). `None`
    /// for anything else -- this pass's own fail-closed default.
    fn exact_delta(
        &self,
        def: DefId,
        rhs: ExprId,
        state: &HashMap<DefId, (u64, u64)>,
    ) -> Option<i64> {
        match self.ast.expr(rhs).clone() {
            Expr::Binary {
                op: BinOp::Add,
                lhs,
                rhs,
            } => {
                if self.is_ident_for(lhs, def) {
                    const_fold(self.ast, rhs).map(|k| k as i64)
                } else if self.is_ident_for(rhs, def) {
                    const_fold(self.ast, lhs).map(|k| k as i64)
                } else {
                    None
                }
            }
            Expr::Binary {
                op: BinOp::Sub,
                lhs,
                rhs,
            } => {
                if self.is_ident_for(lhs, def) {
                    const_fold(self.ast, rhs).map(|k| -(k as i64))
                } else {
                    None
                }
            }
            Expr::Int(k) => {
                let (lo, hi) = state.get(&def).copied()?;
                if lo + 1 == hi {
                    Some(k as i64 - lo as i64)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn is_ident_for(&self, expr: ExprId, def: DefId) -> bool {
        matches!(self.ast.expr(expr), Expr::Ident(_)) && self.res.expr_defs.get(&expr) == Some(&def)
    }

    /// `self.res.def(...).name`, given a rule/fn `ItemId` -- purely for
    /// this feature's own diagnostics.
    fn rule_name(&self, id: ItemId) -> &str {
        self.res
            .item_defs
            .get(&id)
            .map(|def| self.res.def(*def).name.as_str())
            .unwrap_or("?")
    }

    /// All rule/fn items, recursively through modules — the same
    /// exhaustive walk `types/collect.rs`'s `check_all` and
    /// `effects.rs`'s `collect_bodied_items` both already do; callees
    /// are NOT inlined before this pass runs (FIRRTL emission's call
    /// inlining happens well after scheduling), so this walk reaches
    /// every real `Stmt::Assign` in the program directly, including
    /// inside a `fn`/`impl` called from a rule.
    fn collect_bodied_items(&self) -> Vec<ItemId> {
        let mut out = Vec::new();
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Rule { .. } | Item::Fn { .. } => out.push(id),
                _ => {}
            }
        }
        out
    }

    /// Every `DefKind::Local` this pass finds as a `Stmt::Assign` LHS
    /// anywhere in a body -- including nested inside `if`/`while`/`if
    /// let`/`while let` -- computed ONCE per rule/fn body, before any
    /// bound tracking begins. `locals`/`struct_origins` deliberately
    /// never learn an entry for any def in this set (see `Stmt::Let`'s
    /// own arm below): found by a post-ship advisor pass that a local
    /// reassigned via `:=` gets NO forward-flow update at all --
    /// `locals`/`struct_origins` are written only at `Stmt::Let` -- so
    /// `let x = 10; x := untrusted; total := x` composed `x` to its
    /// STALE `let`-time bound even with no branch involved (predates
    /// v18 entirely, present since the very first bounds.rs commit).
    /// Branch-exit poisoning (invalidate an outer entry whenever a
    /// clone's value changed) looked like a fix but a `while` loop
    /// defeats it too: the loop body is checked once against the ENTRY
    /// snapshot, so a reassignment on iteration 1 is never visible when
    /// checking iteration 2's own read of the same local -- confirmed
    /// empirically with a driving repro before choosing this shape
    /// instead. So this pass conservatively refuses to trust ANY local
    /// ever reassigned ANYWHERE in its own body, rather than building
    /// merge/poisoning machinery a loop would still defeat -- the same
    /// "refuse rather than build merge machinery" call this arc already
    /// made for v17's mem reads and v18's `Call`-sourced struct writes.
    /// Swept against every existing test/example first: nothing relies
    /// on a reassigned local's bound composing in a bounded position,
    /// so this costs zero expressiveness against the current suite.
    fn collect_reassigned_locals(&self, body: &[StmtId]) -> HashSet<DefId> {
        let mut out = HashSet::new();
        let mut stack: Vec<StmtId> = body.to_vec();
        while let Some(id) = stack.pop() {
            match self.ast.stmt(id) {
                Stmt::Assign { lhs, .. } => {
                    if let Expr::Ident(_) = self.ast.expr(*lhs)
                        && let Some(def) = self.res.expr_defs.get(lhs).copied()
                        && self.res.def(def).kind == crate::resolve::DefKind::Local
                    {
                        out.insert(def);
                    }
                }
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    stack.extend(then_body.iter().copied());
                    if let Some(else_body) = else_body {
                        stack.extend(else_body.iter().copied());
                    }
                }
                Stmt::IfLet {
                    then_body,
                    else_body,
                    ..
                } => {
                    stack.extend(then_body.iter().copied());
                    if let Some(else_body) = else_body {
                        stack.extend(else_body.iter().copied());
                    }
                }
                Stmt::While { body, .. } => stack.extend(body.iter().copied()),
                Stmt::WhileLet { body, .. } => stack.extend(body.iter().copied()),
                _ => {}
            }
        }
        out
    }

    fn check_item(&mut self, id: ItemId) {
        // v17: widened to a three-way check -- a program with ONLY a
        // bounded mem and no bounded reg/out/param/return anywhere would
        // otherwise skip this whole body walk, silently letting every
        // mem write through unchecked (the same "gated the descent on
        // the wrong condition" class of bug v14 found four times in a
        // row). v18: widened again to FOUR-way for the identical reason,
        // now that a program could have ONLY a bounded struct field and
        // no other bounded reg/out/param/return/mem anywhere -- caught
        // by advisor review of this feature's own plan before any code
        // was written, the same bug class flagged (and discriminated
        // with a dedicated driving example) at v17 above. Widened again
        // (FIVE-way) for `relational_bounds` -- not actually load-bearing
        // for that feature's own soundness (its induction, `check_
        // relational_bound_induction`, is a fully separate pass that
        // never depends on this per-item walk running at all), but kept
        // consistent with this arc's own established discipline: the
        // exact bug class this guard exists to close has been found four
        // times running, and a module whose only bounded thing is an
        // `invariant` is precisely the shape that would trip it if this
        // guard were ever repurposed to gate something that DOES depend
        // on it.
        if self.bounded.is_empty()
            && self.fn_ret_bound.is_empty()
            && self.mem_bounds.is_empty()
            && self.struct_field_bounds.is_empty()
            && self.relational_bounds.is_empty()
        {
            return; // nothing to check anywhere in the program
        }
        let body = match self.ast.item(id) {
            Item::Rule { body, .. } => body.clone(),
            Item::Fn { body, .. } => body.clone(),
            _ => return,
        };
        let mut state: HashMap<DefId, (u64, u64)> = self
            .bounded
            .iter()
            .map(|(d, b)| (*d, (b.lower, b.upper)))
            .collect();
        let mut locals: HashMap<DefId, Option<(u64, u64)>> = HashMap::new();
        self.current_reassigned_locals = self.collect_reassigned_locals(&body);
        // v18: which expression each struct-typed LOCAL was bound to
        // (its own `Stmt::Let` init) -- threaded and cloned/discarded
        // per nested scope exactly like `locals`, NOT a flat `self`-
        // field (caught by advisor review of this feature's own plan:
        // a flat map would leak a branch-local binding's trust past the
        // branch it was computed in, the same class of bug the
        // existing "Branch/loop scoping" section above this struct
        // already guards `state`/`locals` against).
        let mut struct_origins: HashMap<DefId, ExprId> = HashMap::new();
        // v13: this item's own declared return postcondition, if any --
        // `None` for a `rule` (`item_defs` has no entry for one) or a
        // fn with no `where _ < N` (no `fn_ret_bound` entry).
        let item_def = self.res.item_defs.get(&id).copied();
        self.current_ret_bound = item_def
            .and_then(|def| self.fn_ret_bound.get(&def))
            .map(|b| **b);
        self.current_fn_def = item_def;
        self.check_body(&body, &mut state, &mut locals, &mut struct_origins);
    }

    fn check_body(
        &mut self,
        body: &[StmtId],
        state: &mut HashMap<DefId, (u64, u64)>,
        locals: &mut HashMap<DefId, Option<(u64, u64)>>,
        struct_origins: &mut HashMap<DefId, ExprId>,
    ) {
        for stmt in body {
            self.check_stmt(*stmt, state, locals, struct_origins);
        }
    }

    fn check_stmt(
        &mut self,
        id: StmtId,
        state: &mut HashMap<DefId, (u64, u64)>,
        locals: &mut HashMap<DefId, Option<(u64, u64)>>,
        struct_origins: &mut HashMap<DefId, ExprId>,
    ) {
        match self.ast.stmt(id).clone() {
            Stmt::Assign { lhs, rhs } => {
                // v14: `expr_bound` is called on `rhs` unconditionally,
                // even when the LHS isn't a bounded (or even a bare
                // Ident) def at all -- found via the same "grep for the
                // gating-the-descent shape" sweep that caught the
                // `Expr::Call` arg-loop gap above: a mem/struct-field
                // write, or a write to an UNBOUNDED reg (`plain := Bump
                // (50)`), used to `return`/fall through before ever
                // reaching `expr_bound`, silently skipping any nested
                // `Call`'s own argument obligations in `rhs`. The bound
                // CHECK itself still only fires when the LHS resolves
                // to an actual bounded def.
                //
                // `lhs` itself is also swept via `check_calls_in`: a
                // mem write's own index (`m[Bump(50)] := 1`) is neither
                // a bare Ident nor part of `rhs`, so it was never passed
                // to `expr_bound` at all until this call was added --
                // the advisor's own follow-up probe past the four-site
                // sweep above, confirmed with a driving scratch file
                // before being fixed here.
                self.check_calls_in(lhs, state, locals, struct_origins);
                let def = if let Expr::Ident(_) = self.ast.expr(lhs) {
                    self.res.expr_defs.get(&lhs).copied()
                } else {
                    None
                };
                let computed = self.expr_bound(rhs, state, locals, struct_origins);
                // This LHS's own write obligations, one entry per bounded
                // def its write touches -- a bare-Ident write touches at
                // most one (itself), a mem write touches one (the mem's
                // elem bound), a struct-typed write touches one PER
                // bounded field. Recognizing which obligations apply is
                // still three separate shape tests (an assign's LHS really
                // is one of three distinct AST shapes -- that's not
                // duplication to remove); what collapses here is the
                // `found_writes`/`check_against_bound` TAIL, previously
                // written out three times with an identical body.
                let mut obligations: Vec<WriteObligation> = Vec::new();
                if let Some(def) = def
                    && let Some(bounded) = self.bounded.get(&def).copied()
                {
                    obligations.push((def, *bounded, computed, "write".to_string()));
                }
                // v17: a mem write (`m[i] := rhs`) checks `rhs` against
                // the mem's own declared elem bound -- reuses `computed`,
                // already derived unconditionally above, no redundant
                // `expr_bound` call.
                if let Expr::Bracket { callee, .. } = self.ast.expr(lhs)
                    && let Some(mem_def) = self.res.expr_defs.get(callee).copied()
                    && self.res.def(mem_def).kind == crate::resolve::DefKind::Mem
                    && let Some(bounded) = self.mem_bounds.get(&mem_def).copied()
                {
                    obligations.push((mem_def, *bounded, computed, "write".to_string()));
                }
                // v18: a struct-typed reg/out write (`p := Pair{...}`)
                // checks EVERY bounded field of `p`'s own struct type
                // against `rhs` -- reuses `struct_field_bound`, which
                // already handles `rhs` being a plain `StructLit` (check
                // the named field directly) or one with `..base` (recurse
                // into `base`'s own same-named field); a `Call`-sourced
                // struct write (the type-checker's OTHER permitted shape,
                // `types/stmt.rs:450`) has no field-by-field mechanism
                // here at all, so `struct_field_bound` falls through to
                // `None` for it and this correctly, conservatively fails
                // -- a real, documented v1 restriction, not a silent skip.
                if let Some(def) = def
                    && let Some(Ty::Struct {
                        def: struct_def, ..
                    }) = self.ty.state_tys.get(&def).cloned()
                {
                    let fields: Vec<String> = self
                        .struct_field_bounds
                        .keys()
                        .filter(|(sd, _)| *sd == struct_def)
                        .map(|(_, name)| name.clone())
                        .collect();
                    for field_name in fields {
                        let bounded = *self.struct_field_bounds[&(struct_def, field_name.clone())];
                        let field_computed = self.struct_field_bound(
                            rhs,
                            &field_name,
                            state,
                            locals,
                            struct_origins,
                        );
                        obligations.push((
                            def,
                            bounded,
                            field_computed,
                            format!("write to field `{field_name}`"),
                        ));
                    }
                }
                let span = self.ast.expr_spans[rhs.0 as usize].clone();
                for (found_def, bounded, obligation_computed, context) in obligations {
                    self.found_writes.insert(found_def);
                    self.check_against_bound(obligation_computed, bounded, span.clone(), &context);
                }
                // v18 originally patched a struct-typed LOCAL reassign
                // (`p := q`) by overwriting `struct_origins` with the new
                // rhs right here, so `p.data` would keep tracing to the
                // CURRENT value instead of the stale `Stmt::Let` one. A
                // later advisor pass found that fix incomplete: it only
                // held outside a branch/loop, since a nested scope's own
                // clone-and-discard treatment (see `Stmt::If`/`While`
                // below) meant the overwrite never escaped the branch it
                // ran in, and a `while` loop's single-pass body check
                // couldn't see iteration N's reassignment when checking
                // iteration N+1's own read. Superseded by `current_
                // reassigned_locals` (see its own doc comment): any local
                // ever reassigned anywhere in this body never gets a
                // `locals`/`struct_origins` entry in the first place, so
                // there is nothing to overwrite here -- inserting one
                // would in fact reintroduce a narrower, still-unsound
                // window of trust between this reassignment and the next
                // one.
            }
            Stmt::Let { name, init } => {
                let def = def_of_name(self.res, &name);
                let b = self.expr_bound(init, state, locals, struct_origins);
                // A local this pass's own pre-scan (`current_reassigned_
                // locals`, computed once at the top of `check_item`)
                // found reassigned via `Stmt::Assign` SOMEWHERE in this
                // body never gets an entry here at all -- see that set's
                // own doc comment for why a reassigned local's bound
                // can't be trusted at all, not just per-branch.
                if !self.current_reassigned_locals.contains(&def) {
                    locals.insert(def, b);
                    // v18: a struct-typed local's own SINGLE binding site
                    // is recorded so `struct_field_bound`'s `DefKind::
                    // Local` arm can trace `p.data` back to whatever
                    // `init` actually was -- sound because a struct-typed
                    // local bound here is, by construction, never
                    // reassigned later in this body (the reassigned case
                    // is excluded above) -- an untrusted origin (an `in`-
                    // port/mem/fifo-derived value) still correctly fails
                    // to compose once `struct_field_bound` recurses into
                    // it and hits its own `DefKind` gate.
                    if matches!(self.ty.expr_tys.get(&init), Some(Ty::Struct { .. })) {
                        struct_origins.insert(def, init);
                    }
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                // v14: a call embedded in the condition ITSELF (e.g.
                // `if Bump(50) < 5`) is checked against the frozen entry
                // state, before any narrowing -- the call happens as
                // part of evaluating the condition, not inside either
                // branch.
                self.check_calls_in(cond, state, locals, struct_origins);
                let mut then_state = self.narrow_for_condition(cond, state);
                let mut then_locals = locals.clone();
                let mut then_origins = struct_origins.clone();
                self.check_body(
                    &then_body,
                    &mut then_state,
                    &mut then_locals,
                    &mut then_origins,
                );
                if let Some(else_body) = else_body {
                    // v15: narrowed on the NEGATED condition, not the raw
                    // entry state -- see `narrow_for_else`'s own doc
                    // comment.
                    let mut else_state = self.narrow_for_else(cond, state);
                    let mut else_locals = locals.clone();
                    let mut else_origins = struct_origins.clone();
                    self.check_body(
                        &else_body,
                        &mut else_state,
                        &mut else_locals,
                        &mut else_origins,
                    );
                }
            }
            Stmt::IfLet {
                init,
                then_body,
                else_body,
                ..
            } => {
                // v14: `init` is exactly the position a fallible call
                // (`if let x = Classify(a) { ... }`) or a fifo op sits
                // in -- never checked before this pass.
                self.check_calls_in(init, state, locals, struct_origins);
                let mut then_state = state.clone();
                let mut then_locals = locals.clone();
                let mut then_origins = struct_origins.clone();
                self.check_body(
                    &then_body,
                    &mut then_state,
                    &mut then_locals,
                    &mut then_origins,
                );
                if let Some(else_body) = else_body {
                    let mut else_state = state.clone();
                    let mut else_locals = locals.clone();
                    let mut else_origins = struct_origins.clone();
                    self.check_body(
                        &else_body,
                        &mut else_state,
                        &mut else_locals,
                        &mut else_origins,
                    );
                }
            }
            Stmt::While { cond, body } => {
                // Each iteration is its own clock edge (DESIGN.md's
                // `<sequences>` lowering): checking the body once
                // against the frozen entry state (narrowed by the
                // loop's OWN condition, same shape/reasoning as `if`'s
                // `then` branch — the body only ever runs while the
                // condition holds) covers every iteration's own write
                // sites identically. Locals introduced inside the loop
                // don't survive past it (loop-boundary invalidation),
                // same as an `if`.
                //
                // A `while` is later RENDERED as an `if`-shaped
                // structure for FIRRTL emission (`lower/render.rs`'s
                // `while_loop_header`) — checked directly, not assumed,
                // that this doesn't make `check_calls_in` run twice on
                // the same condition: `bounds::check` (`main.rs`) runs
                // exactly once, on the ORIGINAL AST, entirely before any
                // lowering/rendering happens, so there's no second pass
                // to double-report through. Confirmed empirically too
                // (a failing call in a `while` condition under
                // `--firrtl` reports exactly one error, not two).
                self.check_calls_in(cond, state, locals, struct_origins); // v14
                let mut loop_state = self.narrow_for_condition(cond, state);
                let mut loop_locals = locals.clone();
                let mut loop_origins = struct_origins.clone();
                self.check_body(&body, &mut loop_state, &mut loop_locals, &mut loop_origins);
            }
            Stmt::WhileLet { init, body, .. } => {
                self.check_calls_in(init, state, locals, struct_origins); // v14
                let mut loop_state = state.clone();
                let mut loop_locals = locals.clone();
                let mut loop_origins = struct_origins.clone();
                self.check_body(&body, &mut loop_state, &mut loop_locals, &mut loop_origins);
            }
            // A bare call statement (`Bump(x)`) is how a VOID fn/impl —
            // one with no return value, invoked purely for its `writes`
            // effect — is actually called in this language; it's the
            // realistic shape a bounded-param call site takes (v12's
            // own driving example uses exactly this shape), not an edge
            // case. Routes through `expr_bound` purely for that side
            // effect (checking any `Call` reached anywhere in `e`
            // against its callee's declared param bounds); the returned
            // range itself is meaningless here and discarded.
            Stmt::Expr(e) => {
                self.expr_bound(e, state, locals, struct_origins);
            }
            // v13: only meaningful when the ENCLOSING fn declared a
            // postcondition (`current_ret_bound`, set once per item at
            // the top of `check_item`) — every `return` in that body is
            // an independent obligation against it, the same way every
            // write site is independently checked against a reg's own
            // bound. A fn with no declared postcondition still has
            // nothing to check here, same as before this feature.
            Stmt::Return(Some(e)) => {
                // v14: `expr_bound` is called on `e` unconditionally,
                // even when the ENCLOSING fn declared no postcondition
                // at all (`current_ret_bound` is `None`) -- same
                // gating-the-descent shape as the `Stmt::Assign`/
                // `Expr::Call`-arg-loop gaps above: `return Bump(50)`
                // inside a fn with no `where _ < N` used to skip
                // straight past `expr_bound`, silently missing `Bump`'s
                // own argument violation. The postcondition CHECK
                // itself still only fires when one is actually declared.
                let computed = self.expr_bound(e, state, locals, struct_origins);
                if let Some(bounded) = self.current_ret_bound {
                    let span = self.ast.expr_spans[e.0 as usize].clone();
                    self.check_against_bound(computed, bounded, span, "return value");
                    if let Some(fn_def) = self.current_fn_def {
                        self.found_returns.insert(fn_def);
                    }
                }
            }
            Stmt::Tick | Stmt::Break | Stmt::Return(None) => {}
        }
    }

    /// Checks a `computed` provable range (or `None`, unprovable)
    /// against a bounded def's OWN declared `[lower, upper)` range and
    /// declared bit width. Shared by a `Stmt::Assign` write site and an
    /// `Expr::Call` argument (v12) — the only two positions that check
    /// a computed value against a PRE-EXISTING declared bound, as
    /// opposed to `collect_one_bounded_def`, which validates a bound's
    /// own declaration. `context` names what's being checked, for the
    /// error message only (e.g. `"write"`, `"argument for parameter
    /// \`i\`"`).
    fn check_against_bound(
        &mut self,
        computed: Option<(u64, u64)>,
        bounded: BoundedDef,
        span: Span,
        context: &str,
    ) {
        let BoundedDef {
            lower,
            upper,
            width,
        } = bounded;
        match computed {
            None => self.error(
                span,
                format!(
                    "cannot verify this {context} stays within the declared bound \
                     `{lower} <= _ < {upper}` (either an unsupported expression shape, or a \
                     subtraction that isn't provably non-negative here — only a bare bounded \
                     reg/out/local/param, a literal, their sum, product, or a provably-in-range \
                     difference is recognized)"
                ),
            ),
            Some((_, hi)) if hi > 1u64.checked_shl(width as u32).unwrap_or(u64::MAX) => {
                self.error(
                    span,
                    format!(
                        "this {context}'s computed value could reach or exceed the declared \
                         width ([{width}]), which would silently wrap and invalidate the \
                         declared bound `{lower} <= _ < {upper}`"
                    ),
                );
            }
            Some((_, hi)) if hi > upper => self.error(
                span,
                format!(
                    "cannot verify this {context} stays within the declared bound \
                     `{lower} <= _ < {upper}` (computed value could reach {})",
                    hi.saturating_sub(1)
                ),
            ),
            Some((lo, _)) if lo < lower => self.error(
                span,
                format!(
                    "cannot verify this {context} stays within the declared bound \
                     `{lower} <= _ < {upper}` (computed value could go below {lower})"
                ),
            ),
            Some(_) => {}
        }
    }

    /// `if`/`while <bounded reg> < <const>` narrows that reg's tracked
    /// UPPER end to `min(current, const)`; `> <const>`/`>= <const>`
    /// narrows the LOWER end to `max(current, const+1)`/`max(current,
    /// const)` — for the guarded body ONLY, since the caller clones
    /// `state` first, so this never mutates the outer map.
    ///
    /// `<> <const>` narrows EITHER end, but ONLY when `const` is exactly
    /// the current frozen bound's own floor or ceiling: excluding the
    /// floor (`const == lo`) narrows the lower end up to `lo + 1`;
    /// excluding one-past-the-max (`const == hi - 1`) narrows the upper
    /// end down to `const` itself. Excluding any OTHER value (still
    /// inside `[lo, hi)` but not touching either edge) would split the
    /// range into two disjoint pieces a single `(lo, hi)` interval can't
    /// express, so that case is deliberately left a no-op — a real,
    /// checked equality test, not a bounds check like `k <= lo` would be
    /// (that "generalization" is unsound: it could narrow past a `k`
    /// that isn't actually the current edge). `Ne`'s commuted form
    /// (`<const> <> <reg>`, constant on the LEFT) IS recognized too —
    /// NOT because `<>` is symmetric as a language construct in general
    /// (`type_binop`'s own Verse-inspired rule makes a comparison yield
    /// its LHS's own type/value on success, so `x <> k` and `k <> x`
    /// are genuinely different expressions where that returned value is
    /// consumed), but because this function only ever inspects a guard
    /// as a pass/fail predicate deciding which branch to check, never
    /// its returned value — and `x != k` and `k != x` are the same fact
    /// about the same two values, so no new soundness argument is
    /// needed here, just trying both operand orders
    /// (`ident_const_operands`, below). `Lt`/`Gt`/`Ge` stay single-
    /// order regardless: `k < x` and `x < k` are different claims even
    /// as bare predicates, so commuting those would mean recognizing a
    /// different operator in the flipped position, a separate feature.
    /// The `else` branch of
    /// an `if <>` (a provable singleton, `<reg> == <const>`) is left
    /// unnarrowed — conservative, not incorrect, and no example needs
    /// the extra precision there.
    ///
    /// Any other condition shape (or a reg with no PRIOR bound at all) is
    /// a no-op: narrowing only tightens an already-bounded fact, never
    /// invents one.
    fn narrow_for_condition(
        &self,
        cond: ExprId,
        state: &HashMap<DefId, (u64, u64)>,
    ) -> HashMap<DefId, (u64, u64)> {
        let mut narrowed = state.clone();
        if let Expr::Binary { op, lhs, rhs } = self.ast.expr(cond)
            && let Some((def, k)) = self.ident_const_operands(*lhs, *rhs).or_else(|| {
                matches!(op, BinOp::Ne)
                    .then(|| self.ident_const_operands(*rhs, *lhs))
                    .flatten()
            })
            && let Some((lo, hi)) = narrowed.get(&def)
        {
            match op {
                BinOp::Lt => {
                    narrowed.insert(def, (*lo, k.min(*hi)));
                }
                BinOp::Gt => {
                    if let Some(floor) = k.checked_add(1) {
                        narrowed.insert(def, (floor.max(*lo), *hi));
                    }
                }
                BinOp::Ge => {
                    narrowed.insert(def, (k.max(*lo), *hi));
                }
                BinOp::Ne => {
                    if k == *lo {
                        if let Some(new_lo) = lo.checked_add(1) {
                            narrowed.insert(def, (new_lo, *hi));
                        }
                    } else if let Some(edge) = hi.checked_sub(1)
                        && k == edge
                    {
                        narrowed.insert(def, (*lo, k));
                    }
                }
                _ => {}
            }
        }
        narrowed
    }

    /// The ELSE-branch mirror of `narrow_for_condition` above: narrows
    /// `state` on the NEGATED condition, instead of leaving the `else`
    /// branch on the raw, unnarrowed entry state as it did before v15.
    /// The negated condition is just as real a proven fact as the
    /// condition itself — `if i < k`'s `else` branch is exactly `i >=
    /// k`, the identical fact `narrow_for_condition` already proves for
    /// an actual `if i >= k`'s own THEN branch, just reached from the
    /// opposite operator, so `Lt`/`Ge` and `Gt`/`Le`-shaped narrowing
    /// below reuse those same formulas in the mirrored direction. `Ne`'s
    /// own negation is the one genuinely NEW capability here, not just
    /// a mirrored existing formula: `else` of `i <> k` is the exact
    /// singleton `i == k`, sound for ANY `k` — mid-range included,
    /// unlike `narrow_for_condition`'s own `Ne` arm, which only narrows
    /// a THEN branch's EDGE case (v9's documented interior-exclusion
    /// no-op — the same "interval-set domain" question v11/v13/v14 left
    /// open turned out to be provably inert everywhere it was checked;
    /// this singleton is the actual non-inert capture of that same
    /// underlying idea, and it needs no interval-set at all since a
    /// singleton is just an ordinary one-piece interval).
    ///
    /// Every arm's raw result is CLAMPED against the def's own current
    /// `(lo, hi)` before being inserted, and only inserted at all when
    /// that clamp leaves a non-empty range (`clamped_lo < clamped_hi`)
    /// — caught by an advisor pass before committing. A condition that's
    /// always true for the def's current range (`cnt >= 0` on an
    /// unsigned `cnt`, or `i < 20` when `i`'s declared ceiling is 10)
    /// makes the ELSE branch unreachable dead code, and three of the
    /// four raw formulas below (`Lt`/`Gt`/`Ge`, each already built from
    /// `*lo`/`*hi` via `max`/`min`) degrade gracefully to an EMPTY
    /// range in that case, not an incorrect one — but inserting an
    /// empty range unclamped is still the wrong move: it either gets
    /// read back as if it were a real, narrow proof (a coincidental
    /// vacuous accept downstream, not a deliberate one) or, for a
    /// hand-rolled range comparison elsewhere, could misbehave on lo >
    /// hi. `Ne`'s own raw `(k, k+1)` is the one arm that does NOT derive
    /// from `lo`/`hi` at all, so without this clamp it would insert a
    /// singleton for ANY `k`, even one nowhere near the def's actual
    /// proven range (exactly the hazard the dedicated `..._stays_
    /// unnarrowed` test below pins) — clamping against `(lo, hi)` first
    /// closes that the same uniform way the other three arms are
    /// already closed, rather than needing its own separate ad hoc
    /// guard. Either way, leaving an unreachable else branch on the
    /// raw, unnarrowed entry state is trivially still sound: a false
    /// premise (the condition can never actually be false) proves
    /// anything, and `else_state` is cloned and discarded at the end of
    /// the branch, so nothing about a missed narrowing opportunity here
    /// ever escapes into the surrounding walk.
    ///
    /// Soundness here rests on one precondition `narrow_for_condition`
    /// doesn't need: `ident_const_operands` only ever recognizes a bare
    /// bounded ident and a const-foldable literal, both TOTAL
    /// expressions that can't themselves fail. In this language a
    /// comparison is a fallible operation — "else taken" means the
    /// comparison FAILED, which equals "the predicate is false" only
    /// when nothing else in the condition could have failed instead.
    /// `narrow_for_condition`'s own THEN-branch narrowing doesn't need
    /// this: success already implies the predicate held, regardless of
    /// what else is in the condition. If `ident_const_operands` is ever
    /// widened to accept a fallible operand (a call, an optional) for
    /// the then-branch's benefit, `narrow_for_condition` stays sound but
    /// `narrow_for_else` would silently stop being sound — worth
    /// re-checking this function specifically before any such widening.
    fn narrow_for_else(
        &self,
        cond: ExprId,
        state: &HashMap<DefId, (u64, u64)>,
    ) -> HashMap<DefId, (u64, u64)> {
        let mut narrowed = state.clone();
        if let Expr::Binary { op, lhs, rhs } = self.ast.expr(cond)
            && let Some((def, k)) = self.ident_const_operands(*lhs, *rhs).or_else(|| {
                matches!(op, BinOp::Ne)
                    .then(|| self.ident_const_operands(*rhs, *lhs))
                    .flatten()
            })
            && let Some((lo, hi)) = narrowed.get(&def)
        {
            let raw = match op {
                // else of `i < k` is `i >= k`.
                BinOp::Lt => Some((k.max(*lo), *hi)),
                // else of `i > k` is `i <= k`, i.e. `i < k + 1`.
                BinOp::Gt => k.checked_add(1).map(|ceil| (*lo, ceil.min(*hi))),
                // else of `i >= k` is `i < k`.
                BinOp::Ge => Some((*lo, k.min(*hi))),
                // else of `i <> k` is the exact singleton `i == k`.
                BinOp::Ne => k.checked_add(1).map(|ceil| (k, ceil)),
                _ => None,
            };
            if let Some((raw_lo, raw_hi)) = raw {
                let clamped_lo = raw_lo.max(*lo);
                let clamped_hi = raw_hi.min(*hi);
                if clamped_lo < clamped_hi {
                    narrowed.insert(def, (clamped_lo, clamped_hi));
                }
            }
        }
        narrowed
    }

    /// Extracts `(def, k)` from a comparison's two operands in ONE
    /// specific order: `a` must be a bare Ident resolving to a bounded
    /// def, `b` must const-fold to a literal. `narrow_for_condition`
    /// tries this in both operand orders for `Ne` (genuinely symmetric)
    /// and only the direct order for every other op (not symmetric —
    /// see that function's own doc comment).
    fn ident_const_operands(&self, a: ExprId, b: ExprId) -> Option<(DefId, u64)> {
        if !matches!(self.ast.expr(a), Expr::Ident(_)) {
            return None;
        }
        let def = *self.res.expr_defs.get(&a)?;
        let k = const_fold(self.ast, b)?;
        Some((def, k))
    }

    /// v14: recursively finds every "outermost" occurrence of a shape
    /// `expr_bound` has its own dedicated arm for, within `id`, and
    /// delegates to it — purely for that side effect (checking any
    /// argument obligations against a bounded param, any declared
    /// return postcondition trusted onward, and — v16 — exporting a mem
    /// access's own index range) — the returned bound is meaningless to
    /// most callers and freely discarded (Rust allows discarding a
    /// non-`()` return), same idiom `Stmt::Expr`'s own arm already uses
    /// for a bare call statement. Originally closed the gap both v12
    /// and v13's own docs left open: a call used inside an `if`/`while`'s
    /// own CONDITION, or an `if let`/`while let`'s own `init`
    /// (`narrow_for_condition` only ever pattern-matches `cond`'s shape,
    /// never routes it through `expr_bound` at all).
    ///
    /// Stops recursing the instant it finds ANY shape in the list below
    /// (originally just `Call`, widened at v16 to every shape `expr_
    /// bound` has an arm for) rather than continuing the generic
    /// descent into it: `expr_bound` already recurses into its OWN
    /// children for every one of these shapes (`Add`/`Sub`/`Mul` into
    /// both operands, `Call`/`Bracket` into callee/args), so continuing
    /// the generic walk past that point too would double-check (and
    /// double-report) the same site. Every OTHER shape (`Field`,
    /// `Guard`, `StructLit`, `ListLit`, `Range`, `Or`, ...) still
    /// recurses generically via `sub_exprs` — confirmed empirically,
    /// not assumed, that this still reaches a call hidden under one of
    /// those (`m[SomeStructCall().field]` parses and type-checks as a
    /// legal mem index; `Field` isn't in the stop-list below, so this
    /// function recurses into its own `base`, which IS a `Call`,
    /// correctly dispatching to `expr_bound`).
    fn check_calls_in(
        &mut self,
        id: ExprId,
        state: &HashMap<DefId, (u64, u64)>,
        locals: &HashMap<DefId, Option<(u64, u64)>>,
        struct_origins: &HashMap<DefId, ExprId>,
    ) -> Option<(u64, u64)> {
        if matches!(
            self.ast.expr(id),
            Expr::Int(_)
                | Expr::SizedInt { .. }
                | Expr::Ident(_)
                | Expr::Binary {
                    op: BinOp::Add | BinOp::Sub | BinOp::Mul,
                    ..
                }
                | Expr::Call { .. }
                | Expr::Bracket { .. }
        ) {
            // v18: `Field` is deliberately NOT added here, even though
            // `expr_bound` now has a real arm for it -- unlike `Bracket`
            // (v16), that arm's own `struct_field_bound` helper does NOT
            // recurse into an unrecognized `base` shape (a `Call`, ..),
            // so stopping here on a bare `Field` would silently skip a
            // call hidden under one used as, say, a mem index
            // (`m[MakePair(50).data]` -- confirmed by running the
            // existing `call_argument_hidden_under_a_field_access_in_a_
            // mem_index_is_still_checked` test, which exists precisely
            // to pin this). Leaving `Field` out of the stop-list means
            // the generic `sub_exprs` recursion below still finds a
            // `Call` nested under one; a bare `Field` used AS the whole
            // checked position still composes correctly regardless,
            // since `Stmt::Assign`/`Stmt::Return`/etc. all call `expr_
            // bound` on their own top-level expression unconditionally,
            // never through this stop-list at all.
            return self.expr_bound(id, state, locals, struct_origins);
        }
        for child in crate::lower::sub_exprs(self.ast, id) {
            self.check_calls_in(child, state, locals, struct_origins);
        }
        None
    }

    /// The value range an expression is provably confined to, as a
    /// `(lower, upper)` pair (lower inclusive, upper exclusive), or
    /// `None` if this pass can't establish one. A bare bounded reg/
    /// local/param reference, a literal, `Add`, `Sub`, or `Mul` of two
    /// such compose — see this module's own doc comment for why
    /// everything else (a shift, ...) is deliberately left unsupported.
    /// `&mut self` (v12): a `Call` is visited here too, and checking its
    /// arguments against the callee's declared param bounds is a real
    /// SIDE EFFECT (pushes errors), not just a value computation — see
    /// that arm's own comment for why it always returns `None` itself.
    fn expr_bound(
        &mut self,
        id: ExprId,
        state: &HashMap<DefId, (u64, u64)>,
        locals: &HashMap<DefId, Option<(u64, u64)>>,
        struct_origins: &HashMap<DefId, ExprId>,
    ) -> Option<(u64, u64)> {
        match self.ast.expr(id) {
            Expr::Int(v) => Some((*v, v.checked_add(1)?)),
            Expr::SizedInt { value, .. } => Some((*value, value.checked_add(1)?)),
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&id)?;
                if let Some(&b) = state.get(def) {
                    Some(b)
                } else {
                    locals.get(def).copied().flatten()
                }
            }
            Expr::Binary {
                op: BinOp::Add,
                lhs,
                rhs,
            } => {
                // v14: both operands' `expr_bound` are computed BEFORE
                // either is unwrapped with `?` -- found via the same
                // gating-the-descent sweep as the `Stmt::Assign`/
                // `Stmt::Return`/`Expr::Call`-arg-loop fixes elsewhere
                // in this file: chaining `self.expr_bound(*lhs, ...)?`
                // directly into `self.expr_bound(*rhs, ...)?` meant an
                // unprovable LHS (e.g. an unbounded `in` port) short-
                // circuited the whole arm before `rhs` was ever
                // evaluated, silently skipping any `Call` nested in
                // `rhs` (`unbounded + Bump(50)` never checked `Bump`'s
                // own argument). Both calls now always happen; only the
                // ARITHMETIC bails early if either came back `None`.
                let a = self.expr_bound(*lhs, state, locals, struct_origins);
                let b = self.expr_bound(*rhs, state, locals, struct_origins);
                let (a_lo, a_hi) = a?;
                let (b_lo, b_hi) = b?;
                let lo = a_lo.checked_add(b_lo)?;
                let hi = a_hi.checked_add(b_hi)?.checked_sub(1)?;
                Some((lo, hi))
            }
            Expr::Binary {
                op: BinOp::Sub,
                lhs,
                rhs,
            } => {
                // Sound only when the SMALLEST possible `a` still
                // dominates the LARGEST possible `b` — otherwise some
                // combination in range could underflow (wrap, in the
                // reg's real unsigned domain). `b_max`'s `checked_sub`
                // failing (an empty `b` range) and `lo`'s `checked_sub`
                // failing (underflow IS possible for some combination)
                // both naturally return `None` here, the same
                // "unprovable, fails closed" signal `Add`'s
                // `checked_add` already gives on overflow — no separate
                // error path needed. Both operands evaluated before
                // either `?`-unwrap, same v14 reasoning as `Add` above.
                let a = self.expr_bound(*lhs, state, locals, struct_origins);
                let b = self.expr_bound(*rhs, state, locals, struct_origins);
                let (a_lo, a_hi) = a?;
                let (b_lo, b_hi) = b?;
                let b_max = b_hi.checked_sub(1)?;
                let lo = a_lo.checked_sub(b_max)?;
                let hi = a_hi.checked_sub(b_lo)?;
                Some((lo, hi))
            }
            Expr::Binary {
                op: BinOp::Mul,
                lhs,
                rhs,
            } => {
                // Both operands are non-negative (`bits[N]`, unsigned),
                // which is what makes this argument clean, unlike
                // general signed interval multiplication: a product's
                // minimum is exactly `a_lo * b_lo` and its maximum is
                // exactly `a_max * b_max` — the product's own extremes
                // correspond exactly to the operands' extremes, no
                // sign-corner-case reasoning needed. `checked_mul`/
                // `checked_add` fail closed (`None`) on overflow, the
                // same idiom `Add`/`Sub` above already use. Both
                // operands evaluated before either `?`-unwrap, same v14
                // reasoning as `Add`/`Sub` above.
                let a = self.expr_bound(*lhs, state, locals, struct_origins);
                let b = self.expr_bound(*rhs, state, locals, struct_origins);
                let (a_lo, a_hi) = a?;
                let (b_lo, b_hi) = b?;
                let a_max = a_hi.checked_sub(1)?;
                let b_max = b_hi.checked_sub(1)?;
                let lo = a_lo.checked_mul(b_lo)?;
                let hi = a_max.checked_mul(b_max)?.checked_add(1)?;
                Some((lo, hi))
            }
            Expr::Call { callee, args } => {
                // v12: cross-boundary bound propagation. `Bump`'s own
                // body trusts `i < 10` as its declared precondition
                // (checked, like a reg/out, by `check_item`'s own walk
                // of `Item::Fn` — that trust is unconditional there);
                // THIS is what makes that trust sound system-wide,
                // verifying every actual caller upholds it.
                //
                // v13: the mirror in the OTHER direction — if the
                // callee ALSO declared a return postcondition (checked
                // against every `Stmt::Return` in ITS OWN body, the
                // same way `check_item` trusts `i < 10` unconditionally
                // for the callee's own params), this call's result now
                // has a provable range too, returned below instead of
                // unconditionally `None`. A callee with no declared
                // postcondition still composes to `None`, unchanged —
                // this is opt-in propagation, not blanket inference.
                //
                // `Expr::Bracket` (the OTHER `{callee, args}` shape) is
                // deliberately not given a twin arm here. `resolve.rs`
                // resolves a `Bracket`'s callee the same as a `Call`'s
                // (no syntax discrimination), so `Fn[arg]` COULD in
                // principle resolve `callee` to a real `Item::Fn`'s
                // DefId -- but `type_bracket` (`types/expr.rs`) has no
                // `DefKind::Fn` branch (falls through to `Ty::Unknown`,
                // silently, no error), and FIRRTL emission's own
                // `Expr::Bracket` arm only handles `Ty::Mem`/`Ty::Bits`
                // shapes, erroring "this indexing form is not yet
                // supported" on anything else -- confirmed empirically
                // (`Bump[y]` dies at `--firrtl` emission before any
                // hardware is produced). A pre-existing gap in types.rs/
                // lower.rs, not introduced by v12 and not a bounds.rs
                // soundness hole: no program shaped this way reaches
                // codegen regardless of whether its argument obeys the
                // callee's declared bound.
                let (callee, args) = (*callee, args.clone());
                let &fn_def = self.res.expr_defs.get(&callee)?;
                if let Some(params) = self.fn_params.get(&fn_def).cloned() {
                    for (param, arg) in params.iter().zip(&args) {
                        // v14: `expr_bound` is called on EVERY argument
                        // unconditionally, not just ones whose param
                        // has a declared bound to check against — found
                        // empirically, not assumed, while writing this
                        // pass's own tests: `Outer(Bump(50))` where
                        // `Outer`'s own param has NO bound used to skip
                        // evaluating `Bump(50)` at all (`continue`
                        // before ever reaching `expr_bound`), silently
                        // never checking `Bump`'s own argument. The
                        // bound CHECK itself still only fires when one
                        // exists; the recursive descent (needed to
                        // reach a nested `Call`) no longer waits on it.
                        let bounded = if param.bound.is_none() {
                            None
                        } else {
                            let param_def = def_of_name(self.res, &param.name);
                            self.bounded.get(&param_def).copied()
                        };
                        let computed = self.expr_bound(*arg, state, locals, struct_origins);
                        if let Some(bounded) = bounded {
                            let span = self.ast.expr_spans[arg.0 as usize].clone();
                            let context = format!("argument for parameter `{}`", param.name);
                            self.check_against_bound(computed, *bounded, span, &context);
                        }
                    }
                }
                self.fn_ret_bound
                    .get(&fn_def)
                    .map(|bounded| (bounded.lower, bounded.upper))
            }
            // A mem/fifo access's own VALUE composed to `None`
            // unconditionally through v16 -- `callee`/`args` still need
            // checking for a nested `Call`'s own argument obligations:
            // without this arm at all, `Expr::Bracket` fell through to
            // the catch-all below with ZERO recursion, so `y := m[Bump(50)]`
            // (a mem READ used as a value, not an assignment target)
            // silently skipped `Bump`'s own argument check entirely --
            // found empirically while designing v16 below, not assumed,
            // and confirmed to be the same gap at `return m[Bump(50)]`
            // and `Outer(m[Bump(50)])` (a call argument) too, since all
            // three route through this same shallow `expr_bound` call.
            // `check_calls_in` is exactly the established "find every
            // outermost checkable shape and delegate to `expr_bound`"
            // idiom already used elsewhere in this file, reused here
            // rather than duplicating it.
            //
            // v16: `check_calls_in`'s own return value (widened from
            // `()` to `Option<(u64, u64)>`) is now captured here and
            // exported into `site_ranges`, keyed by the index's own
            // `ExprId`, whenever `callee` resolves to a `mem` --
            // `schedule.rs`'s own mem-disjointness proof (`real_range`)
            // consults this map before falling back to its own
            // independent, per-def-only walk. Only a mem's own index
            // is exported: `effects.rs`'s `mem_read_idx`/`mem_write_idx`
            // maps (the only consumer of a per-site mem-index fact) are
            // keyed by mem `DefId` specifically, so a fifo's own index
            // (sharing this exact `Bracket` shape) has no consumer here
            // -- not an oversight, there is genuinely nothing that would
            // read a fifo-index entry. No `ExprId` is ever visited by
            // this pass's forward walk more than once under a different
            // `state` (see `Bounds.site_ranges`'s own doc comment for
            // the full structural argument), so a plain `insert` is
            // correct -- no merge-on-conflict is needed.
            Expr::Bracket { callee, args } => {
                self.check_calls_in(*callee, state, locals, struct_origins);
                let mem_def = self
                    .res
                    .expr_defs
                    .get(callee)
                    .copied()
                    .filter(|d| self.res.def(*d).kind == crate::resolve::DefKind::Mem);
                for arg in args {
                    let computed = self.check_calls_in(*arg, state, locals, struct_origins);
                    if mem_def.is_some()
                        && let Some(range) = computed
                    {
                        self.site_ranges.insert(*arg, range);
                    }
                }
                // v17's own bound (`self.mem_bounds`) is deliberately
                // NOT handed back here -- a mem read's own VALUE still
                // composes to `None`, exactly as before v17. An earlier
                // version of this arm returned the mem's declared bound
                // on a read, but an advisor pass caught a real soundness
                // hole before this shipped: the write-site induction
                // proves "every WRITTEN value satisfies the bound," not
                // "every READ returns an in-range value" -- a mem has no
                // `init`/reset at all (unlike a reg/out), so a read at an
                // address never written (or in an early cycle, before
                // the corresponding write has happened) returns
                // uninitialized data the write-site proof says nothing
                // about. Worse, that unproven value could reach `Stmt::
                // Let` (`let a = m[pc]`), then `m[a]`, exporting a
                // fabricated "proven" range into `site_ranges` --
                // `schedule.rs`'s own disjointness proof would trust it,
                // exactly `subleq.tr`'s own shape (an index loaded out of
                // the mem itself). This module's OWN standing distinction
                // (a bound on an `in` port would be a TRUSTED external
                // contract, "a different feature entirely" from this
                // module's proof-only mandate) applies identically here:
                // a mem's declared bound is only ever a claim about what
                // was WRITTEN, never a substitute for proving what a read
                // returns.
                None
            }
            // v18: a struct field READ's own value, deliberately handed
            // back here -- unlike a mem's `Bracket` arm above, this IS
            // sound: see `struct_field_bounds`'s own doc comment and
            // `struct_field_bound`'s own doc comment for the full
            // argument (a struct-typed reg/out has a mandatory checked
            // init plus every-write-is-a-checked-literal, exactly the
            // induction that already makes a scalar reg/out's bound
            // sound; a struct-typed local traces back to its own single
            // literal binding). `base` itself is walked via `struct_
            // field_bound`, a SEPARATE recursive function from this one
            // (not `expr_bound` again) since it needs to inspect `base`'s
            // own STRUCTURAL shape (a literal, an ident, ..), not a
            // numeric range.
            Expr::Field { base, name } => {
                self.struct_field_bound(*base, name, state, locals, struct_origins)
            }
            _ => None,
        }
    }

    /// A struct FIELD's own provable value bound (v18), given the
    /// EXPRESSION representing the struct VALUE it comes from -- not a
    /// `Field`-access `ExprId` (that's `expr_bound`'s own new arm, which
    /// calls into this), since a `..base`-filled field has no literal
    /// AST node of its own to call `expr_bound` on directly. Returns
    /// `None` for any origin this pass doesn't affirmatively know is
    /// checked -- the safe default, exactly like `expr_bound`'s own
    /// catch-all.
    ///
    /// The `Expr::Ident` case's `DefKind` match is where this feature's
    /// WHOLE soundness argument lives: `Reg`/`Output` are trusted
    /// against the flat `struct_field_bounds` declared fact directly,
    /// because EVERY write to a struct-typed reg/out (including its
    /// mandatory `= init`) is independently checked elsewhere in this
    /// file (`check_struct_field_inits`, `Stmt::Assign`'s own new
    /// branch) -- the same "write-checked, read-trusted" argument v5's
    /// own scalar reg/out bound already rests on, extended one layer.
    /// `Local` is traced back to whatever expression it was bound to
    /// (`struct_origins`, populated by `Stmt::Let`), recursing into
    /// THAT expression instead of trusting the type alone -- needed
    /// because, unlike a reg/out, a local's single binding site could be
    /// an ALIAS of an untrusted value (`let p2 = q` where `q : Pair` is
    /// an `in` port); tracing through means `p2.data` correctly falls
    /// through to `None` once the recursion reaches `q`'s own `Expr::
    /// Ident` and hits the catch-all below. EVERY OTHER `DefKind`
    /// (`Input`, `Mem`, `Fifo`, `Io`, `Param`, ...) falls to that same
    /// catch-all, unconditionally -- an `in`-port-typed struct's bits
    /// arrive over an external wire, never through a checked `StructLit`
    /// at all, exactly as untrusted as an unwritten mem address (the
    /// hole v17 closed for mem reads, found again here through a
    /// different door during this feature's own design review).
    fn struct_field_bound(
        &mut self,
        expr: ExprId,
        field_name: &str,
        state: &HashMap<DefId, (u64, u64)>,
        locals: &HashMap<DefId, Option<(u64, u64)>>,
        struct_origins: &HashMap<DefId, ExprId>,
    ) -> Option<(u64, u64)> {
        match self.ast.expr(expr).clone() {
            // A literal: check the named field directly, or -- if this
            // literal used `..base` and omitted `field_name` -- recurse
            // into `base`'s own same-named field, exactly mirroring how
            // `compile_struct_field_read` (firrtl/expr.rs) itself
            // resolves an omitted field, just for a provable bound
            // instead of codegen.
            Expr::StructLit { fields, base, .. } => match fields
                .iter()
                .find(|(n, _)| n == field_name)
            {
                Some((_, value)) => self.expr_bound(*value, state, locals, struct_origins),
                None => self.struct_field_bound(base?, field_name, state, locals, struct_origins),
            },
            Expr::Ident(_) => {
                let def = self.res.expr_defs.get(&expr).copied()?;
                match self.res.def(def).kind {
                    crate::resolve::DefKind::Reg | crate::resolve::DefKind::Output => {
                        let struct_def = match self.ty.state_tys.get(&def)? {
                            Ty::Struct { def, .. } => *def,
                            _ => return None,
                        };
                        self.struct_field_bounds
                            .get(&(struct_def, field_name.to_string()))
                            .map(|b| (b.lower, b.upper))
                    }
                    crate::resolve::DefKind::Local => {
                        let origin = struct_origins.get(&def).copied()?;
                        self.struct_field_bound(origin, field_name, state, locals, struct_origins)
                    }
                    _ => None, // Input, Mem, Fifo, Io, Param, ... -- always untrusted here
                }
            }
            // Deferred (v1 restriction, not yet supported): a nested
            // struct-of-struct field chain (`outer.inner.x`) falls
            // through here structurally -- `expr` would itself be an
            // `Expr::Field`, which this match doesn't special-case, so
            // it safely composes to `None` rather than attempting (and
            // potentially getting wrong) a deeper chase.
            _ => None,
        }
    }

    /// The stage-1 shadow check's own flat re-keying of `struct_field_
    /// bounds` (itself keyed by the struct TYPE's own `DefId`) into a
    /// map keyed by each Reg/Output VARIABLE's own `DefId` instead --
    /// exactly what `smt::translate_expr`'s new `Expr::Field` arm needs,
    /// since that module has no type information of its own and
    /// shouldn't need any (see `smt::StructFieldBoundsByDef`'s own doc
    /// comment). Filters to `Reg`/`Output` ONLY, mirroring `struct_
    /// field_bound`'s own `DefKind` match exactly: a `Local`'s field
    /// isn't trusted flatly there (it may alias an untrusted value), so
    /// it must be equally absent here, not just coincidentally excluded
    /// by `state_tys` never containing a `Local` entry in the first
    /// place -- an explicit filter, not a structural accident to rely
    /// on. Computed once per `shadow_check_bounds` call, not per
    /// branch/state: unlike a scalar's bound, a struct field's own
    /// declared range never narrows per-guard (mirrors `struct_field_
    /// bounds`'s own flat treatment, same as the real pass).
    fn struct_field_bounds_by_def(&self) -> HashMap<(DefId, String), (u64, u64)> {
        let mut map = HashMap::new();
        for (&def, ty) in &self.ty.state_tys {
            if !matches!(
                self.res.def(def).kind,
                crate::resolve::DefKind::Reg | crate::resolve::DefKind::Output
            ) {
                continue;
            }
            let Ty::Struct {
                def: struct_def, ..
            } = ty
            else {
                continue;
            };
            for ((sd, field), bounded) in &self.struct_field_bounds {
                if sd == struct_def {
                    map.insert((def, field.clone()), (bounded.lower, bounded.upper));
                }
            }
        }
        map
    }

    /// Defense in depth, not trust-by-construction: every bounded def
    /// that `effects.rs` reports as written SOMEWHERE in the program
    /// must be a def this pass ALSO found a real `Stmt::Assign` for
    /// somewhere in its own exhaustive walk. A mismatch means this
    /// pass's own walk missed a real write site — an internal
    /// inconsistency, not a user-facing diagnostic, so it panics rather
    /// than silently reporting an unsound "proof."
    ///
    /// v12: a bounded PARAM's `DefId` is now also a key in `self.bounded`,
    /// but reassigning a param (`i := 0` inside a fn body) can never
    /// trip `effects_says_written` for it: `effects.rs`'s `infer_write`
    /// only calls `sig.writes.insert` after `state_def` succeeds, and
    /// `state_def` requires `DefKind::is_state()` (`effects.rs:726-729`,
    /// `resolve.rs:76-88`) — `DefKind::Param` isn't in that list. So a
    /// param's `DefId` is never inserted into any `sig.writes` set,
    /// `effects_says_written` is always `false` for it, and this check
    /// can't panic on a param regardless of whether `found_writes`
    /// happens to contain it. Confirmed empirically, not just by
    /// reading the match arm: compiled a scratch module reassigning a
    /// bounded param inside its own fn body, no panic.
    ///
    /// v17: `self.mem_bounds`'s own keys are chained in too — a mem
    /// write is `sig.writes.insert(mem_def)` in `effects.rs` exactly
    /// like a reg/out (`state_def` accepts `DefKind::Mem`,
    /// `effects.rs:486-489`), so the identical internal-consistency
    /// argument applies: if `effects.rs` says a mem was written
    /// somewhere but this pass's own (new) `Expr::Bracket`-write-
    /// matching logic never recognized it, that's this pass's own bug,
    /// not a user-facing diagnostic.
    fn check_write_site_exhaustiveness(&self) {
        for def in self.bounded.keys().chain(self.mem_bounds.keys()) {
            let effects_says_written = self.fx.sigs.values().any(|s| s.writes.contains(def));
            if effects_says_written && !self.found_writes.contains(def) {
                panic!(
                    "bounds.rs internal error: effects.rs reports {def:?} written somewhere in \
                     the program, but bounds.rs's own exhaustive walk found no Stmt::Assign to \
                     it anywhere — its own write-site walk is not actually exhaustive"
                );
            }
        }
    }

    /// Stage 1 of DESIGN.md's "Toward a dependent/refinement type
    /// system (SMT-backed, planned)": re-derive every scalar reg/out/
    /// param, mem-element, and (named-field) struct-field write
    /// obligation this pass already checks via interval arithmetic,
    /// independently via Z3 (`smt::check_bound_obligation`), and panic
    /// on any REAL disagreement -- the same "two independent oracles
    /// must agree" idiom `check_write_site_exhaustiveness` above already
    /// uses against `effects.rs`, just against a from-scratch SMT
    /// re-derivation instead of a second static pass. A `Skipped`
    /// verdict is not a disagreement (see `smt`'s own module doc for
    /// exactly what v1 does and doesn't translate yet) -- only a site
    /// both engines actually judged, with different answers, panics.
    ///
    /// Walks bodies independently from `check_stmt`/`check_body` rather
    /// than reusing them: this pass needs only the textual guard
    /// history to a write site (a `Vec<ExprId>` of enclosing
    /// conditions, negated for an `else`), not `check_stmt`'s own
    /// `locals`/`struct_origins` narrowing machinery, which exists to
    /// support composition shapes (`<>` edge narrowing, a `let`-bound
    /// local, a `..base`-sourced struct field, ...) this v1 encoding
    /// doesn't attempt yet.
    fn shadow_check_bounds(&mut self) -> Vec<smt::Mismatch> {
        let mut mismatches = Vec::new();
        let struct_field_bounds_by_def = self.struct_field_bounds_by_def();
        // v13's own composition capability (`Bump(3) + Bump(4)`), re-keyed
        // to plain `(u64, u64)` tuples for `smt::translate_expr`'s own
        // `Expr::Call` arm -- no indirection needed here (unlike struct
        // fields), since `fn_ret_bound` is already keyed by the callee's
        // own `DefId`, exactly what a call site's `callee` resolves to.
        let fn_ret_bounds_by_def: HashMap<DefId, (u64, u64)> = self
            .fn_ret_bound
            .iter()
            .map(|(d, b)| (*d, (b.lower, b.upper)))
            .collect();
        for id in self.collect_bodied_items() {
            let body = match self.ast.item(id) {
                Item::Rule { body, .. } => body.clone(),
                Item::Fn { body, .. } => body.clone(),
                _ => continue,
            };
            // Seeded exactly like `check_item`'s own `state` -- every
            // bounded def's flat declared range -- so `narrow_for_
            // condition`/`narrow_for_else` below narrow the SAME
            // starting point the real pass narrows, not this walker's
            // own approximation of it.
            let state: HashMap<DefId, (u64, u64)> = self
                .bounded
                .iter()
                .map(|(d, b)| (*d, (b.lower, b.upper)))
                .collect();
            // v13: this item's own declared return postcondition, if
            // any -- mirrors `check_item`'s own `current_ret_bound`
            // exactly (`None` for a `rule`, or a `fn` with no `where _ <
            // N`), just threaded as a plain parameter instead of a
            // `self` field, since (unlike the real pass) this walker
            // never needs to change it mid-body.
            let ret_bound = self
                .res
                .item_defs
                .get(&id)
                .and_then(|def| self.fn_ret_bound.get(def))
                .map(|b| **b);
            self.shadow_walk_body(
                &body,
                &state,
                &struct_field_bounds_by_def,
                &fn_ret_bounds_by_def,
                &[],
                &[],
                ret_bound,
                &mut mismatches,
            );
        }
        mismatches
    }

    /// The relational-invariant half of stage 1 (DESIGN.md's own acid
    /// test): re-derives every `invariant` fact's own inductive step
    /// via `smt::check_relational_obligation`'s general `fire_R`
    /// transition encoding, instead of `check_relational_bound_
    /// induction`'s hand-rolled `0..2^n` mask enumeration -- and
    /// compares against THAT function's own already-recorded verdict
    /// (`self.failed_relational`, populated earlier in `bounds::check`,
    /// before this runs).
    ///
    /// Recomputes `contributions` by calling the exact same `walk_
    /// deltas`/`leading_guards` helpers `check_relational_bound_
    /// induction` itself calls -- these are this module's shared,
    /// trusted AST-recognition front end (see `smt::check_relational_
    /// obligation`'s own doc comment for why reusing them here doesn't
    /// undermine this shadow check's independence: what's re-verified
    /// independently is the subset/modular-shift REASONING built on top
    /// of them, not the recognition of what counts as a delta in the
    /// first place). Recomputing rather than storing them from the
    /// earlier real pass keeps this function self-contained and mirrors
    /// how every other `shadow_*` method in this file works from a
    /// fresh walk, not a cached one.
    fn shadow_check_relational_bounds(&mut self) -> Vec<smt::Mismatch> {
        let mut mismatches = Vec::new();
        for index in 0..self.relational_bounds.len() {
            let rb = self.relational_bounds[index].clone();
            let relevant: HashSet<DefId> = rb.terms.iter().map(|(d, _)| *d).collect();
            let base_state: HashMap<DefId, (u64, u64)> = self
                .bounded
                .iter()
                .map(|(d, b)| (*d, (b.lower, b.upper)))
                .collect();

            let mut contributions: Vec<(i64, Vec<ExprId>)> = Vec::new();
            let mut recognized = true;
            for id in self.collect_bodied_items() {
                let Item::Rule { body, .. } = self.ast.item(id).clone() else {
                    continue;
                };
                let Some(sig) = self.fx.sigs.get(&id) else {
                    continue;
                };
                let sig_writes: HashSet<DefId> = sig
                    .writes
                    .iter()
                    .filter(|d| relevant.contains(d))
                    .copied()
                    .collect();
                if sig_writes.is_empty() {
                    continue;
                }
                let Some(deltas) = self.walk_deltas(&body, &relevant, &base_state, rb.modulus)
                else {
                    recognized = false;
                    break;
                };
                let direct: HashSet<DefId> = deltas.keys().copied().collect();
                if direct != sig_writes {
                    recognized = false;
                    break;
                }
                let delta: i64 = deltas
                    .iter()
                    .map(|(d, v)| {
                        let coeff = rb
                            .terms
                            .iter()
                            .find(|(td, _)| td == d)
                            .map(|(_, c)| *c)
                            .unwrap_or(0);
                        coeff * v
                    })
                    .sum();
                contributions.push((delta, self.leading_guards(&body)));
            }
            if !recognized {
                continue; // the real pass ALSO couldn't recognize this -- not comparable, not a disagreement
            }

            let verdict = smt::check_relational_obligation(self.ast, self.res, &rb, &contributions);
            if matches!(verdict, smt::SmtVerdict::Skipped(_)) {
                continue;
            }
            let interval_says_proven = !self.failed_relational.contains(&index);
            let smt_says_proven = matches!(verdict, smt::SmtVerdict::Proved);
            if interval_says_proven != smt_says_proven {
                mismatches.push(smt::Mismatch {
                    span: rb.span.clone(),
                    interval_says_in_bound: interval_says_proven,
                    smt: verdict,
                });
            }
        }
        mismatches
    }

    /// v12: every declared-bounded parameter of a called fn/impl,
    /// checked against its own actual argument -- the call-site half of
    /// param-bound propagation, mirroring `expr_bound`'s own `Expr::
    /// Call` arm. Recurses into a `Call`'s own args, a `Bracket`'s own
    /// callee/args, and an `Add`/`Sub`/`Mul`'s own operands -- the same
    /// three shapes `expr_bound`'s own `Call`/`Bracket` arms and
    /// `check_calls_in`'s own stop-list recurse through to find a call
    /// nested one layer down, confirmed as the ONLY shapes this shadow
    /// check's own `Skipped`-site census found actually exercised
    /// (`Outer(Bump(50))`, `m[Bump(50)]` read as a value, `Bump(3) +
    /// Bump(4)`) -- not the fully general `check_calls_in` sweep (no
    /// `sub_exprs` fallback for every OTHER wrapper shape), since
    /// nothing in the current suite needs it and adding it would be
    /// speculative completeness, not closing a measured gap. A call
    /// inside a GUARD's own operand position (`if Bump(50) < 5`) is a
    /// distinct, deliberately NOT-attempted gap (see `smt`'s own module
    /// doc for why dropping an untranslatable guard's hypothesis to
    /// reach it would be unsound) -- this function is never called on a
    /// guard `ExprId` at all, only on a write's own rhs/return value/
    /// bare call statement, so that gap can't accidentally get "fixed"
    /// here as a side effect.
    #[allow(clippy::too_many_arguments)]
    fn shadow_check_call_params(
        &mut self,
        state: &HashMap<DefId, (u64, u64)>,
        struct_field_bounds_by_def: &HashMap<(DefId, String), (u64, u64)>,
        fn_ret_bounds_by_def: &HashMap<DefId, (u64, u64)>,
        guards: &[ExprId],
        guards_negated: &[ExprId],
        id: ExprId,
        out: &mut Vec<smt::Mismatch>,
    ) {
        if let Expr::Call { callee, args } = self.ast.expr(id).clone() {
            if let Some(&fn_def) = self.res.expr_defs.get(&callee)
                && let Some(params) = self.fn_params.get(&fn_def).cloned()
            {
                for (param, &arg) in params.iter().zip(&args) {
                    if param.bound.is_none() {
                        continue;
                    }
                    let def = def_of_name(self.res, &param.name);
                    let Some(bound) = self.bounded.get(&def).copied() else {
                        continue;
                    };
                    self.shadow_compare(
                        state,
                        struct_field_bounds_by_def,
                        fn_ret_bounds_by_def,
                        *bound,
                        guards,
                        guards_negated,
                        arg,
                        out,
                    );
                }
            }
            // A nested call as ANOTHER call's own argument (`Outer(
            // Bump(50))`) -- mirrors `expr_bound`'s own `Expr::Call` arm
            // calling `expr_bound` on EVERY arg unconditionally, not
            // just ones whose param has a declared bound (v14's own
            // fix, see that arm's doc comment).
            for &arg in &args {
                self.shadow_check_call_params(
                    state,
                    struct_field_bounds_by_def,
                    fn_ret_bounds_by_def,
                    guards,
                    guards_negated,
                    arg,
                    out,
                );
            }
            return;
        }
        if let Expr::Bracket { callee, args } = self.ast.expr(id).clone() {
            // A mem/fifo access's own callee/args -- mirrors `expr_
            // bound`'s own `Expr::Bracket` arm, which recurses into
            // both via `check_calls_in` (`m[Bump(50)]` used as a
            // READ value, not a write target).
            self.shadow_check_call_params(
                state,
                struct_field_bounds_by_def,
                fn_ret_bounds_by_def,
                guards,
                guards_negated,
                callee,
                out,
            );
            for &arg in &args {
                self.shadow_check_call_params(
                    state,
                    struct_field_bounds_by_def,
                    fn_ret_bounds_by_def,
                    guards,
                    guards_negated,
                    arg,
                    out,
                );
            }
            return;
        }
        if let Expr::Binary { op, lhs, rhs } = self.ast.expr(id).clone()
            && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
        {
            // `total := Bump(3) + Bump(4)` -- mirrors `expr_bound`'s own
            // `Add`/`Sub`/`Mul` arms, each of which calls `expr_bound`
            // on both operands unconditionally.
            self.shadow_check_call_params(
                state,
                struct_field_bounds_by_def,
                fn_ret_bounds_by_def,
                guards,
                guards_negated,
                lhs,
                out,
            );
            self.shadow_check_call_params(
                state,
                struct_field_bounds_by_def,
                fn_ret_bounds_by_def,
                guards,
                guards_negated,
                rhs,
                out,
            );
        }
    }

    /// One write site's own obligation, checked both ways and compared
    /// -- shared by the scalar/mem-element/struct-field call sites in
    /// `shadow_walk_body` below, since the comparison logic (call SMT
    /// first, skip if it didn't translate, ONLY THEN call the interval
    /// engine, compare, record a mismatch) is identical for all three.
    /// A struct field's own call site already resolves `rhs` down to
    /// the field's OWN value expression before calling this (the
    /// `StructLit`'s own named-field value) -- exactly what `Checker::
    /// struct_field_bound`'s `Expr::StructLit` arm does internally when
    /// the field IS named directly, so `expr_bound` alone reproduces
    /// the same "old" verdict for all three cases uniformly; no
    /// separate `struct_field_bound` call needed here.
    ///
    /// Calling `expr_bound` only AFTER confirming the SMT verdict isn't
    /// `Skipped` is load-bearing, not incidental: `check_bound_
    /// obligation` only returns a non-`Skipped` verdict when `translate_
    /// expr` accepted `rhs`, which means (by that function's own
    /// structural induction) `rhs`'s whole subtree contains only
    /// `Int`/`SizedInt`/bounded-`Ident`/`Add`/`Sub`/`Mul`/a trusted
    /// struct-field `Field`/an `fn_ret_bounds`-covered `Call` -- UNLIKE
    /// the first four, `Field` and (especially) `Call` are NOT
    /// side-effect-free to re-evaluate: `expr_bound`'s own `Call` arm
    /// pushes a real "argument for parameter" error whenever an
    /// argument violates its declared bound, as a deliberate SIDE
    /// EFFECT of computing a range, not just a value computation (see
    /// that arm's own doc comment). So `rhs` reaching here CAN now
    /// contain a `Call`, and `expr_bound` below WOULD double-push
    /// whatever error the real, live `check_stmt` pass already recorded
    /// for the exact same site -- guarded against explicitly below
    /// (snapshot `self.errors`'s length, discard anything this
    /// reconstruction call itself pushed) rather than relying on it
    /// being structurally impossible, since it no longer is.
    #[allow(clippy::too_many_arguments)]
    fn shadow_compare(
        &mut self,
        bounds_by_def: &HashMap<DefId, (u64, u64)>,
        struct_field_bounds_by_def: &HashMap<(DefId, String), (u64, u64)>,
        fn_ret_bounds_by_def: &HashMap<DefId, (u64, u64)>,
        target: BoundedDef,
        guards: &[ExprId],
        guards_negated: &[ExprId],
        rhs: ExprId,
        out: &mut Vec<smt::Mismatch>,
    ) {
        let verdict = smt::check_bound_obligation(
            self.ast,
            self.res,
            bounds_by_def,
            struct_field_bounds_by_def,
            fn_ret_bounds_by_def,
            target,
            guards,
            guards_negated,
            rhs,
        );
        if matches!(verdict, smt::SmtVerdict::Skipped(_)) {
            return;
        }
        let locals = HashMap::new();
        let struct_origins = HashMap::new();
        // See this fn's own doc comment: `rhs` can now contain a `Call`,
        // whose own argument-bound violation `expr_bound` would push as
        // a REAL error -- a duplicate of whatever the live `check_stmt`
        // pass (or `shadow_check_call_params`, for this same site)
        // already recorded. Truncate back to the pre-call length
        // unconditionally: this reconstruction call exists purely to
        // read back a COMPUTED RANGE for comparison, never to report
        // anything itself.
        let errors_before = self.errors.len();
        let computed = self.expr_bound(rhs, bounds_by_def, &locals, &struct_origins);
        self.errors.truncate(errors_before);
        // Mirrors `check_against_bound`'s own three-way check exactly
        // (width overflow, upper violation, lower violation) -- NOT
        // just a bare `[lower, upper)` comparison, which would silently
        // miss the width-overflow rejection `check_against_bound`
        // reports as a real error (found via a real disagreement this
        // shadow check itself surfaced,
        // `multiplication_composed_bound_exceeding_the_declared_width_
        // is_rejected`).
        let width_limit = 1u64.checked_shl(target.width as u32).unwrap_or(u64::MAX);
        let interval_says_in_bound = matches!(
            computed,
            Some((lo, hi)) if hi <= width_limit && hi <= target.upper && lo >= target.lower
        );
        let smt_says_in_bound = matches!(verdict, smt::SmtVerdict::Proved);
        if interval_says_in_bound != smt_says_in_bound {
            let span = self.ast.expr_spans[rhs.0 as usize].clone();
            out.push(smt::Mismatch {
                span,
                interval_says_in_bound,
                smt: verdict,
            });
        }
    }

    /// `state` mirrors `check_stmt`'s own narrowed-range map exactly
    /// (seeded from `self.bounded`, narrowed per-branch via `narrow_
    /// for_condition`/`narrow_for_else`) -- used ONLY to reproduce the
    /// interval engine's own verdict for comparison. `guards`/`guards_
    /// negated` carry the SAME branch history as `ExprId`s instead, for
    /// `smt::check_bound_obligation`'s own from-scratch translation --
    /// two representations of one fact, not two independent sources of
    /// truth: both are derived from the exact same `cond` at the exact
    /// same `Stmt::If`, one line apart, below.
    #[allow(clippy::too_many_arguments)]
    fn shadow_walk_body(
        &mut self,
        body: &[StmtId],
        state: &HashMap<DefId, (u64, u64)>,
        struct_field_bounds_by_def: &HashMap<(DefId, String), (u64, u64)>,
        fn_ret_bounds_by_def: &HashMap<DefId, (u64, u64)>,
        guards: &[ExprId],
        guards_negated: &[ExprId],
        ret_bound: Option<BoundedDef>,
        out: &mut Vec<smt::Mismatch>,
    ) {
        for &stmt in body {
            match self.ast.stmt(stmt).clone() {
                // v12: a bare call statement (a void fn/impl call, made
                // purely for its `writes` effect) -- the ONLY position
                // `Stmt::Expr` itself can hold a call directly.
                Stmt::Expr(e) => {
                    self.shadow_check_call_params(
                        state,
                        struct_field_bounds_by_def,
                        fn_ret_bounds_by_def,
                        guards,
                        guards_negated,
                        e,
                        out,
                    );
                }
                Stmt::Assign { lhs, rhs } => {
                    // v12: `rhs` may itself be a bare call (`x :=
                    // Bump(y)`, `writes`-only or otherwise) regardless
                    // of whether `lhs` is itself bounded.
                    self.shadow_check_call_params(
                        state,
                        struct_field_bounds_by_def,
                        fn_ret_bounds_by_def,
                        guards,
                        guards_negated,
                        rhs,
                        out,
                    );
                    // This LHS's own write obligations, mirroring
                    // `check_stmt`'s own real check exactly (see that
                    // arm's doc comment) -- three shape tests recognizing
                    // what applies, collapsed to one shared tail: each
                    // obligation is a (declared bound, value expression
                    // to compare it against) pair, drained by one
                    // `shadow_compare` loop below instead of three
                    // identical call sites.
                    let mut obligations: Vec<(BoundedDef, ExprId)> = Vec::new();
                    // Scalar reg/out/param: same shape as v5's own
                    // induction (`self.bounded`), self-reference in
                    // `rhs` resolves via `state` (which already
                    // includes `def`'s own entry, seeded above).
                    if let Expr::Ident(_) = self.ast.expr(lhs)
                        && let Some(&def) = self.res.expr_defs.get(&lhs)
                        && let Some(bound) = self.bounded.get(&def).copied()
                    {
                        obligations.push((*bound, rhs));
                    }
                    // v17: a mem write (`m[i] := rhs`) checks `rhs`
                    // against the mem's own flat declared elem bound --
                    // `mem_bounds` never narrows per-branch (mirrors
                    // `check_stmt`'s own real check, which reads it the
                    // same flat way), so `state` (already narrowed for
                    // SCALAR defs) is still the right hypothesis source
                    // for anything else `rhs` might reference.
                    if let Expr::Bracket { callee, .. } = self.ast.expr(lhs)
                        && let Some(&mem_def) = self.res.expr_defs.get(callee)
                        && self.res.def(mem_def).kind == crate::resolve::DefKind::Mem
                        && let Some(bound) = self.mem_bounds.get(&mem_def).copied()
                    {
                        obligations.push((*bound, rhs));
                    }
                    // v18: a struct-typed write (`p := Pair{...}`)
                    // checks each bounded field's own value against its
                    // declared bound -- ONLY when `rhs` is a `StructLit`
                    // naming that field DIRECTLY. A `..base`-sourced
                    // field (the field omitted, filled from `base`'s
                    // own same-named field) is a deliberate v1 scope cut
                    // for this shadow check (see `smt`'s own module
                    // doc) -- `Checker::struct_field_bound`'s own
                    // `DefKind`-gated provenance trace isn't reproduced
                    // here yet, so that case is silently left
                    // un-compared, not attempted and potentially wrong.
                    if let Expr::Ident(_) = self.ast.expr(lhs)
                        && let Some(&def) = self.res.expr_defs.get(&lhs)
                        && let Some(Ty::Struct {
                            def: struct_def, ..
                        }) = self.ty.state_tys.get(&def).cloned()
                        && let Expr::StructLit { fields, .. } = self.ast.expr(rhs).clone()
                    {
                        let bounded_fields: Vec<String> = self
                            .struct_field_bounds
                            .keys()
                            .filter(|(sd, _)| *sd == struct_def)
                            .map(|(_, name)| name.clone())
                            .collect();
                        for field_name in bounded_fields {
                            let Some((_, value)) = fields.iter().find(|(n, _)| *n == field_name)
                            else {
                                continue; // `..base`-sourced -- see doc comment above
                            };
                            let bound =
                                *self.struct_field_bounds[&(struct_def, field_name.clone())];
                            obligations.push((bound, *value));
                        }
                    }
                    for (bound, value) in obligations {
                        self.shadow_compare(
                            state,
                            struct_field_bounds_by_def,
                            fn_ret_bounds_by_def,
                            bound,
                            guards,
                            guards_negated,
                            value,
                            out,
                        );
                    }
                }
                // v13: only meaningful when the enclosing fn declared a
                // postcondition (`ret_bound`, computed once per item in
                // `shadow_check_bounds`) -- mirrors `check_stmt`'s own
                // `Stmt::Return` arm exactly. `e` may also itself be a
                // bare call (v12's own obligation, checked the same way
                // a bare call statement/assign-rhs is above).
                Stmt::Return(Some(e)) => {
                    self.shadow_check_call_params(
                        state,
                        struct_field_bounds_by_def,
                        fn_ret_bounds_by_def,
                        guards,
                        guards_negated,
                        e,
                        out,
                    );
                    if let Some(bound) = ret_bound {
                        self.shadow_compare(
                            state,
                            struct_field_bounds_by_def,
                            fn_ret_bounds_by_def,
                            bound,
                            guards,
                            guards_negated,
                            e,
                            out,
                        );
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_state = self.narrow_for_condition(cond, state);
                    let mut then_guards = guards.to_vec();
                    then_guards.push(cond);
                    self.shadow_walk_body(
                        &then_body,
                        &then_state,
                        struct_field_bounds_by_def,
                        fn_ret_bounds_by_def,
                        &then_guards,
                        guards_negated,
                        ret_bound,
                        out,
                    );
                    if let Some(else_body) = else_body {
                        let else_state = self.narrow_for_else(cond, state);
                        let mut else_negated = guards_negated.to_vec();
                        else_negated.push(cond);
                        self.shadow_walk_body(
                            &else_body,
                            &else_state,
                            struct_field_bounds_by_def,
                            fn_ret_bounds_by_def,
                            guards,
                            &else_negated,
                            ret_bound,
                            out,
                        );
                    }
                }
                // v1: `While`/`IfLet`/`WhileLet` bodies are out of
                // scope for this shadow check (see `smt`'s own module
                // doc) -- neither walked into nor treated as a
                // disagreement.
                _ => {}
            }
        }
    }

    /// A real user-facing check, not an internal invariant (unlike
    /// `check_write_site_exhaustiveness` above, which panics — that one
    /// has an independent oracle, `effects.rs`, confirming a write site
    /// must exist somewhere; there's no such oracle here). A fn CAN be
    /// written with a declared postcondition and literally no `return`
    /// statement in its body — nothing upstream requires one — and
    /// without this check that postcondition would be trusted at every
    /// call site (`Expr::Call`'s own arm) with ZERO obligations ever
    /// verified against it: the entry in `fn_ret_bound` is created
    /// purely from the DECLARATION at collection time, with no
    /// coupling to whether `check_stmt` ever actually reached a
    /// `Stmt::Return` to check. Caught by an advisor pass before
    /// committing, confirmed empirically (a fn with an empty body and a
    /// declared `where _ < 20` let a caller's composition through
    /// with zero errors).
    fn check_return_site_exhaustiveness(&mut self) {
        for fn_def in self.fn_ret_bound.keys().copied().collect::<Vec<_>>() {
            if !self.found_returns.contains(&fn_def) {
                let span = self.ret_bound_span[&fn_def].clone();
                self.error(
                    span,
                    "this return bound is never checked against an actual `return` statement \
                     in the fn's own body -- a declared postcondition needs at least one \
                     `return <expr>` to prove it against, or callers would trust it with \
                     nothing actually verified"
                        .to_string(),
                );
            }
        }
    }

    /// v17's own sibling of `check_return_site_exhaustiveness` above --
    /// a REAL user-facing check, not an internal invariant (that's
    /// `check_write_site_exhaustiveness`'s job, extended to `mem_bounds`
    /// above). Unlike a return bound (trusted at every call site with no
    /// coupling to whether a `return` ever discharges it), a mem's
    /// declared bound is no longer consulted at any read site at all
    /// (see `expr_bound`'s `Expr::Bracket` arm's own doc comment) -- so
    /// this isn't closing a "trusted with nothing verified" soundness
    /// hole the way `check_return_site_exhaustiveness` does. It's a
    /// smaller, still real problem: a declared `where _ < K` with
    /// ZERO write sites anywhere in the program never gets its only
    /// actual obligation (the write-site check) exercised even once --
    /// dead, misleading metadata (a typo, or a sign the mem is only ever
    /// boot-loaded through a port `bounds.rs` doesn't track) that would
    /// otherwise compile silently with no diagnostic at all.
    fn check_mem_bound_is_proven(&mut self) {
        for mem_def in self.mem_bounds.keys().copied().collect::<Vec<_>>() {
            if !self.found_writes.contains(&mem_def) {
                let span = self.mem_bound_span[&mem_def].clone();
                self.error(
                    span,
                    "this mem bound is never checked against an actual write (`m[...] := ...`) \
                     anywhere in the program -- a declared bound needs at least one write to \
                     prove it against, or this declaration has nothing to verify at all"
                        .to_string(),
                );
            }
        }
    }
}

/// Fold a literal, or arithmetic over literals, to a compile-time
/// constant — deliberately NOT `types/eval.rs`'s fuller env-based
/// `const_eval` (that one's for generic-param elaboration-time folding,
/// via a non-empty `env`; a `where` bound's own limit is always checked
/// against an EMPTY env — `types/stmt.rs`'s `check_where_bound_init`
/// passes `&HashMap::new()` — so `Expr::Ident` never resolves there
/// either). This must stay in sync with what an empty-env `const_eval`
/// actually accepts: a where-bound whose limit/lower/init const_evals
/// successfully in `types.rs` but fails to fold here silently never gets
/// registered in `self.bounded`/etc at all (see each `collect_one_*`
/// caller's own `// types.rs already reported this` comment, which is
/// only true when the two folders agree) — found as a real, shipping
/// gap: `where cnt < 8 + 2` used to type-check clean while leaving
/// `cnt`'s bound completely unchecked, since this function handled only
/// a bare literal. `Add`/`Sub`/`Mul`/`Div`/`Rem`/`Shl`/`Shr` now mirror
/// `const_eval`'s own arithmetic exactly (same `checked_*` semantics,
/// failing closed on overflow rather than wrapping). `Expr::Call`
/// (`clog2`, `const_eval`'s one non-arithmetic case) deliberately still
/// isn't recognized here — no `Resolution` is available to this free
/// function to identify the builtin callee, and no test/example
/// currently needs it in a where-bound position; a `where` bound built
/// from `clog2(..)` still silently goes unchecked today, a narrower
/// instance of the same gap this fix otherwise closes.
fn const_fold(ast: &Ast, id: ExprId) -> Option<u64> {
    match ast.expr(id) {
        Expr::Int(v) => Some(*v),
        Expr::SizedInt { value, .. } => Some(*value),
        Expr::Binary { op, lhs, rhs } => {
            let l = const_fold(ast, *lhs)?;
            let r = const_fold(ast, *rhs)?;
            match op {
                BinOp::Add => l.checked_add(r),
                BinOp::Sub => l.checked_sub(r),
                BinOp::Mul => l.checked_mul(r),
                BinOp::Div => l.checked_div(r),
                BinOp::Rem => l.checked_rem(r),
                BinOp::Shl => l.checked_shl(r as u32),
                BinOp::Shr => l.checked_shr(r as u32),
                _ => None,
            }
        }
        _ => None,
    }
}

/// A state def's own declared bit width, if it's a plain `bits[N]`
/// (concretely known) — mirrors `schedule.rs`'s own `base_width` helper
/// exactly (same shape, same reasoning: anything else fails closed).
/// Checks `state_tys` (reg/mem/fifo) first, falling back to `local_tys`
/// (v12: a fn/impl PARAM's own declared type lives there instead —
/// `types/collect.rs`'s `check_body` populates it for every param and
/// ordinary `let` local alike — never `state_tys`). The two tables are
/// keyed by disjoint `DefId` sets, so this fallback never shadows a
/// reg/out's own entry.
fn base_width(ty: &Types, def: DefId) -> Option<u64> {
    let found = ty.state_tys.get(&def).or_else(|| ty.local_tys.get(&def))?;
    match found {
        Ty::Bits(Width::Known(w)) => Some(*w),
        _ => None,
    }
}

/// A mem's own ELEMENT type's declared bit width (v17), if it's a plain
/// `bits[N]` (concretely known) — the mem-bound sibling of `base_width`
/// immediately above, but reading through one extra layer: a mem's own
/// `state_tys` entry is `Ty::Mem { elem, len }`, not a bare `Ty::Bits`
/// directly (`base_width`'s own match would always miss it), so this
/// unwraps `elem` first before checking the same `Ty::Bits(Width::
/// Known(w))` shape. A mem whose element type is anything else (a
/// struct, an unresolved width) falls closed to `None`, same convention
/// `base_width`/`ret_width` both already follow.
fn mem_elem_width(ty: &Types, def: DefId) -> Option<u64> {
    match ty.state_tys.get(&def) {
        Some(Ty::Mem { elem, .. }) => match elem.as_ref() {
            Ty::Bits(Width::Known(w)) => Some(*w),
            _ => None,
        },
        _ => None,
    }
}

/// A struct field's own declared bit width (v18), if it's a plain
/// `bits[N]` (concretely known) — the struct-field-bound sibling of
/// `mem_elem_width` immediately above, reading through `Types.struct_
/// fields` (the struct's own declared, ordered field list, keyed by the
/// struct's `DefId`) instead of `state_tys`. A field whose type is
/// anything else (a nested struct, an unresolved width) falls closed to
/// `None`, same convention `mem_elem_width`/`ret_width`/`base_width` all
/// already follow.
fn struct_field_width(ty: &Types, struct_def: DefId, field_name: &str) -> Option<u64> {
    let fields = ty.struct_fields.get(&struct_def)?;
    let (_, field_ty) = fields.iter().find(|(name, _)| name == field_name)?;
    match field_ty {
        Ty::Bits(Width::Known(w)) => Some(*w),
        _ => None,
    }
}

/// A type ANNOTATION expression's own declared bit width, if it's the
/// plain `bits[N]` shape with a literal width (v13) — `base_width`
/// looks a bound's width up via a `DefId`'s own entry in `state_tys`/
/// `local_tys`, but a fn's RETURN type has no such def to key off (the
/// return value is never a named binding); this reads the raw AST
/// shape directly instead, self-contained like `const_fold`'s own
/// narrow approach rather than reusing `types/eval.rs`'s fuller
/// `eval_ty`. `[N]` type sugar desugars at parse time to exactly
/// `Bracket { callee: Ident("bits"), args: [width] }` (`parser.rs`).
fn ret_width(ast: &Ast, ty_expr: ExprId) -> Option<u64> {
    let Expr::Bracket { callee, args } = ast.expr(ty_expr) else {
        return None;
    };
    let Expr::Ident(name) = ast.expr(*callee) else {
        return None;
    };
    if name != "bits" || args.len() != 1 {
        return None;
    }
    const_fold(ast, args[0])
}

/// A `Stmt::Let`-bound name's own `DefId` — `firrtl/writes.rs` has an
/// identical helper (`pub(crate)`, but behind a private `mod writes` not
/// reachable outside `firrtl`); mirrored locally here rather than
/// widening that module's visibility, matching how `schedule.rs` already
/// keeps its own small resolution helpers (`state_base`, `base_width`)
/// local instead of importing another pass's.
fn def_of_name(res: &Resolution, name: &crate::ast::Name) -> DefId {
    res.defs
        .iter()
        .enumerate()
        .find(|(_, d)| d.span == name.span)
        .map(|(i, _)| DefId(i as u32))
        .expect("a resolved binding name always has a matching def")
}

/// Decomposes an `Add`/`Sub` tree of `reg`/`out` idents into a signed
/// sum of `(DefId, coefficient)` pairs (merging repeated occurrences of
/// the same def) -- `None` for anything this v1 restriction doesn't
/// recognize (a literal alone with no ident at all, a `Mul`, a `Call`,
/// an `in`/mem/fifo/local ident, ...). A free function (not a `Checker`
/// method) so `provably_disjoint_under_joint_guards` -- the `schedule
/// .rs` consumer, which has no `Checker` to call a method on, only
/// `ast`/`res` -- can reuse it directly; `Checker::linear_form` is a
/// thin wrapper kept for every existing in-file call site.
fn linear_form(ast: &Ast, res: &Resolution, expr: ExprId) -> Option<Vec<(DefId, i64)>> {
    match ast.expr(expr).clone() {
        Expr::Ident(_) => {
            let def = res.expr_defs.get(&expr).copied()?;
            match res.def(def).kind {
                crate::resolve::DefKind::Reg | crate::resolve::DefKind::Output => {
                    Some(vec![(def, 1)])
                }
                _ => None,
            }
        }
        Expr::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
        } => {
            let mut l = linear_form(ast, res, lhs)?;
            l.extend(linear_form(ast, res, rhs)?);
            Some(merge_terms(l))
        }
        Expr::Binary {
            op: BinOp::Sub,
            lhs,
            rhs,
        } => {
            let mut l = linear_form(ast, res, lhs)?;
            l.extend(
                linear_form(ast, res, rhs)?
                    .into_iter()
                    .map(|(d, c)| (d, -c)),
            );
            Some(merge_terms(l))
        }
        _ => None,
    }
}

/// The GUARD-matching sibling of `recognize_invariant`: parses a bare
/// comparison (no `%` peeling -- an ordinary rule guard never needs
/// one) into `(op, terms, const)`, for `narrow_combo_range` to compare
/// against a declared invariant's own `terms`. Returns `None` for any
/// shape `recognize_invariant` would also reject -- silently, since a
/// non-matching or unrecognized guard is simply IGNORED by the
/// induction (see `narrow_combo_range`'s own doc comment for why
/// that's always sound, never a soundness gap).
///
/// Tries `<linear> <op> <const>` first (`push_count - pop_count < 8`);
/// a real rule guard just as often compares two BARE idents directly
/// (`push_count <> pop_count`, not `push_count - pop_count <> 0`) --
/// confirmed against `circular_buffer_disjoint.tr`'s own `pop` guard,
/// which is exactly this shape -- so this also tries `<linear> <op>
/// <linear>`, folding both sides into `(lhs - rhs) <op> 0`. A free
/// function for the same reason `linear_form` is.
fn recognize_comparison(ast: &Ast, res: &Resolution, expr: ExprId) -> Option<Comparison> {
    let Expr::Binary { op, lhs, rhs } = ast.expr(expr).clone() else {
        return None;
    };
    if !matches!(
        op,
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne
    ) {
        return None;
    }
    let valid = |terms: &[(DefId, i64)]| {
        !terms.is_empty() && terms.iter().all(|(_, coeff)| coeff.abs() == 1)
    };
    if let Some(c) = const_fold(ast, rhs)
        && let Some(terms) = linear_form(ast, res, lhs)
        && valid(&terms)
    {
        return Some((op, terms, c));
    }
    if let (Some(l), Some(r)) = (linear_form(ast, res, lhs), linear_form(ast, res, rhs)) {
        let mut terms = l;
        terms.extend(r.into_iter().map(|(d, c)| (d, -c)));
        let terms = merge_terms(terms);
        if valid(&terms) {
            return Some((op, terms, 0));
        }
    }
    None
}

/// This rule's own leading `(cond)?` guard statements -- stops at the
/// first non-guard statement, a v1 restriction matching this feature's
/// own driving example (every guard in this language convention sits
/// at the top of a rule body). A guard appearing later is simply never
/// found here, which only means the induction has less to narrow with
/// -- always sound, never unsound, per `narrow_combo_range`'s own
/// "ignore what doesn't match" argument. A free function for the same
/// reason `linear_form` is.
fn leading_guards(ast: &Ast, body: &[StmtId]) -> Vec<ExprId> {
    let mut out = Vec::new();
    for &sid in body {
        match ast.stmt(sid) {
            Stmt::Expr(e) => match ast.expr(*e) {
                Expr::Guard(inner) => out.push(*inner),
                _ => break,
            },
            _ => break,
        }
    }
    out
}

/// Narrows `range` (a hypothesized `[lower, upper)` for `terms`' own
/// combination, entering this cycle) using ONE firing rule's own guard
/// -- but ONLY when that guard's own linear form is EXACTLY `terms`
/// (same defs, same coefficients): `push_count - pop_count < 8` narrows
/// Fact 1's own `push_count - pop_count` combination directly; `head -
/// tail - push_count + pop_count == 0` (Fact 2) is untouched by either
/// rule's guard, since neither guard's terms match Fact 2's four-def
/// combination at all.
///
/// A non-matching guard is silently ignored, mirroring `narrow_for_
/// condition`'s own per-scalar-def narrowing formulas exactly (`Lt`/
/// `Le`/`Gt`/`Ge` narrow one end; `Ne` narrows an end ONLY when the
/// excluded constant sits exactly on it) -- always sound: ignoring a
/// true fact only leaves the induction with a WIDER hypothesis than
/// necessary, never a narrower one than justified. A free function
/// (taking `terms` directly, not a whole `RelationalBound`) so `provably
/// _disjoint_under_joint_guards` can narrow an arbitrary linear
/// combination, not just a declared invariant's own.
fn narrow_combo_range(
    ast: &Ast,
    res: &Resolution,
    terms: &[(DefId, i64)],
    guard: ExprId,
    range: (u64, u64),
) -> (u64, u64) {
    let Some((op, guard_terms, c)) = recognize_comparison(ast, res, guard) else {
        return range;
    };
    if !same_terms(&guard_terms, terms) {
        return range;
    }
    let (lo, hi) = range;
    match op {
        BinOp::Lt => (lo, hi.min(c)),
        BinOp::Le => (lo, hi.min(c.saturating_add(1))),
        BinOp::Gt => match c.checked_add(1) {
            Some(floor) => (floor.max(lo), hi),
            None => range,
        },
        BinOp::Ge => (c.max(lo), hi),
        BinOp::Ne if c == lo => (lo.saturating_add(1), hi),
        BinOp::Ne if hi > 0 && c == hi - 1 => (lo, hi.saturating_sub(1)),
        _ => range,
    }
}

/// Sums coefficients for repeated occurrences of the same `DefId`
/// (`a - a` cancels to nothing) and drops any zero-coefficient result —
/// `linear_form`'s own normal form.
fn merge_terms(terms: Vec<(DefId, i64)>) -> Vec<(DefId, i64)> {
    let mut merged: Vec<(DefId, i64)> = Vec::new();
    for (def, coeff) in terms {
        if let Some(existing) = merged.iter_mut().find(|(d, _)| *d == def) {
            existing.1 += coeff;
        } else {
            merged.push((def, coeff));
        }
    }
    merged.retain(|(_, coeff)| *coeff != 0);
    merged
}

/// Whether two linear forms name exactly the same defs with exactly the
/// same coefficients — order-independent (`narrow_combo_range`'s own
/// "does this guard match this invariant's combination" check).
fn same_terms(a: &[(DefId, i64)], b: &[(DefId, i64)]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let bmap: HashMap<DefId, i64> = b.iter().copied().collect();
    a.iter().all(|(d, c)| bmap.get(d) == Some(c))
}

/// Whether two branches' own per-def deltas agree MODULO `modulus` —
/// deliberately NOT exact integer equality: `head`'s `if head < 7 {
/// head := head + 1 } else { head := 0 }` yields delta `+1` on the THEN
/// side but `-7` on the ELSE side (`0 - 7`, since `narrow_for_else`
/// pins `head` to exactly 7 there) — genuinely different integers, but
/// congruent mod 8 (the declared modulus for `circular_buffer_disjoint
/// .tr`'s own invariant), which is all `shift_preserves` ever needs:
/// its own final check already reduces the combined delta mod `rb.
/// modulus`, so two per-def deltas differing by an exact multiple of it
/// are interchangeable for that purpose. Missing on one side means 0.
fn deltas_agree(a: &HashMap<DefId, i64>, b: &HashMap<DefId, i64>, modulus: u64) -> bool {
    let defs: HashSet<DefId> = a.keys().chain(b.keys()).copied().collect();
    defs.iter().all(|d| {
        let av = a.get(d).copied().unwrap_or(0);
        let bv = b.get(d).copied().unwrap_or(0);
        (av - bv).rem_euclid(modulus as i64) == 0
    })
}

/// Whether shifting every value in `range` (a hypothesized `[lo, hi)`,
/// `hi` exclusive) by `delta`, then reducing modulo `modulus`, still
/// lands entirely within the declared `[lower, upper)` range — the
/// arithmetic core of `check_relational_bound_induction`'s inductive
/// step. An EMPTY hypothesis range (`lo >= hi`, an unreachable
/// combination of narrowing guards) is vacuously fine — a false premise
/// proves anything, the same reasoning `narrow_for_else`'s own doc
/// comment already relies on for an unreachable `else` branch.
///
/// Critically, this does NOT reduce `lo`/`hi` independently: it reduces
/// `lo + delta` into `[0, modulus)` by SOME multiple `k` of `modulus`,
/// then requires `hi - 1 + delta` to reduce by that SAME `k` — i.e. the
/// shifted interval must not itself straddle a modulus boundary. A
/// plain "reduce each endpoint independently" shift would wrongly
/// accept a straddling interval (e.g. `[-8, -7)` naively "wrapping" to
/// `[8, 9)` when reduced independently mod 8, even though `-8 mod 8 ==
/// 0`, the two ends reducing by DIFFERENT multiples) — the exact
/// mistake flagged before this was written, using `circular_buffer_
/// disjoint.tr`'s own `head`-in-the-`else`-branch case (delta `-8` on
/// the four-term Fact 2 combination, `M = 8`) as the worked example.
fn shift_preserves(range: (u64, u64), delta: i64, modulus: u64, lower: u64, upper: u64) -> bool {
    let (lo, hi) = range;
    if lo >= hi {
        return true;
    }
    let real_lo = lo as i64 + delta;
    let real_hi_inclusive = hi as i64 - 1 + delta;
    let k = real_lo.div_euclid(modulus as i64);
    let reduced_lo = real_lo - k * modulus as i64;
    let reduced_hi_inclusive = real_hi_inclusive - k * modulus as i64;
    if reduced_hi_inclusive >= modulus as i64 {
        return false; // straddles the modulus boundary -- can't soundly reduce
    }
    let reduced_lo = reduced_lo as u64;
    let reduced_hi_exclusive = reduced_hi_inclusive as u64 + 1;
    reduced_lo >= lower && reduced_hi_exclusive <= upper
}
