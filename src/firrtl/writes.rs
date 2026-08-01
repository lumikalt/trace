//! State-write threading: a register/instance-port/memory write folds
//! through if/else as a `mux` (`reg_value_in_stmts`/
//! `inst_port_value_in_stmts`/`mem_write_in_stmts`), same priority-mux
//! pattern as everywhere else in emission. Unlike a register or port, a
//! memory write also threads an explicit write-enable boolean alongside
//! the muxed addr/data — see `mem_write_in_stmts`'s own doc comment for
//! why. A write reached only through a call (`call_writes_reg`/
//! `call_writes_port`; a call can't write a memory, unchanged)
//! recurses into the callee's own body (`callee_reg_write`/
//! `callee_port_write`) with its params bound, mirroring the same
//! mux-threading one level deeper — and `callee_reg_write`/
//! `callee_port_write` themselves call back into `call_writes_reg`/
//! `call_writes_port` for a bare statement or a non-matching write's
//! RHS, so a write threads through an arbitrarily deep chain of nested
//! calls, not just one level — see mod.rs's module doc comment for the
//! "only a bare statement or the whole RHS of `:=`" restriction this
//! all assumes (enforced separately, by checks.rs, against every
//! callee body a call reaches, not just rule bodies). Also home to
//! `enter_rule` (refreshes a rule's local bindings) and the small
//! width-resolution helpers (`width_of`, `concrete_width_of`) most of
//! this file's own methods lean on.

use super::Emitter;
use super::checks::*;
use super::fifo::*;
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Stmt, StmtId};
use crate::resolve::{DefId, DefKind, Resolution};
use crate::types::{Ty, Width};
use std::collections::HashMap;

pub(crate) fn clog2(v: u64) -> u64 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u64
    }
}

pub(crate) fn item_name(ast: &Ast, id: ItemId) -> &str {
    match ast.item(id) {
        Item::Rule { name, .. } | Item::Fn { name, .. } => &name.text,
        _ => "?",
    }
}

pub(crate) fn rule_body(ast: &Ast, id: ItemId) -> Vec<StmtId> {
    match ast.item(id) {
        Item::Rule { body, .. } => body.clone(),
        _ => Vec::new(),
    }
}

/// Is `target` `s` itself, or nested inside `s`'s if/else branches? Used
/// by `set_pos` to find which top-level statement a (possibly nested)
/// statement logically belongs to for local-snapshot purposes.
pub(crate) fn stmt_contains(ast: &Ast, s: StmtId, target: StmtId) -> bool {
    if s == target {
        return true;
    }
    match ast.stmt(s) {
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            then_body.iter().any(|c| stmt_contains(ast, *c, target))
                || else_body
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|c| stmt_contains(ast, *c, target)))
        }
        _ => false,
    }
}

/// Rules that write `mem_name[..] := ..` (write side of the aggregate
/// row already computed by effects.rs; re-derived here structurally
/// since emission needs the actual address/data expressions).
pub(crate) fn writers_of(
    ast: &Ast,
    res: &Resolution,
    rules: &[ItemId],
    mem_name: &str,
) -> Vec<ItemId> {
    rules
        .iter()
        .copied()
        .filter(|r| find_mem_write(ast, res, &rule_body(ast, *r), mem_name).is_some())
        .collect()
}

/// Whether a rule writes `mem_name` ANYWHERE in its body, including
/// nested inside `if`/`else` — used only to decide whether this rule is
/// a writer of this mem at all (`writers_of`); the actual per-branch
/// addr/data/enable is threaded separately by `mem_write_in_stmts`.
pub(crate) fn find_mem_write(
    ast: &Ast,
    res: &Resolution,
    stmts: &[StmtId],
    mem_name: &str,
) -> Option<StmtId> {
    for stmt in stmts {
        if is_mem_write_to(ast, res, *stmt, mem_name) {
            return Some(*stmt);
        }
        let nested = match ast.stmt(*stmt) {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => find_mem_write(ast, res, then_body, mem_name).or_else(|| {
                else_body
                    .as_deref()
                    .and_then(|b| find_mem_write(ast, res, b, mem_name))
            }),
            _ => None,
        };
        if nested.is_some() {
            return nested;
        }
    }
    None
}

