//! Fifo-specific naming/detection: a depth-1 fifo is one data register
//! plus one valid bit (`fifo_valid_name`/`fifo_data_name`); a depth-N
//! (N>1) fifo is N data-slot registers plus `head`/`count` pointer
//! registers (`fifo_slot_name`/`fifo_head_name`/`fifo_count_name`) —
//! see module.rs's `Item::Fifo` arm and state-transition emission for
//! the storage/update logic itself, hand-verified against a real
//! firtool+Icarus simulation of a depth-3 circuit before being ported
//! here (non-power-of-2 depth, to stress head/tail wraparound). `Enq`/
//! `Deq` recognition (`is_fifo_op`, `Emitter::fifo_op`/`fifo_op_stmt`)
//! and the guard condition each op contributes (`fifo_guard_cond`,
//! `rule_fifo_guard_cond`) are depth-generic; both fork internally on
//! `depth == 1` to keep that (by far the most common) case's emitted
//! FIRRTL byte-identical to before depth support existed.
//!
//! A rule MAY both `Enq` and `Deq` the SAME fifo — a "pass-through":
//! this cycle's `Deq` returns the fifo's current (pre-edge) data, same
//! as any other read of a register, while the `Enq`'s value becomes the
//! new data for the NEXT cycle. At depth 1, `valid` stays 1 throughout
//! rather than toggling 0 then back to 1; at depth N, `count` stays
//! unchanged (the Enq's +1 and the Deq's -1 net to zero — verified by
//! hand-tracing the depth-3 circuit above, which first caught a real
//! bug where composing those two updates from the SAME pre-edge `count`
//! independently silently dropped the Enq's credit instead of netting
//! it). Either way the combined precondition is just "there is
//! something to dequeue" (`valid`, or `count > 0`) — NOT the AND of
//! each op's own individual guard (which would always be false, since
//! Deq needs non-empty and Enq needs non-full and a depth-1 fifo can't
//! be both at once). `rule_fifo_guard_cond` computes this per-fifo,
//! folding an Enq+Deq pair of the same fifo into a single term;
//! `compile_guard` (writes.rs) uses it instead of a per-statement
//! `fifo_guard_cond` call for exactly this reason.

use super::Emitter;
use super::writes::rule_body;
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Stmt, StmtId};
use crate::resolve::{DefKind, Resolution};
use crate::types::Ty;

pub(crate) fn fifo_valid_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_valid")
}

pub(crate) fn fifo_data_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_data")
}

pub(crate) fn fifo_head_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_head")
}

pub(crate) fn fifo_count_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_count")
}

pub(crate) fn fifo_slot_name(fifo: &str, i: u64) -> String {
    format!("__fifo_{fifo}_slot{i}")
}

/// The combinational expression a `Deq[]` read compiles to: the single
/// data register at depth 1, or (at depth N) a mux chain over the N
/// slot registers selected by `head` — reading the CURRENT (pre-edge)
/// register contents, same as any other register read, so a combined
/// Enq+Deq on the same fifo naturally reads the old value while the
/// Enq's own connect (module.rs) lands the new one for next cycle.
pub(crate) fn fifo_deq_read_expr(fifo: &str, depth: u64) -> String {
    if depth == 1 {
        return fifo_data_name(fifo);
    }
    let head = fifo_head_name(fifo);
    let head_w = super::writes::clog2(depth).max(1);
    let mut expr = fifo_slot_name(fifo, depth - 1);
    for i in (0..depth - 1).rev() {
        let slot = fifo_slot_name(fifo, i);
        expr = format!("mux(eq({head}, UInt<{head_w}>({i})), {slot}, {expr})");
    }
    expr
}

/// `Deq[]` succeeds iff the fifo is non-empty; `Enq[x]` succeeds iff it
/// is non-full. At depth 1 this is exactly the old single-valid-bit
/// check (`not(valid)`/`valid`); at depth N it's a `count` comparison.
pub(crate) fn fifo_guard_cond(fifo: &str, is_enq: bool, depth: u64) -> String {
    if depth == 1 {
        let valid = fifo_valid_name(fifo);
        if is_enq {
            format!("not({valid})")
        } else {
            valid
        }
    } else {
        let count = fifo_count_name(fifo);
        if is_enq {
            format!("lt({count}, UInt<{cw}>({depth}))", cw = count_width(depth))
        } else {
            format!("gt({count}, UInt<{cw}>(0))", cw = count_width(depth))
        }
    }
}

