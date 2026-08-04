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
//!   carry a `where result < N` postcondition too** — the mirror of
//!   v12 in the OTHER direction. Checked against every `Stmt::Return`
//!   in the fn's own body (a NEW checked position — `current_ret_
//!   bound`, set once per item at the top of `check_item`), then
//!   trusted at every call site: `Expr::Call` returns `Some((lower,
//!   upper))` from the callee's own `fn_ret_bound` entry instead of
//!   unconditionally `None`, letting a caller compose with the call's
//!   own result (`total := Bump(3) + Bump(4)`). `result` is a textual
//!   placeholder, not a real scoped binding — a return value has no
//!   `DefId` of its own (unlike a reg/out/param's self-reference,
//!   checked by `DefId` equality against an existing declaration), so
//!   `resolve.rs`'s `check_ret_bound_shape` checks it by matching the
//!   literal identifier text instead, and this module keys its own
//!   postcondition table (`fn_ret_bound`) by the FN's own `DefId`
//!   rather than folding it into `self.bounded`. Opt-in, not blanket
//!   inference: a fn with no declared postcondition still composes to
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
//! needed for the realistic `let next = i + 1; i := next` shape.
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
}

/// One bounded def's (a `reg`, `out`, or fn/impl param, v12) own
/// declared facts, collected once up front.
#[derive(Clone, Copy)]
struct BoundedDef {
    lower: u64,
    upper: u64,
    width: u64,
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
        found_writes: HashSet::new(),
        found_returns: HashSet::new(),
        site_ranges: HashMap::new(),
        errors: Vec::new(),
    };
    checker.collect_bounded_defs();
    checker.collect_bounded_params();
    let bodied = checker.collect_bodied_items();
    for id in &bodied {
        checker.check_item(*id);
    }
    checker.check_write_site_exhaustiveness();
    checker.check_return_site_exhaustiveness();
    let bounds = Bounds {
        ranges: checker
            .bounded
            .iter()
            .map(|(def, b)| (*def, (b.lower, b.upper)))
            .collect(),
        site_ranges: checker.site_ranges,
    };
    (bounds, checker.errors)
}

