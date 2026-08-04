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
//!   singleton, `<reg> == <const>`) is also left unnarrowed,
//!   conservative but costless since no example needs the extra
//!   precision there.
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
//!   bound, always returning `None` itself (no RETURN-bound propagation
//!   in v1 — a fn's own return value carrying a provable bound a
//!   caller's composition could use is a real, separable follow-up).
//!   Checked at three expression positions: a `Stmt::Assign`'s RHS, a
//!   bare `Stmt::Expr` call statement (how a VOID fn/impl — no return
//!   value, called purely for its `writes` effect — is actually invoked
//!   in this language; the realistic shape, not an edge case), and any
//!   position `expr_bound`'s own `Add`/`Sub`/`Mul` recursion already
//!   reaches (a call nested inside an argument or operand). NOT checked
//!   in v1: `Stmt::Return`'s expr, or a call inside an `if`/`while`
//!   condition — reaching those needs a genuinely generic expression-
//!   tree walk, a real but separable follow-up, not bundled into this
//!   pass.
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
        found_writes: HashSet::new(),
        errors: Vec::new(),
    };
    checker.collect_bounded_defs();
    checker.collect_bounded_params();
    let bodied = checker.collect_bodied_items();
    for id in &bodied {
        checker.check_item(*id);
    }
    checker.check_write_site_exhaustiveness();
    let bounds = Bounds {
        ranges: checker
            .bounded
            .iter()
            .map(|(def, b)| (*def, (b.lower, b.upper)))
            .collect(),
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
    /// Every bounded def this pass found an ACTUAL `Stmt::Assign` for,
    /// anywhere in the program — cross-checked against `fx`'s own
    /// per-item write sets once the whole walk finishes (defense in
    /// depth: catches this pass's own walk silently missing a real
    /// write site, rather than trusting-by-construction that it never
    /// would).
    found_writes: HashSet<DefId>,
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
                Item::Fn { params, .. } => {
                    for param in params {
                        if let Some(bound) = param.bound {
                            let def = def_of_name(self.res, &param.name);
                            self.collect_one_bounded_def(def, bound, param.lower);
                        }
                    }
                    if let Some(&fn_def) = self.res.item_defs.get(&id) {
                        self.fn_params.insert(fn_def, params.clone());
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
        if self.bounded.is_empty() {
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
                let Expr::Ident(_) = self.ast.expr(lhs) else {
                    return; // a mem/struct-field write, irrelevant here
                };
                let Some(def) = self.res.expr_defs.get(&lhs).copied() else {
                    return;
                };
                let Some(bounded) = self.bounded.get(&def).copied() else {
                    return; // not a bounded reg
                };
                self.found_writes.insert(def);
                let computed = self.expr_bound(rhs, state, locals);
                let span = self.ast.expr_spans[rhs.0 as usize].clone();
                self.check_against_bound(computed, bounded, span, "write");
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
                let mut then_state = self.narrow_for_condition(cond, state);
                let mut then_locals = locals.clone();
                self.check_body(&then_body, &mut then_state, &mut then_locals);
                if let Some(else_body) = else_body {
                    let mut else_state = state.clone();
                    let mut else_locals = locals.clone();
                    self.check_body(&else_body, &mut else_state, &mut else_locals);
                }
            }
            Stmt::IfLet {
                then_body,
                else_body,
                ..
            } => {
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
                let mut loop_state = self.narrow_for_condition(cond, state);
                let mut loop_locals = locals.clone();
                self.check_body(&body, &mut loop_state, &mut loop_locals);
            }
            Stmt::WhileLet { body, .. } => {
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
            Stmt::Tick | Stmt::Break | Stmt::Return(_) => {}
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
                let (a_lo, a_hi) = self.expr_bound(*lhs, state, locals)?;
                let (b_lo, b_hi) = self.expr_bound(*rhs, state, locals)?;
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
                // error path needed.
                let (a_lo, a_hi) = self.expr_bound(*lhs, state, locals)?;
                let (b_lo, b_hi) = self.expr_bound(*rhs, state, locals)?;
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
                // same idiom `Add`/`Sub` above already use.
                let (a_lo, a_hi) = self.expr_bound(*lhs, state, locals)?;
                let (b_lo, b_hi) = self.expr_bound(*rhs, state, locals)?;
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
                // verifying every actual caller upholds it. Always
                // returns `None` for compositional purposes — no
                // return-bound propagation in v1 (a fn's own return
                // value carrying a provable bound a caller's
                // composition could use is a real, separable
                // follow-up, not bundled into this pass).
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
                if let Some(&fn_def) = self.res.expr_defs.get(&callee)
                    && let Some(params) = self.fn_params.get(&fn_def).cloned()
                {
                    for (param, arg) in params.iter().zip(&args) {
                        if param.bound.is_none() {
                            continue;
                        }
                        let param_def = def_of_name(self.res, &param.name);
                        let Some(bounded) = self.bounded.get(&param_def).copied() else {
                            continue;
                        };
                        let computed = self.expr_bound(*arg, state, locals);
                        let span = self.ast.expr_spans[arg.0 as usize].clone();
                        let context = format!("argument for parameter `{}`", param.name);
                        self.check_against_bound(computed, bounded, span, &context);
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