/// `count`'s own width: it ranges `0..=depth` (`depth + 1` distinct
/// values), NOT `0..depth` — sized for `depth` alone would be one bit
/// too narrow to ever represent a full buffer.
pub(crate) fn count_width(depth: u64) -> u64 {
    super::writes::clog2(depth + 1).max(1)
}

/// The guard term ONE fifo contributes to its rule, given whether the
/// rule enqueues it, dequeues it, or (the pass-through case) both: just
/// "non-empty" (`valid`, or `count > 0`) when both are present — see
/// this module's own doc comment for why that's the correct combined
/// precondition, not `fifo_guard_cond`'s individual per-op guards
/// AND'ed together.
pub(crate) fn rule_fifo_guard_cond(fifo: &str, saw_enq: bool, saw_deq: bool, depth: u64) -> String {
    if saw_enq && saw_deq {
        fifo_guard_cond(fifo, false, depth)
    } else {
        fifo_guard_cond(fifo, saw_enq, depth)
    }
}

pub(crate) fn is_fifo_op(ast: &Ast, res: &Resolution, expr: ExprId) -> bool {
    let Expr::Bracket { callee, .. } = ast.expr(expr) else {
        return false;
    };
    let Expr::Field { base, name } = ast.expr(*callee) else {
        return false;
    };
    matches!(name.as_str(), "Enq" | "Deq")
        && res
            .expr_defs
            .get(base)
            .is_some_and(|d| res.def(*d).kind == DefKind::Fifo)
}

/// Every fifo op (`Enq`/`Deq`) reachable anywhere within `id`, recursing
/// through arbitrary subexpressions — the fifo-op sibling of calls.rs's
/// `collect_calls`, used the identical way by `check_fifo_op_positions`
/// (checks.rs) to reject a fifo op that isn't sitting in one of the
/// three positions `fifo_op`/`fifo_op_stmt` actually recognize (a bare
/// statement, the whole RHS of `:=`, or a `let` init). Same shape as
/// `collect_calls`: a found op is recorded, then its own `callee`/`args`
/// are still walked too (an `Enq`'s value argument can't itself contain
/// another fifo op in practice, but walking it anyway costs nothing and
/// stays consistent with `collect_calls`'s own arg traversal).
pub(crate) fn collect_fifo_ops(ast: &Ast, res: &Resolution, id: ExprId, out: &mut Vec<ExprId>) {
    match ast.expr(id) {
        Expr::Ident(_) | Expr::Int(_) | Expr::SizedInt { .. } | Expr::Wildcard => {}
        Expr::Unary { operand, .. } => collect_fifo_ops(ast, res, *operand, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_fifo_ops(ast, res, *lhs, out);
            collect_fifo_ops(ast, res, *rhs, out);
        }
        Expr::Guard(inner) | Expr::Spawn(inner) | Expr::Optional(inner) => {
            collect_fifo_ops(ast, res, *inner, out);
        }
        Expr::Field { base, .. } => collect_fifo_ops(ast, res, *base, out),
        Expr::Call { callee, args } => {
            collect_fifo_ops(ast, res, *callee, out);
            for a in args {
                collect_fifo_ops(ast, res, *a, out);
            }
        }
        Expr::Bracket { callee, args } => {
            if is_fifo_op(ast, res, id) {
                out.push(id);
            }
            collect_fifo_ops(ast, res, *callee, out);
            for a in args {
                collect_fifo_ops(ast, res, *a, out);
            }
        }
        Expr::ListLit(items) => {
            for item in items {
                collect_fifo_ops(ast, res, *item, out);
            }
        }
        Expr::Range { lo, hi } => {
            if let Some(lo) = lo {
                collect_fifo_ops(ast, res, *lo, out);
            }
            if let Some(hi) = hi {
                collect_fifo_ops(ast, res, *hi, out);
            }
        }
        // Recursing here (rather than skipping `Or`) is what lets
        // `check_fifo_op_positions` (checks.rs) catch a MISPLACED `or`
        // (nested in if/while, or inside a larger expression): its
        // alternatives are only exempted from that scan when the whole
        // `Or` sits directly in one of `or_chains`' three recognized
        // top-level positions — everywhere else, they surface here and
        // get flagged like any other out-of-position fifo op.
        Expr::Or(alts) => {
            for alt in alts {
                collect_fifo_ops(ast, res, *alt, out);
            }
        }
        Expr::StructLit { name, fields, base } => {
            collect_fifo_ops(ast, res, *name, out);
            for (_, value) in fields {
                collect_fifo_ops(ast, res, *value, out);
            }
            if let Some(base) = base {
                collect_fifo_ops(ast, res, *base, out);
            }
        }
        Expr::OptionTy(_) | Expr::Absent => {}
    }
}