struct Checker<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    fx: &'a Effects,
    ty: &'a Types,
    bounded: HashMap<DefId, BoundedDef>,
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
    fn_ret_bound: HashMap<DefId, BoundedDef>,
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
            BoundedDef {
                lower: lower_val,
                upper,
                width,
            },
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
            BoundedDef {
                lower: lower_val,
                upper,
                width,
            },
        );
        self.ret_bound_span
            .insert(fn_def, self.ast.expr_spans[bound.0 as usize].clone());
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

    fn check_item(&mut self, id: ItemId) {
        if self.bounded.is_empty() && self.fn_ret_bound.is_empty() {
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
        // v13: this item's own declared return postcondition, if any --
        // `None` for a `rule` (`item_defs` has no entry for one) or a
        // fn with no `where result < N` (no `fn_ret_bound` entry).
        let item_def = self.res.item_defs.get(&id).copied();
        self.current_ret_bound = item_def
            .and_then(|def| self.fn_ret_bound.get(&def))
            .copied();
        self.current_fn_def = item_def;
        self.check_body(&body, &mut state, &mut locals);
    }

    fn check_body(
        &mut self,
        body: &[StmtId],
        state: &mut HashMap<DefId, (u64, u64)>,
        locals: &mut HashMap<DefId, Option<(u64, u64)>>,
    ) {
        for stmt in body {
            self.check_stmt(*stmt, state, locals);
        }
    }

    fn check_stmt(
        &mut self,
        id: StmtId,
        state: &mut HashMap<DefId, (u64, u64)>,
        locals: &mut HashMap<DefId, Option<(u64, u64)>>,
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
                self.check_calls_in(lhs, state, locals);
                let def = if let Expr::Ident(_) = self.ast.expr(lhs) {
                    self.res.expr_defs.get(&lhs).copied()
                } else {
                    None
                };
                let bounded = def.and_then(|d| self.bounded.get(&d).copied());
                let computed = self.expr_bound(rhs, state, locals);
                if let (Some(def), Some(bounded)) = (def, bounded) {
                    self.found_writes.insert(def);
                    let span = self.ast.expr_spans[rhs.0 as usize].clone();
                    self.check_against_bound(computed, bounded, span, "write");
                }
            }
            Stmt::Let { name, init } => {
                let def = def_of_name(self.res, &name);
                let b = self.expr_bound(init, state, locals);
                locals.insert(def, b);
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
                self.check_calls_in(cond, state, locals);
                let mut then_state = self.narrow_for_condition(cond, state);
                let mut then_locals = locals.clone();
                self.check_body(&then_body, &mut then_state, &mut then_locals);
                if let Some(else_body) = else_body {
                    // v15: narrowed on the NEGATED condition, not the raw
                    // entry state -- see `narrow_for_else`'s own doc
                    // comment.
                    let mut else_state = self.narrow_for_else(cond, state);
                    let mut else_locals = locals.clone();
                    self.check_body(&else_body, &mut else_state, &mut else_locals);
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
                self.check_calls_in(init, state, locals);
                let mut then_state = state.clone();
                let mut then_locals = locals.clone();
                self.check_body(&then_body, &mut then_state, &mut then_locals);
                if let Some(else_body) = else_body {
                    let mut else_state = state.clone();
                    let mut else_locals = locals.clone();
                    self.check_body(&else_body, &mut else_state, &mut else_locals);
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
                self.check_calls_in(cond, state, locals); // v14
                let mut loop_state = self.narrow_for_condition(cond, state);
                let mut loop_locals = locals.clone();
                self.check_body(&body, &mut loop_state, &mut loop_locals);
            }
            Stmt::WhileLet { init, body, .. } => {
                self.check_calls_in(init, state, locals); // v14
                let mut loop_state = state.clone();
                let mut loop_locals = locals.clone();
                self.check_body(&body, &mut loop_state, &mut loop_locals);
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
                self.expr_bound(e, state, locals);
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
                // inside a fn with no `where result < N` used to skip
                // straight past `expr_bound`, silently missing `Bump`'s
                // own argument violation. The postcondition CHECK
                // itself still only fires when one is actually declared.
                let computed = self.expr_bound(e, state, locals);
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
            return self.expr_bound(id, state, locals);
        }
        for child in crate::lower::sub_exprs(self.ast, id) {
            self.check_calls_in(child, state, locals);
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
                let a = self.expr_bound(*lhs, state, locals);
                let b = self.expr_bound(*rhs, state, locals);
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
                let a = self.expr_bound(*lhs, state, locals);
                let b = self.expr_bound(*rhs, state, locals);
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
                let a = self.expr_bound(*lhs, state, locals);
                let b = self.expr_bound(*rhs, state, locals);
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
                        let computed = self.expr_bound(*arg, state, locals);
                        if let Some(bounded) = bounded {
                            let span = self.ast.expr_spans[arg.0 as usize].clone();
                            let context = format!("argument for parameter `{}`", param.name);
                            self.check_against_bound(computed, bounded, span, &context);
                        }
                    }
                }
                self.fn_ret_bound
                    .get(&fn_def)
                    .map(|bounded| (bounded.lower, bounded.upper))
            }
            // A mem/fifo access's own VALUE has no provable bound here
            // -- composes to `None`, unchanged from before this arm
            // existed. But `callee`/`args` can still contain a nested
            // `Call` whose own argument obligations need checking:
            // without this arm, `Expr::Bracket` fell through to the
            // catch-all below with ZERO recursion, so `y := m[Bump(50)]`
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
                self.check_calls_in(*callee, state, locals);
                let mem_def = self
                    .res
                    .expr_defs
                    .get(callee)
                    .copied()
                    .filter(|d| self.res.def(*d).kind == crate::resolve::DefKind::Mem);
                for arg in args {
                    let computed = self.check_calls_in(*arg, state, locals);
                    if mem_def.is_some()
                        && let Some(range) = computed
                    {
                        self.site_ranges.insert(*arg, range);
                    }
                }
                None
            }
            _ => None,
        }
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
    fn check_write_site_exhaustiveness(&self) {
        for def in self.bounded.keys() {
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
    /// declared `where result < 20` let a caller's composition through
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
}

/// Fold a bare literal to a compile-time constant — deliberately NOT
/// `types/eval.rs`'s fuller env-based `const_eval` (that one's for
/// generic-param elaboration-time folding), same reasoning
/// `schedule.rs`'s own `const_index`/`IndexForm` machinery already
/// documents for the identical narrow need.
fn const_fold(ast: &Ast, id: ExprId) -> Option<u64> {
    match ast.expr(id) {
        Expr::Int(v) => Some(*v),
        Expr::SizedInt { value, .. } => Some(*value),
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
