//! Inlining a call to a user `fn`/`impl` or one of the synthesizable
//! builtins — FIRRTL has no call concept, so `compile_call`/
//! `compile_builtin_call` splice the callee in at its call site instead
//! of emitting it as its own hardware. `validate_call` is the single
//! choke point every inlining entry point (here, and writes.rs's
//! write-hunt) goes through: effect coloring, the call-site module
//! boundary, a call-CYCLE check (`find_call_cycle` — a callee may itself
//! call another callee, just not one that transitively calls back to
//! itself), and — reused from checks.rs, since it's already generic over
//! any statement list, not rule-specific — `check_writing_call_
//! positions_in` against THIS callee's own body, so a writing call
//! inside it sits in a position `callee_reg_write`/`callee_port_write`
//! (writes.rs) can actually find. `compile_callee_body` builds the
//! callee's return value; the rest of this file is each synthesizable
//! builtin's own hand-written FIRRTL construction — `compile_prio` (a
//! fixed-priority `mux` chain), `compile_trunc`/`compile_zext`/
//! `compile_sext` (width changes), `compile_pack`/`compile_reverse`
//! (`cat` chains), `compile_popcount` (an `add` chain), `compile_rotate`
//! (a two-slice `cat`, `rotl`/`rotr` sharing one function),
//! `compile_mux` (a direct FIRRTL `mux`), and `compile_max_min` (a
//! left-folded chain of comparator-gated `mux`es, `max`/`min` sharing
//! one function — the one builtin here that's ALSO sometimes NOT
//! synthesizable, when every argument is a compile-time constant; see
//! its own doc comment).

use super::Emitter;
use super::fifo::fifo_guard_cond;
use super::writes::{def_of_name, item_name};
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Param, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution, is_guard_like};
use crate::types::Ty;

/// Every `fn`/`impl` item directly called anywhere within `stmts` — a
/// `let`'s init, the tail return expression, a state write's RHS, an
/// `if`'s condition, a bare statement, or nested inside another call's
/// own arguments (`prio(Widen(r))`) — each paired with that specific
/// call's own `ExprId`, for precise error spans.
///
/// Deliberately a STATIC property of `stmts` (the callee's own literal
/// AST), never based on runtime call order or `self.locals` argument
/// substitution: an earlier design tracked a dynamic "currently
/// compiling this fn's body" stack, and it false-positived on
/// `Avg(Avg(x, y), z)` — a RULE calling `Avg` twice, once nested as an
/// argument. Compiling `Avg`'s own body dereferences its own param `a`,
/// which lazily pulls in the ARGUMENT expression — and that argument
/// happens to also call `Avg` — but that call lives in the CALL SITE's
/// text (the rule's), not in `Avg`'s own body, which contains no call
/// at all. Walking `stmts` directly is immune to that: it only ever
/// sees what `fn_item` itself literally wrote.
fn direct_callees(ast: &Ast, res: &Resolution, stmts: &[StmtId]) -> Vec<(ItemId, ExprId)> {
    fn body_call_exprs(ast: &Ast, stmts: &[StmtId], out: &mut Vec<ExprId>) {
        for stmt in stmts {
            match ast.stmt(*stmt) {
                Stmt::Let { init, .. } => collect_calls(ast, *init, out),
                Stmt::Assign { lhs, rhs } => {
                    collect_calls(ast, *lhs, out);
                    collect_calls(ast, *rhs, out);
                }
                Stmt::Return(Some(e)) => collect_calls(ast, *e, out),
                Stmt::Return(None) | Stmt::Tick | Stmt::Break => {}
                Stmt::Expr(e) => collect_calls(ast, *e, out),
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    collect_calls(ast, *cond, out);
                    body_call_exprs(ast, then_body, out);
                    if let Some(b) = else_body {
                        body_call_exprs(ast, b, out);
                    }
                }
                Stmt::IfLet {
                    init,
                    then_body,
                    else_body,
                    ..
                } => {
                    collect_calls(ast, *init, out);
                    body_call_exprs(ast, then_body, out);
                    if let Some(b) = else_body {
                        body_call_exprs(ast, b, out);
                    }
                }
                Stmt::While { cond, body } => {
                    collect_calls(ast, *cond, out);
                    body_call_exprs(ast, body, out);
                }
                // `while let`, like plain `While` above, only ever
                // reaches emission fully lowered away (DESIGN.md's
                // "`while` lowering") -- kept for exhaustiveness, same
                // shape as `IfLet`'s arm above.
                Stmt::WhileLet { init, body, .. } => {
                    collect_calls(ast, *init, out);
                    body_call_exprs(ast, body, out);
                }
            }
        }
    }
    let mut call_exprs = Vec::new();
    body_call_exprs(ast, stmts, &mut call_exprs);
    call_exprs
        .into_iter()
        .filter_map(|call_id| {
            let Expr::Call { callee, .. } = ast.expr(call_id) else {
                unreachable!("collect_calls only ever returns Expr::Call ids")
            };
            let def = *res.expr_defs.get(callee)?;
            if !matches!(res.def(def).kind, DefKind::Fn | DefKind::Impl) {
                return None; // a builtin (e.g. `prio`) — no body of its own
            }
            let item = res
                .item_defs
                .iter()
                .find(|(_, d)| **d == def)
                .map(|(item, _)| *item)?;
            Some((item, call_id))
        })
        .collect()
}

/// If `item`'s own body transitively reaches itself through
/// `direct_callees`, the chain that proves it (`item`, ..., `item`).
/// Same on-path DFS shape `visit_module` (module.rs) already uses for
/// instantiation cycles: the same function reached twice on unrelated
/// branches (a diamond) is fine, only a genuine cycle isn't — `path`
/// tracks the current chain, not everything ever visited.
fn find_call_cycle(ast: &Ast, res: &Resolution, item: ItemId) -> Option<Vec<ItemId>> {
    fn walk(
        ast: &Ast,
        res: &Resolution,
        item: ItemId,
        path: &mut Vec<ItemId>,
    ) -> Option<Vec<ItemId>> {
        if path.contains(&item) {
            let mut chain = path.clone();
            chain.push(item);
            return Some(chain);
        }
        let Item::Fn { body, .. } = ast.item(item) else {
            return None;
        };
        path.push(item);
        let found = direct_callees(ast, res, body)
            .into_iter()
            .find_map(|(callee, _)| walk(ast, res, callee, path));
        path.pop();
        found
    }
    walk(ast, res, item, &mut Vec::new())
}

/// Collects every `Expr::Call` reachable from `id`, including `id`
/// itself if it is one (and recursing into ITS OWN arguments too).
/// Used by `check_writing_call_positions` to find a state-writing call
/// hiding somewhere other than the two shapes `call_writes_reg`/
/// `call_writes_port` actually look for, and by `direct_callees` above.
pub(crate) fn collect_calls(ast: &Ast, id: ExprId, out: &mut Vec<ExprId>) {
    match ast.expr(id) {
        Expr::Call { args, .. } => {
            out.push(id);
            for a in args {
                collect_calls(ast, *a, out);
            }
        }
        Expr::Ident(_) | Expr::Int(_) | Expr::SizedInt { .. } | Expr::Wildcard => {}
        Expr::Unary { operand, .. } => collect_calls(ast, *operand, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_calls(ast, *lhs, out);
            collect_calls(ast, *rhs, out);
        }
        Expr::Guard(inner) | Expr::Spawn(inner) | Expr::Optional(inner) | Expr::Logic(inner) => {
            collect_calls(ast, *inner, out);
        }
        Expr::Field { base, .. } => collect_calls(ast, *base, out),
        Expr::Bracket { callee, args } => {
            collect_calls(ast, *callee, out);
            for a in args {
                collect_calls(ast, *a, out);
            }
        }
        Expr::ListLit(items) => {
            for item in items {
                collect_calls(ast, *item, out);
            }
        }
        Expr::Range { lo, hi } => {
            if let Some(lo) = lo {
                collect_calls(ast, *lo, out);
            }
            if let Some(hi) = hi {
                collect_calls(ast, *hi, out);
            }
        }
        Expr::Or(alts) => {
            for alt in alts {
                collect_calls(ast, *alt, out);
            }
        }
        Expr::StructLit { name, fields, base } => {
            collect_calls(ast, *name, out);
            for (_, value) in fields {
                collect_calls(ast, *value, out);
            }
            if let Some(base) = base {
                collect_calls(ast, *base, out);
            }
        }
        Expr::OptionTy(_) | Expr::Absent => {}
    }
}