/// Splits `A or B or C`'s flat alt list into its fifo-op alternatives and
/// an optional trailing default (the last element, when it ISN'T itself
/// fifo-op-shaped) — the ONE place this classification happens; both
/// `or_chains` (statement-level enumeration, for checks.rs/`rule_fifo_
/// ops`) and `Emitter::compile_or` (expr.rs, expression-level value
/// compilation) read it from here so the two can't drift on what counts
/// as a default.
pub(crate) fn classify_or_alts(
    ast: &Ast,
    res: &Resolution,
    alts: &[ExprId],
) -> (Vec<ExprId>, Option<ExprId>) {
    let mut alts = alts.to_vec();
    let default = match alts.last() {
        Some(&last) if !is_fifo_op(ast, res, last) => {
            alts.pop();
            Some(last)
        }
        _ => None,
    };
    (alts, default)
}

/// One `A or B or C` chain sitting in an ALLOWED top-level rule position
/// (a bare statement, the whole RHS of `:=`, or a `let` init) — v0
/// deliberately does NOT recurse into `if`/`while`: an `or` found there
/// is instead rejected by checks.rs's `check_fifo_op_positions` as a
/// fifo op "outside an allowed position" (its alternatives, unlike a
/// top-level chain's, are never exempted from that scan — see checks.rs)
/// — same "not yet, not silently" v0 restriction every other position
/// check in this emitter already applies. `alts`: every alternative
/// EXCEPT a trailing default (shape-checked separately, `check_or_
/// shape`, checks.rs). `default`: `Some(expr)` when the chain ends in an
/// infallible fallback value, per Verse's own `08_failure` semantics —
/// confirmed by hand-lowering both forms to raw FIRRTL and simulating
/// via Icarus before this was written (a default-tailed chain
/// contributes NO guard term at all; a chain with none stays fallible
/// and its alternatives' combined occupancy becomes part of the rule's
/// own guard, ORed together — see `Emitter::compile_guard`, writes.rs).
pub(crate) struct OrChain {
    pub(crate) stmt: StmtId,
    pub(crate) alts: Vec<ExprId>,
    pub(crate) default: Option<ExprId>,
}

pub(crate) fn or_chains(ast: &Ast, res: &Resolution, stmts: &[StmtId]) -> Vec<OrChain> {
    let mut out = Vec::new();
    for stmt in stmts {
        let root = match ast.stmt(*stmt).clone() {
            Stmt::Expr(e) => Some(e),
            Stmt::Assign { rhs, .. } => Some(rhs),
            Stmt::Let { init, .. } => Some(init),
            _ => None,
        };
        let Some(root) = root else { continue };
        let Expr::Or(elems) = ast.expr(root).clone() else {
            continue;
        };
        let (alts, default) = classify_or_alts(ast, res, &elems);
        out.push(OrChain {
            stmt: *stmt,
            alts,
            default,
        });
    }
    out
}

pub(crate) fn contains_fifo_op(ast: &Ast, res: &Resolution, stmt: StmtId) -> bool {
    match ast.stmt(stmt) {
        Stmt::Expr(e) => is_fifo_op(ast, res, *e),
        Stmt::Assign { rhs, .. } | Stmt::Let { init: rhs, .. } => is_fifo_op(ast, res, *rhs),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            then_body.iter().any(|s| contains_fifo_op(ast, res, *s))
                || else_body
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|s| contains_fifo_op(ast, res, *s)))
        }
        Stmt::While { body, .. } => body.iter().any(|s| contains_fifo_op(ast, res, *s)),
        _ => false,
    }
}