/// True for a plain register write OR an output write (`sum := ...`) —
/// both are matched by the user's own name; an output's *emitted* target
/// is its internal backing register, resolved separately (see
/// `Emitter::output_regs`).
pub(crate) fn is_ident_named(ast: &Ast, res: &Resolution, id: ExprId, name: &str) -> bool {
    matches!(ast.expr(id), Expr::Ident(_))
        && res.expr_defs.get(&id).is_some_and(|d| {
            let d = res.def(*d);
            d.name == name && matches!(d.kind, DefKind::Reg | DefKind::Output)
        })
}

pub(crate) fn is_ident_named_inst(ast: &Ast, res: &Resolution, id: ExprId, name: &str) -> bool {
    matches!(ast.expr(id), Expr::Ident(_))
        && res.expr_defs.get(&id).is_some_and(|d| {
            let d = res.def(*d);
            d.name == name && d.kind == DefKind::Inst
        })
}

impl<'a> Emitter<'a> {
    /// Rebuilds `locals_snapshots` for `rule`, one entry per top-level
    /// statement position plus a final trailing entry for "after
    /// everything" (the `set_pos` fallback). Walks the body in program
    /// order, EAGERLY compiling each local's own RHS to FIRRTL text the
    /// moment it's bound — using whichever bindings were already
    /// established by statements strictly before it, so a REASSIGNED
    /// local's later binding can never retroactively change what an
    /// earlier read already resolved to. `local_hint` (below) supplies
    /// the width: a bare-literal RHS (`x := 5`) has no concrete width
    /// of its own to fall back on (`types.rs` never gives a literal
    /// EXPRESSION node one — see that method's own doc comment), so
    /// without this, eager compilation of a bare-literal local would
    /// fail exactly where the OLD lazy design's use-site hint used to
    /// paper over it.
    ///
    /// A local whose type never resolves to a concrete `bits[w]` at all
    /// (`local_hint` returns `None`) — e.g. one used ONLY as a mem-read
    /// address, whose own width the type checker never needs to pin
    /// down (see `type_expr_inner`'s `Ty::Mem` arm: it type-checks the
    /// index argument for its own sake but never widens the LOCAL's
    /// type from it) — can't be eagerly resolved at bind time; the only
    /// width that's ever come from is the READ site. Such a local falls
    /// back to the OLD lazy `locals` map (an uncompiled `ExprId`,
    /// recompiled with the read site's own hint — see `expr.rs`'s Ident
    /// case), same as before this feature existed. That preserves the
    /// single-assignment case exactly, but is unsound for reassignment
    /// (the old single-binding-per-DefId problem, unaddressed here) —
    /// so THIS specific sub-case still rejects reassignment explicitly,
    /// narrower than the old blanket `check_no_reassigned_locals` ever
    /// was.
    ///
    /// Also clears `locals` (the separate callee-param/let map — see
    /// its own field doc comment) before repopulating it with any
    /// unresolvable-width locals found this pass.
    pub(crate) fn enter_rule(&mut self, rule: ItemId) {
        self.locals.clear();
        self.locals_snapshots.clear();
        self.current_pos = 0;
        let body = rule_body(self.ast, rule);
        let mut current: HashMap<DefId, String> = HashMap::new();
        let mut seen: std::collections::HashSet<DefId> = Default::default();
        for (i, stmt) in body.iter().enumerate() {
            self.locals_snapshots.push(current.clone());
            self.current_pos = i;
            let bind = match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => self
                    .res
                    .expr_defs
                    .get(&lhs)
                    .copied()
                    .filter(|def| self.res.def(*def).kind == DefKind::Local)
                    .map(|def| (def, rhs)),
                Stmt::Let { name, init } => self
                    .res
                    .defs
                    .iter()
                    .enumerate()
                    .find(|(_, d)| d.span == name.span)
                    .map(|(i, _)| (DefId(i as u32), init)),
                _ => None,
            };
            let Some((def, rhs)) = bind else { continue };
            match self.local_hint(def) {
                Some(hint) => {
                    let compiled = self
                        .compile_expr_hinted(rhs, Some(hint))
                        .unwrap_or_default();
                    current.insert(def, compiled);
                }
                None => {
                    if !seen.insert(def) {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            format!(
                                "`{}` is reassigned, but its width is never pinned to a \
                                 concrete `bits[w]` anywhere in this rule (e.g. it's only \
                                 ever used as a mem-read index) — FIRRTL emission cannot \
                                 yet support reassigning a local in that shape",
                                self.res.def(def).name
                            ),
                        );
                    }
                    self.locals.insert(def, rhs);
                }
            }
        }
        self.locals_snapshots.push(current);
        self.current_pos = self.locals_snapshots.len() - 1;
    }

    /// The width hint to use when EAGERLY compiling a local's own RHS —
    /// its stable, post-fixpoint width (`types.rs`'s `local_tys`, built
    /// by re-typing the body to a fixed point so a rebound local's
    /// width already reflects the max across ALL its bindings). Safe to
    /// pass unconditionally: for anything OTHER than a bare literal
    /// (e.g. a mem read, already concretely widthed), `compile_expr`'s
    /// hint is only ever consulted by a leaf `Expr::Int`, so it's
    /// simply ignored.
    fn local_hint(&self, def: DefId) -> Option<u64> {
        match self.types.local_tys.get(&def) {
            Some(Ty::Bits(Width::Known(w))) => Some(*w),
            _ => None,
        }
    }

    /// Points `current_pos` at whichever `locals_snapshots` entry is
    /// correct for compiling `stmt` (which may be nested inside an
    /// if/else — the search looks for the enclosing TOP-LEVEL statement
    /// that IS or CONTAINS it, since a rule-local's own binding is only
    /// ever established at top level; anything inside a branch sees
    /// exactly what the branch's own enclosing statement saw). Falls
    /// back to the trailing "after everything" snapshot if `stmt` can't
    /// be found (defensive; every real call site's `stmt` does belong
    /// to `rule`).
    pub(crate) fn set_pos(&mut self, rule: ItemId, stmt: StmtId) {
        let body = rule_body(self.ast, rule);
        self.current_pos = body
            .iter()
            .position(|s| stmt_contains(self.ast, *s, stmt))
            .unwrap_or_else(|| self.locals_snapshots.len().saturating_sub(1));
    }

    pub(crate) fn compile_guard(&mut self, rule: ItemId) -> String {
        let body = rule_body(self.ast, rule);
        // `rule_fifo_ops` (fifo.rs) already finds every fifo op this
        // rule performs, direct OR reached through one failing-callee
        // call — the single enumerator every fifo-touch question in
        // this emitter routes through, so this guard's precondition
        // always agrees with module.rs's state-transition emission and
        // `check_fifo_op_counts`'s collision check. Aggregate saw-enq/
        // saw-deq PER FIFO across the WHOLE rule first (not per
        // statement) so a same-fifo Enq+Deq pair — even split across a
        // direct op and a callee's own op — contributes ONE combined
        // guard term below, not the AND of each op's own individual
        // (mutually exclusive) guard — see fifo.rs's module doc comment.
        let ops = self.rule_fifo_ops(rule);
        let mut fifo_ops: std::collections::HashMap<String, (bool, bool)> = Default::default();
        for op in &ops {
            let entry = fifo_ops.entry(op.fifo.clone()).or_insert((false, false));
            if op.is_enq {
                entry.0 = true;
            } else {
                entry.1 = true;
            }
        }
        let mut conds = Vec::new();
        let mut fifo_conds_emitted: std::collections::HashSet<String> = Default::default();
        for stmt in &body {
            self.set_pos(rule, *stmt);
            let stmt_fifo_conds: Vec<String> = ops
                .iter()
                .filter(|o| o.stmt == *stmt)
                .filter(|o| fifo_conds_emitted.insert(o.fifo.clone()))
                .map(|o| {
                    let (saw_enq, saw_deq) = fifo_ops[&o.fifo];
                    rule_fifo_guard_cond(&o.fifo, saw_enq, saw_deq)
                })
                .collect();
            match self.ast.stmt(*stmt).clone() {
                Stmt::Expr(e) => {
                    if let Expr::Guard(inner) = self.ast.expr(e) {
                        conds.push(
                            self.compile_expr(*inner)
                                .unwrap_or_else(|_| "UInt<1>(1)".to_string()),
                        );
                    } else {
                        conds.extend(stmt_fifo_conds);
                        if let Expr::Call { callee, args } = self.ast.expr(e).clone()
                            && let Some(cond) = self.callee_fail_cond(e, callee, &args)
                        {
                            conds.push(cond);
                        }
                    }
                }
                Stmt::Assign { rhs, .. } => {
                    conds.extend(stmt_fifo_conds);
                    if let Expr::Call { callee, args } = self.ast.expr(rhs).clone()
                        && let Some(cond) = self.callee_fail_cond(rhs, callee, &args)
                    {
                        conds.push(cond);
                    }
                }
                // A `let`-bound failing call is deliberately out of
                // scope for now (`check_failing_call_positions` rejects
                // it before this ever runs) -- only fifo-op folding
                // still applies here, matching existing precedent.
                Stmt::Let { .. } => {
                    conds.extend(stmt_fifo_conds);
                }
                _ => {}
            }
        }
        conds
            .into_iter()
            .reduce(|a, b| format!("and({a}, {b})"))
            .unwrap_or_else(|| "UInt<1>(1)".to_string())
    }

    /// The (write-enable, addr, data) a rule's execution actually drives
    /// onto `mem_name`'s shared writer port this cycle — threads a
    /// nested `if`/`else` through a `mux` exactly like
    /// `reg_value_in_stmts`, with one difference a register doesn't
    /// need: a register always has a well-defined "hold" fallback (its
    /// own current value) for a branch that doesn't write it, but a
    /// memory write has no such thing — when NEITHER branch writes, the
    /// write just doesn't happen at all. So this threads an explicit
    /// write-enable boolean alongside the muxed addr/data (falling back
    /// to a literal 0, the same "no state of its own" idea
    /// `inst_port_value_in_stmts` already uses for instance ports),
    /// rather than reusing a "hold" value the way a register does.
    /// `None` means this rule never writes `mem_name` anywhere in its
    /// body.
    pub(crate) fn mem_write_in_stmts(
        &mut self,
        stmts: &[StmtId],
        rule: ItemId,
        mem_name: &str,
        elem_width: u64,
        addr_width: u64,
    ) -> Option<(String, String, String)> {
        let mut current: Option<(String, String, String)> = None;
        for stmt in stmts {
            self.set_pos(rule, *stmt);
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs }
                    if is_mem_write_to(self.ast, self.res, *stmt, mem_name) =>
                {
                    // Unlike the `Stmt::If` arm below, an unconditional
                    // write has no condition to mux against a prior
                    // `current` — it always fires, so it can only ever
                    // replace whatever came before, silently discarding
                    // it (a memory has one write port; a prior write here
                    // is not "held" the way an unwritten register or fifo
                    // slot naturally would be). That's fine as the FIRST
                    // write in a rule, or as an if-block's implicit
                    // fallback (an earlier unconditional write correctly
                    // becomes a later `if`'s else-branch — see that arm),
                    // but wrong the moment it comes SECOND: silently
                    // dropping an already-computed write, guard condition
                    // included, is exactly the class of bug
                    // `check_fifo_op_counts` exists to reject for fifos.
                    if current.is_some() {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            format!(
                                "`{mem_name}` is written here unconditionally, after an \
                                 earlier write to it in this rule; the earlier write \
                                 would be silently discarded (one write port, so only \
                                 one write can land per cycle) — guard this write with \
                                 an `if` so it only replaces the earlier one on purpose, \
                                 or restructure so `{mem_name}` is written at most once \
                                 unconditionally"
                            ),
                        );
                        continue;
                    }
                    let Expr::Bracket { args, .. } = self.ast.expr(lhs).clone() else {
                        unreachable!()
                    };
                    let addr = self.compile_expr(args[0]).unwrap_or_default();
                    let data = self
                        .compile_expr_hinted(rhs, Some(elem_width))
                        .unwrap_or_default();
                    current = Some(("UInt<1>(1)".to_string(), addr, data));
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_val =
                        self.mem_write_in_stmts(&then_body, rule, mem_name, elem_width, addr_width);
                    let else_val = else_body.as_ref().and_then(|b| {
                        self.mem_write_in_stmts(b, rule, mem_name, elem_width, addr_width)
                    });
                    if then_val.is_some() || else_val.is_some() {
                        let hold = current.clone().unwrap_or_else(|| {
                            (
                                "UInt<1>(0)".to_string(),
                                format!("UInt<{addr_width}>(0)"),
                                format!("UInt<{elem_width}>(0)"),
                            )
                        });
                        let (te, ta, td) = then_val.unwrap_or_else(|| hold.clone());
                        let (ee, ea, ed) = else_val.unwrap_or(hold);
                        // Branch recursion above moved `current_pos` to
                        // whatever its own last statement was — restore
                        // it to THIS (enclosing) statement before
                        // compiling the if's own condition.
                        self.set_pos(rule, *stmt);
                        let cond_str = self
                            .compile_expr(cond)
                            .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                        current = Some((
                            format!("mux({cond_str}, {te}, {ee})"),
                            format!("mux({cond_str}, {ta}, {ea})"),
                            format!("mux({cond_str}, {td}, {ed})"),
                        ));
                    }
                }
                Stmt::While { .. } => {
                    self.error(
                        self.ast.stmt_spans[stmt.0 as usize].clone(),
                        "a loop in an emitted rule body is not supported (sequences \
                         lowering should have removed it before emission)"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        current
    }

    /// The value `reg_name` takes on when this rule's statements run,
    /// threading assignments through nested if/else as a `mux` tree.
    /// `None` means this rule never assigns the register at all. A
    /// branch that doesn't assign it falls back to whatever value was
    /// already accumulated (an earlier assignment in the same rule) or,
    /// failing that, the register's own current value — i.e. it holds,
    /// exactly like an ordinary un-driven path would.
    pub(crate) fn reg_value_in_stmts(
        &mut self,
        stmts: &[StmtId],
        rule: ItemId,
        reg_name: &str,
        width: u64,
    ) -> Option<String> {
        let mut current: Option<String> = None;
        for stmt in stmts {
            self.set_pos(rule, *stmt);
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    if is_ident_named(self.ast, self.res, lhs, reg_name) {
                        current = Some(
                            self.compile_expr_hinted(rhs, Some(width))
                                .unwrap_or_default(),
                        );
                    } else if let Some(v) = self.call_writes_reg(rhs, reg_name, width) {
                        current = Some(v);
                    }
                }
                Stmt::Expr(e) => {
                    if let Some(v) = self.call_writes_reg(e, reg_name, width) {
                        current = Some(v);
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_val = self.reg_value_in_stmts(&then_body, rule, reg_name, width);
                    let else_val = else_body
                        .as_ref()
                        .and_then(|b| self.reg_value_in_stmts(b, rule, reg_name, width));
                    if then_val.is_some() || else_val.is_some() {
                        let hold = current.clone().unwrap_or_else(|| reg_name.to_string());
                        let t = then_val.unwrap_or_else(|| hold.clone());
                        let e = else_val.unwrap_or(hold);
                        self.set_pos(rule, *stmt);
                        let cond_str = self
                            .compile_expr(cond)
                            .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                        current = Some(format!("mux({cond_str}, {t}, {e})"));
                    }
                }
                Stmt::While { .. } => {
                    self.error(
                        self.ast.stmt_spans[stmt.0 as usize].clone(),
                        "a loop in an emitted rule body is not supported (sequences \
                         lowering should have removed it before emission)"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        current
    }

    /// If `expr` is a call to a function whose (already call-graph-
    /// merged) signature writes `reg_name`, finds the value it writes by
    /// running the same validation `compile_call` does (so a write
    /// reached ONLY this way — a bare `Bump(a)` statement whose return
    /// value nothing else ever asks for — still gets fully checked, not
    /// silently skipped), then recursing into the callee's own body with
    /// its params bound. The callee can't itself call anything (v0
    /// restriction, unchanged), so this one level of substitution is
    /// enough regardless of how deep the write is nested in the
    /// callee's own if/else. `None` for any other shape (not a call, or
    /// a call that doesn't write this register).
    pub(crate) fn call_writes_reg(
        &mut self,
        expr: ExprId,
        reg_name: &str,
        width: u64,
    ) -> Option<String> {
        let Expr::Call { callee, args } = self.ast.expr(expr).clone() else {
            return None;
        };
        // Cheap pre-check before the full (error-emitting) validation,
        // so a call that doesn't even write this register doesn't
        // trigger validation — and any of its errors — on every
        // unrelated register's own pass over the same rule.
        let def = *self.res.expr_defs.get(&callee)?;
        if !matches!(self.res.def(def).kind, DefKind::Fn | DefKind::Impl) {
            return None;
        }
        let fn_item = self
            .res
            .item_defs
            .iter()
            .find(|(_, d)| **d == def)
            .map(|(item, _)| *item)?;
        let sig = self.fx.sigs.get(&fn_item)?;
        if !sig.writes.iter().any(|d| self.res.def(*d).name == reg_name) {
            return None;
        }
        let span = self.ast.expr_spans[expr.0 as usize].clone();
        let (_, params, body) = self.validate_call(span, callee).ok()?;

        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for (param, arg) in params.iter().zip(args.iter()) {
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == param.name.span)
            {
                let pdef = DefId(i as u32);
                saved.push((pdef, self.locals.insert(pdef, *arg)));
            }
        }
        let value = self.callee_reg_write(&body, reg_name, width);
        for (pdef, prev) in saved.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.locals.insert(pdef, v);
                }
                None => {
                    self.locals.remove(&pdef);
                }
            }
        }
        value
    }

    /// Finds the write a callee's OWN body makes to `reg_name`, mirroring
    /// `reg_value_in_stmts`'s if/else mux-threading but over a callee's
    /// statements rather than a rule's: also binds the callee's own
    /// `let`s (a rule's own top-level `let`s are pre-populated once by
    /// `enter_rule`, but this walk can start mid-rule, inside a
    /// DIFFERENT item's body, which `enter_rule` never sees). Ignores
    /// `Return` entirely — the return value is a wholly separate walk
    /// (`compile_callee_body`), independent of this one. Recurses a
    /// SECOND level into a nested call via `call_writes_reg` — a bare
    /// statement (`Inner(x)`) or an assign whose RHS is a call that
    /// doesn't match `reg_name` itself but transitively writes it
    /// (`v := Inner(x)` where `Inner` writes `reg_name`, not `v`) — the
    /// same two positions `check_writing_call_positions_in` already
    /// confirmed (in `validate_call`) are the only ones a writing call
    /// can legally occupy inside this body, so nothing else needs a
    /// symmetric arm here.
    pub(crate) fn callee_reg_write(
        &mut self,
        stmts: &[StmtId],
        reg_name: &str,
        width: u64,
    ) -> Option<String> {
        let mut current: Option<String> = None;
        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for stmt in stmts {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Let { name, init } => {
                    if let Some((i, _)) = self
                        .res
                        .defs
                        .iter()
                        .enumerate()
                        .find(|(_, d)| d.span == name.span)
                    {
                        let def = DefId(i as u32);
                        saved.push((def, self.locals.insert(def, init)));
                    }
                }
                Stmt::Assign { lhs, rhs } => {
                    if is_ident_named(self.ast, self.res, lhs, reg_name) {
                        current = Some(
                            self.compile_expr_hinted(rhs, Some(width))
                                .unwrap_or_default(),
                        );
                    } else if let Some(v) = self.call_writes_reg(rhs, reg_name, width) {
                        current = Some(v);
                    }
                }
                Stmt::Expr(e) => {
                    if let Some(v) = self.call_writes_reg(e, reg_name, width) {
                        current = Some(v);
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_val = self.callee_reg_write(&then_body, reg_name, width);
                    let else_val = else_body
                        .as_ref()
                        .and_then(|b| self.callee_reg_write(b, reg_name, width));
                    if then_val.is_some() || else_val.is_some() {
                        let hold = current.clone().unwrap_or_else(|| reg_name.to_string());
                        let t = then_val.unwrap_or_else(|| hold.clone());
                        let e = else_val.unwrap_or(hold);
                        let cond_str = self
                            .compile_expr(cond)
                            .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                        current = Some(format!("mux({cond_str}, {t}, {e})"));
                    }
                }
                _ => {}
            }
        }
        for (def, prev) in saved.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.locals.insert(def, v);
                }
                None => {
                    self.locals.remove(&def);
                }
            }
        }
        current
    }

    /// Same threading as `reg_value_in_stmts`, for an instance's input
    /// port: an if/else-nested write folds into a `mux`. A register's
    /// unwritten path holds its own feedback; a port has no state of its
    /// own, so its unwritten path falls back to the literal `UInt(0)`
    /// already connected unconditionally before any rule's `when` block.
    pub(crate) fn inst_port_value_in_stmts(
        &mut self,
        stmts: &[StmtId],
        rule: ItemId,
        inst_name: &str,
        port_name: &str,
        width: u64,
    ) -> Option<String> {
        let mut current: Option<String> = None;
        for stmt in stmts {
            self.set_pos(rule, *stmt);
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    if let Expr::Field { base, name } = self.ast.expr(lhs).clone()
                        && name == port_name
                        && is_ident_named_inst(self.ast, self.res, base, inst_name)
                    {
                        current = Some(
                            self.compile_expr_hinted(rhs, Some(width))
                                .unwrap_or_default(),
                        );
                    } else if let Some(v) = self.call_writes_port(rhs, inst_name, port_name, width)
                    {
                        current = Some(v);
                    }
                }
                Stmt::Expr(e) => {
                    if let Some(v) = self.call_writes_port(e, inst_name, port_name, width) {
                        current = Some(v);
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_val = self
                        .inst_port_value_in_stmts(&then_body, rule, inst_name, port_name, width);
                    let else_val = else_body.as_ref().and_then(|b| {
                        self.inst_port_value_in_stmts(b, rule, inst_name, port_name, width)
                    });
                    if then_val.is_some() || else_val.is_some() {
                        let hold = current
                            .clone()
                            .unwrap_or_else(|| format!("UInt<{width}>(0)"));
                        let t = then_val.unwrap_or_else(|| hold.clone());
                        let e = else_val.unwrap_or(hold);
                        self.set_pos(rule, *stmt);
                        let cond_str = self
                            .compile_expr(cond)
                            .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                        current = Some(format!("mux({cond_str}, {t}, {e})"));
                    }
                }
                Stmt::While { .. } => {
                    self.error(
                        self.ast.stmt_spans[stmt.0 as usize].clone(),
                        "a loop in an emitted rule body is not supported (sequences \
                         lowering should have removed it before emission)"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        current
    }

    /// Same idea as `call_writes_reg`, for an instance's input port
    /// (`inst_name.port_name`) — matched by the InstPort resource's own
    /// synthesized name (`inst_port_def` builds it as `"{inst}.{port}"`,
    /// which is what `sig.writes` actually contains a `DefId` for).
    pub(crate) fn call_writes_port(
        &mut self,
        expr: ExprId,
        inst_name: &str,
        port_name: &str,
        width: u64,
    ) -> Option<String> {
        let Expr::Call { callee, args } = self.ast.expr(expr).clone() else {
            return None;
        };
        let def = *self.res.expr_defs.get(&callee)?;
        if !matches!(self.res.def(def).kind, DefKind::Fn | DefKind::Impl) {
            return None;
        }
        let fn_item = self
            .res
            .item_defs
            .iter()
            .find(|(_, d)| **d == def)
            .map(|(item, _)| *item)?;
        let sig = self.fx.sigs.get(&fn_item)?;
        let target_name = format!("{inst_name}.{port_name}");
        if !sig
            .writes
            .iter()
            .any(|d| self.res.def(*d).name == target_name)
        {
            return None;
        }
        let span = self.ast.expr_spans[expr.0 as usize].clone();
        let (_, params, body) = self.validate_call(span, callee).ok()?;

        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for (param, arg) in params.iter().zip(args.iter()) {
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == param.name.span)
            {
                let pdef = DefId(i as u32);
                saved.push((pdef, self.locals.insert(pdef, *arg)));
            }
        }
        let value = self.callee_port_write(&body, inst_name, port_name, width);
        for (pdef, prev) in saved.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.locals.insert(pdef, v);
                }
                None => {
                    self.locals.remove(&pdef);
                }
            }
        }
        value
    }

    /// `callee_reg_write`'s counterpart for an instance port, with a
    /// port's own unwritten-path fallback (`UInt<{width}>(0)`, not a
    /// register's "hold my own feedback"). Recurses a second level into
    /// a nested call via `call_writes_port`, same reasoning as
    /// `callee_reg_write`'s own doc comment.
    pub(crate) fn callee_port_write(
        &mut self,
        stmts: &[StmtId],
        inst_name: &str,
        port_name: &str,
        width: u64,
    ) -> Option<String> {
        let mut current: Option<String> = None;
        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for stmt in stmts {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Let { name, init } => {
                    if let Some((i, _)) = self
                        .res
                        .defs
                        .iter()
                        .enumerate()
                        .find(|(_, d)| d.span == name.span)
                    {
                        let def = DefId(i as u32);
                        saved.push((def, self.locals.insert(def, init)));
                    }
                }
                Stmt::Assign { lhs, rhs } => {
                    if let Expr::Field { base, name } = self.ast.expr(lhs).clone()
                        && name == port_name
                        && is_ident_named_inst(self.ast, self.res, base, inst_name)
                    {
                        current = Some(
                            self.compile_expr_hinted(rhs, Some(width))
                                .unwrap_or_default(),
                        );
                    } else if let Some(v) = self.call_writes_port(rhs, inst_name, port_name, width)
                    {
                        current = Some(v);
                    }
                }
                Stmt::Expr(e) => {
                    if let Some(v) = self.call_writes_port(e, inst_name, port_name, width) {
                        current = Some(v);
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let then_val = self.callee_port_write(&then_body, inst_name, port_name, width);
                    let else_val = else_body
                        .as_ref()
                        .and_then(|b| self.callee_port_write(b, inst_name, port_name, width));
                    if then_val.is_some() || else_val.is_some() {
                        let hold = current
                            .clone()
                            .unwrap_or_else(|| format!("UInt<{width}>(0)"));
                        let t = then_val.unwrap_or_else(|| hold.clone());
                        let e = else_val.unwrap_or(hold);
                        let cond_str = self
                            .compile_expr(cond)
                            .unwrap_or_else(|_| "UInt<1>(0)".to_string());
                        current = Some(format!("mux({cond_str}, {t}, {e})"));
                    }
                }
                _ => {}
            }
        }
        for (def, prev) in saved.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.locals.insert(def, v);
                }
                None => {
                    self.locals.remove(&def);
                }
            }
        }
        current
    }

    pub(crate) fn width_of(&mut self, id: ExprId) -> u64 {
        match self.types.expr_tys.get(&id) {
            Some(Ty::Bits(Width::Known(w))) => *w,
            _ => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    "no concrete width for this expression; FIRRTL emission needs one".to_string(),
                );
                1
            }
        }
    }

    /// `width_of`, but first follows `id` through `self.locals`
    /// substitution — a local, or a called function's parameter, has no
    /// width of its own worth trusting when its callee's body was only
    /// ever type-checked once, generically (see `compile_call`'s doc
    /// comment on the `bits[N]`-generic-callee width subtlety: `N` is
    /// never concretely resolved in a generic callee's OWN
    /// `types.expr_tys` entries). Following the chain lands on whatever
    /// expression it's ultimately bound to — back in some concrete call
    /// site's own context, not a generic callee body — which has a
    /// real, instantiated width. Needed anywhere a callee-body
    /// expression's width is used for something OTHER than the callee's
    /// own return value (which `compile_call`/`compile_callee_body`
    /// already hint explicitly): e.g. `prio`'s argument, whose width
    /// (`N`) differs from the call's own output width (`clog2(N)`), so
    /// the existing hint can't stand in for it.
    pub(crate) fn concrete_width_of(&mut self, mut id: ExprId) -> u64 {
        while let Expr::Ident(_) = self.ast.expr(id) {
            let Some(def) = self.res.expr_defs.get(&id).copied() else {
                break;
            };
            if !matches!(self.res.def(def).kind, DefKind::Local | DefKind::Param) {
                break;
            }
            let Some(&bound) = self.locals.get(&def) else {
                break;
            };
            id = bound;
        }
        self.width_of(id)
    }
}
