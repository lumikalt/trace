//! SMT-backed shadow verification of `bounds.rs`'s own `where`-bound
//! inductions (DESIGN.md's "Toward a dependent/refinement type system
//! (SMT-backed, planned)", stage 1). Runs ALONGSIDE the existing
//! interval-arithmetic engine (`Checker::expr_bound`/`check_against_
//! bound`/`struct_field_bound`), never replacing it yet -- computes the
//! same obligation two independent ways and asserts agreement, the
//! discriminating instrument stage 1's own success criterion needs: a
//! before/after test-suite diff can only see a site's final pass/fail
//! verdict change, not a per-site disagreement that happens not to flip
//! any example's outcome.
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
//! difficulty, per the staged plan):
//! - A write's own RHS may reference any CURRENTLY-BOUNDED def (self or
//!   otherwise) and int/sized-int literals, composed via `Add`/`Sub`/
//!   `Mul` -- exactly `expr_bound`'s own v11 shapes, generalized from
//!   the scalar case's original self-reference-only restriction (see
//!   `translate_expr`'s own doc comment for why that generalization is
//!   sound and needed for mem/struct-field obligations, which have no
//!   privileged "self" def to begin with).
//! - An enclosing `if`/`else`'s own condition is a hypothesis when it's
//!   a bare `<bounded def> <op> <const>` (`Lt`/`Gt`/`Ge`/`Ne`), the same
//!   shape `narrow_for_condition`/`narrow_for_else` recognize -- v1
//!   doesn't yet attempt `<>`'s own edge-only narrowing rule or `Ne`'s
//!   commuted form, so a write that only type-checks BECAUSE one of
//!   those finer rules fired correctly reports `Skipped` here rather
//!   than a false disagreement.
//! - A mem write's RHS checks against the mem's own flat declared elem
//!   bound (v17) -- no different in kind from a scalar write, just a
//!   different `target` `BoundedDef`.
//! - A struct-typed write's RHS checks each bounded field's own value
//!   against its declared bound (v18), but ONLY when the field is named
//!   DIRECTLY in the `StructLit` -- the `..base` fallback (recursing
//!   into another reg/output/local's own same-named field,
//!   `Checker::struct_field_bound`'s own `Expr::StructLit`/`Ident`
//!   recursion) is a real, separate v1 scope cut for THIS shadow check,
//!   deferred rather than attempted, since it needs its own structural
//!   walk (mirroring that function's `DefKind`-gated provenance trace)
//!   rather than a plain value expression to translate.
//!
//! Anything outside this v1 shape (a `While`/`IfLet`/`WhileLet` body, a
//! param/return/relational obligation, a `..base`-sourced struct field)
//! is `Skipped`, not compared -- those are later stages' (or this one's
//! own follow-up) work, not claimed to already agree with by this one.
//!
//! **The provenance judgment this whole plan rests on (proven vs.
//! assumed -- see DESIGN.md) is already structurally enforced here, not
//! bolted on:** the only hypotheses this module can ever construct are
//! (a) some def's own declared bound, looked up ONLY for a `DefId`
//! already present in the caller-supplied `bounds_by_def` map -- itself
//! always either `Checker::bounded` (populated exclusively from a
//! CHECKED reg/out/param declaration) or a narrowing of it, never from
//! an unproven read -- and (b) a guard translated straight from the
//! surface AST at the exact site being checked. There is no code path
//! by which an `in` port, a mem read, or any other assumed value could
//! reach a hypothesis slot; `translate_expr`'s `Expr::Ident` arm returns
//! `None` for any def NOT present in `bounds_by_def`, which is the
//! fail-closed default `check_bound_obligation` turns into `Skipped`,
//! not a fabricated pass.

use std::collections::HashMap;

use z3::ast::{BV, Bool as Z3Bool};
use z3::{SatResult, Solver};

use crate::ast::{Ast, BinOp, Expr, ExprId};
use crate::lexer::Span;
use crate::resolve::{DefId, Resolution};

use super::{BoundedDef, RelationalBound};

