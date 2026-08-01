//! Inlining a call to a user `fn`/`impl` or the builtin `prio` — FIRRTL
//! has no call concept, so `compile_call`/`compile_builtin_call` splice
//! the callee in at its call site instead of emitting it as its own
//! hardware. `validate_call` is the single choke point every inlining
//! entry point (here, and writes.rs's write-hunt) goes through: effect
//! coloring, the call-site module boundary, a call-CYCLE check
//! (`find_call_cycle` — a callee may itself call another callee, just
//! not one that transitively calls back to itself), and — reused from
//! checks.rs, since it's already generic over any statement list, not
//! rule-specific — `check_writing_call_positions_in` against THIS
//! callee's own body, so a writing call inside it sits in a position
//! `callee_reg_write`/`callee_port_write` (writes.rs) can actually find.
//! `compile_callee_body` builds the callee's return value; `compile_prio`
//! is the one synthesizable builtin, a fixed-priority `mux` chain.

use super::Emitter;
use super::writes::item_name;
use crate::ast::{Ast, Expr, ExprId, Item, ItemId, Param, Stmt, StmtId};
use crate::lexer::Span;
use crate::resolve::{DefId, DefKind, Resolution};

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
                Stmt::Return(None) | Stmt::Tick => {}
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
                Stmt::While { cond, body } => {
                    collect_calls(ast, *cond, out);
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
        Expr::Guard(inner) | Expr::Spawn(inner) => collect_calls(ast, *inner, out),
        Expr::Field { base, .. } => collect_calls(ast, *base, out),
        Expr::Bracket { callee, args } => {
            collect_calls(ast, *callee, out);
            for a in args {
                collect_calls(ast, *a, out);
            }
        }
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
            self.error(
                span,
                "calling a function that can fail (a guard in its body) is not yet \
                 supported in FIRRTL emission (v0 restriction: an inlined call cannot \
                 gate the caller's rule)"
                    .to_string(),
            );
            return Err(());
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
        let errors_before = self.errors.len();
        self.check_writing_call_positions_in(&body);
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
    /// one needs its own hand-written FIRRTL construction. `prio` is the
    /// only one synthesizable today; the rest (`bits`/`wire`/`list`/`any`
    /// never reach here at all — `bits[N]` is a type-position construct,
    /// `any` is spec/`chooses`-only, both handled entirely by types.rs/
    /// effects.rs before emission — and `clog2`/`trunc`/`pack`/`len` are
    /// real gaps but have no in-repo caller yet) fall through to an
    /// explicit error.
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
            "__race_value" => self.compile_race_value(id, args, hint),
            name => {
                self.error(
                    self.ast.expr_spans[id.0 as usize].clone(),
                    format!(
                        "calling the builtin `{name}` is not yet supported in FIRRTL \
                         emission (v0 restriction: only `prio` — a fixed-priority \
                         encoder — `trunc` — bit truncation — and `pack` — \
                         concatenation — are synthesizable today)"
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

    /// `trunc(value, width)`: the low `width` bits of `value` — exactly
    /// `bits(value, width-1, 0)`, the same FIRRTL `bits` primop
    /// `compile_bit_select` already uses for `x[hi..lo]`, just reached
    /// through a different spelling. `width` must be const-evaluable
    /// (types.rs's own restriction, `type_builtin_call`'s `"trunc"` arm:
    /// a non-const width types as `Bits(Width::Unknown)`, which
    /// `width_of` rejects with its own explicit error) — no separate
    /// check needed here.
    fn compile_trunc(
        &mut self,
        id: ExprId,
        args: &[ExprId],
        hint: Option<u64>,
    ) -> Result<String, ()> {
        let span = self.ast.expr_spans[id.0 as usize].clone();
        let [value, _width] = args else {
            self.error(
                span,
                "`trunc` takes exactly two arguments: (value, width)".to_string(),
            );
            return Err(());
        };
        let w = hint.unwrap_or_else(|| self.width_of(id));
        let value_str = self.compile_expr(*value)?;
        Ok(format!("bits({value_str}, {}, 0)", w.saturating_sub(1)))
    }

    /// `prio(reqs)`: a fixed-priority encoder over `reqs`'s bits — the
    /// LOWEST set bit wins (bit 0 highest priority), matching the
    /// classic fixed-priority-arbiter convention. `reqs == 0` returns
    /// `0`, a defined but not-meaningful value; gating on `reqs != 0`
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
            // guard or fifo op would already have set `sig.fails`,
            // rejected by `validate_call` before this ever runs, so
            // this is deliberately narrower than "any Expr statement".
            Stmt::Expr(e) => matches!(self.ast.expr(*e), Expr::Call { .. }),
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
        // A leading `Assign` (a direct state write) or bare-statement
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
                    let cond_str = self
                        .compile_expr(cond)
                        .unwrap_or_else(|_| "UInt<1>(0)".to_string());
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
}