impl<'a> Emitter<'a> {
    /// Inlines a call to a user `fn`/`impl`: FIRRTL has no call concept,
    /// so the callee's body is spliced into the caller at its call site
    /// rather than emitted as its own hardware. v0 restricts the callee
    /// to a pure value computation the width-hint machinery can thread
    /// through unchanged — anything else is an explicit error, not a
    /// silent miscompile:
    /// - no `<sequences>`/`<elaborates>` color, no state writes, and no
    ///   possible failure (a guard inside the body would already set
    ///   this) — effects.rs's already-merged signature answers all three
    ///   in one check, including through the callee's own calls;
    /// - body shape: zero or more `let` bindings, then either a trailing
    ///   `return <expr>` or an `if`/`else` whose branches both recurse
    ///   into this same shape (mandatory `else` — every reachable path
    ///   must produce a value, folded into a `mux` by
    ///   `compile_callee_body`), no state writes (a redundant but
    ///   cheaper check than the signature one above);
    /// - a callee's own body MAY call another `fn`/`impl` (composition is
    ///   allowed), as a value OR a bare statement, and MAY itself write
    ///   state that way too — as long as it doesn't transitively call
    ///   back to itself (`find_call_cycle`, a genuine cycle, direct or
    ///   indirect, would unroll forever) and any writing call sits in a
    ///   position `check_writing_call_positions_in` allows (a bare
    ///   statement or the whole RHS of `:=` — checked here, against
    ///   THIS callee's own body, the same restriction the rule level
    ///   already enforces, so `callee_reg_write`/`callee_port_write`'s
    ///   own second-level recursion always has something to find).
    ///
    /// Width correctness for a generic callee (`bits[N]` params): this
    /// call expression's own OUTER width (`id`, already instantiated to
    /// a concrete number by types.rs's call-site solver) is used as the
    /// hint threaded into every branch's return expression — never the
    /// callee's own internal, still-generic `types.expr_tys` entry for
    /// its return expression(s), which were only ever checked once, that
    /// generically, independent of any particular call site.
    /// Resolves a call's callee to its `Item::Fn` and runs every check
    /// that doesn't depend on WHICH value this particular call site
    /// wants (its return value, via `compile_call`, or one of its
    /// writes, via `call_writes_reg`/`call_writes_port`): effect
    /// coloring (`<sequences>`/`<elaborates>`/`fails`) and the call-site
    /// module boundary. Shared by both, so a state-writing call reached
    /// ONLY through its write — its return value never used, e.g. a bare
    /// `Bump(a)` statement — still gets fully validated, not silently
    /// skipped just because nothing asks for its return value.
    ///
    /// The module-boundary check: a callee's own state references were
    /// checked ONCE, at their lexical (declaration-site) position, by
    /// resolve.rs's `check_module_boundary` — which only ever compares
    /// against the module enclosing the callee's OWN body, never against
    /// wherever it ends up being called from. A `fn`/`impl` nested
    /// inside module M is still visible to (and callable from) a rule in
    /// a module nested inside M, since scopes nest outward-to-inward;
    /// inlining such a call here would splice a reference to M's own `v`
    /// into a DIFFERENT module's FIRRTL block, where `v` doesn't exist —
    /// caught only by firtool's cryptic "unknown declaration" error
    /// otherwise. `sig.reads`/`sig.writes` is the already-merged
    /// (through the whole call graph, to a fixpoint) answer for "every
    /// state def this call transitively reaches" — so one check here,
    /// against the CALL's own module (`self.module`), covers it
    /// regardless of how deep the call chain is.
    pub(crate) fn validate_call(
        &mut self,
        span: Span,
        callee: ExprId,
    ) -> Result<(ItemId, Vec<Param>, Vec<StmtId>), ()> {
        let def = *self.res.expr_defs.get(&callee).expect("checked by caller");
        let Some(fn_item) = self
            .res
            .item_defs
            .iter()
            .find(|(_, d)| **d == def)
            .map(|(item, _)| *item)
        else {
            self.error(span, "cannot find this function's item".to_string());
            return Err(());
        };
        let Item::Fn { params, body, .. } = self.ast.item(fn_item).clone() else {
            self.error(span, "call target is not a function".to_string());
            return Err(());
        };

        let Some(sig) = self.fx.sigs.get(&fn_item) else {
            self.error(
                span,
                "no effect signature computed for this function".to_string(),
            );
            return Err(());
        };
        for state_def in sig.reads.iter().chain(sig.writes.iter()) {
            if let Some(owner) = self.res.def_owner.get(state_def).copied().flatten()
                && owner != self.module
            {
                self.error(
                    span,
                    format!(
                        "calling `{}` here would reach `{}`, which belongs to a \
                         different module than this call site (modules share no \
                         state with each other, only ports declared on themselves) \
                         — v0 restriction: a fn/impl that reads or writes state can \
                         only be called from within that state's own module",
                        self.res.def(def).name,
                        self.res.def(*state_def).name,
                    ),
                );
                return Err(());
            }
        }
        if sig.sequences || sig.elaborates {
            self.error(
                span,
                "calling a <sequences>/<elaborates> function is not yet supported in \
                 FIRRTL emission (v0 restriction: only a pure <combines> value \
                 computation can be inlined)"
                    .to_string(),
            );
            return Err(());
        }
        if sig.fails {
            self.check_fails_is_foldable_guard(span.clone(), &body)?;
        }
        // Cycle check, not a blanket "no nested calls" ban: a STATIC
        // property of which functions' own bodies reference which other
        // functions BY NAME (`find_call_cycle`), not a dynamic
        // "currently compiling" stack — see that function's own doc
        // comment for why a dynamic stack gives a false positive on
        // `Avg(Avg(x, y), z)`. If `fn_item`'s own body transitively
        // reaches itself this way, inlining it would never terminate.
        if let Some(chain) = find_call_cycle(self.ast, self.res, fn_item) {
            let names: Vec<&str> = chain
                .iter()
                .map(|item| item_name(self.ast, *item))
                .collect();
            self.error(
                span,
                format!(
                    "call cycle: {} — calling a function whose body (transitively) \
                     calls itself is not supported (would require infinite inlining)",
                    names.join(" -> ")
                ),
            );
            return Err(());
        }
        // A callee may compose (call another callee) for its RETURN
        // value, AS a bare statement (for its side effect alone), OR to
        // write state — but a state-writing nested call is only found
        // by `callee_reg_write`/`callee_port_write` (the write-hunt one
        // level into a callee's own body) in the SAME two positions the
        // top-level write-hunt already requires: a bare statement, or
        // the whole RHS of `:=`. Reusing `check_writing_call_positions_
        // in` here (already fully generic over any `&[StmtId]`, not
        // rule-specific despite its name) extends that SAME restriction
        // to `fn_item`'s OWN body — a writing call nested in a `let`, an
        // argument, or a larger expression INSIDE this callee is
        // rejected here just as it already is at the rule level, rather
        // than silently vanishing because nothing walks that deep.
        // `check_writing_call_positions_in` only ever records errors
        // (the same fire-and-forget style module.rs's own per-rule call
        // uses) rather than returning a Result, so a before/after count
        // is how `validate_call` notices a bad position was found and
        // bails out here instead of proceeding to splice a callee whose
        // body it just flagged as broken.
        // `check_logic_args_in` needs the same callee-body reach for the
        // identical reason: a `logic <expr>` inside `fn_item`'s own body
        // (e.g. `Probe() <combines> { return logic f.Deq[] }`) needs its
        // operand's shape validated here too, not just at the rule
        // level — same choke point, same before/after bail-out.
        // `check_no_or_in_callee_body` is NOT the same "extend the rule-
        // level restriction to the callee" pattern the two above are:
        // `or` has no callee-body support to extend at all yet (no fold,
        // no discharge — `or_chains`, fifo.rs, only ever looks at a
        // rule's own top-level statements), so this rejects an `or`
        // ANYWHERE in the body outright, found by hand-testing a
        // defaulted (`fails: false`, so it slips past the guard-fold
        // checks) callee chain that compiled clean with the fifo's own
        // state transition silently missing.
        // `check_no_reassigned_locals_in_callee_body` is the same
        // "reject outright, no fold/support exists" shape as `check_no_
        // or_in_callee_body` just above, for an unrelated reason: see
        // that check's own doc comment (checks.rs) for why a callee-
        // local's `:=` reassignment can't just be threaded through
        // `self.locals` the way a rule-level reassignment can.
        let errors_before = self.errors.len();
        self.check_writing_call_positions_in(&body);
        self.check_logic_args_in(&body);
        self.check_no_or_in_callee_body(&body);
        self.check_no_reassigned_locals_in_callee_body(&body);
        if self.errors.len() > errors_before {
            return Err(());
        }
        Ok((fn_item, params, body))
    }