/// One write site's own independently-computed verdict, compared
/// against the interval-arithmetic engine's existing verdict for the
/// SAME site.
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
/// data, not a panic, so a caller (today, only `bounds::check` itself)
/// can report it with full context.
#[derive(Debug, Clone)]
pub(super) struct Mismatch {
    pub span: Span,
    pub interval_says_in_bound: bool,
    pub smt: SmtVerdict,
}

/// The bit-width every hypothesis/composition is actually computed at
/// -- deliberately NOT any one def's own real storage width.
/// `expr_bound`'s own interval arithmetic tracks a composed value as a
/// full, non-wrapping `u64` range and ONLY THEN compares its `hi`
/// against `2^width` as an explicit, separate overflow check
/// (`check_against_bound`'s "could reach or exceed the declared width"
/// arm) -- it never reduces a composed value mod `2^width` mid-
/// computation. A narrow, width-bit BV sort here would silently do
/// exactly that reduction (bitvector arithmetic wraps), which would
/// hide the very overflow `check_against_bound` exists to catch --
/// confirmed empirically, not assumed: `multiplication_composed_bound_
/// exceeding_the_declared_width_is_rejected` (`reg i : [3] where i < 20
/// = 0`, `i := i * 3` under `i < 4`) declares a LOGICAL bound (20) that
/// exceeds its own 3-bit storage's max representable value (7) --
/// legal, since `i` can never actually reach 20 regardless -- and a
/// naive width-bit encoding computed `3 * 3 = 9 mod 8 = 1`, silently
/// wrapping into an in-bound-looking value where the real engine (and
/// real hardware) sees a genuine overflow. `CALC_WIDTH` matches the
/// interval engine's own `u64` arithmetic exactly, wide enough that
/// none of this module's realistic literals/bounds ever wrap.
const CALC_WIDTH: u32 = 64;

/// Every def this obligation may hypothesise about, mapped to its own
/// declared/narrowed `(lower, upper)` range -- typically the caller's
/// `state` snapshot (`Checker::shadow_walk_body`'s own narrowed map for
/// a scalar write, or the flat `self.bounded` for a mem/struct-field
/// write, which never narrows per-branch, mirroring `mem_bounds`/
/// `struct_field_bounds`'s own flat treatment).
type BoundsByDef = HashMap<DefId, (u64, u64)>;

