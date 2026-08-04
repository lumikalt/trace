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
//! - `reg` only. An `in` has no internal write site at all — a bound on
//!   it would be a TRUSTED external contract (this module's whole point
//!   is proof, not trust), a different feature. An `out` is register-
//!   backed and provable in principle, but has no motivating example
//!   yet — parser.rs rejects `where` on either.
//! - A single strict upper bound against a compile-time constant only:
//!   no `<=`, no lower bounds (a `bits[N]` value is unsigned — 0 is
//!   always the implicit floor), no multi-variable or otherwise
//!   arbitrary bound expressions.
//! - RHS composition supports only bare idents/literals and `Add` — the
//!   motivating pattern (`i := i + 1` under a guard) needs nothing else.
//!   `Sub`/`Mul`/anything else composes to "unknown," which fails the
//!   write-site check closed (a compile error asking for an explicit
//!   restructure), never silently "assumed in range."
//! - No interaction with `schedule.rs`'s v3 banking argument — that
//!   argument's soundness rests on a modular (not real-integer) fact a
//!   proven bound doesn't slot into, and no example needs the
//!   combination.
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
//! Every read of a `reg` within a rule/fn body sees the value from the
//! START of the cycle (DESIGN.md: "a register read... sees the OLD
//! (pre-edge) value... the same rule every other register read in this
//! language follows"), never a value written earlier in the SAME body
//! by an earlier statement. So a bounded reg's tracked bound is FROZEN
//! at its declared limit for the whole walk of one item — narrowed only
//! by an enclosing `if <bounded-reg> < <const>` guard's own branch, and
//! reverted once that branch ends. Writes are CHECKED against it, never
//! allowed to update it. Only `Stmt::Let` locals get real (blocking)
//! forward-flow tracking, needed for the realistic `let next = i + 1; i
//! := next` shape.
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

use crate::ast::{Ast, BinOp, Expr, ExprId, Item, ItemId, Stmt, StmtId};
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

/// Every `reg` with a proven `where` bound, and the upper bound (K in
/// `< K`) itself. Presence in this map, regardless of the value, means
/// the bound was successfully proven across the whole program — a reg
/// with no `where` clause is simply absent. Consumed by `schedule.rs`'s
/// own third disjointness argument.
#[derive(Debug, Default)]
pub struct Bounds {
    pub upper: HashMap<DefId, u64>,
}

/// One bounded reg's own declared facts, collected once up front.
struct BoundedReg {
    limit: u64,
    width: u64,
}

pub fn check(ast: &Ast, res: &Resolution, fx: &Effects, ty: &Types) -> (Bounds, Vec<BoundsError>) {
    let mut checker = Checker {
        ast,
        res,
        fx,
        ty,
        bounded: HashMap::new(),
        found_writes: HashSet::new(),
        errors: Vec::new(),
    };
    checker.collect_bounded_regs();
    let bodied = checker.collect_bodied_items();
    for id in &bodied {
        checker.check_item(*id);
    }
    checker.check_write_site_exhaustiveness();
    let bounds = Bounds {
        upper: checker
            .bounded
            .iter()
            .map(|(def, b)| (*def, b.limit))
            .collect(),
    };
    (bounds, checker.errors)
}