    pub(crate) fn compile_call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();

        let (_, params, fn_body) = self.validate_call(span.clone(), callee)?;

        // Bind params into `self.locals`, saving whatever was there before
        // (from an enclosing call to this SAME function, if any) so it can
        // be restored once this call is fully compiled. Without this,
        // `Avg(Avg(x, y), z)` would silently miscompile: `Avg`'s param
        // DefIds are shared across every call to `Avg`, so compiling the
        // outer call's first argument (which recurses into the inner
        // `Avg(x, y)` call) would rebind them out from under the outer
        // call before it gets to compile its second argument — a real,
        // observed silent drop of `z`, not a hypothetical. Save/restore
        // makes this properly reentrant regardless of how deep or
        // indirect the nesting is (as an argument, or via a `let` whose
        // value is a call), not just the syntactically-nested case.
        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for (param, arg) in params.iter().zip(args.iter()) {
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == param.name.span)
            {
                let def = DefId(i as u32);
                saved.push((def, self.locals.insert(def, *arg)));
            }
        }

        let w = hint.unwrap_or_else(|| self.width_of(id));
        let result = self.compile_callee_body(&fn_body, w, &span);

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
        result
    }

    /// A builtin has no body to splice (unlike a user `fn`/`impl`) — each
    /// one needs its own hand-written FIRRTL construction. `prio`/
    /// `trunc`/`pack`/`zext`/`sext`/`popcount`/`reverse`/`rotl`/`rotr`/
    /// `mux` are synthesizable today; `bits`/`wire`/`list`/`any` never
    /// reach here at all (`bits[N]` is a type-position construct, `any`
    /// is spec/`chooses`-only, both handled entirely by types.rs/
    /// effects.rs before emission), and `clog2`/`len` are DELIBERATELY
    /// compile-time-only, no runtime meaning at all — fall through to an
    /// explicit error. `max`/`min` are the one dual-purpose pair: when
    /// `types.rs` typed this call `Ty::Int` (every argument itself a
    /// compile-time constant), it's the same explicit gap as `clog2`
    /// below — `max`/`min` exist there specifically to fold inside a
    /// type-position width expression (`bits[max(n, m)]`, see `types/
    /// eval.rs`'s `const_eval_expr`), not to become a degenerate
    /// constant-comparing circuit nobody asked for. Once at least one
    /// argument is a real runtime `Bits` value, though, `compile_max_min`
    /// synthesizes an actual comparator+mux chain.
    pub(crate) fn compile_builtin_call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let def = *self.res.expr_defs.get(&callee).expect("checked by caller");
        match self.res.def(def).name.as_str() {
            "prio" => self.compile_prio(id, args, hint),
            "trunc" => self.compile_trunc(id, args, hint),
            "pack" => self.compile_pack(id, args),
            "zext" => self.compile_zext(id, args, hint),
            "sext" => self.compile_sext(id, args, hint),
            "popcount" => self.compile_popcount(id, args, hint),
            "reverse" => self.compile_reverse(id, args),
            "rotl" => self.compile_rotate(id, args, true),
            "rotr" => self.compile_rotate(id, args, false),
            "max" => self.compile_max_min(id, args, hint, true),
            "min" => self.compile_max_min(id, args, hint, false),
            "mux" => self.compile_mux(id, args),
            "__race_value" => self.compile_race_value(id, args, hint),
            name => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    format!(
                        "calling the builtin `{name}` is not yet supported in FIRRTL \
                         emission (v0 restriction: only `prio` — a fixed-priority \
                         encoder — `trunc` — bit truncation — `pack` — concatenation \
                         — `zext`/`sext` — widening — `popcount` — set-bit count — \
                         `reverse` — bit-order reversal — `rotl`/`rotr` — rotate — \
                         and `mux` — 2-way select — are synthesizable today)"
                    ),
                );
                Err(())
            }
        }
    }

    /// `pack(a, b, ...)`: concatenates its arguments into one wider bit
    /// vector, the FIRST argument as the MOST significant bits — matching
    /// FIRRTL's own `cat` primop (`cat(hi, lo)`, `hi` more significant)
    /// directly, and the same "leftmost is most significant" convention
    /// as Chisel's `Cat`/Verilog's `{a, b}`. `types.rs`'s own `"pack"`
    /// typing rule (sum of argument widths) already guarantees this
    /// matches the call's own result width — no hint needed, unlike
    /// `prio`/`trunc`. More than two arguments fold left-to-right
    /// (`cat(cat(a, b), c)`), which preserves the same ordering: `a` is
    /// still more significant than `b`, both more significant than `c`.
    fn compile_pack(&mut self, id: ExprId, args: &[ExprId]) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let Some((&first, rest)) = args.split_first() else {
            self.error(span, "`pack` takes at least one argument".to_string());
            return Err(());
        };
        let mut acc = self.compile_expr(first)?;
        for &a in rest {
            let piece = self.compile_expr(a)?;
            acc = format!("cat({acc}, {piece})");
        }
        Ok(acc)
    }

    /// `logic <expr>`: `expr`'s success as a plain `bits[1]` value — 1 if
    /// `expr` would succeed, 0 if it would fail — WITHOUT gating this
    /// rule and WITHOUT performing `expr`'s own side effect (no real
    /// dequeue/enqueue, no callee write). `checks.rs`'s `check_logic_
    /// args` has already confirmed `arg` is one of the two shapes below
    /// by the time this runs, so both branches here are read-only
    /// lookups, not validation: a fifo op compiles straight to `fifo_
    /// guard_cond` (the existing occupancy/space check `compile_guard`
    /// already computes for a real op — reading it here performs no
    /// dequeue/enqueue of its own, just a register read); a call
    /// compiles straight to `callee_fail_cond` (the existing guard-
    /// condition computation `compile_guard`'s call-folding path
    /// already uses — deliberately NOT `compile_call`/`compile_callee_
    /// body`, which would compute the callee's RETURN value and is a
    /// completely separate, unused-here code path). Neither ever
    /// reaches the write-hunting machinery (`callee_reg_write`/`callee_
    /// port_write`), so a guard-only callee's write genuinely never
    /// gets emitted for this call site — but `check_logic_args` already
    /// rejects a callee that writes anything at all, so that path is
    /// unreachable here in practice, not silently relied upon.
    /// `pub(crate)`: called from expr.rs's `compile_expr_hinted`
    /// (`Expr::Logic` is a real prefix AST node, unlike the other
    /// builtins this file's `compile_builtin_call` still dispatches by
    /// name), not from within this file at all.
    pub(crate) fn compile_logic(&mut self, id: ExprId, arg: ExprId) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        if let Some((fifo, depth, is_enq, _)) = self.fifo_op(arg) {
            return Ok(fifo_guard_cond(&fifo, is_enq, depth));
        }
        // A comparison's success condition IS its own ordinary boolean
        // value — `compile_binop`'s ALREADY-existing `eq`/`neq`/`lt`/...
        // primop output, unchanged. No fifo/call-style "don't perform
        // the side effect" concern here: a comparison never had one.
        if let Expr::Binary { op, lhs, rhs } = self.ast.expr(arg).clone()
            && op.is_comparison()
        {
            return self.compile_binop(arg, op, lhs, rhs);
        }
        if let Expr::Call {
            callee: inner_callee,
            args: inner_args,
        } = self.ast.expr(arg).clone()
            && let Some(cond) = self.callee_fail_cond(arg, inner_callee, &inner_args)
        {
            return Ok(cond);
        }
        self.error(
            span,
            "`logic`'s operand is not a fifo op, a comparison, or a failing \
             call (should have been caught earlier by check_logic_args)"
                .to_string(),
        );
        Err(())
    }

    /// `trunc(value, width)`: the low `width` bits of `value` — exactly
    /// `bits(value, width-1, 0)`, the same FIRRTL `bits` primop
    /// `compile_bit_select` already uses for `x[hi..lo]`, just reached
    /// through a different spelling. `width` itself is never read HERE —
    /// it only ever fed into `type_builtin_call`'s `"trunc"` arm, which
    /// already baked it into `id`'s own `Ty::Bits(Width::Known(_))` (this
    /// function's `width_of(id)` fallback reads it back out from there).
    ///
    /// `trunc(value)`, one argument: same emission, but `width_of(id)`
    /// can't help — types.rs typed it `Bits(Width::Unknown)` (see that
    /// arm's own doc comment: bottom-up type-checking has no target width
    /// to infer FROM at that point). `hint` is where it actually comes
    /// from instead: this function's own top-down "what width does this
    /// expression's result need to be" parameter, already threaded down
    /// from every write target/return type/etc. through `compile_expr_
    /// hinted`'s recursion — the same mechanism that lets `hint.unwrap_
    /// or_else(|| self.width_of(id))` already prefer a caller-supplied
    /// hint over `width_of` even for the explicit two-argument form
    /// above. This reaches further than a direct `x := trunc(a)` might
    /// suggest: a `let`-bound local whose own width never pins down to a
    /// concrete `bits[w]` (`trunc(a)`'s `Unknown` is exactly that shape)
    /// is compiled LAZILY at its read site instead of eagerly at its
    /// binding (`writes.rs`'s `enter_rule`/`local_hint`), so `let x =
    /// trunc(a) ... b := x` still resolves correctly — the hint comes
    /// from `x`'s read site (`b`'s own width), not its binding.
    ///
    /// Where a hint genuinely never reaches this call — nested inside an
    /// arithmetic operator's operand, say (`compile_binop` computes its
    /// OWN hint from `types.expr_tys`, not from an outer parameter, so a
    /// width-less operand there gets none) — `width_of(id)`'s generic
    /// "no concrete width for this expression" is a correct but unhelpful
    /// diagnosis: it doesn't say WHAT to do. A one-argument `trunc`
    /// specifically CAN always be fixed the same way (spell the width
    /// out), so that's checked and reported here instead, before ever
    /// reaching `width_of`'s generic fallback.
    fn compile_trunc(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        if !matches!(args.len(), 1 | 2) {
            self.error(span, "`trunc` takes (value) or (value, width)".to_string());
            return Err(());
        }
        if args.len() == 1 && hint.is_none() {
            self.error(
                span,
                "cannot infer `trunc`'s width here; use `trunc(value, width)` with an \
                 explicit width instead"
                    .to_string(),
            );
            return Err(());
        }
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let value_str = self.compile_expr(args[0])?;
        Ok(format!("bits({value_str}, {}, 0)", w.saturating_sub(1)))
    }

    /// `zext(value, width)`: widen `value` to `width` bits, filling the
    /// new high bits with zero. Every value in this language is already a
    /// plain FIRRTL `UInt` (see `compile_shift`'s doc comment on
    /// `AShr` — there's no separate signed type), and FIRRTL's own `pad`
    /// primop zero-extends a `UInt` operand, so this is `pad` directly —
    /// no cast needed, unlike `sext` below. `pad(e, n)` is a defined
    /// no-op when `n <= width(e)`, so this also covers the trivial
    /// `width == value`'s own width case without a special case here (a
    /// genuinely narrower `width` is a `types.rs` error already, which
    /// halts the pipeline before emission is reached).
    ///
    /// `hint`, preferred over `width_of(id)` exactly like `compile_trunc`
    /// does: `width` is a real argument here (unlike `trunc`'s inferred
    /// 1-arg form), so this normally resolves directly through `types.rs`'s
    /// own `Ty::Bits(Width::Known(_))` — but inside a GENERIC callee body,
    /// a width expression built from an unsolved implicit param (e.g.
    /// `zext(a, max(n, m))`) types to `Width::Unknown` there too (the
    /// `"zext"|"sext"` arm's own `type_builtin_call` doc comment), so the
    /// top-down `hint` this call was found latent to have been silently
    /// DROPPING (never threaded past `compile_builtin_call`) is exactly
    /// what resolves it at the concrete call site, same channel `trunc`
    /// already relies on.
    fn compile_zext(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        if args.len() != 2 {
            self.error(span, "`zext` takes (value, width)".to_string());
            return Err(());
        }
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let value_str = self.compile_expr(args[0])?;
        Ok(format!("pad({value_str}, {w})"))
    }

    /// `sext(value, width)`: widen `value` to `width` bits, filling the
    /// new high bits by REPLICATING the current top bit — real sign
    /// extension, unlike `zext`. `pad` on a `UInt` zero-extends (that's
    /// `compile_zext`, above); to get its `SInt` sign-extending behavior
    /// instead, `value` is cast to `SInt` first (`asSInt`), padded there,
    /// then cast back to this language's normal `UInt` representation
    /// (`asUInt`) — the identical `asUInt(pad(asSInt(...), w))` shape
    /// `compile_shift`'s `AShr` case already uses and has confirmed
    /// against real firtool + simulation, reused here rather than
    /// re-deriving and re-verifying the same primop composition. `hint`:
    /// same reasoning as `compile_zext`'s own doc comment.
    fn compile_sext(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        if args.len() != 2 {
            self.error(span, "`sext` takes (value, width)".to_string());
            return Err(());
        }
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let value_str = self.compile_expr(args[0])?;
        Ok(format!("asUInt(pad(asSInt({value_str}), {w}))"))
    }

    /// `prio(reqs)`: a fixed-priority encoder over `reqs`'s bits — the
    /// LOWEST set bit wins (bit 0 highest priority), matching the
    /// classic fixed-priority-arbiter convention. `reqs = 0` returns
    /// `0`, a defined but not-meaningful value; gating on `reqs <> 0`
    /// (when that matters) is the CALLER's job, the same way a `fails`
    /// precondition is established by the caller, not the failing
    /// primop itself. Built as a right-nested `mux` chain, checked from
    /// bit 0 outward (bit 0's `mux` is OUTERMOST, so it wins ties):
    /// `mux(bit0, 0, mux(bit1, 1, mux(bit2, 2, ... UInt(0))))`.
    pub(crate) fn compile_prio(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let Some(&reqs) = args.first() else {
            self.error(span, "`prio` takes exactly one argument".to_string());
            return Err(());
        };
        let n = self.concrete_width_of(reqs);
        let reqs_str = self.compile_expr(reqs)?;
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let mut acc = format!("UInt<{w}>(0)");
        for i in (0..n).rev() {
            let bit = format!("bits({reqs_str}, {i}, {i})");
            acc = format!("mux({bit}, UInt<{w}>({i}), {acc})");
        }
        Ok(acc)
    }

    /// `popcount(bits)`: the number of set bits, built as a plain `add`
    /// chain over each individual bit — no dedicated FIRRTL primop for
    /// this exists, unlike `prio`'s `mux` chain or `pack`'s `cat`. Each
    /// bit is widened to the RESULT's own width (`w`, from `types.rs`'s
    /// `popcount_result_width`) before summing — but FIRRTL's `add`
    /// still GROWS by one bit per operation (`max(w1, w2) + 1`, the
    /// carry-out bit), unlike `prio`'s `mux` chain, which never widens.
    /// `w` bits is already exactly enough to hold the true sum (that's
    /// `popcount_result_width`'s whole job), so the accumulated string's
    /// own inflated FIRRTL-inferred width (up to `w + n` after `n`
    /// additions) is discarded bits of zero, never a real value this
    /// throws away — but leaving it inflated would still be wrong: a
    /// caller nested further arithmetic around this call (`compile_
    /// binop`) reasons about ITS width purely from `types.rs`'s `w`, so
    /// splicing in a wider actual expression would silently combine at
    /// the WRONG width there. `bits(acc, w-1, 0)` at the end brings the
    /// string back down to exactly `w`, the same "truncate before
    /// handing back" step `compile_unop`'s `Neg` case already takes for
    /// the identical reason (`sub`'s own carry-out bit).
    fn compile_popcount(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let Some(&value) = args.first() else {
            self.error(span, "`popcount` takes exactly one argument".to_string());
            return Err(());
        };
        let n = self.concrete_width_of(value);
        let value_str = self.compile_expr(value)?;
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let mut acc = format!("UInt<{w}>(0)");
        for i in 0..n {
            let bit = format!("pad(bits({value_str}, {i}, {i}), {w})");
            acc = format!("add({acc}, {bit})");
        }
        Ok(format!("bits({acc}, {}, 0)", w.saturating_sub(1)))
    }

    /// `reverse(bits)`: bit order flipped, same width — a `cat` chain
    /// identical in shape to `compile_pack`'s, except every "argument" is
    /// one single-bit slice of the SAME value rather than distinct
    /// expressions, walked from bit 0 (becomes the new high bit) up to
    /// the top bit (becomes the new low bit).
    fn compile_reverse(&mut self, id: ExprId, args: &[ExprId]) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let Some(&value) = args.first() else {
            self.error(span, "`reverse` takes exactly one argument".to_string());
            return Err(());
        };
        let n = self.concrete_width_of(value);
        let value_str = self.compile_expr(value)?;
        let mut acc = format!("bits({value_str}, 0, 0)");
        for i in 1..n {
            let bit = format!("bits({value_str}, {i}, {i})");
            acc = format!("cat({acc}, {bit})");
        }
        Ok(acc)
    }

    /// `rotl(value, n)`/`rotr(value, n)`: rotate by `n` bits, same width
    /// as `value`. `left`: true for `rotl`, false for `rotr` — one
    /// function for both directions, which share everything except which
    /// end of `value` moves first.
    ///
    /// A CONSTANT `n` (checked first, via `const_eval`) compiles to a
    /// single static two-slice `cat` — cheaper hardware than the general
    /// dynamic form below, and the shape every existing test pins. Rotate-
    /// left by constant `n`: the new high `w - n` bits are the value's own
    /// low `w - n` bits, and the new low `n` bits are the value's own high
    /// `n` bits — `rotr` by `n` is the same split with the two pieces (and
    /// their sizes) swapped, i.e. `rotl` by `w - n`. `n == 0` (after
    /// reducing `n` modulo the value's own width, so an amount `>= width`
    /// rotates the same as its remainder) is a special case: the `cat`
    /// formula would otherwise need a zero-width `bits` slice, which
    /// FIRRTL rejects.
    ///
    /// A DYNAMIC `n` (any other expression) uses the classic double-width
    /// trick instead, since FIRRTL's `bits` needs static bounds: `dup =
    /// cat(value, value)` is `2w` bits with `dup[j] == value[j mod w]` for
    /// every `j` in `[0, 2w)` (both halves of the concatenation are the
    /// SAME value) — so a `w`-bit window of `dup`, dynamically positioned,
    /// already IS a rotation, with no per-bit-position case-work. `rotr`
    /// by `n` is `dup`'s low `w` bits after shifting right by `n`
    /// (`(dup >> n)[i] == dup[i + n] == value[(i + n) mod w]`, exactly
    /// `rotr`'s definition); `rotl` by `n` is the same shift by `w - n`
    /// instead (derived the identical way, `dup[i - n + w] ==
    /// value[(i - n) mod w]`). `n` is reduced modulo `w` FIRST via `rem`
    /// (FIRRTL's own `rem(a, b)` result width is `min(width(a),
    /// width(b))`, sound here since a remainder is always `<= a` and
    /// `< b`) so `w - n_mod` always lands in `[1, w]` — never negative,
    /// never needing `n == 0`'s special case the constant path needs
    /// (shifting `dup` right by exactly `w`, `rotl`'s `n_mod == 0` case,
    /// correctly reads back `dup`'s own high half, `value` unchanged).
    /// Verified against real firtool + simulation across several runtime
    /// amounts, not just reasoned through (`examples/call_rotl_dynamic.tr`
    /// / `call_rotr_dynamic.tr`).
    fn compile_rotate(&mut self, id: ExprId, args: &[ExprId], left: bool) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let name = if left { "rotl" } else { "rotr" };
        if args.len() != 2 {
            self.error(span, format!("`{name}` takes (value, amount)"));
            return Err(());
        }
        let w = self.concrete_width_of(args[0]);
        let value_str = self.compile_expr(args[0])?;

        if let Some(raw_n) = self.const_eval(args[1]) {
            let n = raw_n % w;
            if n == 0 {
                return Ok(value_str);
            }
            let hi = if left { w - n } else { n };
            return Ok(format!(
                "cat(bits({value_str}, {}, 0), bits({value_str}, {}, {}))",
                hi - 1,
                w - 1,
                hi
            ));
        }

        let n_str = self.compile_expr(args[1])?;
        let dup = format!("cat({value_str}, {value_str})");
        let n_mod = format!("rem({n_str}, UInt<{w}>({w}))");
        let shamt = if left {
            format!("sub(UInt<{w}>({w}), {n_mod})")
        } else {
            n_mod
        };
        Ok(format!("tail(dshr({dup}, {shamt}), {w})"))
    }

    /// `mux(sel, a, b)`: `sel` selects `a` when 1, `b` when 0 — a direct,
    /// one-to-one lowering to FIRRTL's own `mux` primop, which already
    /// pads the narrower of `a`/`b` to the wider one itself (the same
    /// rule `types.rs`'s own `"mux"` arm computes as this call's result
    /// width), so no explicit padding is needed here.
    fn compile_mux(&mut self, id: ExprId, args: &[ExprId]) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        if args.len() != 3 {
            self.error(span, "`mux` takes (sel, a, b)".to_string());
            return Err(());
        }
        let sel = self.compile_expr(args[0])?;
        let a = self.compile_expr(args[1])?;
        let b = self.compile_expr(args[2])?;
        Ok(format!("mux({sel}, {a}, {b})"))
    }

    /// `max(a, b, ...)`/`min(a, b, ...)`: when `types.rs` typed this call
    /// `Ty::Int` (every argument itself a compile-time constant), this is
    /// the same deliberate, explicit gap `clog2`/`len` already have —
    /// `max`/`min` exist there to fold inside a type-position width
    /// expression (`types/eval.rs`'s `const_eval_expr`), not to become a
    /// circuit that compares two constants. Otherwise (at least one
    /// argument a real `Bits` value), a left fold of pairwise `mux(cmp,
    /// next, acc)` — `max` keeps `next` when `gt(next, acc)`, `min` when
    /// `lt(next, acc)`; ties don't matter, since either arm of an equal
    /// comparison is the same value. Every argument is compiled HINTED
    /// to the call's own result width `w` (not `compile_expr` bare):
    /// a real `Bits` operand ignores the hint and emits at its own
    /// declared width regardless (FIRRTL's `gt`/`lt` already compare
    /// differently-width `UInt` operands correctly) — but a bare
    /// INT-typed argument absorbed into a wider `Bits` sibling (`max(a,
    /// 10)`, `a : [8]`) has no width of its own at all (`Expr::Int`'s own
    /// `compile_expr` arm needs a hint or `width_of` to know what to
    /// emit), so skipping the hint here would make EXACTLY that shape
    /// fail despite `types.rs` already having accepted it (`check_
    /// literal_fits` there confirms `10` fits `w`).
    ///
    /// The trailing `bits(acc, w-1, 0)` matters in a way `compile_mux`
    /// above never needed: FIRRTL's own `mux` result width is `max` of
    /// its two arms' widths, REGARDLESS of which builtin built it — fine
    /// for `max` (`types.rs`'s own width rule there is ALSO the max of
    /// every `Bits` argument, so the chain's natural width already
    /// equals `w`) but wrong for `min`, whose declared width is instead
    /// the NARROWEST argument's own width (`min(a, b)` can never exceed
    /// either operand, so it never needs more bits than the smaller one
    /// — `types.rs`'s own doc comment on this). A `min(a, b)` with `a :
    /// [4]`, `b : [8]` would otherwise emit an 8-bit `mux` chain while
    /// `types.rs` declared the call `[4]`, silently lying about its own
    /// width the identical way `compile_popcount`'s own missing
    /// truncation once did — caught by advisor before shipping, not by
    /// this file's own tests, which (before this fix) only ever compiled
    /// EQUAL-width `max`/`min` arguments.
    fn compile_max_min(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
        is_max: bool,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let name = if is_max { "max" } else { "min" };
        if args.len() < 2 {
            self.error(span, format!("`{name}` takes at least two arguments"));
            return Err(());
        }
        if matches!(self.types.expr_tys.get(&id), Some(Ty::Int)) {
            self.error(
                span,
                format!(
                    "calling the builtin `{name}` is not yet supported in FIRRTL emission \
                     when every argument is a compile-time constant (v0 restriction); use \
                     `bits[{name}(n, m)]` instead, since `{name}` is foldable directly \
                     inside a type-position width expression"
                ),
            );
            return Err(());
        }
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let mut acc = self.compile_expr_hinted(args[0], Some(w))?;
        for &a in &args[1..] {
            let next = self.compile_expr_hinted(a, Some(w))?;
            let cmp = if is_max {
                format!("gt({next}, {acc})")
            } else {
                format!("lt({next}, {acc})")
            };
            acc = format!("mux({cmp}, {next}, {acc})");
        }
        Ok(format!("bits({acc}, {}, 0)", w.saturating_sub(1)))
    }

    /// `__race_value(d1, r1, d2, r2, ...)`: lower.rs's own rewrite of a
    /// value-producing `race[...]` at render time (never written by a
    /// user — a real `race[h1, h2]` in trace source never reaches this
    /// point; it's macro-expanded away before emission, same as `sync`).
    /// A right-nested priority `mux`, the same shape `compile_prio`
    /// builds: `d1`'s pair is OUTERMOST, so it wins a tie against a
    /// later pair, matching `race`'s own declared-first-wins convention.
    /// The LAST pair's own `d` is never checked — by construction, the
    /// segment this sits in already guarded on "at least one is done",
    /// so if every earlier pair's `d` was false, the last one must be
    /// true. This is why the trace-source `if`/`else` splice this
    /// replaces doesn't work (a local first bound inside `if`/`else`
    /// doesn't resolve outside it) but a raw FIRRTL `mux` does: `mux` is
    /// an ordinary combinational expression, substitutable anywhere,
    /// with none of `if`/`else`'s statement-level scoping.
    ///
    /// The tie-break priority this builds is, honestly, defensive: for
    /// handles all named in ONE `race[...]` group, every one of their
    /// own segments already reads every OTHER named handle's `done`
    /// (the cancellation mechanism `render_rule` builds), which makes
    /// any two of their own final segments mutually conflict in the
    /// ordinary scheduler — so at most one of them can EVER actually
    /// become done, full stop. Two `d`s both reading 1 here should
    /// never actually happen for this construct's real use. Built as a
    /// real priority mux anyway, not left as an unchecked assumption:
    /// correct and cheap either way, and a safe fallback if that
    /// invariant is ever weakened later. Confirmed via real simulation
    /// that this path is unreachable today, not just reasoned about —
    /// see sim/race_value_tb.v's own comment.
    pub(crate) fn compile_race_value(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        if args.len() < 2 || !args.len().is_multiple_of(2) {
            self.error(
                span,
                "`__race_value` needs an even, non-empty list of (done, result) \
                 pairs (a lower.rs codegen bug, not a source mistake — this \
                 builtin is never user-written)"
                    .to_string(),
            );
            return Err(());
        }
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let pairs: Vec<(ExprId, ExprId)> = args.chunks(2).map(|c| (c[0], c[1])).collect();
        let (&(_, last_result), rest) = pairs.split_last().expect("checked non-empty above");
        let mut acc = self.compile_expr_hinted(last_result, Some(w))?;
        for &(done, result) in rest.iter().rev() {
            let done_str = self.compile_expr(done)?;
            let result_str = self.compile_expr_hinted(result, Some(w))?;
            acc = format!("mux(eq({done_str}, UInt<1>(1)), {result_str}, {acc})");
        }
        Ok(acc)
    }

    /// Binds `params` to `args` (a call's actual arguments) AND `body`'s
    /// own top-level `let`s (their init expressions, substituted
    /// wherever referenced later — the same "splice the whole init
    /// expression at each use site" strategy `compile_callee_body`'s own
    /// locals loop uses, not "compute once, reuse the compiled string")
    /// into `self.locals`, saving whatever was there before so it can be
    /// restored via `restore_callee_context`. Shared by `callee_fail_
    /// cond` (a guard referencing a preceding callee-local, not just a
    /// param, needs this too — `let y = x + 1 / (y <> 0)?` previously
    /// failed to resolve `y` at all) and `fifo.rs`'s `compile_fifo_op_
    /// value` (an `Enq`'s value referencing a `let`-bound `Deq` result
    /// from earlier in the same callee, the "bridge" pattern).
    pub(crate) fn bind_callee_context(
        &mut self,
        params: &[Param],
        args: &[ExprId],
        body: &[StmtId],
    ) -> Vec<(DefId, Option<ExprId>)> {
        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for (param, arg) in params.iter().zip(args.iter()) {
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == param.name.span)
            {
                let def = DefId(i as u32);
                saved.push((def, self.locals.insert(def, *arg)));
            }
        }
        for stmt in body {
            if let Stmt::Let { name, init } = self.ast.stmt(*stmt)
                && let Some((i, _)) = self
                    .res
                    .defs
                    .iter()
                    .enumerate()
                    .find(|(_, d)| d.span == name.span)
            {
                let def = DefId(i as u32);
                saved.push((def, self.locals.insert(def, *init)));
            }
        }
        saved
    }

    pub(crate) fn restore_callee_context(&mut self, saved: Vec<(DefId, Option<ExprId>)>) {
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
    }

    /// If `call_expr` (a `Call` with the given `callee`/`args`) targets a
    /// function whose own effect signature can fail, folds that
    /// failure's condition into the caller's own guard: binds `callee`'s
    /// params (and its own top-level `let`s) to this call's context via
    /// `bind_callee_context` — the same reentrant save/restore
    /// `compile_call` uses for a return value, so a guard written in
    /// terms of a parameter OR a preceding callee-local compiles against
    /// the actual substituted expression, not an unbound name — then
    /// compiles the callee's own top-level guard(s), AND-reduced. `None`
    /// if `callee` isn't a fails-ing call at all — an ordinary call has
    /// nothing to fold. A fifo op's own guard contribution is NOT
    /// handled here — see fifo.rs's `rule_fifo_ops`, folded separately
    /// by `compile_guard` (writes.rs) so it can be combined with the
    /// rule's OTHER touches of the same fifo (the Enq+Deq pass-through
    /// case) before deciding the actual guard term.
    ///
    /// `validate_call` is the single authority on whether folding this
    /// call is even sound; this re-runs it (safe — `Emitter::error`
    /// dedupes by (span, message), so a call already rejected there
    /// doesn't double-report) purely to reuse its resolution/validation,
    /// and returns `None` on failure since the call's own body
    /// compilation (elsewhere, when this statement's RHS is compiled)
    /// will already surface the real error.
    pub(crate) fn callee_fail_cond(
        &mut self,
        call_expr: ExprId,
        callee: ExprId,
        args: &[ExprId],
    ) -> Option<String> {
        let def = *self.res.expr_defs.get(&callee)?;
        if !matches!(self.res.def(def).kind, DefKind::Fn | DefKind::Impl) {
            return None;
        }
        let fn_item = *self
            .res
            .item_defs
            .iter()
            .find(|(_, d)| **d == def)
            .map(|(item, _)| item)?;
        let sig = self.fx.sigs.get(&fn_item)?;
        if !sig.fails {
            return None;
        }
        let span = self.ast.expr_spans[call_expr.0 as usize].clone();
        let Ok((_, params, body)) = self.validate_call(span, callee) else {
            return None;
        };
        let saved = self.bind_callee_context(&params, args, &body);
        let mut conds = Vec::new();
        for stmt in &body {
            if let Stmt::Expr(e) = self.ast.stmt(*stmt).clone() {
                if let Expr::Guard(inner) = self.ast.expr(e) {
                    let inner = *inner;
                    conds.push(self.compile_guard_unwrap_cond(inner));
                } else if is_guard_like(self.ast, self.res, e) {
                    // An implicit guard: `e` itself IS the condition, no
                    // `Guard` wrapper to unwrap -- `compile_guard_
                    // unwrap_cond` handles a bare comparison correctly
                    // here too (same fix as `compile_guard`'s own
                    // implicit-guard fold, writes.rs), not `compile_
                    // expr` directly, which would give a comparison's
                    // `lhs` VALUE instead of its boolean.
                    conds.push(self.compile_guard_unwrap_cond(e));
                }
            }
        }
        self.restore_callee_context(saved);
        conds.into_iter().reduce(|a, b| format!("and({a}, {b})"))
    }

    /// Compiles a callee's body (or an `if`/`else` branch of one, which
    /// has the identical shape) to a single value: zero or more `let`
    /// bindings, then either a trailing `return <expr>` or an `if`/`else`
    /// whose branches both recurse into this same shape. An `if` with no
    /// `else` is rejected — every reachable path must produce a value,
    /// there's no such thing as a "held" return the way an unwritten
    /// register path holds its own feedback. Each branch's `let`s are
    /// bound/restored around that branch's own recursive call, so they
    /// never leak into a sibling branch or the caller.
    pub(crate) fn compile_callee_body(
        &mut self,
        stmts: &[StmtId],
        hint: u64,
        span: &Span,
    ) -> Result<String, ()> {
        let Some((&last, rest)) = stmts.split_last() else {
            self.error(
                span.clone(),
                "calling a function with an empty body (or an empty `if`/`else` \
                 branch) is not yet supported in FIRRTL emission (v0 restriction: \
                 every branch must end with `return`)"
                    .to_string(),
            );
            return Err(());
        };
        if !rest.iter().all(|s| match self.ast.stmt(*s) {
            Stmt::Let { .. } | Stmt::Assign { .. } => true,
            // A bare-statement CALL (its return value unused, e.g. for a
            // side-effecting write) is allowed; nothing else bare — a
            // guard (explicit `?` or implicit, `resolve::is_guard_like`)
            // or fifo op would already have set `sig.fails`, rejected
            // by `validate_call` before this ever runs, so this is
            // deliberately narrower than "any Expr statement".
            Stmt::Expr(e) => {
                matches!(self.ast.expr(*e), Expr::Call { .. })
                    || self.fifo_op(*e).is_some()
                    || is_guard_like(self.ast, self.res, *e)
            }
            _ => false,
        }) {
            self.error(
                span.clone(),
                "this function's body is too complex to inline for its RETURN value \
                 (v0 restriction: only `let` bindings, state writes, and bare-\
                 statement calls may come before a trailing `return`, or a trailing \
                 `if`/`else` whose branches both end that way — an `if`/`else` \
                 anywhere else, a loop, or a fifo/guard operation is not supported \
                 here). Called as a bare statement, with its return value unused, a \
                 conditional write like this one IS still supported — see \
                 `callee_reg_write`/`callee_port_write`, a separate walk that \
                 doesn't share this restriction"
                    .to_string(),
            );
            return Err(());
        }
        // A leading `Assign` (a direct STATE write — `validate_call`'s own
        // `check_no_reassigned_locals_in_callee_body` call already ruled
        // out a LOCAL reassignment reaching this far) or bare-statement
        // `Expr` (a call, possibly writing state transitively) is a
        // side effect this walk — which only ever builds the RETURN
        // value — doesn't care about: its own value/write is found
        // separately, by `callee_reg_write`/`callee_port_write`, when
        // some register's/port's own write-threading walk reaches this
        // same statement. `validate_call`'s own
        // `check_writing_call_positions_in` call already confirmed any
        // writing call in this body sits in one of the two positions
        // that walk actually looks in, so nothing here needs to
        // re-check that.

        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for s in rest {
            let Stmt::Let { name, init } = self.ast.stmt(*s) else {
                continue;
            };
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == name.span)
            {
                let def = DefId(i as u32);
                saved.push((def, self.locals.insert(def, *init)));
            }
        }

        let result = match self.ast.stmt(last).clone() {
            Stmt::Return(Some(ret_expr)) => self.compile_expr_hinted(ret_expr, Some(hint)),
            Stmt::If {
                cond,
                then_body,
                else_body: Some(else_body),
            } => match (
                self.compile_callee_body(&then_body, hint, span),
                self.compile_callee_body(&else_body, hint, span),
            ) {
                (Ok(t), Ok(e)) => {
                    let cond_str = self.compile_guard_unwrap_cond(cond);
                    Ok(format!("mux({cond_str}, {t}, {e})"))
                }
                _ => Err(()),
            },
            Stmt::If {
                else_body: None, ..
            } => {
                self.error(
                    span.clone(),
                    "an `if` inside an inlined function's body must have an `else` \
                     (v0 restriction: every reachable path must produce a value, \
                     there is no way to \"hold\" a return the way an unwritten \
                     register path holds its own feedback)"
                        .to_string(),
                );
                Err(())
            }
            Stmt::IfLet {
                name,
                init,
                then_body,
                else_body: Some(else_body),
            } => {
                // See writes.rs's identical match for why this isn't a
                // plain `let Expr::Guard(opt) = ... else { unreachable!()
                // }` anymore: a bare `fifo.Deq[]`/failing-call if-let init
                // is never Guard-wrapped.
                let opt = match self.ast.expr(init).clone() {
                    Expr::Guard(inner) => inner,
                    _ if self.fifo_op(init).is_some()
                        || matches!(self.ast.expr(init), Expr::Call { .. }) =>
                    {
                        init
                    }
                    _ => unreachable!(
                        "types.rs requires an `if let` init to be `opt?`, a fifo `Deq[]`, \
                         or a failing call"
                    ),
                };
                let def = def_of_name(self.res, &name);
                let prev = self.if_let_binds.insert(def, opt);
                let then_result = self.compile_callee_body(&then_body, hint, span);
                match prev {
                    Some(p) => {
                        self.if_let_binds.insert(def, p);
                    }
                    None => {
                        self.if_let_binds.remove(&def);
                    }
                }
                match (
                    then_result,
                    self.compile_callee_body(&else_body, hint, span),
                ) {
                    (Ok(t), Ok(e)) => {
                        let cond_str = self.compile_guard_unwrap_cond(opt);
                        Ok(format!("mux({cond_str}, {t}, {e})"))
                    }
                    _ => Err(()),
                }
            }
            Stmt::IfLet {
                else_body: None, ..
            } => {
                self.error(
                    span.clone(),
                    "an `if let` inside an inlined function's body must have an \
                     `else` (v0 restriction: every reachable path must produce a \
                     value, there is no way to \"hold\" a return the way an \
                     unwritten register path holds its own feedback)"
                        .to_string(),
                );
                Err(())
            }
            _ => {
                self.error(
                    span.clone(),
                    "this function's body must end with `return <expr>`, or an \
                     `if`/`else` whose branches both do (v0 restriction)"
                        .to_string(),
                );
                Err(())
            }
        };

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
        result
    }

    /// A struct- or `?T`-returning callee's return value, decomposed to
    /// just ONE leaf field's own value -- the return-value analogue of
    /// `struct_field_value_in_stmts`'s per-field write-threading over a
    /// rule body, called once per leaf field from `compile_field_path_
    /// value`'s new `Expr::Call` case (writes.rs) rather than once for
    /// the whole struct: `compile_callee_body` builds ONE FIRRTL string
    /// (a scalar return has exactly one leaf), so a struct/Option return
    /// instead calls this once per flat field, each call independently
    /// re-walking the body and re-binding params/lets via `bind_callee_
    /// context`/`restore_callee_context` -- sharing one binding across
    /// leaves is the exact reentrancy bug `Avg(Avg(x, y), z)` already
    /// taught this codebase not to repeat.
    ///
    /// Same body shape as `compile_callee_body` (`let`s and bare-
    /// statement calls, then a trailing `return`, or an `if`/`else`
    /// whose branches both recurse and combine via a per-leaf `mux`) --
    /// deliberately not re-validated with `compile_callee_body`'s own
    /// specific wording here; an unsupported shape returns `None`, and
    /// the caller (`compile_field_path_value`) is the one that turns
    /// that into a real, emitted error, never a silently-dropped write.
    pub(crate) fn compile_callee_body_field(
        &mut self,
        stmts: &[StmtId],
        path: &[String],
        root_ty: &Ty,
        width: u64,
    ) -> Option<String> {
        let (&last, rest) = stmts.split_last()?;

        let mut saved: Vec<(DefId, Option<ExprId>)> = Vec::new();
        for s in rest {
            let Stmt::Let { name, init } = self.ast.stmt(*s) else {
                continue;
            };
            if let Some((i, _)) = self
                .res
                .defs
                .iter()
                .enumerate()
                .find(|(_, d)| d.span == name.span)
            {
                let def = DefId(i as u32);
                saved.push((def, self.locals.insert(def, *init)));
            }
        }

        let result = match self.ast.stmt(last).clone() {
            Stmt::Return(Some(ret_expr)) => {
                // `return p` (`p` this callee's OWN struct/`?T` param,
                // UNCHANGED) is the return-side twin of `compile_struct_
                // field_read`'s param chase-through -- handled HERE,
                // specifically, rather than as a general `Expr::Ident`
                // case inside `compile_field_path_value` itself: that
                // function is ALSO reached from `compile_struct_field_
                // read`'s pre-existing Local-arm fallback (an ordinary
                // struct field READ off a local aliasing something
                // unresolvable), and putting the chase-through there
                // instead fires in THAT context too, silently
                // relegalizing a callee-local-aliases-a-param
                // (`let x = p; return x.data`) -- a pattern `a_callee_
                // local_aliasing_a_struct_typed_param_is_rejected` pins
                // as rejected. Checking `ret_expr` directly, right here,
                // means only a LITERAL `return p` (never a param reached
                // by chasing through some intermediate local) resolves
                // this way. Gated on `ret_expr`'s type EXACTLY matching
                // `root_ty` for the same reason `compile_struct_field_
                // read`'s own gate is: an ordinary `T`-into-`?T`
                // present-coercion (`return x`, `x : bits[8]`, this fn
                // returns `?bits[8]`) has a DIFFERENT type and must fall
                // through to `compile_field_path_value`'s own coercion-
                // synthesis path below instead.
                if matches!(self.ast.expr(ret_expr), Expr::Ident(_))
                    && self.types.expr_tys.get(&ret_expr) == Some(root_ty)
                    && matches!(
                        self.res
                            .expr_defs
                            .get(&ret_expr)
                            .map(|d| self.res.def(*d).kind),
                        Some(DefKind::Param)
                    )
                {
                    self.compile_struct_field_read(ret_expr, ret_expr, path, Some(width))
                        .ok()
                } else {
                    self.compile_field_path_value(ret_expr, path, root_ty, width)
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body: Some(else_body),
            } => match (
                self.compile_callee_body_field(&then_body, path, root_ty, width),
                self.compile_callee_body_field(&else_body, path, root_ty, width),
            ) {
                (Some(t), Some(e)) => {
                    let cond_str = self.compile_guard_unwrap_cond(cond);
                    Some(format!("mux({cond_str}, {t}, {e})"))
                }
                _ => None,
            },
            Stmt::IfLet {
                name,
                init,
                then_body,
                else_body: Some(else_body),
            } => {
                // See writes.rs's identical match for why this isn't a
                // plain `let Expr::Guard(opt) = ... else { unreachable!()
                // }` anymore: a bare `fifo.Deq[]`/failing-call if-let init
                // is never Guard-wrapped.
                let opt = match self.ast.expr(init).clone() {
                    Expr::Guard(inner) => inner,
                    _ if self.fifo_op(init).is_some()
                        || matches!(self.ast.expr(init), Expr::Call { .. }) =>
                    {
                        init
                    }
                    _ => unreachable!(
                        "types.rs requires an `if let` init to be `opt?`, a fifo `Deq[]`, \
                         or a failing call"
                    ),
                };
                let def = def_of_name(self.res, &name);
                let prev = self.if_let_binds.insert(def, opt);
                let then_result = self.compile_callee_body_field(&then_body, path, root_ty, width);
                match prev {
                    Some(p) => {
                        self.if_let_binds.insert(def, p);
                    }
                    None => {
                        self.if_let_binds.remove(&def);
                    }
                }
                match (
                    then_result,
                    self.compile_callee_body_field(&else_body, path, root_ty, width),
                ) {
                    (Some(t), Some(e)) => {
                        let cond_str = self.compile_guard_unwrap_cond(opt);
                        Some(format!("mux({cond_str}, {t}, {e})"))
                    }
                    _ => None,
                }
            }
            _ => None,
        };

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
        result
    }

    /// Entry point for `compile_field_path_value`'s `Expr::Call` case
    /// (writes.rs): binds `callee`'s params/lets to this specific call's
    /// arguments via `bind_callee_context` (reentrant -- safe to call
    /// once per leaf field, see `compile_callee_body_field`'s own doc
    /// comment), then extracts just `path`'s leaf value from the
    /// callee's return. `root_ty` is the CALLER's already-type-checked
    /// target type (the reg/field this call's result is being written
    /// into) -- trusted directly rather than re-derived from the
    /// callee's own declared return type, since `type_write`'s
    /// `check_assignable` already proved the two match before emission
    /// ever runs.
    pub(crate) fn compile_call_field_value(
        &mut self,
        call_expr: ExprId,
        callee: ExprId,
        args: &[ExprId],
        path: &[String],
        root_ty: &Ty,
        width: u64,
    ) -> Option<String> {
        let span = self.ast.expr_spans[call_expr.0 as usize].clone();
        let errors_before = self.errors.len();
        let (_, params, body) = self.validate_call(span.clone(), callee).ok()?;
        let saved = self.bind_callee_context(&params, args, &body);
        let result = self.compile_callee_body_field(&body, path, root_ty, width);
        self.restore_callee_context(saved);
        // `validate_call` always emits its own error before returning
        // `Err` (see its own doc comment), so a `None` result reaching
        // here with no new error recorded means `compile_callee_body_
        // field` itself hit an unsupported body shape -- give that its
        // own message rather than letting it surface as the CALLER's
        // generic "cannot resolve this field" (or, worse for a write,
        // silently drop the write entirely the way the pre-fix aliasing
        // bug did).
        if result.is_none() && self.errors.len() == errors_before {
            self.error(
                span,
                "this function's struct/`?T` return value is too complex to inline \
                 here (v0 restriction: the callee's body must end with `return \
                 <expr>`, or an `if`/`else` whose branches both do)"
                    .to_string(),
            );
        }
        result
    }
}
