//! Fifo-specific naming/detection: a depth-1 fifo is one data register
//! plus one valid bit (`fifo_valid_name`/`fifo_data_name`), `Enq`/`Deq`
//! recognition (`is_fifo_op`, `Emitter::fifo_op`/`fifo_op_stmt`), and the
//! guard condition each op contributes (`fifo_guard_cond`). See the
//! module doc comment (mod.rs) for the depth-1 restriction this all
//! assumes.

use super::Emitter;
use super::writes::*;
use crate::ast::{Ast, Expr, ExprId, ItemId, Stmt, StmtId};
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
        Stmt::Assign { rhs, .. } => is_fifo_op(ast, res, *rhs),
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

    /// The fifo op directly reachable from `stmt`, if any — the two
    /// shapes DESIGN.md's examples use: `x := f.Deq[]` and a bare
    /// `f.Enq[x]` statement.
    pub(crate) fn fifo_op_stmt(&self, stmt: StmtId) -> Option<(String, bool, Option<ExprId>)> {
        let expr = match self.ast.stmt(stmt) {
            Stmt::Expr(e) => *e,
            Stmt::Assign { rhs, .. } => *rhs,
            _ => return None,
        };
        self.fifo_op(expr)
    }

    /// A depth-1 fifo cannot both `Enq` and `Deq` in the same cycle:
    /// that would require its valid bit to be both 1 (for `Deq`) and 0
    /// (for `Enq`) at once, an always-false guard. Reject it explicitly
    /// rather than silently synthesizing permanently dead hardware.
    pub(crate) fn check_fifo_same_cycle(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut enqueued = std::collections::HashSet::new();
        let mut dequeued = std::collections::HashSet::new();
        for stmt in &body {
            let Some((fifo, is_enq, _)) = self.fifo_op_stmt(*stmt) else {
                continue;
            };
            if is_enq {
                enqueued.insert(fifo);
            } else {
                dequeued.insert(fifo);
            }
        }
        for fifo in enqueued.intersection(&dequeued) {
            self.error(
                self.ast.item_spans[rule.0 as usize].clone(),
                format!(
                    "this rule both enqueues and dequeues `{fifo}` in the same cycle; \
                     not supported for a depth-1 fifo (v0 restriction): split into two \
                     rules"
                ),
            );
        }
    }
}
