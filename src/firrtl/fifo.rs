//! Fifo-specific naming/detection: a depth-1 fifo is one data register
//! plus one valid bit (`fifo_valid_name`/`fifo_data_name`), `Enq`/`Deq`
//! recognition (`is_fifo_op`, `Emitter::fifo_op`/`fifo_op_stmt`), and the
//! guard condition each op contributes (`fifo_guard_cond`,
//! `rule_fifo_guard_cond`). See the module doc comment (mod.rs) for the
//! depth-1 restriction this all assumes.
//!
//! A rule MAY both `Enq` and `Deq` the SAME fifo — a "pass-through":
//! this cycle's `Deq` returns the fifo's current (pre-edge) data, same
//! as any other read of a register, while the `Enq`'s value becomes the
//! new data for the NEXT cycle; `valid` stays 1 throughout rather than
//! toggling 0 then back to 1. The combined precondition is just
//! `valid == 1` (there must be something to dequeue) — NOT the AND of
//! each op's own individual guard (`valid` for `Deq`, `not(valid)` for
//! `Enq`), which would always be false. `rule_fifo_guard_cond` computes
//! this per-fifo, folding an Enq+Deq pair of the same fifo into a
//! single term; `compile_guard` (writes.rs) uses it instead of a
//! per-statement `fifo_guard_cond` call for exactly this reason.

use super::Emitter;
use super::writes::rule_body;
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Stmt, StmtId};
use crate::resolve::{DefKind, Resolution};

pub(crate) fn fifo_valid_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_valid")
}

pub(crate) fn fifo_data_name(fifo: &str) -> String {
    format!("__fifo_{fifo}_data")
}

/// `Deq[]` succeeds iff the fifo is valid; `Enq[x]` succeeds iff it is
/// not (depth-1: there is no room for a second element).
pub(crate) fn fifo_guard_cond(fifo: &str, is_enq: bool) -> String {
    let valid = fifo_valid_name(fifo);
    if is_enq {
        format!("not({valid})")
    } else {
        valid
    }
}

/// The guard term ONE fifo contributes to its rule, given whether the
/// rule enqueues it, dequeues it, or (the pass-through case) both:
/// `valid` alone when both are present — see this module's own doc
/// comment for why that's the correct combined precondition, not
/// `fifo_guard_cond`'s individual `valid`/`not(valid)` AND'ed together.
pub(crate) fn rule_fifo_guard_cond(fifo: &str, saw_enq: bool, saw_deq: bool) -> String {
    if saw_enq && saw_deq {
        fifo_valid_name(fifo)
    } else {
        fifo_guard_cond(fifo, saw_enq)
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
    /// `expr` is `<fifo>.Enq[x]` or `<fifo>.Deq[]` -> the fifo's name,
    /// whether it is `Enq`, and (for `Enq`) the value argument.
    pub(crate) fn fifo_op(&self, expr: ExprId) -> Option<(String, bool, Option<ExprId>)> {
        let Expr::Bracket { callee, args } = self.ast.expr(expr) else {
            return None;
        };
        let Expr::Field { base, name } = self.ast.expr(*callee) else {
            return None;
        };
        let def = self.res.expr_defs.get(base)?;
        if self.res.def(*def).kind != DefKind::Fifo {
            return None;
        }
        let fifo = self.res.def(*def).name.clone();
        match name.as_str() {
            "Deq" => Some((fifo, false, None)),
            "Enq" => Some((fifo, true, args.first().copied())),
            _ => None,
        }
    }

    /// The fifo op directly reachable from `stmt`, if any — the shapes
    /// DESIGN.md's examples use (`x := f.Deq[]`, a bare `f.Enq[x]`
    /// statement) plus `let x = f.Deq[]`, an equally legal binding form
    /// (see DESIGN.md's "Locals") that must resolve to the identical fifo
    /// op its `:=` counterpart would.
    pub(crate) fn fifo_op_stmt(&self, stmt: StmtId) -> Option<(String, bool, Option<ExprId>)> {
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
            if let Some((fifo, is_enq, value)) = self.fifo_op_stmt(*stmt) {
                out.push(RuleFifoOp {
                    stmt: *stmt,
                    fifo,
                    is_enq,
                    value,
                    callee_ctx: None,
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
                if let Some((fifo, is_enq, value)) = self.fifo_op_stmt(*cstmt) {
                    out.push(RuleFifoOp {
                        stmt: *stmt,
                        fifo,
                        is_enq,
                        value,
                        callee_ctx: Some((fn_item, args.clone())),
                    });
                }
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
/// op sitting directly in the rule.
#[derive(Clone)]
pub(crate) struct RuleFifoOp {
    pub(crate) stmt: StmtId,
    pub(crate) fifo: String,
    pub(crate) is_enq: bool,
    pub(crate) value: Option<ExprId>,
    pub(crate) callee_ctx: Option<(ItemId, Vec<ExprId>)>,
}