struct Checker<'a> {
    ast: &'a Ast,
    res: &'a Resolution,
    fx: &'a Effects,
    ty: &'a Types,
    bounded: HashMap<DefId, BoundedReg>,
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

    /// Every reg with a `where` bound, keyed by its own `DefId`. The
    /// bound's shape (`Binary { Lt, Ident(self), <const> }`) is
    /// guaranteed by construction: the parser only ever builds a
    /// `where` clause this way (hard-requires the literal `<` token),
    /// and `resolve.rs` already requires the LHS self-reference this
    /// same reg — so nothing left to validate here but extracting the
    /// limit and the reg's own declared width.
    fn collect_bounded_regs(&mut self) {
        let mut stack: Vec<ItemId> = self.ast.roots.clone();
        while let Some(id) = stack.pop() {
            match self.ast.item(id) {
                Item::Module { items, .. } => stack.extend(items.iter().copied()),
                Item::Reg {
                    bound: Some(bound), ..
                } => {
                    let Expr::Binary { rhs, .. } = self.ast.expr(*bound) else {
                        continue;
                    };
                    let Some(limit) = const_fold(self.ast, *rhs) else {
                        continue; // types.rs already reported this
                    };
                    let Some(def) = self.res.item_defs.get(&id).copied() else {
                        continue;
                    };
                    let Some(width) = base_width(self.ty, def) else {
                        // A concretely-known `bits[N]` width is exactly
                        // what every check below needs to clamp a
                        // composed bound against — an unknown width
                        // (e.g. a non-elaboration-constant size
                        // expression, silently `Ty::Bits(Width::
                        // Unknown)` per `types/eval.rs`, with no error
                        // of its own) must not let the whole `where`
                        // clause go unchecked with zero diagnostic.
                        let span = self.ast.expr_spans[bound.0 as usize].clone();
                        self.error(
                            span,
                            "a `where` bound needs a reg with a concretely-known `bits[N]` \
                             width to check against (v0 restriction)"
                                .to_string(),
                        );
                        continue;
                    };
                    self.bounded.insert(def, BoundedReg { limit, width });
                }
                _ => {}
            }
        }
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
        let mut state: HashMap<DefId, u64> =
            self.bounded.iter().map(|(d, b)| (*d, b.limit)).collect();
        let mut locals: HashMap<DefId, Option<u64>> = HashMap::new();
        self.check_body(&body, &mut state, &mut locals);
    }

    fn check_body(
        &mut self,
        body: &[StmtId],
        state: &mut HashMap<DefId, u64>,
        locals: &mut HashMap<DefId, Option<u64>>,
    ) {
        for stmt in body {
            self.check_stmt(*stmt, state, locals);
        }
    }

    fn check_stmt(
        &mut self,
        id: StmtId,
        state: &mut HashMap<DefId, u64>,
        locals: &mut HashMap<DefId, Option<u64>>,
    ) {
        match self.ast.stmt(id).clone() {
            Stmt::Assign { lhs, rhs } => {
                let Expr::Ident(_) = self.ast.expr(lhs) else {
                    return; // a mem/struct-field write, irrelevant here
                };
                let Some(def) = self.res.expr_defs.get(&lhs).copied() else {
                    return;
                };
                let Some(bounded) = self.bounded.get(&def) else {
                    return; // not a bounded reg
                };
                let limit = bounded.limit;
                let width = bounded.width;
                self.found_writes.insert(def);
                let computed = self.expr_bound(rhs, state, locals);
                let span = self.ast.expr_spans[rhs.0 as usize].clone();
                match computed {
                    None => self.error(
                        span,
                        format!(
                            "cannot verify this write stays within the declared bound `< \
                             {limit}` (unsupported expression shape — only a bare bounded \
                             reg/local, a literal, or their sum is recognized)"
                        ),
                    ),
                    Some(c) if c > 1u64.checked_shl(width as u32).unwrap_or(u64::MAX) => {
                        self.error(
                            span,
                            format!(
                                "this write's computed value could reach or exceed the reg's \
                                 own declared width ([{width}]), which would silently wrap and \
                                 invalidate the declared bound `< {limit}`"
                            ),
                        );
                    }
                    Some(c) if c > limit => self.error(
                        span,
                        format!(
                            "cannot verify this write stays within the declared bound `< \
                             {limit}` (computed value could reach {})",
                            c.saturating_sub(1)
                        ),
                    ),
                    Some(_) => {}
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
            Stmt::Expr(_) | Stmt::Tick | Stmt::Break | Stmt::Return(_) => {}
        }
    }

    /// `if`/`while <bounded reg> < <const>` narrows that reg's tracked
    /// bound to `min(current, const)` for the guarded body ONLY — the
    /// caller clones `state` first, so this never mutates the outer
    /// map. Any other condition shape (or a reg with no PRIOR bound at
    /// all) is a no-op: narrowing only tightens an already-bounded
    /// fact, never invents one.
    fn narrow_for_condition(
        &self,
        cond: ExprId,
        state: &HashMap<DefId, u64>,
    ) -> HashMap<DefId, u64> {
        let mut narrowed = state.clone();
        if let Expr::Binary {
            op: BinOp::Lt,
            lhs,
            rhs,
        } = self.ast.expr(cond)
            && matches!(self.ast.expr(*lhs), Expr::Ident(_))
            && let Some(def) = self.res.expr_defs.get(lhs)
            && let Some(k) = const_fold(self.ast, *rhs)
            && let Some(existing) = narrowed.get(def)
        {
            narrowed.insert(*def, k.min(*existing));
        }
        narrowed
    }

    /// The value range an expression is provably confined to, as an
    /// exclusive upper bound (`Some(K)` = value is in `[0, K)`), or
    /// `None` if this pass can't establish one. Only a bare bounded
    /// reg/local reference, a literal, or `Add` of two such compose —
    /// see this module's own doc comment for why everything else
    /// (`Sub`, `Mul`, a call, ...) is deliberately left unsupported.
    fn expr_bound(
        &self,
        id: ExprId,
        state: &HashMap<DefId, u64>,
        locals: &HashMap<DefId, Option<u64>>,
    ) -> Option<u64> {
        match self.ast.expr(id) {
            Expr::Int(v) => v.checked_add(1),
            Expr::SizedInt { value, .. } => value.checked_add(1),
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
                let a = self.expr_bound(*lhs, state, locals)?;
                let b = self.expr_bound(*rhs, state, locals)?;
                a.checked_add(b)?.checked_sub(1)
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
fn base_width(ty: &Types, def: DefId) -> Option<u64> {
    match ty.state_tys.get(&def) {
        Some(Ty::Bits(Width::Known(w))) => Some(*w),
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
