//! SMT-backed shadow verification of `bounds.rs`'s own scalar reg/out/
//! param `where`-bound induction (DESIGN.md's "Toward a dependent/
//! refinement type system (SMT-backed, planned)", stage 1). Runs
//! ALONGSIDE the existing interval-arithmetic engine
//! (`Checker::expr_bound`/`check_against_bound`), never replacing it
//! yet -- computes the same obligation two independent ways and asserts
//! agreement, the discriminating instrument stage 1's own success
//! criterion needs: a before/after test-suite diff can only see a
//! site's final pass/fail verdict change, not a per-site disagreement
//! that happens not to flip any example's outcome.
//!
//! `Bounds.site_ranges` (schedule.rs's own per-`ExprId` interval export,
//! consulted by `real_range`) is deliberately OUT of scope here -- it
//! stays on the existing interval walk through this stage, per
//! DESIGN.md's own stage-1 scope note; an SMT query answers "is this
//! provable," not "what's the tightest interval," and extracting one
//! would need its own optimization queries, better deferred to stage 4
//! where `real_range` retires anyway.
//!
//! v1 scope, matching the narrowest real slice first (ascending
//! difficulty, per the staged plan): a write's own RHS may reference
//! ONLY the def being written (self-reference) and int/sized-int
//! literals, composed via `Add`/`Sub`/`Mul` -- exactly `expr_bound`'s
//! own v11 shapes, one def at a time, no cross-def composition (e.g. a
//! call's return value, or a DIFFERENT bounded def read as an operand)
//! yet. An enclosing `if`/`else`'s own condition is a hypothesis when
//! it's a bare `<def> <op> <const>` (`Lt`/`Gt`/`Ge`/`Ne`), the same
//! shape `narrow_for_condition`/`narrow_for_else` recognize -- v1
//! doesn't yet attempt `<>`'s own edge-only narrowing rule or `Ne`'s
//! commuted form, so a write that only type-checks BECAUSE one of those
//! finer rules fired correctly reports `Skipped` here rather than a
//! false disagreement (see `check_write_obligation`'s own doc comment).
//!
//! Anything outside this v1 shape (a `While`/`IfLet`/`WhileLet` body, a
//! mem/struct-field/param/return/relational obligation) is `Skipped`,
//! not compared -- those are later stages' own work, not claimed to
//! already agree with by this one.
//!
//! **The provenance judgment this whole plan rests on (proven vs.
//! assumed -- see DESIGN.md) is already structurally enforced here, not
//! bolted on:** the only hypotheses this module can ever construct are
//! (a) `def`'s own declared bound, which is only ever looked up for a
//! `DefId` already present in `Checker::bounded` -- itself populated
//! exclusively from a CHECKED reg/out/param declaration, never from an
//! unproven read -- and (b) a guard translated straight from the
//! surface AST at the exact site being checked. There is no code path
//! by which an `in` port, a mem read, or any other assumed value could
//! reach a hypothesis slot; `translate_expr`'s `Expr::Ident` arm
//! requires the identifier to resolve to the SAME `def` being proven
//! (the write target's own self-reference) and returns `None` for any
//! other def, which is the fail-closed default `check_write_obligation`
//! turns into `Skipped`, not a fabricated pass.

use z3::ast::{BV, Bool as Z3Bool};
use z3::{SatResult, Solver};

use crate::ast::{Ast, BinOp, Expr, ExprId};
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};

use super::BoundedDef;

/// One scalar write site's own independently-computed verdict, compared
/// against `Checker::expr_bound`'s existing verdict for the SAME site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SmtVerdict {
    /// Proved in-bound: UNSAT on the negated claim.
    Proved,
    /// Proved OUT of bound: SAT, with a concrete counterexample.
    Disproved(String),
    /// This site's shape isn't in v1's scope yet (see module doc) --
    /// not compared, not a claim of agreement OR disagreement.
    Skipped(&'static str),
}

/// A real disagreement between the existing interval-arithmetic engine
/// and this SMT shadow check -- either one is unsound, or (far more
/// likely at this early stage) the SMT encoding has a bug. Surfaced as
/// data, not a panic, so a caller (today, only `tests/bounds.rs`'s own
/// shadow-check test) can report it with full context.
#[derive(Debug, Clone)]
pub(super) struct Mismatch {
    pub span: Span,
    pub interval_says_in_bound: bool,
    pub smt: SmtVerdict,
}