/// Translate `id` into a `CALC_WIDTH`-bit bitvector term, ONLY when
/// every sub-expression is one of v1's recognized shapes (a reference
/// to some def PRESENT in `bounds_by_def`, an int/sized-int literal, or
/// `Add`/`Sub`/`Mul` composing two such terms) -- `None` for anything
/// else, this encoding's own "not in scope yet" signal, matching
/// `expr_bound`'s identical fail-closed default.
///
/// One entry var is created per distinct referenced def, cached in
/// `entries` for the whole obligation -- this is what makes two reads
/// of the SAME def within one RHS (`i + i`) refer to one consistent
/// symbolic value rather than two independently-quantified ones, and
/// what lets `check_bound_obligation` assert each one's own hypothesis
/// exactly once regardless of how many times it's read. Generalized
/// from this feature's original scalar-only "self-reference exactly"
/// restriction: a mem/struct-field write has no privileged "self" def
/// to begin with (an array/field has no prior value the way a scalar
/// reg/out's own induction reads one), so the SAME mechanism that
/// composes `i := i + 1` also composes `m[idx] := other_bounded_reg +
/// 1` for free, with no separate code path.
fn translate_expr(
    ast: &Ast,
    res: &Resolution,
    bounds_by_def: &BoundsByDef,
    entries: &mut HashMap<DefId, BV>,
    id: ExprId,
) -> Option<BV> {
    match ast.expr(id) {
        Expr::Int(v) => Some(BV::from_u64(*v, CALC_WIDTH)),
        Expr::SizedInt { value, .. } => Some(BV::from_u64(*value, CALC_WIDTH)),
        Expr::Ident(_) => {
            let &d = res.expr_defs.get(&id)?;
            if !bounds_by_def.contains_key(&d) {
                return None; // not a def this obligation can hypothesise about
            }
            Some(
                entries
                    .entry(d)
                    .or_insert_with(|| BV::new_const(format!("entry_{}", d.0), CALC_WIDTH))
                    .clone(),
            )
        }
        Expr::Binary { op, lhs, rhs } if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) => {
            let a = translate_expr(ast, res, bounds_by_def, entries, *lhs)?;
            let b = translate_expr(ast, res, bounds_by_def, entries, *rhs)?;
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
/// `<def> <op> <const>` comparison against a def PRESENT in
/// `bounds_by_def` -- the same operand order `narrow_for_condition`
/// recognizes (v1 doesn't yet attempt `<>`'s own edge-only rule or
/// `Ne`'s commuted form -- see module doc). `negate`: true for an
/// `else` branch, whose hypothesis is the condition's own negation.
/// Shares `entries` with `translate_expr` so a guard naming the SAME
/// def a write's RHS also reads (`if i < 8 { i := i + 1 }`) hypothesises
/// about the identical symbolic value, not a second, unconnected one.
fn translate_guard(
    ast: &Ast,
    res: &Resolution,
    bounds_by_def: &BoundsByDef,
    entries: &mut HashMap<DefId, BV>,
    id: ExprId,
    negate: bool,
) -> Option<Z3Bool> {
    let Expr::Binary { op, lhs, rhs } = ast.expr(id) else {
        return None;
    };
    let &d = res.expr_defs.get(lhs)?;
    if !bounds_by_def.contains_key(&d) {
        return None;
    }
    let entry = entries
        .entry(d)
        .or_insert_with(|| BV::new_const(format!("entry_{}", d.0), CALC_WIDTH))
        .clone();
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

/// One write site's own obligation: does `rhs`, evaluated under every
/// def in `bounds_by_def` (each hypothesised to hold its own declared/
/// narrowed range) and every one of `guards`/`guards_negated` holding,
/// always land in `[target.lower, target.upper)`, without reaching or
/// exceeding `target`'s own declared width? Mirrors `expr_bound`'s own
/// composition plus `check_against_bound`'s own three-way comparison
/// (logical upper, logical lower, width overflow), as ONE Z3 query
/// rather than interval arithmetic. `target` is whichever bound this
/// SITE is checked against -- a scalar reg/out/param's own declared
/// bound (in which case it's typically also a key of `bounds_by_def`,
/// so a self-reference in `rhs` resolves), a mem's flat elem bound, or
/// a struct field's own declared bound; this function doesn't care
/// which, since the obligation shape is identical either way.
///
/// A `Skipped` verdict here is not itself a disagreement -- it means
/// this v1 encoding couldn't translate the site at all (an unrecognized
/// RHS shape, or a guard finer than `Lt`/`Gt`/`Ge`/`Ne` at the exact
/// operand order above). The interval engine may still have proved such
/// a site via a rule this encoding doesn't implement yet (e.g. `<>`'s
/// edge-only narrowing) -- that's real, out-of-scope-for-now
/// under-approximation, not unsoundness in either direction.
pub(super) fn check_bound_obligation(
    ast: &Ast,
    res: &Resolution,
    bounds_by_def: &BoundsByDef,
    target: BoundedDef,
    guards: &[ExprId],
    guards_negated: &[ExprId],
    rhs: ExprId,
) -> SmtVerdict {
    let mut entries: HashMap<DefId, BV> = HashMap::new();
    let mut hyps = Vec::new();
    for &g in guards {
        match translate_guard(ast, res, bounds_by_def, &mut entries, g, false) {
            Some(h) => hyps.push(h),
            None => return SmtVerdict::Skipped("a guard uses a shape v1 doesn't translate yet"),
        }
    }
    for &g in guards_negated {
        match translate_guard(ast, res, bounds_by_def, &mut entries, g, true) {
            Some(h) => hyps.push(h),
            None => return SmtVerdict::Skipped("a guard uses a shape v1 doesn't translate yet"),
        }
    }
    let Some(new_value) = translate_expr(ast, res, bounds_by_def, &mut entries, rhs) else {
        return SmtVerdict::Skipped("rhs uses a shape v1 doesn't translate yet");
    };

    let solver = Solver::new();
    for (def, entry) in &entries {
        let (lower, upper) = bounds_by_def[def];
        solver.assert(entry.bvuge(BV::from_u64(lower, CALC_WIDTH)));
        solver.assert(entry.bvult(BV::from_u64(upper, CALC_WIDTH)));
    }
    for h in &hyps {
        solver.assert(h.clone());
    }
    // Load-bearing, not a stray sanity check: an UNSATISFIABLE
    // hypothesis set (an unreachable branch -- e.g. `else` of an
    // always-true unsigned `cnt >= 0`, whose negation `cnt < 0` is a
    // flat contradiction over a bitvector with no sign) would otherwise
    // make EVERY later claim vacuously UNSAT-on-its-negation, i.e.
    // "Proved" for free, regardless of what `rhs` actually computes --
    // exactly the hazard flagged before this module was written (an
    // inconsistent hypothesis set proves everything, silently). Found
    // empirically, not anticipated: `else_branch_of_an_always_true_
    // condition_stays_conservatively_unnarrowed` disagreed for exactly
    // this reason -- the interval engine's own `narrow_for_else`
    // deliberately does NOT exploit an unreachable branch's vacuous
    // truth (it leaves the state at its prior, unclamped range instead
    // of narrowing to empty; see that function's own doc comment), so a
    // real dead-code `else` there still gets checked against the FULL
    // prior bound and correctly REJECTS an out-of-range write, while
    // this encoding's raw hypotheses would otherwise vacuously accept
    // it. Reported as `Skipped`, not `Proved`: a genuinely unreachable
    // site is exactly the "not compared" case, not a claim this
    // encoding actually verified anything about.
    if solver.check() == SatResult::Unsat {
        return SmtVerdict::Skipped(
            "guards/entry hypotheses are jointly unsatisfiable (unreachable code)",
        );
    }
    let lo = BV::from_u64(target.lower, CALC_WIDTH);
    let hi = BV::from_u64(target.upper, CALC_WIDTH);
    // Prove `new_value` satisfies BOTH of `check_against_bound`'s own
    // checks -- the logical `[lower, upper)` range AND the "doesn't
    // reach or exceed 2^width" silent-wrap guard -- by refuting their
    // combined negation. `width_limit` mirrors `check_against_bound`'s
    // own `1u64.checked_shl(width).unwrap_or(u64::MAX)` formula exactly
    // (including its `>= 64`-width fallback), computed at `CALC_WIDTH`
    // rather than truncated to `target.width` itself, so a declared
    // logical bound that legitimately exceeds its own register's
    // storage width (see `CALC_WIDTH`'s own doc comment) is still
    // compared correctly instead of silently wrapping into a bogus
    // small constant.
    let width_limit = BV::from_u64(
        1u64.checked_shl(target.width as u32).unwrap_or(u64::MAX),
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
            let new_val = model.eval(&new_value, true).and_then(|v| v.as_u64());
            SmtVerdict::Disproved(format!(
                "written value {new_val:?}, outside [{}, {}) or >= 2^{}",
                target.lower, target.upper, target.width
            ))
        }
        SatResult::Unknown => SmtVerdict::Skipped("solver returned unknown"),
    }
}

/// One `invariant` fact's own inductive step, checked as a single
/// general "does ANY subset of the contributing rules jointly firing
/// preserve the combination's own declared range" query -- DESIGN.md's
/// own `fire_R` transition-relation plan, rather than `check_
/// relational_bound_induction`'s hand-rolled mask-enumeration loop
/// (`0..2^n`, capped at `n <= 2` purely because that loop doesn't
/// generalize past it cheaply). A boolean `fired` var per contributing
/// rule, each `ite`-selecting its own declared delta into the total,
/// covers every subset AT ONCE via ordinary solving -- no enumeration
/// needed, and (unlike the hand-rolled loop) nothing HERE is capped at
/// two rules structurally. `n > 2` is still treated as `Skipped` below
/// regardless, a deliberate stage-1 scope cut so this doesn't silently
/// prove MORE than the mechanism it's shadowing -- lifting that cap is
/// a real, separate capability increase for a later stage, not
/// something to slip in unannounced here.
///
/// `contributions` are taken as GIVEN (each rule's own already-computed
/// net delta and leading guards) rather than re-derived from the AST
/// here -- `Checker::walk_deltas`/`leading_guards` are the shared,
/// trusted front end both this function and `check_relational_bound_
/// induction` build on (matching how every other obligation in this
/// module trusts `Checker::bounded`/the AST itself as shared substrate,
/// not something to re-derive independently). What IS re-derived here,
/// independently: the subset/modular-shift reasoning built on top of
/// those deltas -- exactly the part with the straddle-avoidance
/// subtlety `shift_preserves`'s own doc comment describes, and the part
/// most worth a second, differently-shaped proof of.
pub(super) fn check_relational_obligation(
    ast: &Ast,
    res: &Resolution,
    rb: &RelationalBound,
    contributions: &[(i64, Vec<ExprId>)],
) -> SmtVerdict {
    if contributions.len() > 2 {
        return SmtVerdict::Skipped(
            "more than two contributing rules (a v1 cap this shadow check also honors, though \
             the fire_R encoding itself doesn't need it)",
        );
    }
    let combo = BV::new_const("combo", CALC_WIDTH);
    let lo = BV::from_u64(rb.lower, CALC_WIDTH);
    let hi = BV::from_u64(rb.upper, CALC_WIDTH);
    let modulus = BV::from_u64(rb.modulus, CALC_WIDTH);

    let solver = Solver::new();
    solver.assert(combo.bvuge(&lo));
    solver.assert(combo.bvult(&hi));

    let mut deltas: Vec<BV> = Vec::new();
    for (delta, guards) in contributions {
        let fired = Z3Bool::new_const(format!("fired_{}", deltas.len()));
        for &guard in guards {
            let Some((op, terms, k)) = super::recognize_comparison(ast, res, guard) else {
                continue; // an unrecognized shape narrows nothing, same as narrow_combo_range's own silent skip
            };
            if !super::same_terms(&terms, &rb.terms) {
                continue; // this guard's own combination isn't THIS invariant's -- narrows nothing, mirroring narrow_combo_range
            }
            let k = BV::from_u64(k, CALC_WIDTH);
            let constraint = match op {
                BinOp::Lt => combo.bvult(&k),
                BinOp::Le => combo.bvule(&k),
                BinOp::Gt => combo.bvugt(&k),
                BinOp::Ge => combo.bvuge(&k),
                BinOp::Eq => combo.eq(&k),
                BinOp::Ne => combo.eq(&k).not(),
                _ => continue,
            };
            solver.assert(fired.implies(&constraint));
        }
        // `delta as u64` reinterprets a negative i64's own two's-
        // complement bit pattern directly -- exactly the representation
        // `bvadd`'s own modular arithmetic needs to subtract correctly.
        let delta_bv = BV::from_u64(*delta as u64, CALC_WIDTH);
        let zero = BV::from_u64(0, CALC_WIDTH);
        deltas.push(fired.ite(&delta_bv, &zero));
    }
    let total_delta = deltas
        .into_iter()
        .reduce(|a, b| a.bvadd(&b))
        .unwrap_or_else(|| BV::from_u64(0, CALC_WIDTH));
    let combo_new = combo.bvadd(&total_delta);
    let reduced = combo_new.bvurem(&modulus);

    // Same vacuous-hypothesis guard as `check_bound_obligation` -- a
    // guard that only narrows to an empty range (jointly unsatisfiable
    // with another fired rule's own guard) must not be allowed to
    // vacuously "prove" the claim; see that function's own doc comment
    // for the full argument and the real disagreement that found it.
    if solver.check() == SatResult::Unsat {
        return SmtVerdict::Skipped("guard hypotheses are jointly unsatisfiable (unreachable)");
    }

    let violates = Z3Bool::or(&[reduced.bvult(&lo), reduced.bvuge(&hi)]);
    solver.assert(violates);
    match solver.check() {
        SatResult::Unsat => SmtVerdict::Proved,
        SatResult::Sat => {
            let model = solver.get_model().expect("a sat query always has a model");
            let combo_val = model.eval(&combo, true).and_then(|v| v.as_u64());
            let reduced_val = model.eval(&reduced, true).and_then(|v| v.as_u64());
            SmtVerdict::Disproved(format!(
                "combination {combo_val:?} shifts to {reduced_val:?} mod {}, outside [{}, {})",
                rb.modulus, rb.lower, rb.upper
            ))
        }
        SatResult::Unknown => SmtVerdict::Skipped("solver returned unknown"),
    }
}
