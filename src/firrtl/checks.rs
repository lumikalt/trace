//! Pre-compilation validation, run once per rule before any expression is
//! compiled: guard/fifo-op/failing-call placement (`check_guard_
//! placement`), at most one `Enq` and at most one `Deq` per fifo
//! (`check_fifo_op_counts`), a state-writing call only in an allowed
//! position (`check_writing_call_positions`), a failing call likewise
//! (`check_failing_call_positions`), and whether a `sig.fails` callee's
//! own fail condition is actually foldable into a caller's guard at all
//! (`check_fails_is_foldable_guard`, called from calls.rs's
//! `validate_call` against the CALLEE's own body). Each violation is an
//! explicit `Emitter::error`, never a silent skip — see mod.rs's module
//! doc comment. A memory write MAY nest in `if`/`else` (threaded through a
//! `mux` by `writes.rs`'s `mem_write_in_stmts`, same as a register or
//! instance-port write) — there is no separate preflight check for it
//! here; instead `mem_write_in_stmts` itself rejects a second
//! *unconditional* write to the same memory in one rule inline, as it
//! walks the statements (a preflight count would false-positive on
//! `if { m[a] := x } m[a] := y`, which is fine — the unconditional write
//! becomes that `if`'s implicit else). A rule enqueueing AND dequeueing
//! the SAME fifo is
//! likewise no longer a preflight rejection (see fifo.rs's module doc
//! comment for the pass-through semantics `compile_guard`, not a check
//! here, computes) — but two `Enq`s (or two `Deq`s) of that same fifo
//! still is one, since neither has a coherent meaning for a depth-1
//! buffer. A local reassigned at a rule's top level is no longer a
//! preflight rejection either — `enter_rule`/`set_pos` (writes.rs)
//! resolve each reference against a position-correct snapshot instead
//! (see DESIGN.md's "Reassigned locals" section).

use super::Emitter;
use super::calls::*;
use super::fifo::*;
use super::writes::*;
use crate::ast::{Ast, Expr, ExprId, ItemId, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefKind, Resolution, is_builtin_named, is_guard_like};
use std::collections::HashMap;

pub(crate) fn is_mem_write_to(ast: &Ast, res: &Resolution, stmt: StmtId, mem_name: &str) -> bool {
    let Stmt::Assign { lhs, .. } = ast.stmt(stmt) else {
        return false;
    };
    let Expr::Bracket { callee, .. } = ast.expr(*lhs) else {
        return false;
    };
    matches!(res.expr_defs.get(callee), Some(d) if res.def(*d).name == mem_name)
}