/// The bit-width every hypothesis/composition is actually computed at
/// -- deliberately NOT `bound.width` (the register's own real storage
/// width). `expr_bound`'s own interval arithmetic tracks a composed
/// value as a full, non-wrapping `u64` range and ONLY THEN compares its
/// `hi` against `2^width` as an explicit, separate overflow check
/// (`check_against_bound`'s "could reach or exceed the declared width"
/// arm) -- it never reduces a composed value mod `2^width` mid-
/// computation. A narrow, `width`-bit BV sort here would silently do
/// exactly that reduction (bitvector arithmetic wraps), which would
/// hide the very overflow `check_against_bound` exists to catch --
/// confirmed empirically, not assumed: `multiplication_composed_bound_
/// exceeding_the_declared_width_is_rejected` (`reg i : [3] where i < 20
/// = 0`, `i := i * 3` under `i < 4`) declares a LOGICAL bound (20) that
/// exceeds its own 3-bit storage's max representable value (7) --
/// legal, since `i` can never actually reach 20 regardless -- and a
/// naive `width`-bit encoding computed `3 * 3 = 9 mod 8 = 1`, silently
/// wrapping into an in-bound-looking value where the real engine (and
/// real hardware) sees a genuine overflow. `CALC_WIDTH` matches the
/// interval engine's own `u64` arithmetic exactly, wide enough that
/// none of this module's realistic literals/bounds ever wrap.
const CALC_WIDTH: u32 = 64;

/// Translate `id` into a `CALC_WIDTH`-bit bitvector term, ONLY when
/// every sub-expression is one of v1's recognized shapes (self-
/// reference to `def`, an int/sized-int literal, or `Add`/`Sub`/`Mul`
/// composing two such terms) -- `None` for anything else, this
/// encoding's own "not in scope yet" signal, matching `expr_bound`'s
/// identical fail-closed default.
fn translate_expr(ast: &Ast, res: &Resolution, def: DefId, entry: &BV, id: ExprId) -> Option<BV> {
    match ast.expr(id) {
        Expr::Int(v) => Some(BV::from_u64(*v, CALC_WIDTH)),
        Expr::SizedInt { value, .. } => Some(BV::from_u64(*value, CALC_WIDTH)),
        Expr::Ident(_) => {
            let d = res.expr_defs.get(&id)?;
            if *d == def {
                Some(entry.clone())
            } else {
                None // v1: self-reference only, no cross-def composition yet
            }
        }
        Expr::Binary { op, lhs, rhs } if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) => {
            let a = translate_expr(ast, res, def, entry, *lhs)?;
            let b = translate_expr(ast, res, def, entry, *rhs)?;
            Some(match op {
                BinOp::Add => a.bvadd(&b),
                BinOp::Sub => a.bvsub(&b),
                BinOp::Mul => a.bvmul(&b),
                _ => unreachable!("guarded by the match arm's own pattern"),
            })
        }
        _ => None,
    }
}

/// Translate a guard condition into a hypothesis, ONLY when it's a bare
/// `<def> <op> <const>` comparison against `def` itself -- the same
/// operand order `narrow_for_condition` recognizes (v1 doesn't yet
/// attempt `<>`'s own edge-only rule or `Ne`'s commuted form -- see
/// module doc). `negate`: true for an `else` branch, whose hypothesis is
/// the condition's own negation.
fn translate_guard(
    ast: &Ast,
    res: &Resolution,
    def: DefId,
    entry: &BV,
    id: ExprId,
    negate: bool,
) -> Option<Z3Bool> {
    let Expr::Binary { op, lhs, rhs } = ast.expr(id) else {
        return None;
    };
    let &d = res.expr_defs.get(lhs)?;
    if d != def {
        return None;
    }
    let Expr::Int(k) = ast.expr(*rhs) else {
        return None;
    };
    let k = BV::from_u64(*k, CALC_WIDTH);
    let pos = match op {
        BinOp::Lt => entry.bvult(&k),
        BinOp::Gt => entry.bvugt(&k),
        BinOp::Ge => entry.bvuge(&k),
        BinOp::Ne => entry.eq(&k).not(),
        _ => return None,
    };
    Some(if negate { pos.not() } else { pos })
}

