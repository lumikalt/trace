//! Pre-compilation validation, run once per rule before any expression is
//! compiled: guard/fifo-op placement (`check_guard_placement`,
//! `check_fifo_same_cycle`), no reassigned locals
//! (`check_no_reassigned_locals`), a state-writing call only in an
//! allowed position (`check_writing_call_positions`), and a memory write
//! not nested in `if`/`while` (`find_nested_mem_write`, checked by
//! module.rs directly). Each violation is an explicit `Emitter::error`,
//! never a silent skip — see mod.rs's module doc comment.

use super::Emitter;
use super::calls::*;
use super::fifo::*;
use super::writes::*;
use crate::ast::{Ast, Expr, ExprId, ItemId, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};

pub(crate) fn is_mem_write_to(ast: &Ast, res: &Resolution, stmt: StmtId, mem_name: &str) -> bool {
    let Stmt::Assign { lhs, .. } = ast.stmt(stmt) else {
        return false;
    };
    let Expr::Bracket { callee, .. } = ast.expr(*lhs) else {
        return false;
    };
    matches!(res.expr_defs.get(callee), Some(d) if res.def(*d).name == mem_name)
}

/// A memory write nested inside `if`/`while` is not supported: unlike
/// register writes (which thread through a `mux`), memory writes also
/// need an addr/data pair muxed together, and no example needs it yet.
/// Only descends into control flow — a top-level write is fine and is
/// found separately by `find_mem_write`.
pub(crate) fn find_nested_mem_write(ast: &Ast, stmts: &[StmtId]) -> Option<Span> {
    for stmt in stmts {
        match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(s) = find_any_mem_write_deep(ast, then_body) {
                    return Some(ast.stmt_spans[s.0 as usize].clone());
                }
                if let Some(s) = else_body
                    .as_deref()
                    .and_then(|b| find_any_mem_write_deep(ast, b))
                {
                    return Some(ast.stmt_spans[s.0 as usize].clone());
                }
            }
            Stmt::While { body, .. } => {
                if let Some(s) = find_any_mem_write_deep(ast, body) {
                    return Some(ast.stmt_spans[s.0 as usize].clone());
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn find_any_mem_write_deep(ast: &Ast, stmts: &[StmtId]) -> Option<StmtId> {
    for stmt in stmts {
        // Bracket-indexed assignment is only produced by a mem write
        // (`m[addr] := v`) — no other lvalue in the grammar has this
        // shape — so a structural check is precise without `Resolution`.
        if let Stmt::Assign { lhs, .. } = ast.stmt(*stmt)
            && matches!(ast.expr(*lhs), Expr::Bracket { .. })
        {
            return Some(*stmt);
        }
        let nested = match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => find_any_mem_write_deep(ast, then_body).or_else(|| {
                else_body
                    .as_deref()
                    .and_then(|b| find_any_mem_write_deep(ast, b))
            }),
            Stmt::While { body, .. } => find_any_mem_write_deep(ast, body),
            _ => None,
        };
        if nested.is_some() {
            return nested;
        }
    }
    None
}

impl<'a> Emitter<'a> {
    /// A guard (`expr?`) or fifo op (`Enq[x]`/`Deq[]`) may only appear
    /// before any state write in the same rule, and only at the top
    /// level — both are failure conditions that must gate the whole
    /// rule, per the module doc comment.
    pub(crate) fn check_guard_placement(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut seen_write = false;
        for stmt in &body {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    if is_state_write(self.ast, self.res, lhs) {
                        seen_write = true;
                    } else if self.fifo_op(rhs).is_some() && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a fifo operation after a state write is not yet supported \
                             (v0 restriction): it must gate the whole rule"
                                .to_string(),
                        );
                    }
                }
                Stmt::Expr(e) => {
                    if matches!(self.ast.expr(e), Expr::Guard(_)) && seen_write {
                        self.error(
                            self.ast.expr_spans[e.0 as usize].clone(),
                            "a guard after a state write is not yet supported (v0 \
                             restriction): a guard must gate the whole rule"
                                .to_string(),
                        );
                    } else if self.fifo_op(e).is_some() && seen_write {
                        self.error(
                            self.ast.expr_spans[e.0 as usize].clone(),
                            "a fifo operation after a state write is not yet supported \
                             (v0 restriction): it must gate the whole rule"
                                .to_string(),
                        );
                    }
                }
                Stmt::If { .. } | Stmt::While { .. } => {
                    if contains_guard(self.ast, *stmt) {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a guard nested in if/while is not yet supported (v0 restriction)"
                                .to_string(),
                        );
                    }
                    if contains_fifo_op(self.ast, self.res, *stmt) {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a fifo operation nested in if/while is not yet supported \
                             (v0 restriction)"
                                .to_string(),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// A local reassigned within one emitted rule is not supported: a
    /// later reference to it would need to know *which* assignment it
    /// follows (locals are inlined by binding, not by program order —
    /// see `enter_rule`), and picking the wrong one silently compiles a
    /// different value than the source reads. Reject it outright rather
    /// than risk that.
    pub(crate) fn check_no_reassigned_locals(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut seen: std::collections::HashSet<DefId> = Default::default();
        for stmt in &body {
            let Stmt::Assign { lhs, .. } = self.ast.stmt(*stmt) else {
                continue;
            };
            let Some(def) = self.res.expr_defs.get(lhs).copied() else {
                continue;
            };
            if self.res.def(def).kind != DefKind::Local {
                continue;
            }
            if !seen.insert(def) {
                self.error(
                    self.ast.stmt_spans[stmt.0 as usize].clone(),
                    format!(
                        "`{}` is reassigned in this rule; FIRRTL emission does not yet \
                         support reassigning a local (v0 restriction: locals are \
                         inlined at their single binding site, not read in program \
                         order)",
                        self.res.def(def).name
                    ),
                );
            }
        }
    }

    /// `call_writes_reg`/`call_writes_port` only ever look for a
    /// state-writing call in two positions: a bare statement (`Bump(a)`)
    /// or the whole right-hand side of `:=` (`result := Bump(a)`). A
    /// writing call anywhere else — nested in a `let`, an argument to
    /// another call, buried in a larger expression like `Bump(a) + 1` —
    /// would never be found by either walk, so its write would silently
    /// vanish from the emitted hardware while its return value (if any)
    /// still inlines fine. Reject that outright, explicitly, rather than
    /// let it happen: walk every expression reachable from this rule,
    /// and for every `Expr::Call` found that isn't one of the two
    /// allowed positions, error if its callee's (merged) signature
    /// writes anything.
    pub(crate) fn check_writing_call_positions(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        self.check_writing_call_positions_in(&body);
    }

    pub(crate) fn check_writing_call_positions_in(&mut self, stmts: &[StmtId]) {
        for stmt in stmts {
            let allowed = match self.ast.stmt(*stmt).clone() {
                Stmt::Expr(e) if matches!(self.ast.expr(e), Expr::Call { .. }) => Some(e),
                Stmt::Assign { rhs, .. } if matches!(self.ast.expr(rhs), Expr::Call { .. }) => {
                    Some(rhs)
                }
                _ => None,
            };
            let mut roots: Vec<ExprId> = Vec::new();
            match self.ast.stmt(*stmt).clone() {
                Stmt::Expr(e) => roots.push(e),
                Stmt::Assign { lhs, rhs } => {
                    roots.push(lhs);
                    roots.push(rhs);
                }
                Stmt::Let { init, .. } => roots.push(init),
                Stmt::Return(Some(e)) => roots.push(e),
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    roots.push(cond);
                    self.check_writing_call_positions_in(&then_body);
                    if let Some(b) = &else_body {
                        self.check_writing_call_positions_in(b);
                    }
                }
                Stmt::While { cond, body } => {
                    roots.push(cond);
                    self.check_writing_call_positions_in(&body);
                }
                Stmt::Return(None) | Stmt::Tick => {}
            }
            for root in roots {
                let mut calls = Vec::new();
                collect_calls(self.ast, root, &mut calls);
                for call in calls {
                    if Some(call) == allowed {
                        continue;
                    }
                    let Expr::Call { callee, .. } = self.ast.expr(call).clone() else {
                        unreachable!()
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
                    let Some(sig) = self.fx.sigs.get(&fn_item) else {
                        continue;
                    };
                    if !sig.writes.is_empty() {
                        self.error(
                            self.ast.expr_spans[call.0 as usize].clone(),
                            "a call to a function that writes state may only appear \
                             as a whole statement, or as the entire right-hand side \
                             of `:=` (v0 restriction: not nested inside a larger \
                             expression, a `let`, or as an argument to another \
                             call — the write would not be found there)"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }
}

pub(crate) fn is_state_write(ast: &Ast, res: &Resolution, lhs: ExprId) -> bool {
    match ast.expr(lhs) {
        Expr::Ident(_) => res
            .expr_defs
            .get(&lhs)
            .is_some_and(|d| res.def(*d).kind.is_state()),
        Expr::Bracket { callee, .. } => res
            .expr_defs
            .get(callee)
            .is_some_and(|d| res.def(*d).kind.is_state()),
        Expr::Field { base, .. } => res
            .expr_defs
            .get(base)
            .is_some_and(|d| res.def(*d).kind.is_state()),
        _ => false,
    }
}

pub(crate) fn contains_guard(ast: &Ast, stmt: StmtId) -> bool {
    match ast.stmt(stmt) {
        Stmt::Expr(e) => matches!(ast.expr(*e), Expr::Guard(_)),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            then_body.iter().any(|s| contains_guard(ast, *s))
                || else_body
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|s| contains_guard(ast, *s)))
        }
        Stmt::While { body, .. } => body.iter().any(|s| contains_guard(ast, *s)),
        _ => false,
    }
}