impl<'a> Emitter<'a> {
    /// `expr` is `<fifo>.Enq[x]` or `<fifo>.Deq[]` -> the fifo's name and
    /// depth, whether it is `Enq`, and (for `Enq`) the value argument.
    pub(crate) fn fifo_op(&self, expr: ExprId) -> Option<(String, u64, bool, Option<ExprId>)> {
        let Expr::Bracket { callee, args } = self.ast.expr(expr) else {
            return None;
        };
        let Expr::Field { base, name } = self.ast.expr(*callee) else {
            return None;
        };
        let def = *self.res.expr_defs.get(base)?;
        if self.res.def(def).kind != DefKind::Fifo {
            return None;
        }
        let fifo = self.res.def(def).name.clone();
        let depth = match self.state_width(def) {
            Some(Ty::Fifo { depth, .. }) => depth,
            _ => 1,
        };
        match name.as_str() {
            "Deq" => Some((fifo, depth, false, None)),
            "Enq" => Some((fifo, depth, true, args.first().copied())),
            _ => None,
        }
    }

    /// The fifo op directly reachable from `stmt`, if any — the shapes
    /// DESIGN.md's examples use (`x := f.Deq[]`, a bare `f.Enq[x]`
    /// statement) plus `let x = f.Deq[]`, an equally legal binding form
    /// (see DESIGN.md's "Locals") that must resolve to the identical fifo
    /// op its `:=` counterpart would.
    pub(crate) fn fifo_op_stmt(&self, stmt: StmtId) -> Option<(String, u64, bool, Option<ExprId>)> {
        let expr = match self.ast.stmt(stmt) {
            Stmt::Expr(e) => *e,
            Stmt::Assign { rhs, .. } => *rhs,
            Stmt::Let { init, .. } => *init,
            _ => return None,
        };
        self.fifo_op(expr)
    }

    /// Every fifo op a rule performs, whether directly at its own top
    /// level or reached through exactly one bare-statement/`:=`-RHS call
    /// to a `sig.fails`-foldable callee (the same restriction `check_
    /// fails_is_foldable_guard` already enforces on the callee side — a
    /// callee this permissive to call can only have TOP-LEVEL fifo ops
    /// in its own body, never nested deeper). `stmt` is always the
    /// RULE's own originating statement (the call site, for a via-callee
    /// op), so callers can `set_pos`/dedupe/group by it exactly like a
    /// direct op.
    ///
    /// The single point every fifo-touch question in this module routes
    /// through: module.rs's per-fifo state-transition emission,
    /// `compile_guard`'s pass-through precondition, and `check_fifo_op_
    /// counts`'s double-op collision check all call this, rather than
    /// each independently re-scanning `rule_body` — three independent
    /// scans reaching through the call boundary would drift out of
    /// agreement with each other, exactly the silent-miscompile class
    /// this whole area of the compiler exists to close off.
    pub(crate) fn rule_fifo_ops(&self, rule: ItemId) -> Vec<RuleFifoOp> {
        let body = rule_body(self.ast, rule);
        let mut out = Vec::new();
        for stmt in &body {
            if let Some((fifo, depth, is_enq, value)) = self.fifo_op_stmt(*stmt) {
                out.push(RuleFifoOp {
                    stmt: *stmt,
                    fifo,
                    depth,
                    is_enq,
                    value,
                    callee_ctx: None,
                    select: None,
                });
                continue;
            }
            let call_expr = match self.ast.stmt(*stmt) {
                Stmt::Expr(e) if matches!(self.ast.expr(*e), Expr::Call { .. }) => Some(*e),
                Stmt::Assign { rhs, .. } if matches!(self.ast.expr(*rhs), Expr::Call { .. }) => {
                    Some(*rhs)
                }
                _ => None,
            };
            let Some(call_expr) = call_expr else { continue };
            let Expr::Call { callee, args } = self.ast.expr(call_expr).clone() else {
                continue;
            };
            let Some(&def) = self.res.expr_defs.get(&callee) else {
                continue;
            };
            if !matches!(self.res.def(def).kind, DefKind::Fn | DefKind::Impl) {
                continue;
            }
            let Some(fn_item) = self
                .res
                .item_defs
                .iter()
                .find(|(_, d)| **d == def)
                .map(|(item, _)| *item)
            else {
                continue;
            };
            if !self.fx.sigs.get(&fn_item).is_some_and(|s| s.fails) {
                continue;
            }
            let Item::Fn {
                body: callee_body, ..
            } = self.ast.item(fn_item).clone()
            else {
                continue;
            };
            for cstmt in &callee_body {
                if let Some((fifo, depth, is_enq, value)) = self.fifo_op_stmt(*cstmt) {
                    out.push(RuleFifoOp {
                        stmt: *stmt,
                        fifo,
                        depth,
                        is_enq,
                        value,
                        callee_ctx: Some((fn_item, args.clone())),
                        select: None,
                    });
                }
            }
        }
        // `A or B or C` — v0: Deq-only alternatives, depth-1 fifos only
        // (shape/depth violations are `check_or_shape`'s job, checks.rs;
        // this stays permissive like `fifo_op_stmt` above and simply
        // skips anything that isn't a plain Deq, so a malformed chain
        // produces the SAME useful entries for the alts that ARE valid
        // rather than aborting the whole enumeration). Each alt's
        // `select` is a priority-pick condition — "this one's ready AND
        // none of the earlier alternatives were" — built up incrementally
        // exactly like `prio`'s own mutual-exclusivity convention, hand-
        // verified against real firtool+Icarus before this was written.
        for chain in or_chains(self.ast, self.res, &body) {
            let mut none_selected: Option<String> = None;
            for &alt in &chain.alts {
                let Some((fifo, depth, is_enq, _)) = self.fifo_op(alt) else {
                    continue;
                };
                if is_enq {
                    continue;
                }
                let own = fifo_guard_cond(&fifo, false, depth);
                let select = match &none_selected {
                    None => own.clone(),
                    Some(ns) => format!("and({own}, {ns})"),
                };
                none_selected = Some(match &none_selected {
                    None => format!("not({own})"),
                    Some(ns) => format!("and({ns}, not({own}))"),
                });
                out.push(RuleFifoOp {
                    stmt: chain.stmt,
                    fifo,
                    depth,
                    is_enq: false,
                    value: None,
                    callee_ctx: None,
                    select: Some(select),
                });
            }
        }
        out
    }