impl<'a> Emitter<'a> {
    /// A guard (`expr?`) or fifo op (`Enq[x]`/`Deq[]`) may only appear
    /// before any state write in the same rule, and only at the top
    /// level — both are failure conditions that must gate the whole
    /// rule, per the module doc comment.
    pub(crate) fn check_guard_placement(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        // An `or` chain WITHOUT a default stays fallible (its
        // alternatives' combined occupancy folds into the rule's own
        // guard, `compile_guard`) and so needs the same "before any
        // write" placement a bare fifo op/failing call needs; a
        // defaulted chain is unconditional (hand-lowered + Icarus-
        // confirmed: it contributes NO guard term at all) and so has no
        // placement restriction of its own.
        let fallible_or: std::collections::HashSet<StmtId> = or_chains(self.ast, self.res, &body)
            .into_iter()
            .filter(|c| c.default.is_none())
            .map(|c| c.stmt)
            .collect();
        let mut seen_write = false;
        for stmt in &body {
            match self.ast.stmt(*stmt).clone() {
                Stmt::Assign { lhs, rhs } => {
                    // Checked independently of the `is_state_write`
                    // branch below (not chained onto it as another
                    // `else if`, which is how this arm read before this
                    // audit): a fifo op's or failing call's own LHS is
                    // routinely ALSO state (`r0 := f.Deq[]`, `out :=
                    // SomeFailingCall()` — an entirely ordinary,
                    // idiomatic shape, not an edge case), so an else-if
                    // chain keyed on "is the lhs a write" took the
                    // `seen_write = true` branch and silently skipped
                    // this check on exactly that statement — found while
                    // auditing whether a post-`tick` guard/fifo-op is
                    // caught (TODO.md), reproduced with no `sequences`
                    // involved at all: `r1 := 2` then `r0 := f.Deq[]` in
                    // a PLAIN rule compiled clean with no error. Not a
                    // wrong-hardware bug (`compile_guard` already folds
                    // every fifo op in the body regardless of position,
                    // so the emitted guard was still correct) — a
                    // validation-completeness gap, the same else-if-
                    // behind-`is_state_write` class the `or` case above
                    // was fixed for earlier this session.
                    let contributes_guard = fallible_or.contains(stmt)
                        || self.fifo_op(rhs).is_some()
                        || self.is_failing_call(rhs);
                    if fallible_or.contains(stmt) && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "an `or` chain with no default, after a state write, is \
                             not yet supported (v0 restriction): its alternatives \
                             must gate the whole rule"
                                .to_string(),
                        );
                    } else if self.fifo_op(rhs).is_some() && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a fifo operation after a state write is not yet supported \
                             (v0 restriction): it must gate the whole rule"
                                .to_string(),
                        );
                    } else if self.is_failing_call(rhs) && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a call to a function that can fail, after a state write, \
                             is not yet supported (v0 restriction): its guard must \
                             gate the whole rule"
                                .to_string(),
                        );
                    }
                    // A write only closes the guard window when it's a
                    // PLAIN, unconditional write — not when the SAME
                    // statement is itself the thing contributing a
                    // failure condition (`r0 := f.Deq[]`: the write and
                    // the guard are one statement, so there's no "already
                    // committed before a failure is checked" hazard for
                    // mod.rs's own restriction to protect against). Two
                    // independent fifo-op-driven writes (`a := f.Deq[]`
                    // then `b := g.Deq[]`) must both stay open — advisor-
                    // caught: an EARLIER version of this fix set
                    // `seen_write` unconditionally on any state-writing
                    // LHS, which would have rejected exactly that
                    // ordinary pattern.
                    if is_state_write(self.ast, self.res, lhs) && !contributes_guard {
                        seen_write = true;
                    }
                }
                Stmt::Expr(e) => {
                    if is_guard_like(self.ast, self.res, e) && seen_write {
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
                    } else if self.is_failing_call(e) && seen_write {
                        self.error(
                            self.ast.expr_spans[e.0 as usize].clone(),
                            "a call to a function that can fail, after a state write, \
                             is not yet supported (v0 restriction): its guard must \
                             gate the whole rule"
                                .to_string(),
                        );
                    } else if fallible_or.contains(stmt) && seen_write {
                        self.error(
                            self.ast.expr_spans[e.0 as usize].clone(),
                            "an `or` chain with no default, after a state write, is \
                             not yet supported (v0 restriction): its alternatives \
                             must gate the whole rule"
                                .to_string(),
                        );
                    }
                }
                Stmt::Let { init, .. } => {
                    if self.fifo_op(init).is_some() && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a fifo operation after a state write is not yet supported \
                             (v0 restriction): it must gate the whole rule"
                                .to_string(),
                        );
                    } else if self.is_failing_call(init) && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a call to a function that can fail, after a state write, \
                             is not yet supported (v0 restriction): its guard must \
                             gate the whole rule"
                                .to_string(),
                        );
                    } else if fallible_or.contains(stmt) && seen_write {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "an `or` chain with no default, after a state write, is \
                             not yet supported (v0 restriction): its alternatives \
                             must gate the whole rule"
                                .to_string(),
                        );
                    }
                }
                Stmt::If { .. } | Stmt::While { .. } => {
                    if contains_guard(self.ast, self.res, *stmt) {
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
                    if self.contains_failing_call(*stmt) {
                        self.error(
                            self.ast.stmt_spans[stmt.0 as usize].clone(),
                            "a call to a function that can fail, nested in if/while, \
                             is not yet supported (v0 restriction)"
                                .to_string(),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// At most one `Enq` and at most one `Deq` per fifo per rule,
    /// regardless of the fifo's depth — a second `Enq[x]` on the same
    /// fifo silently discards the first candidate value (only the last
    /// write lands, at any depth: one `Enq[x]` fills one slot, not one
    /// slot per statement), and a second `Deq[]` does not dequeue a
    /// second value (both would read the same pre-edge data, not
    /// distinct entries). Both are almost certainly a mistake, not the
    /// one-`Enq`-plus-one-`Deq` pass-through this emitter does support
    /// (fifo.rs's module doc comment) — multi-slot fills/drains in one
    /// rule are a deliberately separate, unimplemented feature, not
    /// something a depth>1 fifo gets for free. Enqueuing one fifo and
    /// dequeuing a DIFFERENT one is unaffected — this counts occurrences
    /// per fifo, not per rule.
    pub(crate) fn check_fifo_op_counts(&mut self, rule: ItemId) {
        let ops = self.rule_fifo_ops(rule);
        let mut enqs: HashMap<String, Vec<StmtId>> = HashMap::new();
        let mut deqs: HashMap<String, Vec<StmtId>> = HashMap::new();
        for op in &ops {
            let bucket = if op.is_enq { &mut enqs } else { &mut deqs };
            bucket.entry(op.fifo.clone()).or_default().push(op.stmt);
        }
        for (kind, map, message) in [
            (
                "Enq",
                &enqs,
                "only the last value would land, silently discarding the earlier \
                 one(s); use a `reg` if you need to hold more than one candidate \
                 value in a cycle",
            ),
            (
                "Deq",
                &deqs,
                "every `Deq[]` this cycle reads the same value rather than \
                 advancing to a new one; bind it to a local once and reuse that \
                 local",
            ),
        ] {
            let mut fifos: Vec<&String> = map.keys().collect();
            fifos.sort();
            for fifo in fifos {
                let stmts = &map[fifo];
                if stmts.len() > 1 {
                    self.error(
                        self.ast.stmt_spans[stmts[1].0 as usize].clone(),
                        format!("`{fifo}.{kind}` appears more than once in this rule; {message}"),
                    );
                }
            }
        }
        // A fifo used as an `or` alternative may not ALSO be touched
        // directly (or via a different `or` chain) elsewhere in the same
        // rule: an alternative's state transition is conditionally gated
        // (`RuleFifoOp::select`), but the combined pass-through guard
        // `rule_fifo_guard_cond` computes assumes every op on a fifo is
        // unconditional — composing the two has not been verified, so
        // it's rejected outright (v0 restriction) rather than silently
        // assumed to compose.
        let mut by_fifo: HashMap<String, (bool, bool)> = HashMap::new();
        for op in &ops {
            let entry = by_fifo.entry(op.fifo.clone()).or_default();
            if op.select.is_some() {
                entry.1 = true;
            } else {
                entry.0 = true;
            }
        }
        let mut fifos: Vec<&String> = by_fifo.keys().collect();
        fifos.sort();
        for fifo in fifos {
            let (unconditional, or_alt) = by_fifo[fifo];
            if unconditional && or_alt {
                let stmt = ops
                    .iter()
                    .find(|o| &o.fifo == fifo && o.select.is_some())
                    .map(|o| o.stmt);
                if let Some(stmt) = stmt {
                    self.error(
                        self.ast.stmt_spans[stmt.0 as usize].clone(),
                        format!(
                            "`{fifo}` is used as an `or` alternative here but is also \
                             touched directly elsewhere in this rule (v0 restriction): \
                             a fifo used as an `or` alternative may only be touched \
                             through that `or` chain"
                        ),
                    );
                }
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
    /// writes anything. `check_writing_call_positions_in` (below) is
    /// already fully generic over any `&[StmtId]`, not rule-specific
    /// despite this wrapper's name — `calls.rs`'s `validate_call` reuses
    /// it directly against a CALLEE's own body too, so the identical
    /// restriction applies at every level a call reaches, not just the
    /// rule that starts the chain.
    pub(crate) fn check_writing_call_positions(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        self.check_writing_call_positions_in(&body);
    }

    pub(crate) fn check_writing_call_positions_in(&mut self, stmts: &[StmtId]) {
        let mut bad = Vec::new();
        self.calls_outside_allowed_positions(stmts, false, &mut bad);
        let logic_args = self.logic_arg_exprs(stmts);
        bad.retain(|e| !logic_args.contains(e));
        for call in bad {
            let Some(fn_item) = self.call_target_fn(call) else {
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

    /// Every `logic(...)` call's sole argument, found anywhere within
    /// `stmts` (any nesting depth, any statement, including inside
    /// `if`/`while`) — regardless of whether that argument is actually a
    /// VALID `logic(...)` argument (`check_logic_args_in` decides that
    /// separately). This is the exemption list `check_failing_call_
    /// positions`/`check_fifo_op_positions`/`check_writing_call_
    /// positions_in` all subtract from their own "outside allowed
    /// positions" findings: once wrapped in `logic(...)`, a fifo op or a
    /// failing/writing call is no longer something that needs to be
    /// foldable into a rule's own guard, or found by the write-hunt —
    /// `logic` converts it into a plain, already-composable `bits[1]`
    /// VALUE with no fold/write obligation left, so the position
    /// restrictions that exist specifically to guarantee foldability/
    /// write-discovery no longer apply. `stmts` generic (not rule-
    /// specific) since `check_writing_call_positions_in` is ALSO the
    /// callee-body check `validate_call` runs (calls.rs) — a `logic(...)`
    /// inside a callee's own body needs the identical exemption a rule
    /// body does. Reuses `collect_calls` (an ordinary `Expr::Call` IS
    /// what `logic(...)` looks like syntactically) rather than a new
    /// generic expression walker — same "roots per statement, recurse
    /// into if/while" traversal shape as `calls_outside_allowed_
    /// positions`/`fifo_ops_outside_allowed_positions`, kept as its own
    /// copy for the same reason those two are already separate copies of
    /// each other.
    fn logic_arg_exprs(&self, stmts: &[StmtId]) -> Vec<ExprId> {
        fn roots_of(ast: &Ast, stmts: &[StmtId], out: &mut Vec<ExprId>) {
            for stmt in stmts {
                match ast.stmt(*stmt).clone() {
                    Stmt::Expr(e) => out.push(e),
                    Stmt::Assign { lhs, rhs } => {
                        out.push(lhs);
                        out.push(rhs);
                    }
                    Stmt::Let { init, .. } => out.push(init),
                    Stmt::Return(Some(e)) => out.push(e),
                    Stmt::If {
                        cond,
                        then_body,
                        else_body,
                    } => {
                        out.push(cond);
                        roots_of(ast, &then_body, out);
                        if let Some(b) = &else_body {
                            roots_of(ast, b, out);
                        }
                    }
                    Stmt::While { cond, body } => {
                        out.push(cond);
                        roots_of(ast, &body, out);
                    }
                    Stmt::Return(None) | Stmt::Tick => {}
                }
            }
        }
        let mut roots = Vec::new();
        roots_of(self.ast, stmts, &mut roots);
        let mut out = Vec::new();
        for root in roots {
            let mut calls = Vec::new();
            collect_calls(self.ast, root, &mut calls);
            for call in calls {
                let Expr::Call { callee, args } = self.ast.expr(call) else {
                    continue;
                };
                if args.len() == 1 && is_builtin_named(self.res, *callee, "logic") {
                    out.push(args[0]);
                }
            }
        }
        out
    }

    /// `logic(e)`'s ONLY legal `e`: a direct fifo op (`f.Deq[]`/
    /// `f.Enq[x]`), or a direct call to a `sig.fails` fn/impl whose OWN
    /// `sig.writes` is empty. Anything else is rejected here with a
    /// specific reason, rather than falling through to `check_failing_
    /// call_positions`/`check_fifo_op_positions`/`check_writing_call_
    /// positions_in`'s generic "nested in a larger expression" messages
    /// (those three are told to ignore every `logic(...)` argument
    /// entirely via `logic_arg_exprs`, so this is the one and only error
    /// source for anything wrong inside a `logic(...)` call — including
    /// inside a callee's own body, via `check_logic_args`'s callee-body
    /// caller `validate_call`, calls.rs). The guard+write restriction
    /// mirrors Verse's own `logic{ exp }`, confirmed against its primary
    /// source (`02_primitives`): it only accepts a `<decides>`-effect
    /// expression, and `<decides>` means side-effect-free-but-fallible
    /// BY Verse's own effect system — a state-writing computation isn't
    /// legal `logic{}` input there either. This isn't merely "hard to
    /// implement": silently discarding a callee's write because it
    /// happened to be reached through `logic(...)` would be a confusing
    /// footgun even if internally safe, so it's rejected outright rather
    /// than silently honored.
    pub(crate) fn check_logic_args(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        self.check_logic_args_in(&body);
    }

    /// `or`'s v0 shape: every alternative in a chain must be a plain
    /// `Deq[]` on a depth-1 fifo — not depth>1 (nothing here has been
    /// hand-lowered/verified for a multi-slot fifo yet — a separate,
    /// larger gap), and not a call (call-alternatives are a separate,
    /// larger gap too — see TODO.md). `Enq` needs no explicit rejection
    /// here: it has no value of its own for `or` to select between
    /// (confirmed against DESIGN.md — `Enq[x]` only ever appears as a
    /// bare statement or wrapped in `logic(...)`, never as a value-
    /// producing expression), so types.rs's ordinary width-assignability
    /// check on `Or`'s alternatives already rejects it (a `unit`-typed
    /// `Enq` bracket can never match a fifo element's `bits[N]`) before
    /// this ever runs — a dedicated message here would be unreachable
    /// dead code. Only the chain's LAST element may instead be a plain
    /// default value; `or_chains` (fifo.rs) already made that
    /// classification, so `chain.alts` here is exactly the set that must
    /// be `Deq[]`. Rule-level only — a chain nested in `if`/`while` is a
    /// DIFFERENT gap surfaced by `check_fifo_op_positions`'s generic
    /// position message instead (see that check's own comment); a chain
    /// inside a callee's own body is caught separately, by `check_no_or_
    /// in_callee_body` below (this check alone does NOT run against a
    /// callee body at all, unlike `check_logic_args`/`check_writing_
    /// call_positions_in` — `or_chains` only recognizes rule-body-level
    /// positions, so a callee-body chain would otherwise produce zero
    /// `RuleFifoOp` entries and silently compile to a value read with no
    /// state transition; found by hand-testing before this was written,
    /// not by construction).
    pub(crate) fn check_or_shape(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        for chain in or_chains(self.ast, self.res, &body) {
            for &alt in &chain.alts {
                let span = self.ast.expr_spans[alt.0 as usize].clone();
                let Some((_, depth, _, _)) = self.fifo_op(alt) else {
                    self.error(
                        span,
                        "every alternative in an `or` chain must be a fifo `Deq[]` \
                         (v0 restriction); only the LAST one may instead be a plain \
                         default value"
                            .to_string(),
                    );
                    continue;
                };
                if depth != 1 {
                    self.error(
                        span,
                        "an `or` alternative on a fifo with depth > 1 is not yet \
                         supported (v0 restriction)"
                            .to_string(),
                    );
                }
            }
        }
    }

    /// An `or` chain anywhere inside a callee's own body — v0 restriction,
    /// called from `validate_call` (calls.rs) against every callee body a
    /// call reaches, mirroring `check_writing_call_positions_in`/`check_
    /// logic_args_in`'s own callee-body reuse. Unlike those two, `or`
    /// itself has NO callee-body support to fall back to (no fold, no
    /// discharge) — `or_chains` (fifo.rs) only ever looks at a RULE
    /// body's own top-level statements, so a chain reached only through a
    /// call would silently produce zero `RuleFifoOp` entries: no `select`,
    /// no state transition, while `compile_or` (expr.rs) still happily
    /// compiles its VALUE — a fifo that's read every cycle but never
    /// actually dequeued. Rejects any `or`, anywhere in the body (not just
    /// a well-positioned one — position validity is irrelevant when the
    /// whole feature isn't supported here yet), via the same generic
    /// recursive descent `collect_read_sites_expr` (module.rs) uses.
    pub(crate) fn check_no_or_in_callee_body(&mut self, stmts: &[StmtId]) {
        fn roots_of(ast: &Ast, stmts: &[StmtId], out: &mut Vec<ExprId>) {
            for stmt in stmts {
                match ast.stmt(*stmt).clone() {
                    Stmt::Expr(e) => out.push(e),
                    Stmt::Assign { lhs, rhs } => {
                        out.push(lhs);
                        out.push(rhs);
                    }
                    Stmt::Let { init, .. } => out.push(init),
                    Stmt::Return(Some(e)) => out.push(e),
                    Stmt::If {
                        cond,
                        then_body,
                        else_body,
                    } => {
                        out.push(cond);
                        roots_of(ast, &then_body, out);
                        if let Some(b) = &else_body {
                            roots_of(ast, b, out);
                        }
                    }
                    Stmt::While { cond, body } => {
                        out.push(cond);
                        roots_of(ast, &body, out);
                    }
                    Stmt::Return(None) | Stmt::Tick => {}
                }
            }
        }
        fn collect_or_exprs(ast: &Ast, id: ExprId, out: &mut Vec<ExprId>) {
            if matches!(ast.expr(id), Expr::Or(_)) {
                out.push(id);
            }
            for child in crate::lower::sub_exprs(ast, id) {
                collect_or_exprs(ast, child, out);
            }
        }
        let mut roots = Vec::new();
        roots_of(self.ast, stmts, &mut roots);
        for root in roots {
            let mut ors = Vec::new();
            collect_or_exprs(self.ast, root, &mut ors);
            for or_expr in ors {
                self.error(
                    self.ast.expr_spans[or_expr.0 as usize].clone(),
                    "an `or` chain is not yet supported inside a callee's own body \
                     (v0 restriction: `or` only works directly in a rule) — write \
                     it directly in the calling rule instead"
                        .to_string(),
                );
            }
        }
    }

    pub(crate) fn check_logic_args_in(&mut self, stmts: &[StmtId]) {
        for arg in self.logic_arg_exprs(stmts) {
            let span = self.ast.expr_spans[arg.0 as usize].clone();
            if is_fifo_op(self.ast, self.res, arg) {
                continue;
            }
            let Expr::Call { .. } = self.ast.expr(arg).clone() else {
                self.error(
                    span,
                    "`logic(...)` needs a fifo op or a call to a function that can \
                     fail as its sole argument"
                        .to_string(),
                );
                continue;
            };
            let Some(fn_item) = self.call_target_fn(arg) else {
                self.error(
                    span,
                    "`logic(...)` needs a fifo op or a call to a function that can \
                     fail as its sole argument"
                        .to_string(),
                );
                continue;
            };
            let Some(callee_sig) = self.fx.sigs.get(&fn_item) else {
                continue;
            };
            if !callee_sig.fails {
                self.error(
                    span,
                    "`logic(...)`'s call argument must be able to fail (declare \
                     `<fails>`, or a bare condition/fifo op inside it) — a call \
                     that always succeeds has nothing for `logic(...)` to test"
                        .to_string(),
                );
            } else if !callee_sig.writes.is_empty() {
                self.error(
                    span,
                    "`logic(...)` cannot test a function that also writes state \
                     (v0 restriction): testing success here would either silently \
                     discard the write or require it to happen regardless of \
                     whether the result is used — write the call directly instead \
                     if you need its effect"
                        .to_string(),
                );
            }
        }
    }

    /// A failing call may only appear as a whole statement, or as the
    /// entire right-hand side of `:=` — the only positions
    /// `compile_guard`'s fold (`callee_fail_cond`, calls.rs) actually
    /// looks in (deliberately not `let` too, even though the fifo-op
    /// fold allows that: fewer allowed positions is a smaller surface
    /// to get wrong, and nothing needs it yet — add it later alongside
    /// a `callee_fail_cond` call site for `Stmt::Let` if an example
    /// wants it). Anywhere else (nested in a larger expression, an
    /// argument, a condition) its guard would silently never reach the
    /// rule's own guard, exactly the hole `check_writing_call_
    /// positions_in` already closes for a writing call — same shared
    /// traversal (`calls_outside_allowed_positions`), different
    /// predicate. Rule-level only: a callee's own body can never
    /// contain a nested failing call at all (`check_fails_is_foldable_
    /// guard` forbids it outright, any position), so there is nothing
    /// for this to additionally catch there.
    pub(crate) fn check_failing_call_positions(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut bad = Vec::new();
        self.calls_outside_allowed_positions(&body, false, &mut bad);
        let logic_args = self.logic_arg_exprs(&body);
        bad.retain(|e| !logic_args.contains(e));
        for call in bad {
            let Some(fn_item) = self.call_target_fn(call) else {
                continue;
            };
            if self.fx.sigs.get(&fn_item).is_some_and(|s| s.fails) {
                self.error(
                    self.ast.expr_spans[call.0 as usize].clone(),
                    "a call to a function that can fail may only appear as a whole \
                     statement, or as the entire right-hand side of `:=` (v0 \
                     restriction: not nested inside a larger expression, a \
                     condition, a `let`, or as an argument to another call — its \
                     guard would not be folded into the caller's rule there)"
                        .to_string(),
                );
            }
        }
    }

    /// A fifo op (`Enq`/`Deq`) is recognized ONLY in the exact positions
    /// `fifo_op_stmt` (fifo.rs) matches structurally — a bare statement,
    /// the entire right-hand side of `:=`, or a `let` init — since
    /// `rule_fifo_ops`, module.rs's per-fifo state-transition emission,
    /// and `compile_guard`'s pass-through precondition all route through
    /// it. Anywhere else (an arithmetic/comparison operand, wrapped in
    /// `not`, an `if`/`while` condition, a call argument) the op is
    /// invisible to all three: no occupancy guard, no enqueue/dequeue
    /// state transition, and the fifo's raw data register is read as if
    /// it were valid — a silent miscompile, not a caught error, until
    /// this check (found via the "Verse `not`-discharge" audit: `not
    /// (fifo.Deq[])` was the entry point, but the same gap is reachable
    /// with plain arithmetic and no `not` involved at all — confirmed
    /// against real firtool-format FIRRTL output before this was
    /// written). Mirrors `check_failing_call_positions` exactly: same
    /// shared-traversal shape, `collect_fifo_ops` (fifo.rs) standing in
    /// for `collect_calls`.
    pub(crate) fn check_fifo_op_positions(&mut self, rule: ItemId) {
        let body = rule_body(self.ast, rule);
        let mut bad = Vec::new();
        self.fifo_ops_outside_allowed_positions(&body, &mut bad);
        let logic_args = self.logic_arg_exprs(&body);
        // `or_chains` only recognizes a chain sitting directly at one of
        // the three allowed top-level positions (see its own doc
        // comment, fifo.rs) — it deliberately does NOT recurse into
        // `if`/`while` or a larger expression, so a misplaced `or`'s
        // alternatives stay unexempted here and fall through to this
        // same generic "fifo operation ... not in an allowed position"
        // message below (see `logic_wrapped_call_is_allowed_inside_an_
        // if_condition`'s sibling test for `or` pinning this).
        let or_alts: Vec<ExprId> = or_chains(self.ast, self.res, &body)
            .into_iter()
            .flat_map(|c| c.alts)
            .collect();
        bad.retain(|e| !logic_args.contains(e) && !or_alts.contains(e));
        for op in bad {
            self.error(
                self.ast.expr_spans[op.0 as usize].clone(),
                "a fifo operation may only appear as a whole statement, or as \
                 the entire right-hand side of `:=`/`let` (v0 restriction: not \
                 nested inside a larger expression, a condition, or as an \
                 argument to a call — its guard and state transition would not \
                 be recognized there)"
                    .to_string(),
            );
        }
    }

    /// Every fifo op reachable within `stmts` that ISN'T sitting as a
    /// whole bare statement, the entire right-hand side of `:=`, or a
    /// `let` init — the fifo-op sibling of `calls_outside_allowed_
    /// positions`, kept as its own copy (rather than parameterizing that
    /// one over a collector function) since the two checks' allowed-
    /// position sets already independently differ (`let` counts here
    /// unconditionally; `calls_outside_allowed_positions` only counts it
    /// when its caller opts in).
    fn fifo_ops_outside_allowed_positions(&self, stmts: &[StmtId], out: &mut Vec<ExprId>) {
        for stmt in stmts {
            let allowed = match self.ast.stmt(*stmt).clone() {
                Stmt::Expr(e) if is_fifo_op(self.ast, self.res, e) => Some(e),
                Stmt::Assign { rhs, .. } if is_fifo_op(self.ast, self.res, rhs) => Some(rhs),
                Stmt::Let { init, .. } if is_fifo_op(self.ast, self.res, init) => Some(init),
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
                    self.fifo_ops_outside_allowed_positions(&then_body, out);
                    if let Some(b) = &else_body {
                        self.fifo_ops_outside_allowed_positions(b, out);
                    }
                }
                Stmt::While { cond, body } => {
                    roots.push(cond);
                    self.fifo_ops_outside_allowed_positions(&body, out);
                }
                Stmt::Return(None) | Stmt::Tick => {}
            }
            for root in roots {
                let mut ops = Vec::new();
                collect_fifo_ops(self.ast, self.res, root, &mut ops);
                for op in ops {
                    if Some(op) != allowed {
                        out.push(op);
                    }
                }
            }
        }
    }

    /// Every `Expr::Call` reachable within `stmts` that ISN'T sitting as
    /// a whole bare statement or the entire right-hand side of `:=`
    /// (the two positions a nested write or a nested fail condition can
    /// actually be found/folded from) — including inside `if`/`while`
    /// sub-bodies. Shared traversal for `check_writing_call_positions_
    /// in` (flags a state-writing target) and `check_failing_call_
    /// positions` (flags a failing target): same reachable-call
    /// enumeration, different predicate over what's disallowed outside
    /// the two safe positions. `allow_let`: whether a `let`'s own init
    /// also counts as an allowed position — `false` for both current
    /// callers (a write inside a `let` init is never findable by the
    /// write-hunt walk at all, and a failing call there is deliberately
    /// out of scope for now too, see `check_failing_call_positions`'s
    /// own doc comment) but kept as a parameter rather than hardcoded,
    /// since the two checks' allowed-position sets are conceptually
    /// independent even though they agree today.
    fn calls_outside_allowed_positions(
        &self,
        stmts: &[StmtId],
        allow_let: bool,
        out: &mut Vec<ExprId>,
    ) {
        for stmt in stmts {
            let allowed = match self.ast.stmt(*stmt).clone() {
                Stmt::Expr(e) if matches!(self.ast.expr(e), Expr::Call { .. }) => Some(e),
                Stmt::Assign { rhs, .. } if matches!(self.ast.expr(rhs), Expr::Call { .. }) => {
                    Some(rhs)
                }
                Stmt::Let { init, .. }
                    if allow_let && matches!(self.ast.expr(init), Expr::Call { .. }) =>
                {
                    Some(init)
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
                    self.calls_outside_allowed_positions(&then_body, allow_let, out);
                    if let Some(b) = &else_body {
                        self.calls_outside_allowed_positions(b, allow_let, out);
                    }
                }
                Stmt::While { cond, body } => {
                    roots.push(cond);
                    self.calls_outside_allowed_positions(&body, allow_let, out);
                }
                Stmt::Return(None) | Stmt::Tick => {}
            }
            for root in roots {
                let mut calls = Vec::new();
                collect_calls(self.ast, root, &mut calls);
                for call in calls {
                    if Some(call) != allowed {
                        out.push(call);
                    }
                }
            }
        }
    }

    /// Resolves a `Call` expression's callee to the `ItemId` of the
    /// `fn`/`impl` it targets, or `None` if it isn't a call to one at
    /// all (a builtin, or unresolved).
    fn call_target_fn(&self, call: ExprId) -> Option<ItemId> {
        let Expr::Call { callee, .. } = self.ast.expr(call).clone() else {
            return None;
        };
        let def = *self.res.expr_defs.get(&callee)?;
        if !matches!(self.res.def(def).kind, DefKind::Fn | DefKind::Impl) {
            return None;
        }
        self.res
            .item_defs
            .iter()
            .find(|(_, d)| **d == def)
            .map(|(item, _)| *item)
    }

    /// Whether `expr` is itself a call to a function whose (merged)
    /// effect signature can fail — used by `check_guard_placement` to
    /// treat a failing call the same as a bare guard or fifo op for the
    /// "must precede any write, not nested in if/while" placement rule.
    pub(crate) fn is_failing_call(&self, expr: ExprId) -> bool {
        self.call_target_fn(expr)
            .is_some_and(|fn_item| self.fx.sigs.get(&fn_item).is_some_and(|s| s.fails))
    }

    /// Whether `stmt` (an `if`/`while`, recursively) contains a call to
    /// a failing function anywhere within it — excluding one wrapped in
    /// `logic(...)` (found via the `and`-redundancy probes when adding
    /// `or`: `logic(...)`'s whole point is converting a failing call
    /// into a plain, already-composable `bits[1]` value with no
    /// remaining guard-fold obligation, so it may appear inside an
    /// `if`/`while` condition same as any other value — this is the
    /// same exemption `check_failing_call_positions`/`check_fifo_op_
    /// positions`/`check_writing_call_positions_in` already apply,
    /// `check_guard_placement`'s own if/while sub-check had simply never
    /// been taught about it).
    fn contains_failing_call(&self, stmt: StmtId) -> bool {
        let mut calls = Vec::new();
        collect_all_calls_in(self.ast, std::slice::from_ref(&stmt), &mut calls);
        let logic_args = self.logic_arg_exprs(std::slice::from_ref(&stmt));
        calls
            .iter()
            .any(|call| !logic_args.contains(call) && self.is_failing_call(*call))
    }

    /// A `sig.fails` callee is only inlinable when its ENTIRE fail
    /// condition reduces to bare, top-level guards `(cond)?` directly
    /// in its own body — the only shape `callee_fail_cond`
    /// (calls.rs) actually folds into a caller's own guard. Anything
    /// else that could ALSO be contributing to `sig.fails` is rejected
    /// outright here, rather than silently folding only PART of the
    /// real fail condition (which would let the caller's rule fire on
    /// a cycle it shouldn't — the same silent-drop class `check_
    /// writing_call_positions_in` already guards against for writes):
    /// - a fifo op anywhere in the body (its own state-transition
    ///   emission isn't built here; a separate, larger follow-up),
    /// - a guard nested inside one of the callee's own `if`/`else`
    ///   branches — syntactically legal per `compile_callee_body`'s
    ///   shape (a guard is an allowed "bare statement" there), but
    ///   invisible to `callee_fail_cond`'s flat top-level scan; caught
    ///   here by comparing "guards anywhere" against "guards at the
    ///   top level" and rejecting if they differ,
    /// - a nested call to ANOTHER function that can itself fail — v0
    ///   folds one level only, not a chain of failing callees.
    pub(crate) fn check_fails_is_foldable_guard(
        &mut self,
        span: Span,
        body: &[StmtId],
    ) -> Result<(), ()> {
        let top_level_guards = body
            .iter()
            .filter(|s| {
                matches!(self.ast.stmt(**s), Stmt::Expr(e) if is_guard_like(self.ast, self.res, *e))
            })
            .count();
        let any_guards = body
            .iter()
            .filter(|s| contains_guard(self.ast, self.res, **s))
            .count();
        let top_level_fifo_ops = body
            .iter()
            .filter(|s| self.fifo_op_stmt(**s).is_some())
            .count();
        let any_fifo_ops = body
            .iter()
            .filter(|s| contains_fifo_op(self.ast, self.res, **s))
            .count();
        let mut calls = Vec::new();
        collect_all_calls_in(self.ast, body, &mut calls);
        let nested_failing_call = calls.iter().any(|call| self.is_failing_call(*call));
        if top_level_guards + top_level_fifo_ops == 0
            || any_guards != top_level_guards
            || any_fifo_ops != top_level_fifo_ops
            || nested_failing_call
        {
            self.error(
                span,
                "calling a function that can fail is only supported when its \
                 failure reduces entirely to bare, top-level guards `(cond)?` \
                 and/or fifo ops directly in its own body (v0 restriction): a \
                 guard or fifo op nested inside an if/else branch, or a nested \
                 call to another failing function, can't yet be folded into \
                 the caller's own guard"
                    .to_string(),
            );
            return Err(());
        }
        Ok(())
    }
}

/// Every `Expr::Call` reachable anywhere within `stmts`, including
/// inside `if`/`while` sub-bodies — unlike `calls_outside_allowed_
/// positions`, no position filtering, just existence anywhere. Used by
/// `check_fails_is_foldable_guard`/`contains_failing_call` to find a
/// nested failing call hiding at ANY depth, not just the disallowed
/// positions a writing/failing call at the RULE level already checks.
pub(crate) fn collect_all_calls_in(ast: &Ast, stmts: &[StmtId], out: &mut Vec<ExprId>) {
    for stmt in stmts {
        let mut roots: Vec<ExprId> = Vec::new();
        match ast.stmt(*stmt).clone() {
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
                collect_all_calls_in(ast, &then_body, out);
                if let Some(b) = &else_body {
                    collect_all_calls_in(ast, b, out);
                }
            }
            Stmt::While { cond, body } => {
                roots.push(cond);
                collect_all_calls_in(ast, &body, out);
            }
            Stmt::Return(None) | Stmt::Tick => {}
        }
        for root in roots {
            collect_calls(ast, root, out);
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

pub(crate) fn contains_guard(ast: &Ast, res: &Resolution, stmt: StmtId) -> bool {
    match ast.stmt(stmt) {
        Stmt::Expr(e) => is_guard_like(ast, res, *e),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            then_body.iter().any(|s| contains_guard(ast, res, *s))
                || else_body
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|s| contains_guard(ast, res, *s)))
        }
        Stmt::While { body, .. } => body.iter().any(|s| contains_guard(ast, res, *s)),
        _ => false,
    }
}