/// One write site's own obligation: does `rhs`, evaluated with `def`'s
/// value at `entry` (hypothesised in-bound) and every one of `guards`
/// holding, always land in `[bound.lower, bound.upper)`? Mirrors
/// `expr_bound`'s own composition plus `check_against_bound`'s own
/// comparison, as ONE Z3 query rather than interval arithmetic.
///
/// A `Skipped` verdict here is not itself a disagreement -- it means
/// this v1 encoding couldn't translate the site at all (an unrecognized
/// RHS shape, or a guard finer than `Lt`/`Gt`/`Ge`/`Ne` at the exact
/// operand order above). The interval engine may still have proved such
/// a site via a rule this encoding doesn't implement yet (e.g. `<>`'s
/// edge-only narrowing) -- that's real, out-of-scope-for-now
/// under-approximation, not unsoundness in either direction.
pub(super) fn check_write_obligation(
    ast: &Ast,
    res: &Resolution,
    def: DefId,
    bound: BoundedDef,
    guards: &[ExprId],
    guards_negated: &[ExprId],
    rhs: ExprId,
) -> SmtVerdict {
    let entry = BV::new_const("entry", CALC_WIDTH);

    let mut hyps = Vec::new();
    for &g in guards {
        match translate_guard(ast, res, def, &entry, g, false) {
            Some(h) => hyps.push(h),
            None => return SmtVerdict::Skipped("a guard uses a shape v1 doesn't translate yet"),
        }
    }
    for &g in guards_negated {
        match translate_guard(ast, res, def, &entry, g, true) {
            Some(h) => hyps.push(h),
            None => return SmtVerdict::Skipped("a guard uses a shape v1 doesn't translate yet"),
        }
    }
    let Some(new_value) = translate_expr(ast, res, def, &entry, rhs) else {
        return SmtVerdict::Skipped("rhs uses a shape v1 doesn't translate yet");
    };

    let solver = Solver::new();
    let lo = BV::from_u64(bound.lower, CALC_WIDTH);
    let hi = BV::from_u64(bound.upper, CALC_WIDTH);
    solver.assert(entry.bvuge(&lo));
    solver.assert(entry.bvult(&hi));
    for h in &hyps {
        solver.assert(h.clone());
    }
    // Prove `new_value` satisfies BOTH of `check_against_bound`'s own
    // checks -- the logical `[lower, upper)` range AND the "doesn't
    // reach or exceed 2^width" silent-wrap guard -- by refuting their
    // combined negation. `width_limit` mirrors `check_against_bound`'s
    // own `1u64.checked_shl(width).unwrap_or(u64::MAX)` formula exactly
    // (including its `>= 64`-width fallback), computed at `CALC_WIDTH`
    // rather than truncated to `bound.width` itself, so a declared
    // logical bound that legitimately exceeds its own register's
    // storage width (see `CALC_WIDTH`'s own doc comment) is still
    // compared correctly instead of silently wrapping into a bogus
    // small constant.
    let width_limit = BV::from_u64(
        1u64.checked_shl(bound.width as u32).unwrap_or(u64::MAX),
        CALC_WIDTH,
    );
    let violates = Z3Bool::or(&[
        new_value.bvult(&lo),
        new_value.bvuge(&hi),
        new_value.bvuge(&width_limit),
    ]);
    solver.assert(violates);
    match solver.check() {
        SatResult::Unsat => SmtVerdict::Proved,
        SatResult::Sat => {
            let model = solver.get_model().expect("a sat query always has a model");
            let entry_val = model.eval(&entry, true).and_then(|v| v.as_u64());
            let new_val = model.eval(&new_value, true).and_then(|v| v.as_u64());
            SmtVerdict::Disproved(format!(
                "entry {entry_val:?} -> written value {new_val:?}, outside [{}, {}) or >= 2^{}",
                bound.lower, bound.upper, bound.width
            ))
        }
        SatResult::Unknown => SmtVerdict::Skipped("solver returned unknown"),
    }
}