    /// Compiles `op`'s `Enq` value — `None` for a `Deq` (nothing to
    /// compile). For a via-callee op, binds the callee's own params AND
    /// top-level `let`s (`bind_callee_context`, calls.rs) around the
    /// compile, so a value like `buf.Enq[x]` (a param) or `output.Enq[y]`
    /// where `y := input.Deq[]` was bound earlier in the SAME callee
    /// (the "bridge" pattern, DESIGN.md's `examples/fifo_bridge.tr`
    /// wrapped in a callee) both resolve against the call's actual
    /// arguments and the callee's own local bindings, not an unbound
    /// name.
    pub(crate) fn compile_fifo_op_value(&mut self, op: &RuleFifoOp, hint: u64) -> Option<String> {
        let value = op.value?;
        let Some((fn_item, args)) = &op.callee_ctx else {
            return Some(
                self.compile_expr_hinted(value, Some(hint))
                    .unwrap_or_default(),
            );
        };
        let Item::Fn { params, body, .. } = self.ast.item(*fn_item).clone() else {
            return None;
        };
        let saved = self.bind_callee_context(&params, args, &body);
        let result = self
            .compile_expr_hinted(value, Some(hint))
            .unwrap_or_default();
        self.restore_callee_context(saved);
        Some(result)
    }
}

/// One fifo op a rule performs — see `Emitter::rule_fifo_ops`'s own doc
/// comment for the full picture. `callee_ctx`: `Some((fn_item, args))`
/// when this op was found inside a callee's own body (reached through a
/// call at `stmt`), so `compile_fifo_op_value` knows to bind that
/// callee's params/locals before compiling `value`; `None` for a fifo
/// op sitting directly in the rule. `select`: `None` for every op this
/// struct represented before `or` existed — gate its state transition on
/// `fires_rule` alone, and DO contribute its occupancy to the rule's own
/// guard, exactly today's behavior. `Some(cond)` marks an `or`-
/// alternative (`Emitter::rule_fifo_ops`'s or-chain handling, below): gate
/// its state transition on `fires_rule AND cond` instead, and do NOT
/// contribute occupancy to the rule's guard here — `compile_guard`
/// (writes.rs) folds a whole chain's alternatives into ONE ORed term
/// separately. A consumer that forgets this distinction reproduces
/// exactly the silent-no-state-transition bug class this file's fifo-op-
/// position checks (checks.rs) exist to close off elsewhere.
#[derive(Clone)]
pub(crate) struct RuleFifoOp {
    pub(crate) stmt: StmtId,
    pub(crate) fifo: String,
    pub(crate) depth: u64,
    pub(crate) is_enq: bool,
    pub(crate) value: Option<ExprId>,
    pub(crate) callee_ctx: Option<(ItemId, Vec<ExprId>)>,
    pub(crate) select: Option<String>,
}
